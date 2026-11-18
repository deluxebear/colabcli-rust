mod common;
use colabcli_rust::{
    credentials::TokenStorageMode,
    drivefs::{lease::Lease, remote::RemoteRequest},
    models::Secret,
};
use common::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
fn command(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_colab"));
    c.env("COLAB_TOKEN_STORAGE", "file");
    c.args(["--control-plane-url", "http://127.0.0.1:1"])
        .arg("--config-dir")
        .arg(root);
    for name in [
        "COLAB_ACCOUNT",
        "COLAB_DRIVE_ACCOUNT",
        "COLAB_DFS_ACCOUNT",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "CLOUDSDK_CONFIG",
    ] {
        c.env_remove(name);
    }
    c
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, (Vec<u8>, u32)>) {
        let meta = fs::symlink_metadata(path).unwrap();
        #[cfg(unix)]
        let permissions = {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode()
        };
        #[cfg(not(unix))]
        let permissions = 0;
        let content = if meta.file_type().is_symlink() {
            fs::read_link(path)
                .unwrap()
                .to_string_lossy()
                .as_bytes()
                .to_vec()
        } else if meta.is_file() {
            fs::read(path).unwrap()
        } else {
            vec![]
        };
        result.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            (content, permissions),
        );
        if meta.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), result);
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}
fn drive(f: &Fixture, account: &str) {
    f.storage.write(&f.storage.profile_path(&id(account),"dfs_credentials.json").unwrap(),&json!({"flavor":"imported","client_id":"private-client","client_secret":"private-secret","refresh_token":"private-refresh","user_id":"private-gaia","email":"private@example.invalid","scopes":["https://www.googleapis.com/auth/drive"],"private_unknown":"private-extra"})).unwrap();
}
#[test]
fn information_and_injection_compatibility_do_not_create_configuration_or_python_files() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing-config");
    let python = temp.path().join("missing-python-site");
    for args in [
        vec!["ext", "version"],
        vec!["drive", "version"],
        vec!["dfs", "version", "--json"],
    ] {
        let value = success(command(&missing).args(args).output().unwrap());
        assert_eq!(value["builtin"], true);
        assert_eq!(value["python_required"], false);
        assert!(!missing.exists());
    }
    for group in ["ext", "drive", "dfs"] {
        let value = success(
            command(&missing)
                .args([group, "install", "--json", "--site-dir"])
                .arg(&python)
                .output()
                .unwrap(),
        );
        assert_eq!(value["changed"], false);
        assert_eq!(value["python_injection"], "not_applicable");
        let quiet = command(&missing)
            .args([group, "install", "--quiet"])
            .output()
            .unwrap();
        assert!(quiet.status.success());
        assert!(quiet.stdout.is_empty());
        assert!(quiet.stderr.is_empty());
        assert!(!missing.exists());
        assert!(!python.exists());
    }
    let value = success(
        command(&missing)
            .args(["ext", "doctor", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(value["registry"], "missing");
    assert!(!missing.exists());
    let strict = command(&missing)
        .args(["ext", "status", "--json", "--strict"])
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(1));
    assert!(!missing.exists());
}
#[test]
fn doctor_honors_compute_and_drive_routing_reports_pending_and_never_leaks_or_mutates() {
    let f = Fixture::new(TokenStorageMode::File);
    let compute = f.add("compute");
    let storage = f.add("storage");
    f.accounts
        .set_drive(Some(&compute), Some(&storage))
        .unwrap();
    f.tokens.save(&compute, &token()).unwrap();
    drive(&f, "storage");
    let mut state = session("work");
    state.extra.insert(
        "automation_pending".into(),
        json!({"private":"private-pending"}),
    );
    state.extra.insert(
        "drive_mounts".into(),
        json!({"private-path":{"private":"private-state"}}),
    );
    f.storage.save_session(&compute, state).unwrap();
    for name in [
        "control_operation.json",
        "control_parallel_release.json",
        "oauth_refresh_pending.json",
    ] {
        f.storage
            .write(
                &f.storage.profile_path(&compute, name).unwrap(),
                &json!({"private":"private-journal"}),
            )
            .unwrap();
    }
    f.storage
        .write(
            &f.storage
                .profile_path(&storage, "dfs_refresh_pending.json")
                .unwrap(),
            &json!({"private":"private-frozen"}),
        )
        .unwrap();
    let lease = Lease {
        version: 1,
        compute_account: compute.clone(),
        drive_account: storage.clone(),
        session: "work".into(),
        endpoint: "private-endpoint".into(),
        request: RemoteRequest::new("/content/drive").unwrap(),
        fingerprint: "a".repeat(64),
        identity: "b".repeat(64),
    };
    f.storage
        .write(
            &f.storage
                .profile_path(
                    &storage,
                    &format!("managed/drivefs-{}.json", lease.request.operation_id),
                )
                .unwrap(),
            &lease,
        )
        .unwrap();
    let before = snapshot(f.storage.root());
    for args in [
        vec!["ext", "doctor"],
        vec!["ext", "status"],
        vec!["drive", "doctor"],
        vec!["dfs", "doctor"],
    ] {
        let output = command(f.storage.root())
            .args(args)
            .args(["-a", "compute", "--json"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        assert!(!text.contains("private-"));
        assert!(!text.contains("fixture-refresh"));
        let value = success(output);
        assert_eq!(value["account"]["id"], "compute");
        assert_eq!(value["account"]["drive_account"], "storage");
        assert_eq!(value["drive_leases"]["count"], 1);
        assert_eq!(value["sessions"][0]["operation_pending"], true);
        assert_eq!(
            value["pending_files"]["control_parallel_release.json"],
            "present"
        );
        assert_eq!(
            value["pending_files"]["drive/dfs_refresh_pending.json"],
            "present"
        );
        assert_eq!(value["authentication"]["state"], "refresh_required");
        assert_eq!(snapshot(f.storage.root()), before);
    }
}
#[test]
fn doctor_does_not_query_keyring_recover_transactions_or_migrate_legacy_state() {
    let f = Fixture::new(TokenStorageMode::File);
    drive(&f, "default");
    f.storage
        .write(
            &f.storage
                .profile_path(&id("default"), "oauth_token.json")
                .unwrap(),
            &json!({"private":"never-inspect-in-keyring-mode"}),
        )
        .unwrap();
    let before = snapshot(f.storage.root());
    let value = success(
        command(f.storage.root())
            .args(["--token-storage", "keyring", "ext", "doctor", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(value["authentication"]["state"], "not_inspected");
    assert_eq!(snapshot(f.storage.root()), before);
    f.storage
        .write(
            &f.storage.path("account-transaction.json").unwrap(),
            &json!({"operation":"rename","private":"do-not-recover"}),
        )
        .unwrap();
    let before = snapshot(f.storage.root());
    let value = success(
        command(f.storage.root())
            .args(["ext", "doctor", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(value["selection"], "blocked_pending_account_repair");
    assert_eq!(snapshot(f.storage.root()), before);
    let legacy = tempfile::tempdir().unwrap();
    fs::write(legacy.path().join("sessions.json"), b"{}").unwrap();
    let before = snapshot(legacy.path());
    let value = success(
        command(legacy.path())
            .args(["ext", "doctor", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(value["registry"], "missing");
    assert_eq!(snapshot(legacy.path()), before);
}
#[test]
fn doctor_handles_corrupt_and_unsafe_records_without_raw_errors_or_writes() {
    let f = Fixture::new(TokenStorageMode::File);
    let registry = f.storage.path("accounts.json").unwrap();
    let original = fs::read(&registry).unwrap();
    fs::write(&registry, b"private-corrupt-registry").unwrap();
    let value = success(
        command(f.storage.root())
            .args(["ext", "doctor", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(value["registry"], "invalid_or_unreadable");
    assert!(!value.to_string().contains("private-"));
    fs::write(&registry, &original).unwrap();
    let sessions = f
        .storage
        .profile_path(&id("default"), "sessions.json")
        .unwrap();
    f.storage
        .write(&sessions, &json!({"private-invalid":null}))
        .unwrap();
    #[cfg(unix)]
    {
        let directory = f.storage.profile_path(&id("default"), "managed").unwrap();
        fs::create_dir_all(&directory).unwrap();
        std::os::unix::fs::symlink(
            &registry,
            directory.join(format!("drivefs-{}.json", "a".repeat(32))),
        )
        .unwrap();
    }
    let before = snapshot(f.storage.root());
    let value = success(
        command(f.storage.root())
            .args(["ext", "doctor", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(value["sessions"]["state"], "invalid_or_unreadable");
    assert!(!value.to_string().contains("private-"));
    #[cfg(unix)]
    assert_eq!(value["drive_leases"]["state"], "invalid_or_unreadable");
    assert_eq!(snapshot(f.storage.root()), before);
}
#[test]
fn doctor_adc_only_inspects_supported_type_and_does_not_evaluate_external_sources() {
    let f = Fixture::new(TokenStorageMode::File);
    let path = f.storage.path("adc.json").unwrap();
    for (value, state) in [
        (
            json!({"type":"authorized_user","client_id":"private-client","refresh_token":"private-token","client_secret":"private-secret"}),
            "authorized_user_configured",
        ),
        (
            json!({"type":"service_account","client_email":"private-service@example.invalid","private_key":"private-unparsed-key"}),
            "service_account_configured",
        ),
        (
            json!({"type":"impersonated_service_account","service_account_impersonation_url":"https://private.invalid","source_credentials":{"type":"authorized_user","refresh_token":"private-refresh","client_secret":"private-source"}}),
            "impersonated_service_account_configured",
        ),
        (
            json!({"type":"impersonated_service_account","service_account_impersonation_url":"https://private.invalid","source_credentials":{"type":"external_account_authorized_user","token_url":"https://private.invalid","client_id":"private-client","client_secret":"private-secret","refresh_token":"private-refresh"}}),
            "impersonated_service_account_configured",
        ),
        (
            json!({"type":"impersonated_service_account","service_account_impersonation_url":"https://private.invalid","source_credentials":{"type":"external_account"}}),
            "unsupported_or_incomplete",
        ),
        (
            json!({"type":"external_account","credential_source":{"executable":{"command":"private-do-not-run"}},"token_url":"https://private.invalid"}),
            "unsupported_or_incomplete",
        ),
        (
            json!({"type":"external_account","audience":"private-audience","subject_token_type":"urn:ietf:params:oauth:token-type:jwt","token_url":"https://private.invalid","credential_source":{"file":"private-missing-file","headers":{"Authorization":"private-header"}}}),
            "external_account_configured",
        ),
        (
            json!({"type":"external_account","audience":"private-audience","subject_token_type":"urn:ietf:params:oauth:token-type:saml2","token_url":"https://private.invalid","credential_source":{"url":"https://private.invalid/never-request"}}),
            "external_account_configured",
        ),
        (
            json!({"type":"external_account_authorized_user","token_url":"https://private.invalid","client_id":"private-client","client_secret":"private-secret","refresh_token":"private-refresh"}),
            "external_account_authorized_user_configured",
        ),
        (
            json!({"type":"external_account_authorized_user","token":"private-token","expiry":"private-expiry-not-parsed"}),
            "external_account_authorized_user_configured",
        ),
        (
            json!({"type":"external_account","audience":"private-audience","token_url":"https://private.invalid","subject_token_type":"urn:ietf:params:oauth:token-type:jwt","credential_source":{"executable":{"command":"private-do-not-run"}}}),
            "external_account_executable_configured",
        ),
        (
            json!({"type":"external_account","audience":"private-audience","token_url":"https://private.invalid","subject_token_type":"urn:ietf:params:aws:token-type:aws4_request","credential_source":{"environment_id":"aws1","regional_cred_verification_url":"https://private.invalid","url":"http://private.invalid"}}),
            "external_account_aws_configured",
        ),
        (
            json!({"type":"external_account_authorized_user","client_secret":"private-secret"}),
            "unsupported_or_incomplete",
        ),
    ] {
        f.storage.write(&path, &value).unwrap();
        let before = snapshot(f.storage.root());
        let report = success(
            command(f.storage.root())
                .args(["--auth", "adc", "ext", "doctor", "--json"])
                .env("GOOGLE_APPLICATION_CREDENTIALS", &path)
                .output()
                .unwrap(),
        );
        assert_eq!(report["authentication"]["state"], state);
        assert!(!report.to_string().contains("private-"));
        assert_eq!(snapshot(f.storage.root()), before);
    }
}
#[test]
fn doctor_strict_only_checks_local_issues_and_does_not_claim_online_verification() {
    let f = Fixture::new(TokenStorageMode::File);
    let mut credentials = token();
    credentials.token = Secret::new("private-cached");
    credentials.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &credentials).unwrap();
    drive(&f, "default");
    let value = success(
        command(f.storage.root())
            .args(["ext", "doctor", "--json", "--strict"])
            .output()
            .unwrap(),
    );
    assert_eq!(value["issues"], json!([]));
    assert_eq!(value["authentication"]["online_validity"], "not_checked");
    assert_eq!(value["client"]["remote_drivefs"], "not_checked");
}
