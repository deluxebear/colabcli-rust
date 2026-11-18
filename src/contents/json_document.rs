//! Preserve document number lexemes without changing JSON handling elsewhere.
use super::{Entry, MAX_FILE_BYTES};
use crate::{Error, Result};
use serde::{Deserialize, de::IgnoredAny};
use serde_json::{Deserializer, Value};
#[cfg(test)]
mod tests;

fn invalid() -> Error {
    Error::ContentsProtocol("无效的 Contents JSON")
}
fn whitespace(raw: &str, position: &mut usize) {
    while raw
        .as_bytes()
        .get(*position)
        .is_some_and(u8::is_ascii_whitespace)
    {
        *position += 1;
    }
}
fn read<'a, T: Deserialize<'a>>(raw: &'a str, position: &mut usize) -> Result<T> {
    let mut stream = Deserializer::from_str(&raw[*position..]).into_iter::<T>();
    let value = stream.next().ok_or_else(invalid)?.map_err(|_| invalid())?;
    *position += stream.byte_offset();
    Ok(value)
}

pub(super) fn decode_entry(body: &[u8], download: bool) -> Result<(Entry, Option<String>)> {
    if !download {
        return serde_json::from_slice(body)
            .map(|entry| (entry, None))
            .map_err(|_| invalid());
    }
    let raw = std::str::from_utf8(body).map_err(|_| invalid())?;
    let mut position = 0;
    whitespace(raw, &mut position);
    if raw.as_bytes().get(position) != Some(&b'{') {
        return Err(invalid());
    }
    position += 1;
    let mut metadata = String::new();
    let mut copied = 0;
    let mut content = None;
    loop {
        whitespace(raw, &mut position);
        if raw.as_bytes().get(position) == Some(&b'}') {
            position += 1;
            break;
        }
        let key: String = read(raw, &mut position)?;
        whitespace(raw, &mut position);
        if raw.as_bytes().get(position) != Some(&b':') {
            return Err(invalid());
        }
        position += 1;
        whitespace(raw, &mut position);
        let start = position;
        let _: IgnoredAny = read(raw, &mut position)?;
        if key == "content" {
            content = Some(&raw[start..position]);
            metadata.push_str(&raw[copied..start]);
            metadata.push_str("null");
            copied = position;
        }
        whitespace(raw, &mut position);
        match raw.as_bytes().get(position) {
            Some(b',') => position += 1,
            Some(b'}') => {
                position += 1;
                break;
            }
            _ => return Err(invalid()),
        }
    }
    whitespace(raw, &mut position);
    if position != raw.len() {
        return Err(invalid());
    }
    metadata.push_str(&raw[copied..]);
    let mut entry: Entry = serde_json::from_str(&metadata).map_err(|_| invalid())?;
    if entry.format.as_deref() == Some("json") {
        Ok((entry, Some(content.unwrap_or("null").to_owned())))
    } else {
        entry.content = content
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| invalid())?
            .unwrap_or(Value::Null);
        Ok((entry, None))
    }
}

fn number(raw: &str) -> Result<String> {
    if !raw.contains(['.', 'e', 'E']) {
        return Ok(if raw == "-0" { "0" } else { raw }.to_owned());
    }
    let value: f64 = raw
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite())
        .ok_or(Error::ContentsProtocol("JSON content 浮点数无效"))?;
    let shortest = format!("{value:?}");
    if let Some((mantissa, exponent)) = shortest.split_once('e') {
        let exponent: i32 = exponent.parse().map_err(|_| invalid())?;
        Ok(format!("{mantissa}e{exponent:+03}"))
    } else {
        Ok(shortest)
    }
}
fn newline(output: &mut String, depth: usize) {
    output.push('\n');
    output.extend(std::iter::repeat_n(' ', depth * 2));
}

