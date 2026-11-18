mod common;
use chrono::{Duration as Delta, Utc};
use colabcli_rust::{
    Error,
    auth::{AdcProvider, MetadataOptions, PUBLIC_SCOPES, TokenClient},
    context::{AccountContext, AccountSource},
    models::{AuthProvider, ProfileId},
};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc, thread, time::Duration};
use tiny_http::{Header, Request, Response, Server, StatusCode};
fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn context(s: &Server, path: Option<&Path>, required: bool) -> AccountContext {
    AccountContext {
        profile: ProfileId::new("metadata").unwrap(),
        source: AccountSource::Flag,
        provider: AuthProvider::Adc,
        adc_file: path.map(Path::to_path_buf),
        quota_project: None,
        adc_file_required: required,
        cancellation: None,
        aws: Default::default(),
        certificate: Default::default(),
        allow_executables: false,
        metadata: MetadataOptions {
            host: s.server_addr().to_string(),
            probe_host: s.server_addr().to_string(),
            disabled: false,
            timeout_seconds: 1,
            mtls: Box::default(),
        },
    }
}
fn provider(c: &AccountContext) -> AdcProvider {
    AdcProvider::from_context(c, TokenClient::new().unwrap())
}
fn recv(s: &Server) -> Request {
    let r = s
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("expected request");
    assert_eq!(r.method().as_str(), "GET");
    assert!(!r.headers().iter().any(|h| h.field.equiv("Authorization")));
    assert!(
        !r.headers()
            .iter()
            .any(|h| h.field.equiv("x-goog-user-project"))
    );
    assert_eq!(
        r.headers()
            .iter()
            .find(|h| h.field.equiv("Metadata-Flavor"))
            .unwrap()
            .value
            .as_str(),
        "Google"
    );
    r
}
fn google(r: Request, value: Value) {
    r.respond(
        Response::from_string(value.to_string())
            .with_header(Header::from_bytes("Metadata-Flavor", "Google").unwrap()),
    )
    .unwrap();
}
fn token(s: &Server) -> Request {
    let r = recv(s);
    let u = url::Url::parse(&format!("http://fixture{}", r.url())).unwrap();
    assert_eq!(
        u.path(),
        "/computeMetadata/v1/instance/service-accounts/default/token"
    );
    assert_eq!(
        u.query_pairs().collect::<Vec<_>>(),
        vec![("scopes".into(), PUBLIC_SCOPES.join(",").into())]
    );
    r
}
fn success(token: &str) -> Value {
    json!({"access_token":token,"token_type":"Bearer","expires_in":3600})
}
fn quiet(s: &Server) {
    assert!(s.recv_timeout(Duration::from_millis(75)).unwrap().is_none());
}

