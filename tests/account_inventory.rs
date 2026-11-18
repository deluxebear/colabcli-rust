mod common;
use colabcli_rust::{
    credentials::TokenStorageMode,
    models::{Account, AuthProvider},
};
use common::*;
use serde_json::{Value, json};
use std::process::Command;

fn command(f: &Fixture, mode: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_colab"));
    command
        .env_clear()
        .env("HOME", f.temp.path())
        .args(["--config-dir"])
        .arg(f.storage.root())
        .args(["--token-storage", mode]);
    command
}
fn run(command: &mut Command) -> Value {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    for secret in [
        "fixture-access",
        "fixture-refresh",
        "fixture-secret",
        "fixture-proxy",
    ] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
    serde_json::from_str(&text).unwrap()
}

#[test]
fn inventory_projects_sessions_and_drive_without_exposing_or_modifying_credentials() {
    let f = Fixture::new(TokenStorageMode::File);
    let work = id("work");
    f.accounts
        .add(work.clone(), account("work@example.invalid"))
        .unwrap();
    f.accounts
        .set_drive(Some(&work), Some(&id("default")))
        .unwrap();
    f.tokens.save(&work, &token()).unwrap();
    f.storage.save_session(&work, session("z-last")).unwrap();
    f.storage.save_session(&work, session("a-first")).unwrap();
    f.storage
        .save_session(&id("default"), session("other"))
        .unwrap();
    let path = f.storage.profile_path(&work, "oauth_token.json").unwrap();
    let before = std::fs::read(&path).unwrap();
    let rows = run(command(&f, "file").args(["account", "list", "--json"]));
    let row = rows["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["account"] == "work")
        .unwrap();
    assert_eq!(row["sessions"], json!(["a-first", "z-last"]));
    assert_eq!(row["auth"]["storage"], "file");
    assert_eq!(row["drive"]["source"], "bound:work");
    assert_eq!(row["drive"]["ready"], false);
    assert!(row.get("created_at").is_none());
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn missing_or_invalid_file_leaves_native_readiness_unknown_without_unlocking() {
    let f = Fixture::new(TokenStorageMode::File);
    let path = f
        .storage
        .profile_path(&id("default"), "oauth_token.json")
        .unwrap();
    f.storage.ensure_parent(&path).unwrap();
    std::fs::write(&path, "invalid-json").unwrap();
    for mode in ["auto", "keyring"] {
        let row = run(command(&f, mode).args(["account", "current", "--json"]));
        assert!(row["auth"]["ready"].is_null());
        assert_eq!(row["auth"]["storage"], "keyring (not inspected)");
    }
    let row = run(command(&f, "file").args(["account", "current", "--json"]));
    assert_eq!(row["auth"]["ready"], false);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "invalid-json");
}

#[test]
fn current_exposes_flag_and_environment_sources_with_resolved_paths() {
    let f = Fixture::new(TokenStorageMode::File);
    f.accounts.add(id("work"), Account::default()).unwrap();
    let row = run(command(&f, "file")
        .env("COLAB_ACCOUNT", "work")
        .args(["account", "current", "--json"]));
    assert_eq!(row["account"], "work");
    assert_eq!(row["source"], "env");
    assert_eq!(
        row["sessions_path"],
        f.storage
            .profile_path(&id("work"), "sessions.json")
            .unwrap()
            .to_str()
            .unwrap()
    );
    let row = run(command(&f, "file")
        .env("COLAB_ACCOUNT", "work")
        .env("COLAB_DRIVE_ACCOUNT", "work")
        .args(["--account", "default", "account", "current", "--json"]));
    assert_eq!(row["account"], "default");
    assert_eq!(row["source"], "flag");
    assert_eq!(row["drive_account"], "work");
    assert_eq!(row["drive_source"], "env");
}

#[test]
fn adc_inventory_checks_only_file_presence_without_parsing_or_refreshing() {
    let f = Fixture::new(TokenStorageMode::File);
    f.accounts
        .add(
            id("adc"),
            Account {
                auth_provider: AuthProvider::Adc,
                adc_credentials_file: Some("~/fixture-adc.json".into()),
                ..Account::default()
            },
        )
        .unwrap();
    let path = f.temp.path().join("fixture-adc.json");
    std::fs::write(&path, "invalid-json-fixture-secret").unwrap();
    let row = run(command(&f, "auto").args(["--account", "adc", "account", "current", "--json"]));
    assert_eq!(row["auth"]["ready"], true);
    assert_eq!(row["auth"]["detail"], "~/fixture-adc.json");
    std::fs::remove_file(path).unwrap();
    let row = run(command(&f, "auto").args(["--account", "adc", "account", "current", "--json"]));
    assert_eq!(row["auth"]["ready"], false);
}

#[test]
fn list_preserves_persisted_order_without_rewriting_registry() {
    let f = Fixture::new(TokenStorageMode::File);
    let path = f.storage.path("accounts.json").unwrap();
    f.storage.write(&path,&json!({"version":1,"active":"default","accounts":{"z-last":{},"default":{},"a-first":{}}})).unwrap();
    let before = std::fs::read(&path).unwrap();
    let row = run(command(&f, "file").args(["account", "list", "--json"]));
    let names: Vec<_> = row["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["account"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["z-last", "default", "a-first"]);
    assert_eq!(std::fs::read(path).unwrap(), before);
}
