//! Rig outputs as MCP tool results.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rig_core::message::{
    AssistantContent, DocumentSourceKind, Image, ImageMediaType, MimeType, ToolResultContent,
};
use rig_core::tool::ToolOutput;
use rmcp::model::{CallToolResult, ContentBlock};

/// A Rig tool's output as an MCP result: text and JSON as text blocks (a lone
/// JSON object also as `structuredContent`, which MCP requires to be an
/// object), images as image blocks.
pub(crate) fn tool_result(output: ToolOutput) -> CallToolResult {
    let items = output.into_content();
    let lone_json = match items.as_slice() {
        [only] => only.as_json().filter(|json| json.is_object()).cloned(),
        _ => None,
    };
    let blocks = items
        .iter()
        .filter_map(|item| match item {
            ToolResultContent::Image(image) => image_block(image),
            other => other.as_text().map(ContentBlock::text).or_else(|| {
                other
                    .as_json()
                    .map(|json| ContentBlock::text(json.to_string()))
            }),
        })
        .collect();
    let mut result = CallToolResult::success(blocks);
    result.structured_content = lone_json;
    result
}

/// The images among an agent's final content, as image blocks.
pub(crate) fn image_blocks(content: &[AssistantContent]) -> Vec<ContentBlock> {
    content
        .iter()
        .filter_map(|item| match item {
            AssistantContent::Image(image) => image_block(image),
            _ => None,
        })
        .collect()
}

/// An image block: MCP carries images inline as base64, so an image only
/// known by URL becomes a text line with the link.
fn image_block(image: &Image) -> Option<ContentBlock> {
    let mime = |bytes: Option<&[u8]>| {
        image
            .media_type
            .clone()
            .or_else(|| bytes.and_then(ImageMediaType::sniff))
            .map_or("image/png", |kind| kind.to_mime_type())
    };
    match &image.data {
        DocumentSourceKind::Base64(data) => {
            let bytes = BASE64.decode(data).ok();
            Some(ContentBlock::image(data.clone(), mime(bytes.as_deref())))
        }
        DocumentSourceKind::Raw(bytes) => {
            Some(ContentBlock::image(BASE64.encode(bytes), mime(Some(bytes))))
        }
        DocumentSourceKind::Url(url) => Some(ContentBlock::text(format!("Image: {url}"))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use rig_core::message::{AssistantContent, DocumentSourceKind, Image, ToolResultContent};
    use serde_json::json;

    use super::*;

    #[test]
    fn lone_json_is_also_structured_content() {
        let result = tool_result(ToolOutput::json(json!({"total": 42})));
        assert_eq!(result.structured_content, Some(json!({"total": 42})));
        assert_eq!(result.content.len(), 1);

        let text = tool_result(ToolOutput::text("hi"));
        assert_eq!(text.structured_content, None);

        // MCP's structuredContent is an object: a bare number stays text.
        let number = tool_result(ToolOutput::json(json!(42)));
        assert_eq!(number.structured_content, None);
        assert_eq!(
            serde_json::to_value(&number.content).unwrap()[0]["text"],
            "42"
        );
    }

    #[test]
    fn images_become_image_blocks_and_urls_links() {
        let png = Image {
            data: DocumentSourceKind::Raw(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec()),
            ..Default::default()
        };
        let output = ToolOutput::content(vec![
            ToolResultContent::text("chart:"),
            ToolResultContent::Image(png.clone()),
        ])
        .unwrap();
        let result = tool_result(output);
        let blocks = serde_json::to_value(&result.content).unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["mimeType"], "image/png");

        let linked = AssistantContent::Image(Image {
            data: DocumentSourceKind::url("https://x/a.png"),
            ..Default::default()
        });
        let blocks = serde_json::to_value(image_blocks(&[linked])).unwrap();
        assert_eq!(blocks[0]["text"], "Image: https://x/a.png");
    }
}
