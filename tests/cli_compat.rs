use colabcli_rust::cli_compat;
use serde_json::{Value, json};

fn normalize_ids(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.contains_key("cell_type") {
                map.insert("id".into(), json!("<UUID>"));
            }
            for value in map.values_mut() {
                normalize_ids(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_ids(item);
            }
        }
        _ => {}
    }
}

#[test]
fn history_exports_match_executed_official_fixtures_without_mutating_audit_records() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../research/official-cli-fixtures.json")).unwrap();
    let events = vec![
        json!({"timestamp":"2026-11-16T00:00:00+00:00","event_type":"execution","code":"print('fixture-output')","outputs":[{"output_type":"stream","name":"stdout","text":"fixture-output\n"}],"account":"default","session":"compare","event_id":"audit-id"}),
    ];
    let original = events.clone();
    let directory = tempfile::tempdir().unwrap();
    for (name, expected) in fixtures["history_exports"].as_object().unwrap() {
        let target = directory.path().join(name);
        cli_compat::export_history(&events, "compare", &target).unwrap();
        let actual = std::fs::read_to_string(target).unwrap();
        if name.ends_with(".ipynb") {
            let mut actual: Value = serde_json::from_str(&actual).unwrap();
            normalize_ids(&mut actual);
            assert_eq!(&actual, expected, "{name}");
        } else {
            assert_eq!(actual, expected.as_str().unwrap(), "{name}");
        }
    }
    assert_eq!(events, original);
}

#[test]
fn compatible_export_redacts_credentials_and_keeps_private_file_permissions() {
    let events = vec![
        json!({"timestamp":"2026-11-16T00:00:00Z","event_type":"session_created","endpoint":"ep-example","api_key":"must-not-export","nested":{"access_token":"must-not-export"}}),
    ];
    let directory = tempfile::tempdir().unwrap();
    for extension in ["jsonl", "md", "txt", "ipynb"] {
        let target = directory.path().join(format!("history.{extension}"));
        cli_compat::export_history(&events, "work", &target).unwrap();
        assert!(
            !std::fs::read_to_string(&target)
                .unwrap()
                .contains("must-not-export")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn nbformat_multiline_conversion_preserves_binary_mime_and_unknown_metadata() {
    let mut notebook = json!({"cells":[
        {"cell_type":"code","source":"one\r\ntwo\r三\u{2028}四","metadata":{"keep":true},"outputs":[
            {"output_type":"stream","text":"a\u{85}b\u{1c}"},
            {"output_type":"display_data","data":{"text/plain":"x\ny","image/png":"AAE=",
                "image/svg+xml":"<svg>\r\n</svg>", "application/javascript":"let x = 1;\n",
                "application/json":{"line":"x\ny"}}},
            {"output_type":"error","ename":"Error","text":"extension\nfield"}
        ]},
        {"cell_type":"markdown","source":"","attachments":{"chart":{"image/svg+xml":"<svg>\n</svg>","image/png":"AAE="}}},
        {"cell_type":"raw","source":["already\n","split"]}
    ]});
    cli_compat::notebook_lines(&mut notebook);
    assert_eq!(
        notebook["cells"][0]["source"],
        json!(["one\r\n", "two\r", "三\u{2028}", "四"])
    );
    assert_eq!(
        notebook["cells"][0]["outputs"][0]["text"],
        json!(["a\u{85}", "b\u{1c}"])
    );
    assert_eq!(
        notebook["cells"][0]["outputs"][1]["data"]["text/plain"],
        json!(["x\n", "y"])
    );
    assert_eq!(
        notebook["cells"][0]["outputs"][1]["data"]["image/png"],
        "AAE="
    );
    assert_eq!(notebook["cells"][0]["metadata"]["keep"], true);
    let data = &notebook["cells"][0]["outputs"][1]["data"];
    assert_eq!(data["image/svg+xml"], json!(["<svg>\r\n", "</svg>"]));
    assert_eq!(data["application/javascript"], json!(["let x = 1;\n"]));
    assert_eq!(data["application/json"], json!({"line":"x\ny"}));
    assert_eq!(
        notebook["cells"][0]["outputs"][2]["text"],
        "extension\nfield"
    );
    assert_eq!(notebook["cells"][1]["source"], json!([]));
    assert_eq!(
        notebook["cells"][1]["attachments"]["chart"]["image/svg+xml"],
        json!(["<svg>\n", "</svg>"])
    );
    assert_eq!(
        notebook["cells"][1]["attachments"]["chart"]["image/png"],
        "AAE="
    );
    assert_eq!(
        notebook["cells"][2]["source"],
        json!(["already\n", "split"])
    );
}