#[test]
fn missing_default_adc_discovers_metadata_and_concurrent_calls_share_only_provider_cache() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let path = temp.path().join("missing");
    let mut c = context(&server, Some(&path), false);
    c.quota_project = Some("metadata-project".into());
    let p = Arc::new(provider(&c));
    let second = provider(&c);
    let handler = thread::spawn(move || {
        for name in ["first", "second"] {
            let r = recv(&server);
            assert_eq!(r.url(), "/");
            google(r, json!({}));
            google(token(&server), success(name));
        }
        quiet(&server);
    });
    let jobs: Vec<_> = (0..8)
        .map(|_| {
            let p = p.clone();
            thread::spawn(move || assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "first"))
        })
        .collect();
    for j in jobs {
        j.join().unwrap();
    }
    assert_eq!(second.access_token(Utc::now()).unwrap().expose(), "second");
    assert_eq!(
        p.request_credentials(Utc::now())
            .unwrap()
            .quota_project_id
            .as_deref(),
        Some("metadata-project")
    );
    assert_eq!(
        second
            .request_credentials(Utc::now())
            .unwrap()
            .quota_project_id
            .as_deref(),
        Some("metadata-project")
    );
    assert!(!path.exists());
    handler.join().unwrap();
}
#[test]
fn explicit_files_and_invalid_default_files_never_fall_back_to_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let path = temp.path().join("adc");
    let c = context(&server, Some(&path), true);
    assert!(matches!(
        provider(&c).access_token(Utc::now()),
        Err(Error::Io { .. })
    ));
    for required in [false, true] {
        fs::write(&path, "private-marker-invalid-json").unwrap();
        let c = context(&server, Some(&path), required);
        assert!(provider(&c).access_token(Utc::now()).is_err());
    }
    fs::write(&path,json!({"type":"authorized_user","token":"cached-file","expiry":"2099-01-01T00:00:00Z","client_id":"fixture"}).to_string()).unwrap();
    let c = context(&server, Some(&path), false);
    assert_eq!(
        provider(&c).access_token(Utc::now()).unwrap().expose(),
        "cached-file"
    );
    quiet(&server);
}
#[test]
fn disabled_or_invalid_metadata_configuration_makes_no_requests() {
    let server = server();
    let mut c = context(&server, None, false);
    c.metadata.disabled = true;
    assert!(matches!(
        provider(&c).access_token(Utc::now()),
        Err(Error::NeedsAuth)
    ));
    for host in [
        "http://private-marker",
        "user:private-marker@host",
        "host/path",
        "host?private-marker",
        "host#private-marker",
        "host\\private-marker",
    ] {
        let mut c = context(&server, None, false);
        c.metadata.host = host.into();
        assert!(
            !provider(&c)
                .access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("private-marker")
        );
    }
    let mut c = context(&server, None, false);
    c.metadata.timeout_seconds = 0;
    assert!(provider(&c).access_token(Utc::now()).is_err());
    quiet(&server);
}
#[test]
fn discovery_requires_google_flavor_and_does_not_follow_redirects() {
    let server = server();
    let p = provider(&context(&server, None, false));
    let handler = thread::spawn(move || {
        let r = recv(&server);
        r.respond(Response::from_string("private-marker")).unwrap();
        let r = recv(&server);
        r.respond(
            Response::from_string("private-marker")
                .with_status_code(StatusCode(302))
                .with_header(
                    Header::from_bytes("Location", "http://private-marker.invalid/").unwrap(),
                ),
        )
        .unwrap();
        let r = recv(&server);
        google(r, json!({}));
        google(token(&server), success("recovered"));
        quiet(&server);
    });
    for _ in 0..2 {
        assert!(matches!(p.access_token(Utc::now()), Err(Error::NeedsAuth)));
    }
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "recovered");
    handler.join().unwrap();
}
#[test]
fn metadata_expiry_and_forced_refresh_preserve_valid_old_token_after_failure() {
    let server = server();
    let p = provider(&context(&server, None, false));
    let handler = thread::spawn(move || {
        let r = recv(&server);
        google(r, json!({}));
        google(token(&server), success("old"));
        let r = token(&server);
        r.respond(Response::from_string("private-marker").with_status_code(StatusCode(503)))
            .unwrap();
        google(token(&server), success("new"));
        quiet(&server);
    });
    let now = Utc::now();
    assert_eq!(p.access_token(now).unwrap().expose(), "old");
    assert_eq!(
        p.access_token(now + Delta::seconds(3500)).unwrap().expose(),
        "old"
    );
    assert!(
        !p.refresh_access_token(now)
            .unwrap_err()
            .to_string()
            .contains("private-marker")
    );
    assert_eq!(p.access_token(now).unwrap().expose(), "old");
    assert_eq!(
        p.access_token(now + Delta::seconds(3550)).unwrap().expose(),
        "new"
    );
    handler.join().unwrap();
}
#[test]
fn malformed_metadata_tokens_are_rejected_without_poisoning_cache() {
    let server = server();
    let p = provider(&context(&server, None, false));
    let mut bad = Vec::new();
    for (key, v) in [
        ("access_token", json!("")),
        ("access_token", json!("private marker")),
        ("expires_in", json!(0)),
        ("expires_in", json!(i64::MAX)),
        ("token_type", json!("MAC")),
    ] {
        let mut body = success("invalid");
        body[key] = v;
        bad.push(body);
    }
    let mut missing = success("invalid");
    missing.as_object_mut().unwrap().remove("expires_in");
    bad.push(missing);
    let n = bad.len();
    let handler = thread::spawn(move || {
        let r = recv(&server);
        google(r, json!({}));
        for v in bad {
            google(token(&server), v);
        }
        google(token(&server), success("recovered"));
        quiet(&server);
    });
    for _ in 0..n {
        assert!(p.access_token(Utc::now()).is_err());
    }
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "recovered");
    handler.join().unwrap();
}
#[test]
fn metadata_token_redirect_missing_flavor_invalid_json_and_oversize_are_retryable() {
    let server = server();
    let p = provider(&context(&server, None, false));
    let handler = thread::spawn(move || {
        let r = recv(&server);
        google(r, json!({}));
        let r = token(&server);
        r.respond(
            Response::from_string("private-marker")
                .with_status_code(StatusCode(302))
                .with_header(
                    Header::from_bytes("Location", "http://private-marker.invalid/").unwrap(),
                ),
        )
        .unwrap();
        let r = token(&server);
        r.respond(Response::from_string(success("without-flavor").to_string()))
            .unwrap();
        for body in ["private-marker".into(), "x".repeat(65537)] {
            let r = token(&server);
            r.respond(
                Response::from_string(body)
                    .with_header(Header::from_bytes("Metadata-Flavor", "Google").unwrap()),
            )
            .unwrap();
        }
        google(token(&server), success("recovered"));
        quiet(&server);
    });
    for _ in 0..4 {
        assert!(
            !p.access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("private-marker")
        );
    }
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "recovered");
    handler.join().unwrap();
}

