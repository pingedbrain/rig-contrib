//! A2A message parts as Rig user content.

use a2a::{Part, PartContent};
use rig_core::message::{
    Audio, AudioMediaType, DocumentContent, DocumentData, DocumentMediaType, DocumentSourceKind,
    Image, ImageMediaType, Message, MimeType, UserContent, Video, VideoMediaType,
};

/// A message part that cannot be passed to a model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum UnsupportedPart {
    /// The message has no parts.
    #[error("the message has no parts")]
    Empty,
    /// A file part's media type has no Rig content kind.
    #[error("the media type `{0}` is not supported")]
    MediaType(String),
    /// A file URL part without a media type.
    #[error("a file URL part needs a mediaType")]
    UntypedUrl,
    /// Raw bytes without a media type that are neither a known image nor text.
    #[error("raw bytes without a mediaType must be an image or UTF-8 text")]
    UntypedBytes,
}

/// The user message for an A2A message: text and structured data as text,
/// files as image, audio, video or document content by their media type.
pub(crate) fn user_message(message: &a2a::Message) -> Result<Message, UnsupportedPart> {
    if message.parts.is_empty() {
        return Err(UnsupportedPart::Empty);
    }
    let content = message
        .parts
        .iter()
        .map(user_content)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Message::User { content })
}

fn user_content(part: &Part) -> Result<UserContent, UnsupportedPart> {
    let media_type = part.media_type.as_deref().filter(|media| !media.is_empty());
    match &part.content {
        PartContent::Text(text) => Ok(UserContent::text(text)),
        PartContent::Data(value) => Ok(UserContent::text(value.to_string())),
        PartContent::Raw(bytes) => raw_content(bytes, media_type),
        PartContent::Url(url) => {
            let media_type = media_type.ok_or(UnsupportedPart::UntypedUrl)?;
            file_content(DocumentSourceKind::url(url), media_type)
                .ok_or_else(|| UnsupportedPart::MediaType(media_type.to_owned()))
        }
    }
}

fn raw_content(bytes: &[u8], media_type: Option<&str>) -> Result<UserContent, UnsupportedPart> {
    let text = std::str::from_utf8(bytes).ok();
    let Some(media_type) = media_type else {
        return match (ImageMediaType::sniff(bytes), text) {
            (Some(kind), _) => Ok(image(DocumentSourceKind::Raw(bytes.to_vec()), kind)),
            (None, Some(text)) => Ok(UserContent::text(text)),
            (None, None) => Err(UnsupportedPart::UntypedBytes),
        };
    };
    // A UTF-8 text document (anything but PDF) travels as its own text, which
    // every provider accepts; other files keep their bytes.
    if let (Some(kind), Some(text)) = (DocumentMediaType::from_mime_type(media_type), text)
        && kind != DocumentMediaType::PDF
    {
        return Ok(UserContent::Document(DocumentContent {
            data: DocumentData::Text(text.to_owned()),
            media_type: Some(kind),
            ..Default::default()
        }));
    }
    if media_type == "application/json"
        && let Some(text) = text
    {
        return Ok(UserContent::text(text));
    }
    file_content(DocumentSourceKind::Raw(bytes.to_vec()), media_type)
        .ok_or_else(|| UnsupportedPart::MediaType(media_type.to_owned()))
}

/// The content a file of `media_type` maps to, or `None` when Rig has no kind for it.
fn file_content(data: DocumentSourceKind, media_type: &str) -> Option<UserContent> {
    if let Some(kind) = ImageMediaType::from_mime_type(media_type) {
        return Some(image(data, kind));
    }
    if let Some(kind) = AudioMediaType::from_mime_type(media_type) {
        return Some(UserContent::Audio(Audio {
            data,
            media_type: Some(kind),
        }));
    }
    if let Some(kind) = VideoMediaType::from_mime_type(media_type) {
        return Some(UserContent::Video(Video {
            data,
            media_type: Some(kind),
            ..Default::default()
        }));
    }
    DocumentMediaType::from_mime_type(media_type).map(|kind| {
        UserContent::Document(DocumentContent {
            data: DocumentData::File(data),
            media_type: Some(kind),
            ..Default::default()
        })
    })
}

fn image(data: DocumentSourceKind, kind: ImageMediaType) -> UserContent {
    UserContent::Image(Image {
        data,
        media_type: Some(kind),
        ..Default::default()
    })
}
