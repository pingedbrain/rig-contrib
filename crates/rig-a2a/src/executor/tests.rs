use rig_core::message::{AssistantContent, DocumentSourceKind, Image, ImageMediaType};

use super::*;

#[test]
fn failures_are_classified_by_status_and_retryability() {
    use rig_core::error::{ErrorKind, ErrorReport};
    let report = |status: Option<u16>, retryable: bool| {
        let mut report = ErrorReport::new(ErrorKind::ProviderResponse, "upstream said no")
            .with_retryable(retryable);
        if let Some(status) = status {
            report = report.with_http_status(status);
        }
        PromptError::Report(report)
    };
    assert!(failure_message(&report(Some(429), true)).contains("rate limiting"));
    assert!(failure_message(&report(Some(401), false)).contains("credentials"));
    assert!(failure_message(&report(Some(503), true)).contains("unavailable"));
    assert!(failure_message(&report(None, true)).contains("could not be reached"));
    assert_eq!(
        failure_message(&report(None, false)),
        "The agent run failed."
    );
}

#[test]
fn model_images_become_file_parts_after_the_text() {
    let image = |data| {
        AssistantContent::Image(Image {
            data,
            media_type: Some(ImageMediaType::PNG),
            ..Default::default()
        })
    };
    let content = [
        AssistantContent::text("here"),
        image(DocumentSourceKind::base64("aGk=")),
        image(DocumentSourceKind::url("https://x/a.png")),
        image(DocumentSourceKind::file_id("file-1")),
    ];
    let parts = answer_parts("here", &content);
    assert_eq!(
        parts.len(),
        3,
        "text, the decoded image, the URL image; no file id"
    );
    assert_eq!(parts.first().and_then(Part::as_text), Some("here"));
    let image = parts.get(1).unwrap();
    assert_eq!(image.content, a2a::PartContent::Raw(b"hi".to_vec()));
    assert_eq!(image.media_type.as_deref(), Some("image/png"));
    assert_eq!(
        parts.get(2).map(|part| &part.content),
        Some(&a2a::PartContent::Url("https://x/a.png".into()))
    );
}