enum Document {
    Atom(String),
    Array(Vec<Document>),
    Object(indexmap::IndexMap<String, Document>),
}
fn parse_document(raw: &str, position: &mut usize, depth: usize) -> Result<Document> {
    if depth >= 128 {
        return Err(invalid());
    }
    whitespace(raw, position);
    match raw.as_bytes().get(*position).copied().ok_or_else(invalid)? {
        b'"' => {
            let value: String = read(raw, position)?;
            Ok(Document::Atom(
                serde_json::to_string(&value).map_err(|_| invalid())?,
            ))
        }
        b'[' => {
            *position += 1;
            whitespace(raw, position);
            let mut values = Vec::new();
            if raw.as_bytes().get(*position) == Some(&b']') {
                *position += 1;
                return Ok(Document::Array(values));
            }
            loop {
                values.push(parse_document(raw, position, depth + 1)?);
                whitespace(raw, position);
                match raw.as_bytes().get(*position) {
                    Some(b',') => *position += 1,
                    Some(b']') => {
                        *position += 1;
                        break;
                    }
                    _ => return Err(invalid()),
                }
            }
            Ok(Document::Array(values))
        }
        b'{' => {
            *position += 1;
            whitespace(raw, position);
            let mut values = indexmap::IndexMap::new();
            if raw.as_bytes().get(*position) == Some(&b'}') {
                *position += 1;
                return Ok(Document::Object(values));
            }
            loop {
                whitespace(raw, position);
                let key: String = read(raw, position)?;
                whitespace(raw, position);
                if raw.as_bytes().get(*position) != Some(&b':') {
                    return Err(invalid());
                }
                *position += 1;
                let value = parse_document(raw, position, depth + 1)?;
                // Python dict keeps the first position and the last value.
                values.insert(key, value);
                whitespace(raw, position);
                match raw.as_bytes().get(*position) {
                    Some(b',') => *position += 1,
                    Some(b'}') => {
                        *position += 1;
                        break;
                    }
                    _ => return Err(invalid()),
                }
            }
            Ok(Document::Object(values))
        }
        b'-' | b'0'..=b'9' => {
            let start = *position;
            while raw
                .as_bytes()
                .get(*position)
                .is_some_and(|c| c.is_ascii_digit() || b"+-.eE".contains(c))
            {
                *position += 1;
            }
            Ok(Document::Atom(number(&raw[start..*position])?))
        }
        b't' | b'f' | b'n' => {
            let start = *position;
            while raw
                .as_bytes()
                .get(*position)
                .is_some_and(u8::is_ascii_lowercase)
            {
                *position += 1;
            }
            let literal = &raw[start..*position];
            if !matches!(literal, "true" | "false" | "null") {
                return Err(invalid());
            }
            Ok(Document::Atom(literal.to_owned()))
        }
        _ => Err(invalid()),
    }
}
fn write_document(value: &Document, output: &mut String, depth: usize) -> Result<()> {
    match value {
        Document::Atom(atom) => output.push_str(atom),
        Document::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                newline(output, depth + 1);
                write_document(value, output, depth + 1)?;
            }
            if !values.is_empty() {
                newline(output, depth);
            }
            output.push(']');
        }
        Document::Object(values) => {
            output.push('{');
            for (index, (key, value)) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                newline(output, depth + 1);
                output.push_str(&serde_json::to_string(key).map_err(|_| invalid())?);
                output.push_str(": ");
                write_document(value, output, depth + 1)?;
            }
            if !values.is_empty() {
                newline(output, depth);
            }
            output.push('}');
        }
    }
    if output.len() as u64 > MAX_FILE_BYTES {
        return Err(Error::FileTooLarge(MAX_FILE_BYTES));
    }
    Ok(())
}

pub(super) fn render(raw: &str) -> Result<Vec<u8>> {
    // Validate with serde before retaining raw numeric tokens. No arithmetic
    // conversion is performed by IgnoredAny, so very large integers stay exact.
    let mut validated = 0;
    let _: IgnoredAny = read(raw, &mut validated)?;
    whitespace(raw, &mut validated);
    if validated != raw.len() {
        return Err(invalid());
    }
    let mut position = 0;
    let document = parse_document(raw, &mut position, 0)?;
    let mut output = String::new();
    write_document(&document, &mut output, 0)?;
    // Python dumps defaults to ensure_ascii, including DEL and surrogate pairs.
    use std::fmt::Write;
    let mut ascii = String::with_capacity(output.len());
    for character in output.chars() {
        if character.is_ascii() && character != '\u{7f}' {
            ascii.push(character);
        } else {
            let mut units = [0; 2];
            for unit in character.encode_utf16(&mut units) {
                write!(ascii, "\\u{unit:04x}").unwrap();
            }
        }
        if ascii.len() as u64 > MAX_FILE_BYTES {
            return Err(Error::FileTooLarge(MAX_FILE_BYTES));
        }
    }
    Ok(ascii.into_bytes())
}
