mod common;
use colabcli_rust::{
    Error,
    credentials::TokenStorageMode,
    history::{self, History, HistoryQuery},
};
use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, process::Command, thread};
fn history(f: &Fixture) -> History {
    History::new(f.storage.clone(), id("default"))
}
fn path(f: &Fixture, name: &str) -> std::path::PathBuf {
    f.storage
        .profile_path(
            &id("default"),
            &format!(
                "history/rust-v1/{:x}.jsonl",
                Sha256::digest(name.as_bytes())
            ),
        )
        .unwrap()
}
#[test]
fn history_is_account_scoped_safe_for_arbitrary_names_and_preserves_user_code() {
    let f = Fixture::new(TokenStorageMode::File);
    let other = f.add("other");
    let name = "../nested/name";
    history(&f).append(name,Some("ep"),"execution",json!({"code":"print('user code')","token":"private","nested":{"client_secret":"private-secret"},"url":"https://host/path?token=private-query&x=ok"}),Some("event-1")).unwrap();
    let events = history(&f).read(name, HistoryQuery::default()).unwrap();
    assert_eq!(events[0]["code"], "print('user code')");
    let text = serde_json::to_string(&events).unwrap();
    for secret in ["private-secret", "private-query", "\"private\""] {
        assert!(!text.contains(secret));
    }
    assert!(
        History::new(f.storage.clone(), other)
            .read(name, HistoryQuery::default())
            .unwrap()
            .is_empty()
    );
    assert_eq!(history(&f).sessions().unwrap(), vec![name]);
    assert!(path(&f, name).is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path(&f, name)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
#[test]
fn concurrent_append_and_idempotent_event_ids_do_not_lose_records() {
    let f = Fixture::new(TokenStorageMode::File);
    let mut workers = Vec::new();
    for worker in 0..6 {
        let history = history(&f);
        workers.push(thread::spawn(move || {
            for index in 0..10 {
                history
                    .append(
                        "same",
                        None,
                        "execution",
                        json!({"code":format!("{worker}-{index}")}),
                        Some(&format!("{worker}-{index}")),
                    )
                    .unwrap();
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    history(&f)
        .append(
            "same",
            None,
            "execution",
            json!({"code":"duplicate"}),
            Some("0-0"),
        )
        .unwrap();
    assert_eq!(
        history(&f)
            .read("same", HistoryQuery::default())
            .unwrap()
            .len(),
        60
    );
}
#[test]
fn append_journal_recovers_partial_and_fully_written_lines_without_duplicates() {
    for full in [false, true] {
        let f = Fixture::new(TokenStorageMode::File);
        let file = path(&f, "recover");
        let pending = file.with_extension("jsonl.append.json");
        f.storage.ensure_parent(&file).unwrap();
        let line = format!(
            "{}\n",
            json!({"session":"recover","event_type":"execution","event_id":"recover-id","code":"pass"})
        );
        fs::write(
            &file,
            if full {
                line.as_bytes()
            } else {
                &line.as_bytes()[..11]
            },
        )
        .unwrap();
        f.storage
            .write(&pending, &json!({"offset":0,"line":line}))
            .unwrap();
        let events = history(&f)
            .read("recover", HistoryQuery::default())
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(!pending.exists());
        history(&f)
            .append("recover", None, "execution", json!({}), Some("recover-id"))
            .unwrap();
        assert_eq!(
            history(&f)
                .read("recover", HistoryQuery::default())
                .unwrap()
                .len(),
            1
        );
    }
}
#[test]
fn corrupt_or_unjournaled_tail_is_never_silently_discarded() {
    let f = Fixture::new(TokenStorageMode::File);
    let file = path(&f, "bad");
    f.storage.ensure_parent(&file).unwrap();
    fs::write(&file, b"{truncated").unwrap();
    assert!(matches!(
        history(&f).read("bad", HistoryQuery::default()),
        Err(Error::InvalidData(_))
    ));
    assert!(
        history(&f)
            .append("bad", None, "execution", json!({}), None)
            .is_err()
    );
    assert_eq!(fs::read(file).unwrap(), b"{truncated");
}
#[test]
fn legacy_history_filters_and_tail_limits_work_without_network() {
    let f = Fixture::new(TokenStorageMode::File);
    let legacy = f
        .storage
        .profile_path(&id("default"), "history/old.jsonl")
        .unwrap();
    f.storage.ensure_parent(&legacy).unwrap();
    fs::write(&legacy,format!("{}\n",json!({"event_type":"execution","timestamp":"legacy","code":"old","token":"legacy-secret"}))).unwrap();
    history(&f)
        .append("old", None, "file_operation", json!({"op":"ls"}), None)
        .unwrap();
    history(&f)
        .append("old", None, "execution", json!({"code":"new"}), None)
        .unwrap();
    let events = history(&f)
        .read(
            "old",
            HistoryQuery {
                event_type: Some("execution"),
                lines: Some(1),
            },
        )
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["code"], "new");
    assert!(
        history(&f)
            .read(
                "old",
                HistoryQuery {
                    lines: Some(0),
                    ..HistoryQuery::default()
                }
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(history(&f).sessions().unwrap(), vec!["old"]);
    assert!(
        fs::read_to_string(legacy)
            .unwrap()
            .contains("legacy-secret")
    );
}
#[test]
fn all_export_formats_preserve_code_streams_errors_and_mime_bundles() {
    let f = Fixture::new(TokenStorageMode::File);
    history(&f).append("export",None,"execution",json!({"code":"print('```')","status":"succeeded","msg_id":"msg","execution_count":7,
        "outputs":[{"output_type":"stream","name":"stderr","text":"warn"},{"output_type":"display_data","data":{"image/png":"aGVsbG8=","text/plain":"plot"},"metadata":{},"transient":{"display_id":"x"}},{"output_type":"error","ename":"Oops","evalue":"bad","traceback":["trace"]}]}),None).unwrap();
    let events = history(&f).read("export", HistoryQuery::default()).unwrap();
    for extension in ["ipynb", "md", "txt", "jsonl"] {
        let output = f.temp.path().join(format!("out.{extension}"));
        history::export(&events, "export", &output).unwrap();
        let text = fs::read_to_string(output).unwrap();
        assert!(text.contains("print("));
        if extension == "ipynb" {
            let notebook: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(notebook["cells"][1]["outputs"][0]["name"], "stderr");
            assert_eq!(
                notebook["cells"][1]["outputs"][1]["data"]["image/png"],
                "aGVsbG8="
            );
            assert!(
                notebook["cells"][1]["outputs"][1]
                    .get("transient")
                    .is_none()
            );
        }
        if extension == "md" {
            assert!(text.contains("````python"));
            assert!(text.contains("trace"));
        }
    }
    assert!(history::export(&events, "export", &f.temp.path().join("invalid.pdf")).is_err());
}
#[test]
fn stopped_session_history_routes_to_owner_and_cli_export_is_offline() {
    let f = Fixture::new(TokenStorageMode::File);
    let other = f.add("other");
    History::new(f.storage.clone(), other)
        .append(
            "stopped",
            Some("ep-old"),
            "session_terminated",
            json!({}),
            None,
        )
        .unwrap();
    let output = f.temp.path().join("history.ipynb");
    let result = Command::new(env!("CARGO_BIN_EXE_colab"))
        .arg("--config-dir")
        .arg(f.temp.path())
        .args(["--token-storage", "file", "log", "-s", "stopped", "-o"])
        .arg(&output)
        .env_remove("COLAB_ACCOUNT")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.exists());
    let result = Command::new(env!("CARGO_BIN_EXE_colab"))
        .arg("--config-dir")
        .arg(f.temp.path())
        .args(["--token-storage", "file", "log", "-s", "stopped", "--json"])
        .env_remove("COLAB_ACCOUNT")
        .output()
        .unwrap();
    let events: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(events[0]["account"], "other");
}
#[test]
fn stale_history_writer_cannot_recreate_removed_profile() {
    let f = Fixture::new(TokenStorageMode::File);
    let other = f.add("other");
    let old = History::new(f.storage.clone(), other.clone());
    f.accounts.remove(&other, false).unwrap();
    assert!(matches!(
        old.append("session", None, "execution", json!({}), None),
        Err(Error::AccountMissing(_))
    ));
    assert!(!f.storage.profile_path(&other, "history").unwrap().exists());
}
#[cfg(unix)]
#[test]
fn history_symlinks_are_rejected() {
    let f = Fixture::new(TokenStorageMode::File);
    let target = f.temp.path().join("outside");
    fs::create_dir_all(&target).unwrap();
    let link = f.storage.profile_path(&id("default"), "history").unwrap();
    f.storage.ensure_parent(&link).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(matches!(
        history(&f).append("name", None, "execution", json!({}), None),
        Err(Error::UnsafePath(_))
    ));
    assert!(fs::read_dir(target).unwrap().next().is_none());
}

#[test]
fn input_replies_and_url_credentials_are_redacted_on_legacy_read_and_export() {
    let f = Fixture::new(TokenStorageMode::File);
    let legacy = f
        .storage
        .profile_path(&id("default"), "history/input.jsonl")
        .unwrap();
    f.storage.ensure_parent(&legacy).unwrap();
    fs::write(legacy,format!("{}\n",json!({"event_type":"input_reply","value":"password-never-export","url":"https://private-user:private-password@host/path"}))).unwrap();
    let events = history(&f).read("input", HistoryQuery::default()).unwrap();
    let encoded = serde_json::to_string(&events).unwrap();
    for secret in ["password-never-export", "private-user", "private-password"] {
        assert!(!encoded.contains(secret));
    }
    let output = f.temp.path().join("input.jsonl");
    history::export(&events, "input", &output).unwrap();
    assert!(
        !fs::read_to_string(output)
            .unwrap()
            .contains("password-never-export")
    );
}