#[test]
fn metadata_discovery_timeout_releases_cache_lock_and_next_call_can_retry() {
    let server = server();
    let p = provider(&context(&server, None, false));
    let (done, wait) = std::sync::mpsc::channel();
    let handler = thread::spawn(move || {
        let r = recv(&server);
        wait.recv_timeout(Duration::from_secs(8)).unwrap();
        let _ = r.respond(
            Response::from_string("late")
                .with_header(Header::from_bytes("Metadata-Flavor", "Google").unwrap()),
        );
        let r = recv(&server);
        google(r, json!({}));
        google(token(&server), success("after-timeout"));
        quiet(&server);
    });
    let started = std::time::Instant::now();
    assert!(matches!(p.access_token(Utc::now()), Err(Error::NeedsAuth)));
    assert!(started.elapsed() < Duration::from_secs(5));
    done.send(()).unwrap();
    assert_eq!(
        p.access_token(Utc::now()).unwrap().expose(),
        "after-timeout"
    );
    handler.join().unwrap();
}

#[test]
fn cli_snapshots_metadata_mtls_mode_and_unused_invalid_mode_does_not_break_file_adc() {
    use colabcli_rust::credentials::TokenStorageMode;
    use std::process::Command;
    let fixture = common::Fixture::new(TokenStorageMode::File);
    fixture
        .accounts
        .update(|registry| {
            registry
                .accounts
                .get_mut(&common::id("default"))
                .unwrap()
                .auth_provider = AuthProvider::Adc;
            Ok(())
        })
        .unwrap();
    let server = server();
    let address = server.server_addr().to_string();
    let run = |mode: &str| {
        Command::new(env!("CARGO_BIN_EXE_colab"))
            .env_clear()
            .env("HOME", fixture.temp.path())
            .env("CLOUDSDK_CONFIG", fixture.temp.path())
            .env("NO_GCE_CHECK", "false")
            .env("GCE_METADATA_HOST", &address)
            .env("GCE_METADATA_IP", &address)
            .env("GCE_METADATA_TIMEOUT", "1")
            .env("GCE_METADATA_MTLS_MODE", mode)
            .arg("--config-dir")
            .arg(fixture.temp.path())
            .args([
                "--token-storage",
                "file",
                "--control-plane-url",
                &format!("http://{address}"),
                "sessions",
                "--json",
            ])
            .output()
            .unwrap()
    };
    for (mode, expected) in [
        ("invalid", "GCE_METADATA_MTLS_MODE"),
        ("strict", "默认 metadata 主机"),
    ] {
        let output = run(mode);
        assert!(output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(value["error"].as_str().unwrap().contains(expected));
        quiet(&server);
    }
    let worker = thread::spawn(move || {
        google(recv(&server), json!({}));
        google(token(&server), success("metadata-fixture"));
        for expected in ["metadata-fixture", "cached-file"] {
            let r = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(
                r.headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .unwrap()
                    .value
                    .as_str(),
                format!("Bearer {expected}")
            );
            assert!(!r.headers().iter().any(|h| h.field.equiv("Metadata-Flavor")));
            r.respond(Response::from_string(json!({"assignments":[]}).to_string()))
                .unwrap();
        }
        quiet(&server);
    });
    let output = run("none");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["error"].is_null(), "{value}");
    let path = fixture.temp.path().join("adc.json");
    fs::write(&path,json!({"type":"authorized_user","token":"cached-file","expiry":(Utc::now()+Delta::hours(1)).to_rfc3339(),
        "client_id":"fixture","refresh_token":"fixture","token_uri":format!("http://{address}/unused-token")}).to_string()).unwrap();
    fixture
        .accounts
        .update(|registry| {
            registry
                .accounts
                .get_mut(&common::id("default"))
                .unwrap()
                .adc_credentials_file = Some(path.to_string_lossy().into_owned());
            Ok(())
        })
        .unwrap();
    let output = run("invalid");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["error"].is_null(), "{value}");
    worker.join().unwrap();
}
