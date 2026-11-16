mod common;
use colabcli_rust::{
    Error,
    credentials::TokenStorageMode,
    models::{Registry, SessionState},
    storage::Storage,
};
use common::*;
use serde_json::{Value, json};
use std::{fs, sync::Arc, thread, time::Duration};

#[test]
fn secret_debug_never_discloses_credentials() {
    let credential = token();
    let text = format!("{credential:?} {:?}", session("test"));
    for sensitive in [
        "fixture-access",
        "fixture-refresh",
        "fixture-secret",
        "fixture-proxy",
    ] {
        assert!(!text.contains(sensitive));
    }
}
#[test]
fn python_session_defaults_and_unknown_fields_roundtrip() {
    let fixture = Fixture::new(TokenStorageMode::File);
    let raw = json!({"name":"s","token":"proxy","url":"https://runtime.invalid","endpoint":"e","future_field":{"x":1}});
    let state: SessionState = serde_json::from_value(raw).unwrap();
    assert_eq!(state.variant, "DEFAULT");
    assert_eq!(state.machine_shape, "STANDARD");
    assert!(state.kernel_id.is_none());
    fixture.storage.save_session(&id("default"), state).unwrap();
    let sessions = fixture.storage.sessions(&id("default")).unwrap();
    assert_eq!(sessions["s"].extra["future_field"]["x"], 1);
}
#[test]
fn corrupted_json_is_reported_without_overwrite() {
    let f = Fixture::new(TokenStorageMode::File);
    let path = f.storage.path("accounts.json").unwrap();
    fs::write(&path, b"{bad").unwrap();
    assert!(matches!(f.accounts.registry(), Err(Error::InvalidData(_))));
    assert!(f.accounts.use_account("default").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"{bad");
}
#[test]
fn future_schema_is_rejected_and_unknown_metadata_preserved() {
    let f = Fixture::new(TokenStorageMode::File);
    f.accounts
        .update(|r| {
            r.extra.insert("future".into(), json!({"flag":true}));
            Ok(())
        })
        .unwrap();
    assert_eq!(f.accounts.registry().unwrap().extra["future"]["flag"], true);
    let path = f.storage.path("accounts.json").unwrap();
    let mut raw: Value = f.storage.read(&path).unwrap().unwrap();
    raw["version"] = json!(7);
    f.storage.write(&path, &raw).unwrap();
    assert!(matches!(f.accounts.registry(), Err(Error::Schema(7))));
    assert_eq!(
        f.storage.read::<Value>(&path).unwrap().unwrap()["version"],
        7
    );
}
#[test]
fn traversal_and_absolute_paths_are_rejected() {
    let f = Fixture::new(TokenStorageMode::File);
    for path in ["../escape", "a/../../escape", "/absolute", ""] {
        assert!(f.storage.path(path).is_err());
    }
}
#[cfg(unix)]
#[test]
fn symlinked_profile_and_file_cannot_escape_root() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new(TokenStorageMode::File);
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(f.temp.path().join("accounts")).unwrap();
    symlink(outside.path(), f.temp.path().join("accounts/evil")).unwrap();
    assert!(matches!(
        f.storage.profile_path(&id("evil"), "oauth_token.json"),
        Err(Error::UnsafePath(_))
    ));
    symlink(
        outside.path().join("secret.json"),
        f.temp.path().join("link.json"),
    )
    .unwrap();
    assert!(f.storage.path("link.json").is_err());
    assert!(!outside.path().join("secret.json").exists());
}
#[cfg(unix)]
#[test]
fn private_files_and_directories_have_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(TokenStorageMode::File);
    f.tokens.save(&id("default"), &token()).unwrap();
    let file = f
        .storage
        .profile_path(&id("default"), "oauth_token.json")
        .unwrap();
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}
#[test]
fn competing_lock_times_out_and_is_released_on_drop() {
    let f = Fixture::new(TokenStorageMode::File);
    let storage = f
        .storage
        .clone()
        .with_lock_timeout(Duration::from_millis(30));
    let path = storage.path("test.lock").unwrap();
    let guard = storage.lock(&path).unwrap();
    assert!(matches!(storage.lock(&path), Err(Error::LockTimeout(_))));
    drop(guard);
    assert!(storage.lock(&path).is_ok());
}
#[test]
fn concurrent_session_updates_do_not_lose_records() {
    let f = Fixture::new(TokenStorageMode::File);
    let storage = Arc::new(f.storage.clone());
    let handles: Vec<_> = (0..12)
        .map(|n| {
            let storage = storage.clone();
            thread::spawn(move || {
                storage
                    .save_session(&id("default"), session(&format!("s{n}")))
                    .unwrap()
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(storage.sessions(&id("default")).unwrap().len(), 12);
}
#[test]
fn failed_serialization_preserves_previous_file() {
    use serde::Serialize;
    struct Invalid;
    impl Serialize for Invalid {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("fixture"))
        }
    }
    let f = Fixture::new(TokenStorageMode::File);
    let path = f.storage.path("sample.json").unwrap();
    f.storage.write(&path, &json!({"good":true})).unwrap();
    assert!(f.storage.write(&path, &Invalid).is_err());
    assert_eq!(
        f.storage.read::<Value>(&path).unwrap().unwrap(),
        json!({"good":true})
    );
}
#[test]
fn bootstrap_copies_legacy_files_without_deleting_or_overwriting() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("history")).unwrap();
    fs::write(temp.path().join("sessions.json"), "{}").unwrap();
    fs::write(
        temp.path().join("history/s.jsonl"),
        "{\"event_type\":\"test\"}\n",
    )
    .unwrap();
    let storage = Storage::new(temp.path()).unwrap();
    let backend = Arc::new(MemoryBackend::default());
    let tokens = colabcli_rust::credentials::TokenStore::new(
        storage.clone(),
        backend,
        TokenStorageMode::File,
    );
    let accounts = colabcli_rust::accounts::Accounts::new(storage.clone(), tokens);
    accounts.bootstrap().unwrap();
    let dest = storage
        .profile_path(&id("default"), "history/s.jsonl")
        .unwrap();
    assert_eq!(
        fs::read(&dest).unwrap(),
        fs::read(temp.path().join("history/s.jsonl")).unwrap()
    );
    fs::write(&dest, "new\n").unwrap();
    accounts.bootstrap().unwrap();
    assert_eq!(fs::read_to_string(dest).unwrap(), "new\n");
    assert!(temp.path().join("sessions.json").exists());
}
#[test]
fn malformed_registry_does_not_silently_become_default() {
    let raw = json!({"version":1,"active":"default","accounts":{}});
    let registry: Registry = serde_json::from_value(raw).unwrap();
    assert!(registry.validate().is_err());
}

#[test]
fn stale_session_writer_cannot_recreate_a_removed_account() {
    let f = Fixture::new(TokenStorageMode::File);
    let work = f.add("work");
    f.accounts.remove(&work, true).unwrap();
    assert!(matches!(
        f.storage.save_session(&work, session("late")),
        Err(Error::AccountMissing(_))
    ));
    assert!(!f.temp.path().join("accounts/work").exists());
}

#[test]
fn settings_defaults_and_unknown_fields_survive_updates() {
    let f = Fixture::new(TokenStorageMode::File);
    let initial = f.storage.settings().unwrap();
    assert!(initial.enable_update_check);
    assert!(initial.last_check.is_none());
    f.storage
        .update_settings(|s| {
            s.extra.insert("future".into(), json!(123));
            s.enable_update_check = false;
            Ok(())
        })
        .unwrap();
    f.storage
        .update_settings(|s| {
            s.latest_version = Some("0.2.0".into());
            Ok(())
        })
        .unwrap();
    let saved = f.storage.settings().unwrap();
    assert!(!saved.enable_update_check);
    assert_eq!(saved.extra["future"], 123);
}
