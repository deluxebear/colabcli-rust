mod common;
use colabcli_rust::credentials::TokenStorageMode;
use common::*;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn command(f: &Fixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_colab"));
    command
        .env_clear()
        .env("HOME", f.temp.path())
        .arg("--config-dir")
        .arg(f.storage.root())
        .args(["--token-storage", "file"]);
    command
}
fn payload(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(path: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, output);
            } else if path.extension().is_none_or(|extension| extension != "lock") {
                output.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, &mut result);
    result
}
fn value(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn invalid_global_selector_prevents_all_query_and_mutation_payload_changes() {
    let f = Fixture::new(TokenStorageMode::File);
    f.add("work");
    f.tokens.save(&id("work"), &token()).unwrap();
    let before = payload(f.storage.root());
    for arguments in [
        vec!["account", "list", "--json"],
        vec!["account", "current", "--json"],
        vec!["account", "use", "work"],
        vec!["account", "rename", "work", "new"],
        vec!["account", "remove", "work", "--force"],
        vec!["account", "set-drive", "work", "--default"],
    ] {
        let output = command(&f)
            .args(["-a", "missing"])
            .args(arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .starts_with("[colab] Account 'missing' not found")
        );
        assert_eq!(payload(f.storage.root()), before);
    }
}

#[test]
fn local_drive_target_does_not_replace_root_validation_and_can_override_valid_root() {
    let f = Fixture::new(TokenStorageMode::File);
    f.add("work");
    f.add("storage");
    let output = command(&f)
        .args([
            "-a",
            "missing",
            "account",
            "set-drive",
            "storage",
            "-a",
            "work",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("[colab]"));
    assert!(
        f.accounts.registry().unwrap().accounts[&id("work")]
            .drive_account
            .is_none()
    );
    let output = command(&f)
        .args([
            "-a",
            "default",
            "account",
            "set-drive",
            "storage",
            "-a",
            "work",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        f.accounts.registry().unwrap().accounts[&id("work")].drive_account,
        Some(id("storage"))
    );
    assert!(
        f.accounts.registry().unwrap().accounts[&id("default")]
            .drive_account
            .is_none()
    );
}

#[test]
fn environment_is_trimmed_and_empty_root_selector_uses_environment_or_active() {
    let f = Fixture::new(TokenStorageMode::File);
    f.add("work");
    for root in [vec![], vec!["-a", ""]] {
        let row = value(
            command(&f)
                .env("COLAB_ACCOUNT", " work ")
                .args(root)
                .args(["account", "current", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(row["account"], "work");
        assert_eq!(row["source"], "env");
    }
    let row = value(
        command(&f)
            .env("COLAB_ACCOUNT", "  ")
            .args(["-a", "", "account", "current", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(row["account"], "default");
    assert_eq!(row["source"], "active");
    let output = command(&f)
        .env("COLAB_ACCOUNT", "missing")
        .args(["account", "use", "work"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(f.accounts.registry().unwrap().active, id("default"));
}

#[test]
fn root_scanner_skips_declared_option_values_and_keeps_attached_and_repeated_selectors() {
    let f = Fixture::new(TokenStorageMode::File);
    f.add("work");
    for root in [
        vec!["-awork"],
        vec!["--account=work"],
        vec!["-a", "missing", "-a", "work"],
    ] {
        let row = value(
            command(&f)
                .args(root)
                .args(["account", "current", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(row["account"], "work");
        assert_eq!(row["source"], "flag");
    }
    let before = payload(f.storage.root());
    let output = command(&f)
        .args([
            "-c",
            "account",
            "--account=missing",
            "account",
            "set-drive",
            "work",
            "--default",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(payload(f.storage.root()), before);
}

#[test]
fn separator_keeps_option_looking_selector_in_the_subcommand_scope() {
    let f = Fixture::new(TokenStorageMode::File);
    let output = command(&f)
        .args(["account", "use", "--", "--account"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .starts_with("[account] Account '--account' not found")
    );
}
