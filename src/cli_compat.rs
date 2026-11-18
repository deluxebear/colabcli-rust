//! Official CLI presentation and interchange formats. Durable internal records retain extra metadata.
use crate::{Error, Result, contents};
use serde_json::{Value, json};
use std::path::Path;

pub fn validate_environment(bindings: &[String]) -> Result<()> {
    for binding in bindings {
        let Some((key, _)) = binding.split_once('=') else {
            eprintln!(
                "[colab] Invalid --env value {}. Expected KEY=VALUE.",
                python_repr(&json!(binding))
            );
            return Err(Error::CliExit(2));
        };
        let mut chars = key.chars();
        if !chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            eprintln!(
                "[colab] Invalid --env key {}. Expected a valid environment variable name.",
                python_repr(&json!(key))
            );
            return Err(Error::CliExit(2));
        }
    }
    Ok(())
}

fn quoted(value: &str, quote: char) -> String {
    let mut text = String::new();
    text.push(quote);
    for ch in value.chars() {
        match ch {
            '\\' => text.push_str("\\\\"),
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            '\t' => text.push_str("\\t"),
            c if c == quote => {
                text.push('\\');
                text.push(c);
            }
            c if c.is_control() => text.push_str(&format!("\\u{:04x}", c as u32)),
            c => text.push(c),
        }
    }
    text.push(quote);
    text
}

/// Python-style string representation used by merged CLI diagnostics.
pub fn python_string_repr(text: &str) -> String {
    python_repr(&json!(text))
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::String(text) => quoted(
            text,
            if text.contains('\'') && !text.contains('"') {
                '"'
            } else {
                '\''
            },
        ),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", python_repr(&json!(k)), python_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => value.to_string(),
    }
}

fn spaced_json(value: &Value) -> String {
    match value {
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", spaced_json(&json!(k)), spaced_json(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(spaced_json).collect::<Vec<_>>().join(", ")
        ),
        Value::String(_) => {
            let mut output = String::new();
            for ch in value.to_string().chars() {
                if ch.is_ascii() {
                    output.push(ch);
                } else {
                    for unit in ch.encode_utf16(&mut [0; 2]) {
                        output.push_str(&format!("\\u{unit:04x}"));
                    }
                }
            }
            output
        }
        _ => value.to_string(),
    }
}

pub fn timestamp(event: &Value) -> String {
    event["timestamp"]
        .as_str()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("")
        .replace('T', " ")
}

pub fn history_line(event: &Value) -> String {
    let ts = timestamp(event);
    let text = |key: &str| event[key].as_str().unwrap_or("");
    let label = match text("event_type") {
        "execution" => format!(
            "EXEC: {}...",
            text("code")
                .trim()
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect::<String>()
        ),
        "file_operation" => format!(
            "FILE: {} {}",
            text("op"),
            event
                .get("path")
                .or_else(|| event.get("remote"))
                .and_then(Value::as_str)
                .unwrap_or("")
        ),
        "automation" => format!("AUTO: {}", text("op")),
        "stdin_request" => format!("INPT: {}", text("prompt").trim()),
        "input_reply" => format!("RPLY: {}", text("value").trim()),
        _ => format!(
            "EVENT: {}",
            event["event_type"].as_str().unwrap_or("unknown")
        ),
    };
    format!("[{ts}] {label}")
}

