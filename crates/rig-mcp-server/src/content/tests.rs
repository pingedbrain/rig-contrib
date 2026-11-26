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
    let blocks = serde_json::to_value(&number.content).unwrap();
    assert_eq!(blocks.pointer("/0/text"), Some(&json!("42")));
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
    assert_eq!(blocks.pointer("/0/type"), Some(&json!("text")));
    assert_eq!(blocks.pointer("/1/type"), Some(&json!("image")));
    assert_eq!(blocks.pointer("/1/mimeType"), Some(&json!("image/png")));

    let linked = AssistantContent::Image(Image {
        data: DocumentSourceKind::url("https://x/a.png"),
        ..Default::default()
    });
    let blocks = serde_json::to_value(image_blocks(&[linked])).unwrap();
    assert_eq!(
        blocks.pointer("/0/text"),
        Some(&json!("Image: https://x/a.png"))
    );
}
