use super::*;

#[test]
fn floats_use_python_exponents_and_integers_keep_every_digit() {
    assert_eq!(
        render("[18446744073709551616,-9223372036854775809,1e-7,1e-5,1e16,-0.0,1.00,-0]").unwrap(),
        b"[\n  18446744073709551616,\n  -9223372036854775809,\n  1e-07,\n  1e-05,\n  1e+16,\n  -0.0,\n  1.0,\n  0\n]"
    );
    assert!(render("[1e999]").is_err());
}

#[test]
fn empty_nesting_and_literal_private_keys_remain_objects() {
    let raw = r#"{"$serde_json::private::Number":"123","x":[[],{},["\u00e9"]]}"#;
    let expected = b"{\n  \"$serde_json::private::Number\": \"123\",\n  \"x\": [\n    [],\n    {},\n    [\n      \"\\u00e9\"\n    ]\n  ]\n}";
    assert_eq!(render(raw).unwrap(), expected);
}

#[test]
fn duplicate_document_keys_keep_first_position_and_last_value() {
    let raw = r#"{"a":1,"b":0,"\u0061":2,"nested":[{"c":3,"c":4}]}"#;
    assert_eq!(
        render(raw).unwrap(),
        b"{\n  \"a\": 2,\n  \"b\": 0,\n  \"nested\": [\n    {\n      \"c\": 4\n    }\n  ]\n}"
    );
    assert!(render(&format!("{}0{}", "[".repeat(129), "]".repeat(129))).is_err());
}

#[test]
fn content_is_extracted_without_rounding_and_other_metadata_stays_typed() {
    let raw = br#"{"name":"a","path":"a","type":"file","format":"json","\u0063ontent":[18446744073709551616],"extra":{"content":"not the document"}}"#;
    let (entry, document) = decode_entry(raw, true).unwrap();
    assert!(entry.content.is_null());
    assert_eq!(entry.extra["extra"]["content"], "not the document");
    assert_eq!(document.as_deref(), Some("[18446744073709551616]"));
    let huge = format!(
        r#"{{"name":"a","path":"a","type":"file","format":"json","content":{}}}"#,
        "9".repeat(1000)
    );
    let (_, document) = decode_entry(huge.as_bytes(), true).unwrap();
    assert_eq!(
        render(document.as_deref().unwrap()).unwrap(),
        vec![b'9'; 1000]
    );
}

#[test]
fn malformed_or_duplicate_response_fields_do_not_become_valid_documents() {
    for content in ["[1,]", "[tru]", "{\"a\":}", "1.2.3", "\"unterminated"] {
        let raw = format!(
            r#"{{"name":"a","path":"a","type":"file","format":"json","content":{content}}}"#
        );
        assert!(decode_entry(raw.as_bytes(), true).is_err(), "{content}");
    }
    for raw in [
        r#"{"name":"a","path":"a","type":"file","format":"json","content":1,"content":2}"#,
        r#"{"name":"a","path":"a","type":"file","format":"json","content":1} false"#,
        r#"{"name":"a","path":"a","type":"file","format":"json","content":1,}"#,
    ] {
        assert!(decode_entry(raw.as_bytes(), true).is_err());
    }
}