/// Serialize multiline fields in the same representation used by nbformat's writer.
pub fn notebook_lines(notebook: &mut Value) {
    fn lines(value: &mut Value) {
        if let Some(text) = value.as_str() {
            let mut parts = Vec::new();
            let mut start = 0;
            let mut chars = text.char_indices().peekable();
            while let Some((position, ch)) = chars.next() {
                if !matches!(
                    ch,
                    '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}'
                        ..='\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
                ) {
                    continue;
                }
                let mut end = position + ch.len_utf8();
                if ch == '\r' && chars.peek().is_some_and(|&(_, next)| next == '\n') {
                    chars.next();
                    end += 1;
                }
                parts.push(&text[start..end]);
                start = end;
            }
            if start < text.len() {
                parts.push(&text[start..]);
            }
            *value = json!(parts);
        }
    }
    fn mime_bundle(bundle: &mut Value) {
        if let Some(entries) = bundle.as_object_mut() {
            for (mime, value) in entries {
                if mime.starts_with("text/")
                    || matches!(mime.as_str(), "application/javascript" | "image/svg+xml")
                {
                    lines(value);
                }
            }
        }
    }
    if let Some(cells) = notebook["cells"].as_array_mut() {
        for cell in cells {
            if let Some(source) = cell.get_mut("source") {
                lines(source);
            }
            if let Some(attachments) = cell.get_mut("attachments").and_then(Value::as_object_mut) {
                for bundle in attachments.values_mut() {
                    mime_bundle(bundle);
                }
            }
            if cell["cell_type"] != "code" {
                continue;
            }
            if let Some(outputs) = cell.get_mut("outputs").and_then(Value::as_array_mut) {
                for output in outputs {
                    match output["output_type"].as_str() {
                        Some("stream") => {
                            if let Some(text) = output.get_mut("text") {
                                lines(text);
                            }
                        }
                        Some("display_data" | "execute_result") => {
                            if let Some(data) = output.get_mut("data") {
                                mime_bundle(data);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn compatible_events(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .map(|event| {
            let mut event = event.clone();
            crate::history::redact(&mut event);
            if let Some(map) = event.as_object_mut() {
                for key in ["account", "session", "event_id"] {
                    map.remove(key);
                }
            }
            event
        })
        .collect()
}

fn export_notebook(events: &[Value], name: &str) -> Value {
    let markdown = |source: String| json!({"cell_type":"markdown","id":crate::jupyter::codec::message_id(),"metadata":{},"source":source});
    let code_cell = |source: String, outputs: Vec<Value>| json!({"cell_type":"code","id":crate::jupyter::codec::message_id(),"metadata":{},"source":source,"execution_count":null,"outputs":outputs});
    let mapped_outputs = |event: &Value| -> Vec<Value> {
        event["outputs"].as_array().into_iter().flatten().filter_map(|output| {
            if output.get("text").is_some() { Some(json!({"output_type":"stream","name":"stdout","text":output["text"]})) }
            else if output.get("data").is_some() { Some(json!({"output_type":"display_data","metadata":{},"data":output["data"]})) }
            else if output["output_type"] == "error" { Some(json!({"output_type":"error","ename":output.get("ename").unwrap_or(&json!("Error")),"evalue":output.get("evalue").unwrap_or(&json!("")),"traceback":output.get("traceback").unwrap_or(&json!([]))})) }
            else { None }
        }).collect()
    };
    let mut cells = vec![markdown(format!(
        "# Colab Session: {name}\nGenerated from colab-cli history log."
    ))];
    for event in events {
        let text = |key: &str| event[key].as_str().unwrap_or("");
        match text("event_type") {
            "execution" => {
                let mut code = text("code").to_owned();
                if event["source"] == "piped" && !code.starts_with('!') {
                    code.insert_str(0, "%%bash\n");
                }
                cells.push(code_cell(code, mapped_outputs(event)));
            }
            "session_created" => cells.push(markdown(format!(
                "**Session Created**: {}\n- Endpoint: `{}`\n- Hardware: `{}`",
                timestamp(event),
                text("endpoint"),
                text("accelerator")
            ))),
            "file_operation" => cells.push(markdown(format!(
                "*File Operation*: `{}` on `{}`",
                text("op"),
                event
                    .get("path")
                    .or_else(|| event.get("remote"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ))),
            "automation" => {
                cells.push(markdown(format!(
                    "### Automation: {} ({})",
                    text("op"),
                    timestamp(event)
                )));
                if !text("code").is_empty() {
                    cells.push(code_cell(text("code").into(), vec![]));
                }
            }
            "automation_result"
                if event["outputs"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty()) =>
            {
                cells.push(code_cell(
                    "# Result of previous automation".into(),
                    mapped_outputs(event),
                ))
            }
            "stdin_request" => cells.push(markdown(format!(
                "> **Input Requested**: {}",
                text("prompt")
            ))),
            "input_reply" => cells.push(markdown(format!("> **User Input**: `{}`", text("value")))),
            _ => {}
        }
    }
    let mut nb = json!({"nbformat":4,"nbformat_minor":5,"metadata":{"kernelspec":{"name":"python3","display_name":"Python 3 (Google Colab)","language":"python"}},"cells":cells});
    notebook_lines(&mut nb);
    nb
}

pub fn export_history(events: &[Value], name: &str, path: &Path) -> Result<()> {
    let events = compatible_events(events);
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let output = match extension.as_str() {
        "jsonl" => events
            .iter()
            .map(|event| format!("{}\n", spaced_json(event)))
            .collect(),
        "ipynb" => {
            serde_json::to_string_pretty(&export_notebook(&events, name))
                .map_err(|_| Error::History("notebook 导出失败"))?
                + "\n"
        }
        "txt" => {
            let mut output = format!("Colab Session: {name}\n{}\n\n", "=".repeat(20));
            for event in &events {
                output.push_str(&format!(
                    "[{}] {}: ",
                    timestamp(event),
                    event["event_type"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_uppercase()
                ));
                if event["event_type"] == "execution" {
                    output.push_str(event["code"].as_str().unwrap_or("").trim());
                } else {
                    output.push_str(&python_repr(event));
                }
                output.push('\n');
            }
            output
        }
        "md" => {
            let mut output = format!("# Colab Session: {name}\n\n");
            for event in &events {
                let text = |key: &str| event[key].as_str().unwrap_or("");
                match text("event_type") {
                    "execution" => {
                        output.push_str(&format!(
                            "### Execution ({})\n```python\n{}\n```\n\n",
                            timestamp(event),
                            text("code")
                        ));
                        for item in event["outputs"].as_array().into_iter().flatten() {
                            if let Some(text) = item["text"].as_str() {
                                output.push_str(&format!("**Output**:\n```\n{text}```\n\n"));
                            }
                        }
                    }
                    "session_created" => output.push_str(&format!(
                        "## Session Created: {}\n- Endpoint: `{}`\n\n",
                        timestamp(event),
                        text("endpoint")
                    )),
                    "file_operation" => output.push_str(&format!(
                        "*File Operation*: `{}` on `{}`\n\n",
                        text("op"),
                        event
                            .get("path")
                            .or_else(|| event.get("remote"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                    )),
                    _ => {}
                }
            }
            output
        }
        _ => return Err(Error::History("导出扩展名必须为 .ipynb/.md/.txt/.jsonl")),
    };
    contents::write_atomic(path, output.as_bytes())
}

/// Match the plain (NO_COLOR) Typer/Rich diagnostic layout without exposing exception locals.
pub fn argument_error(command: Option<&str>, message: &str) -> Error {
    if let Some(command) = command {
        let positional = if command == "run" {
            " {script} [script_args]..."
        } else {
            ""
        };
        eprintln!(
            "Usage: colab {command} [OPTIONS]{positional}\nTry 'colab {command} -h' for help."
        );
    }
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(80)
        .clamp(20, 1000);
    eprintln!("╭─ Error {}╮", "─".repeat(width - 10));
    let content_width = width - 4;
    let mut current = String::new();
    let mut lines = Vec::new();
    for word in message.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > content_width
        {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    lines.push(current);
    for line in lines {
        eprintln!(
            "│ {line}{}│",
            " ".repeat((width - 3).saturating_sub(line.chars().count()))
        );
    }
    eprintln!("╰{}╯", "─".repeat(width - 2));
    Error::CliExit(2)
}
