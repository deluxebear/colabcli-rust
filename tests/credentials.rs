mod common;
use colabcli_rust::{
    Error,
    credentials::{CredentialBackend, TokenStorageMode},
};
use common::*;
use std::sync::atomic::Ordering;
#[test]
fn file_mode_legacy_migration_never_reads_system_keyring() {
    let f = Fixture::new(TokenStorageMode::File);
    let legacy = colabcli_rust::models::Secret::new(serde_json::to_string(&token()).unwrap());
    f.backend.set("oauth_token", &legacy).unwrap();
    // File mode must not import a credential from a backend the user explicitly excluded.
    assert!(!f.tokens.migrate_legacy().unwrap());
    assert!(f.tokens.load(&id("default")).unwrap().is_none());
}
#[test]
fn file_mode_migrates_an_existing_legacy_file_without_a_keyring() {
    let f = Fixture::new(TokenStorageMode::File);
    f.backend.fail_get.store(true, Ordering::SeqCst);
    f.storage
        .write(&f.storage.path("token.json").unwrap(), &token())
        .unwrap();
    assert!(f.tokens.migrate_legacy().unwrap());
    assert!(f.tokens.load(&id("default")).unwrap().is_some());
}
#[test]
fn auto_does_not_report_inaccessible_keyring_as_missing_credentials() {
    let f = Fixture::new(TokenStorageMode::Auto);
    f.backend.fail_get.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.tokens.load(&id("default")),
        Err(Error::CredentialUnavailable)
    ));
}
#[test]
fn auto_uses_a_saved_file_even_when_keyring_is_locked() {
    let f = Fixture::new(TokenStorageMode::Auto);
    f.storage
        .write(
            &f.storage
                .profile_path(&id("default"), "oauth_token.json")
                .unwrap(),
            &token(),
        )
        .unwrap();
    f.backend.fail_get.store(true, Ordering::SeqCst);
    assert_eq!(
        f.tokens
            .load(&id("default"))
            .unwrap()
            .unwrap()
            .token
            .expose(),
        "fixture-access"
    );
}

#[test]
fn cli_file_mode_reports_login_steps_and_keyring_timeout_rejects_invalid_values() {
    let temp = tempfile::tempdir().unwrap();
    let cli = || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_colab"));
        command
            .arg("--config-dir")
            .arg(temp.path())
            .env_remove("COLAB_ACCOUNT")
            .env_remove("COLAB_TOKEN_STORAGE")
            .env_remove("COLAB_KEYRING_TIMEOUT");
        command
    };
    let output = cli()
        .args(["--token-storage", "file", "sessions"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let message = String::from_utf8(output.stderr).unwrap();
    assert!(message.contains("account login"));
    assert!(message.contains("-c"));
    assert!(!message.contains("凭据存储不可用"));
    for value in ["0", "301", "invalid"] {
        let output = cli()
            .args(["--keyring-timeout", value, "sessions"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}
