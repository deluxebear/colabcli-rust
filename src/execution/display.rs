//! Text projection only; never alters the recorded MIME bundle or fetches resources.
use pulldown_cmark::{Options, Parser, html};
use serde_json::Value;
use std::borrow::Cow;

// DOM parsing expands input in memory. Larger bundles retain their plain/source
// representation instead of making presentation failure an uncertain execution.
const MAX_RICH_INPUT: usize = 1024 * 1024;

fn value_text(value: &Value) -> Option<Cow<'_, str>> {
    if let Some(text) = value.as_str() {
        return Some(Cow::Borrowed(text));
    }
    let lines = value.as_array()?;
    let mut text = String::new();
    for line in lines {
        text.push_str(line.as_str()?);
    }
    Some(Cow::Owned(text))
}

/// Match the upstream textual MIME preference: Markdown, HTML, then plain text.
/// Unsupported/malformed representations do not hide a valid lower-priority one.
/// HTML/Markdown are converted locally; scripts, styles and remote resources are
/// never executed. On conversion failure/oversize, prefer plain text, then source.
pub fn text(data: &Value) -> Option<String> {
    let plain = || data.get("text/plain").and_then(value_text);
    for mime in ["text/markdown", "text/html"] {
        let Some(source) = data.get(mime).and_then(value_text) else {
            continue;
        };
        if source.len() <= MAX_RICH_INPUT {
            let mut rendered_html = String::new();
            let input = if mime == "text/markdown" {
                let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
                html::push_html(&mut rendered_html, Parser::new_ext(&source, options));
                rendered_html.as_str()
            } else {
                source.as_ref()
            };
            if let Ok(rendered) = html2text::config::rich().string_from_read(input.as_bytes(), 80) {
                return Some(rendered.trim_end_matches('\n').to_owned());
            }
        }
        return Some(plain().unwrap_or(source).into_owned());
    }
    plain().map(Cow::into_owned)
}
