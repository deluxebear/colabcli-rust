use colabcli_rust::execution::display;
use serde_json::{Value, json};

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn textual_mime_priority_and_content_match_fixed_official_rendering_samples() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../research/display-text-fixtures.json")).unwrap();
    for case in fixtures["cases"].as_array().unwrap() {
        let text = display::text(&case["data"]).unwrap_or_default();
        let name = case["name"].as_str().unwrap();
        if case["comparison"] == "whitespace" {
            assert_eq!(
                normalized(&text),
                normalized(case["official_text"].as_str().unwrap()),
                "{name}"
            );
        }
        for required in case["required_fragments"].as_array().unwrap() {
            assert!(
                normalized(&text).contains(required.as_str().unwrap()),
                "{name}: {text:?}"
            );
        }
        for forbidden in case["forbidden_fragments"].as_array().unwrap() {
            assert!(
                !text.contains(forbidden.as_str().unwrap()),
                "{name}: {text:?}"
            );
        }
    }
}

#[test]
fn multiline_notebook_values_work_and_bad_mime_types_do_not_hide_valid_text() {
    assert_eq!(
        display::text(&json!({"text/markdown":["**hello", " world**"]})).as_deref(),
        Some("hello world")
    );
    assert_eq!(
        display::text(&json!({"text/html": ["<p>hello", " world</p>"]})).as_deref(),
        Some("hello world")
    );
    assert_eq!(
        display::text(
            &json!({"text/markdown":null,"text/html":23,"text/plain":["plain\n","lines"]})
        )
        .as_deref(),
        Some("plain\nlines")
    );
    assert_eq!(
        display::text(&json!({"text/markdown":["wrong",23],"text/html":"<p>valid</p>"})).as_deref(),
        Some("valid")
    );
    assert!(display::text(&json!({"application/json":{"ignored":true}})).is_none());
}

#[test]
fn oversized_rich_input_falls_back_without_turning_presentation_into_execution_failure() {
    let source = format!("<p>{}</p>", "x".repeat(1024 * 1024));
    assert_eq!(
        display::text(&json!({"text/html":source,"text/plain":"readable fallback"})).as_deref(),
        Some("readable fallback")
    );
    assert_eq!(
        display::text(&json!({"text/html":source})).as_deref(),
        Some(source.as_str())
    );
    let markdown = "*".repeat(1024 * 1024 + 1);
    assert_eq!(
        display::text(&json!({"text/markdown":markdown})).as_deref(),
        Some(markdown.as_str())
    );
}
