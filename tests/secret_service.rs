#![cfg(all(target_os = "linux", feature = "native-keyring"))]
//! Opt-in native tests run only by the private, disposable Secret Service harness.
use colabcli_rust::{
    Error,
    auth::OAuthCredentials,
    credentials::{CredentialBackend, SystemKeyring, TokenStorageMode, TokenStore},
    models::ProfileId,
    storage::Storage,
};
use serde_json::json;
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
fn guard(phase: &str) {
    assert_eq!(
        unsafe { libc::geteuid() },
        1000,
        "isolated non-root container required"
    );
    assert_eq!(std::env::var("HOME").as_deref(), Ok("/validation/home"));
    assert_eq!(
        std::env::var("COLAB_NATIVE_STORE_TEST_ISOLATED").as_deref(),
        Ok("1")
    );
    assert_eq!(
        std::env::var("COLAB_NATIVE_STORE_TEST_PHASE").as_deref(),
        Ok(phase)
    );
    assert!(
        std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .unwrap()
            .starts_with("unix:path=/tmp/")
    );
}
fn token(name: &str) -> OAuthCredentials {
    serde_json::from_value(json!({"token":format!("synthetic-{name}-中文"),"refresh_token":format!("synthetic-refresh-{name}"),"client_id":"synthetic-client","client_secret":"synthetic-secret","expiry":"2030-01-01T00:00:00Z","scopes":["openid"],"future":{"unicode":"雪"}})).unwrap()
}
fn python(action: &str, slot: &str, input: Option<&OAuthCredentials>) {
    let mut child = Command::new("python3")
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/secret-service-validation/client.py"),
        )
        .args([action, slot])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(input).unwrap())
            .unwrap();
    } else {
        drop(child.stdin.take());
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "independent Python Secret Service operation failed"
    );
    assert!(output.stdout.is_empty());
}
fn backend() -> Arc<SystemKeyring> {
    Arc::new(SystemKeyring::with_timeout(Duration::from_secs(1)).unwrap())
}
fn profile(name: &str) -> ProfileId {
    ProfileId::new(name).unwrap()
}
#[test]
#[ignore = "requires the private Secret Service container harness"]
fn native_secret_service_interoperates_with_python_and_legacy_slots() {
    guard("healthy");
    let temporary = tempfile::tempdir().unwrap();
    let storage = Storage::new(temporary.path()).unwrap();
    let store = TokenStore::new(storage.clone(), backend(), TokenStorageMode::Keyring);
    let a = profile("interop-a");
    let b = profile("interop-b");
    python("set", &TokenStore::slot(&a), Some(&token("python-a")));
    assert_eq!(
        store.load(&a).unwrap().unwrap().token.expose(),
        token("python-a").token.expose()
    );
    store.save(&a, &token("rust-rotated")).unwrap();
    python("check", &TokenStore::slot(&a), Some(&token("rust-rotated")));
    let first = store.clone();
    let second = store.clone();
    let a2 = a.clone();
    let b2 = b.clone();
    let x = std::thread::spawn(move || {
        for _ in 0..3 {
            first.save(&a2, &token("parallel-a")).unwrap();
        }
    });
    let y = std::thread::spawn(move || {
        for _ in 0..3 {
            second.save(&b2, &token("parallel-b")).unwrap();
        }
    });
    x.join().unwrap();
    y.join().unwrap();
    python("check", &TokenStore::slot(&a), Some(&token("parallel-a")));
    python("check", &TokenStore::slot(&b), Some(&token("parallel-b")));
    python("delete", &TokenStore::slot(&b), None);
    assert!(store.load(&b).unwrap().is_none());
    store.delete(&a).unwrap();
    python("missing", &TokenStore::slot(&a), None);
    python("set", "oauth_token", Some(&token("legacy")));
    assert!(store.migrate_legacy().unwrap());
    assert!(!store.migrate_legacy().unwrap());
    python("check", "oauth_token", Some(&token("legacy")));
    python("check", "oauth_token:default", Some(&token("legacy")));
    store.delete(&ProfileId::default_profile()).unwrap();
    python("delete", "oauth_token", None);
    assert!(
        !storage
            .profile_path(&a, "oauth_token.json")
            .unwrap()
            .exists()
    );
    assert!(
        !storage
            .profile_path(&a, "oauth_keyring_fallback.json")
            .unwrap()
            .exists()
    );
}
#[test]
#[ignore = "requires the private Secret Service container harness"]
fn native_secret_service_locked_collection_is_a_bounded_error() {
    guard("locked");
    let started = Instant::now();
    let result = backend().get("oauth_token:locked-probe");
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(4));
    let error = result.err().unwrap().to_string();
    assert!(!error.contains("synthetic"));
}
#[test]
#[ignore = "requires the private Secret Service container harness"]
fn native_secret_service_unresponsive_bus_has_a_bounded_caller_wait() {
    guard("timeout");
    let backend = backend();
    for _ in 0..2 {
        let started = Instant::now();
        assert!(matches!(
            backend.get("oauth_token:timeout-probe"),
            Err(Error::CredentialTimeout(1))
        ));
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
#[test]
#[ignore = "requires the private Secret Service container harness"]
fn native_secret_service_outage_preserves_strict_and_auto_storage_contracts() {
    guard("outage");
    let storage = Storage::new("/validation/home/fallback-config").unwrap();
    let backend = backend();
    let id = profile("fallback");
    assert!(backend.get("oauth_token:outage").is_err());
    let strict = TokenStore::new(storage.clone(), backend.clone(), TokenStorageMode::Keyring);
    assert!(strict.save(&id, &token("strict")).is_err());
    assert!(
        !storage
            .profile_path(&id, "oauth_token.json")
            .unwrap()
            .exists()
    );
    let auto = TokenStore::new(storage.clone(), backend.clone(), TokenStorageMode::Auto);
    auto.save(&id, &token("outage")).unwrap();
    assert_eq!(
        auto.load(&id).unwrap().unwrap().token.expose(),
        token("outage").token.expose()
    );
    use std::os::unix::fs::PermissionsExt;
    for file in ["oauth_token.json", "oauth_keyring_fallback.json"] {
        let path = storage.profile_path(&id, file).unwrap();
        assert!(path.exists());
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let file_id = profile("file");
    let file = TokenStore::new(storage, backend, TokenStorageMode::File);
    file.save(&file_id, &token("file")).unwrap();
    assert_eq!(
        file.load(&file_id).unwrap().unwrap().token.expose(),
        token("file").token.expose()
    );
    assert!(file.delete(&file_id).is_err());
    assert!(file.load(&file_id).unwrap().is_some());
}
#[test]
#[ignore = "requires the private Secret Service container harness"]
fn native_secret_service_recovery_keeps_file_authoritative_until_explicit_keyring_write() {
    guard("recovered");
    let storage = Storage::new("/validation/home/fallback-config").unwrap();
    let id = profile("fallback");
    let backend = backend();
    let auto = TokenStore::new(storage.clone(), backend.clone(), TokenStorageMode::Auto);
    python("set", &TokenStore::slot(&id), Some(&token("stale")));
    assert_eq!(
        auto.load(&id).unwrap().unwrap().token.expose(),
        token("outage").token.expose()
    );
    auto.save(&id, &token("file-rotated")).unwrap();
    python("check", &TokenStore::slot(&id), Some(&token("stale")));
    let strict = TokenStore::new(storage.clone(), backend, TokenStorageMode::Keyring);
    strict.save(&id, &token("explicit")).unwrap();
    python("check", &TokenStore::slot(&id), Some(&token("explicit")));
    for file in ["oauth_token.json", "oauth_keyring_fallback.json"] {
        assert!(!storage.profile_path(&id, file).unwrap().exists());
    }
    assert_eq!(
        auto.load(&id).unwrap().unwrap().token.expose(),
        token("explicit").token.expose()
    );
    strict.delete(&id).unwrap();
    python("missing", &TokenStore::slot(&id), None);
}
