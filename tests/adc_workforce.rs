use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{Duration as Delta, Utc};
use colabcli_rust::{
    Error,
    auth::{AdcProvider, ControlCredentials, TokenClient},
    colab_api::{ApiConfig, ColabApi},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, sync::Arc, thread, time::Duration};
use tiny_http::{Request, Response, Server, StatusCode};
fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn base(s: &Server) -> String {
    format!("http://{}", s.server_addr())
}
fn document(base: &str, name: &str) -> Value {
    json!({"type":"external_account_authorized_user", "audience":"//iam.googleapis.com/locations/global/workforcePools/p/providers/p", "token_url":format!("{base}/v1/oauth/token"),"refresh_token":format!("refresh-{name}"),"client_id":name,"client_secret":format!("secret:{name}&你好"),"token_info_url":"https://unused.invalid/introspect","revoke_url":"https://unused.invalid/revoke"})
}
fn provider(path: &Path, doc: &Value) -> AdcProvider {
    fs::write(path, doc.to_string()).unwrap();
    AdcProvider::new(path.into(), TokenClient::new().unwrap())
}
fn recv(s: &Server) -> Request {
    s.recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("expected request")
}
fn form(r: &mut Request, name: &str) -> BTreeMap<String, String> {
    assert_eq!(r.method().as_str(), "POST");
    assert_eq!(r.url(), "/v1/oauth/token");
    let auth = r
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .unwrap()
        .value
        .as_str();
    assert_eq!(
        STANDARD
            .decode(auth.strip_prefix("Basic ").unwrap())
            .unwrap(),
        format!("{name}:secret:{name}&你好").as_bytes()
    );
    let mut body = String::new();
    r.as_reader().read_to_string(&mut body).unwrap();
    let f: BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(f.len(), 2);
    assert_eq!(f["grant_type"], "refresh_token");
    f
}
fn success(token: &str) -> Value {
    json!({"access_token":token,"expires_in":3600,"token_type":"Bearer"})
}
fn reply(r: Request, v: Value) {
    r.respond(Response::from_string(v.to_string())).unwrap();
}
fn quiet(s: &Server) {
    assert!(s.recv_timeout(Duration::from_millis(75)).unwrap().is_none());
}

#[test]
fn workforce_user_can_source_impersonation_without_mixing_tokens_or_quota() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    let server = server();
    let uri = base(&server);
    let mut source = document(&uri, "workforce");
    source["quota_project_id"] = json!("workforce-project");
    let target_path = "/v1/projects/-/serviceAccounts/fixture@example.invalid:generateAccessToken";
    let p = provider(
        &path,
        &json!({
            "type":"impersonated_service_account", "source_credentials":source,
            "service_account_impersonation_url":format!("{uri}{target_path}"),
            "quota_project_id":"target-project", "delegates":["projects/-/serviceAccounts/delegate@example.invalid"],
        }),
    );
    let original = fs::read(&path).unwrap();
    let now = Utc::now();
    let handler = thread::spawn(move || {
        let mut sts = recv(&server);
        assert!(
            !sts.headers()
                .iter()
                .any(|h| h.field.equiv("x-goog-user-project"))
        );
        assert_eq!(
            form(&mut sts, "workforce")["refresh_token"],
            "refresh-workforce"
        );
        reply(sts, success("workforce-source-token"));
        for failed in [true, false] {
            let mut iam = recv(&server);
            assert_eq!(iam.url(), target_path);
            assert_eq!(iam.method().as_str(), "POST");
            for (name, expected) in [
                ("Authorization", "Bearer workforce-source-token"),
                ("x-goog-user-project", "workforce-project"),
            ] {
                let value = iam.headers().iter().find(|h| h.field.equiv(name)).unwrap();
                assert_eq!(value.value.as_str(), expected);
            }
            let body: Value = serde_json::from_reader(iam.as_reader()).unwrap();
            assert_eq!(
                body,
                json!({"scope":colabcli_rust::auth::PUBLIC_SCOPES,"lifetime":"3600s",
                                   "delegates":["projects/-/serviceAccounts/delegate@example.invalid"]})
            );
            if failed {
                iam.respond(
                    Response::from_string("private-iam-response").with_status_code(StatusCode(503)),
                )
                .unwrap();
            } else {
                reply(
                    iam,
                    json!({"accessToken":"impersonated-target-token","expireTime":(now + Delta::hours(1)).to_rfc3339()}),
                );
            }
        }
        let assignments = recv(&server);
        assert_eq!(assignments.method().as_str(), "GET");
        assert_eq!(assignments.url(), "/tun/m/assignments?authuser=0");
        for (name, expected) in [
            ("Authorization", "Bearer impersonated-target-token"),
            ("x-goog-user-project", "target-project"),
        ] {
            let value = assignments
                .headers()
                .iter()
                .find(|h| h.field.equiv(name))
                .unwrap();
            assert_eq!(value.value.as_str(), expected);
        }
        reply(assignments, json!({"assignments":[]}));
        quiet(&server);
    });
    assert!(matches!(
        p.request_credentials(now),
        Err(Error::OAuthStatus(503))
    ));
    for _ in 0..2 {
        let result = p.request_credentials(now).unwrap();
        assert_eq!(result.token.expose(), "impersonated-target-token");
        assert_eq!(result.quota_project_id.as_deref(), Some("target-project"));
    }
    let api = ColabApi::with_credentials_source(
        ApiConfig {
            base_url: uri.parse().unwrap(),
            timeout: Duration::from_secs(3),
            retries: 0,
            backoff: Duration::ZERO,
        },
        ControlCredentials::adc(p),
    )
    .unwrap();
    assert!(api.assignments().unwrap().is_empty());
    assert_eq!(fs::read(path).unwrap(), original);
    handler.join().unwrap();
}

#[test]
fn workforce_concurrent_profiles_isolate_basic_credentials_and_cache_without_expanding_scopes() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let mut providers = Vec::new();
    for name in ["a", "b"] {
        let mut doc = document(&uri, name);
        doc["scopes"] = json!(["retained-grant-scope"]);
        providers.push(Arc::new(provider(&temp.path().join(name), &doc)));
    }
    let handler = thread::spawn(move || {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..2 {
            let mut r = recv(&server);
            let auth = r
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .unwrap()
                .value
                .as_str();
            let decoded = String::from_utf8(
                STANDARD
                    .decode(auth.strip_prefix("Basic ").unwrap())
                    .unwrap(),
            )
            .unwrap();
            let name = decoded.split(':').next().unwrap();
            let name = name.to_string();
            let f = form(&mut r, &name);
            assert_eq!(f["refresh_token"], format!("refresh-{name}"));
            assert!(seen.insert(name.clone()));
            reply(r, success(&name));
        }
        quiet(&server);
    });
    let workers: Vec<_> = (0..12)
        .map(|i| {
            let p = providers[i % 2].clone();
            thread::spawn(move || {
                assert_eq!(
                    p.access_token(Utc::now()).unwrap().expose(),
                    if i % 2 == 0 { "a" } else { "b" }
                );
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    handler.join().unwrap();
}

#[test]
fn workforce_refresh_rotation_is_memory_only_and_survives_failed_forced_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let path = temp.path().join("adc");
    let p = provider(&path, &document(&uri, "a"));
    let original = fs::read(&path).unwrap();
    let handler = thread::spawn(move || {
        for i in 0..4 {
            let mut r = recv(&server);
            let f = form(&mut r, "a");
            assert_eq!(
                f["refresh_token"],
                if i == 0 { "refresh-a" } else { "rotated" }
            );
            if i == 2 {
                r.respond(
                    Response::from_string("private-marker").with_status_code(StatusCode(503)),
                )
                .unwrap();
            } else {
                let mut v = success(&format!("access-{i}"));
                if i == 0 {
                    v["refresh_token"] = json!("rotated");
                }
                reply(r, v);
            }
        }
        quiet(&server);
    });
    let now = Utc::now();
    assert_eq!(p.access_token(now).unwrap().expose(), "access-0");
    assert_eq!(p.refresh_access_token(now).unwrap().expose(), "access-1");
    assert!(
        !p.refresh_access_token(now)
            .unwrap_err()
            .to_string()
            .contains("private-marker")
    );
    assert_eq!(p.access_token(now).unwrap().expose(), "access-1");
    assert_eq!(p.refresh_access_token(now).unwrap().expose(), "access-3");
    assert_eq!(fs::read(path).unwrap(), original);
    handler.join().unwrap();
}

#[test]
fn workforce_cached_token_only_is_usable_but_cannot_force_refresh_or_outlive_expiry_margin() {
    let temp = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let p = provider(
        &temp.path().join("adc"),
        &json!({"type":"external_account_authorized_user","token":"cached","expiry":(now+Delta::hours(1)).to_rfc3339()}),
    );
    assert_eq!(p.access_token(now).unwrap().expose(), "cached");
    assert!(matches!(p.refresh_access_token(now), Err(Error::NeedsAuth)));
    assert_eq!(p.access_token(now).unwrap().expose(), "cached");
    assert!(matches!(
        p.access_token(now + Delta::seconds(3550)),
        Err(Error::NeedsAuth)
    ));
}

#[test]
fn workforce_refreshes_near_expiry_without_using_token_info_or_revoke_endpoints() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let p = provider(&temp.path().join("adc"), &document(&uri, "a"));
    let handler = thread::spawn(move || {
        for i in 0..2 {
            let mut r = recv(&server);
            assert_eq!(form(&mut r, "a")["refresh_token"], "refresh-a");
            reply(r, success(&format!("access-{i}")));
        }
        quiet(&server);
    });
    let now = Utc::now();
    assert_eq!(p.access_token(now).unwrap().expose(), "access-0");
    assert_eq!(
        p.access_token(now + Delta::seconds(3500)).unwrap().expose(),
        "access-0"
    );
    assert_eq!(
        p.access_token(now + Delta::seconds(3550)).unwrap().expose(),
        "access-1"
    );
    handler.join().unwrap();
}

#[test]
fn workforce_bad_response_cannot_rotate_refresh_or_replace_valid_access_token() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let p = provider(&temp.path().join("adc"), &document(&uri, "a"));
    let mut bad = Vec::new();
    for (key, value) in [
        ("access_token", json!("")),
        ("access_token", json!("private marker")),
        ("token_type", json!("MAC")),
        ("expires_in", json!(0)),
        ("expires_in", json!(-1)),
        ("expires_in", json!(i64::MAX)),
        ("expires_in", json!("3600")),
        ("refresh_token", json!("")),
    ] {
        let mut v = success("invalid");
        v["refresh_token"] = json!("must-not-commit");
        v[key] = value;
        bad.push(v);
    }
    let mut missing = success("invalid");
    missing.as_object_mut().unwrap().remove("expires_in");
    missing["refresh_token"] = json!("must-not-commit");
    bad.push(missing);
    let count = bad.len();
    let handler = thread::spawn(move || {
        let mut r = recv(&server);
        form(&mut r, "a");
        reply(r, success("valid-old"));
        for value in bad {
            let mut r = recv(&server);
            assert_eq!(form(&mut r, "a")["refresh_token"], "refresh-a");
            reply(r, value);
        }
        let mut r = recv(&server);
        assert_eq!(form(&mut r, "a")["refresh_token"], "refresh-a");
        reply(r, success("valid-new"));
        quiet(&server);
    });
    let now = Utc::now();
    assert_eq!(p.access_token(now).unwrap().expose(), "valid-old");
    for _ in 0..count {
        let error = p.refresh_access_token(now).unwrap_err().to_string();
        assert!(!error.contains("private marker"));
        assert_eq!(p.access_token(now).unwrap().expose(), "valid-old");
    }
    assert_eq!(p.refresh_access_token(now).unwrap().expose(), "valid-new");
    handler.join().unwrap();
}

#[test]
fn workforce_invalid_configuration_fails_locally_without_echoing_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let valid = document(&uri, "a");
    let mut cases = Vec::new();
    for (key, value) in [
        (
            "token_url",
            json!("http://sts.googleapis.com/v1/oauth/token"),
        ),
        ("token_url", json!(format!("{uri}/v1/token"))),
        (
            "token_url",
            json!(format!("{uri}/v1/oauth/token?private-marker")),
        ),
        (
            "token_url",
            json!("https://private-marker@sts.googleapis.com/v1/oauth/token"),
        ),
        ("universe_domain", json!("private-marker")),
        (
            "audience",
            json!(
                "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/p"
            ),
        ),
        ("client_id", json!("bad:private-marker")),
        ("refresh_token", json!("")),
        ("client_secret", json!("")),
        ("expiry", json!("private-marker")),
        ("token", json!("private marker")),
    ] {
        let mut v = valid.clone();
        v[key] = value;
        cases.push(v);
    }
    for key in ["token_url", "client_id", "client_secret", "refresh_token"] {
        let mut v = valid.clone();
        v.as_object_mut().unwrap().remove(key);
        cases.push(v);
    }
    for v in cases {
        let p = provider(&temp.path().join("adc"), &v);
        assert!(
            !p.access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("private-marker")
        );
    }
    quiet(&server);
}

#[test]
fn workforce_http_redirect_invalid_json_and_oversize_are_retryable_without_secret_errors() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let p = provider(&temp.path().join("adc"), &document(&uri, "a"));
    let handler = thread::spawn(move || {
        for (code, body) in [
            (302, "private-marker".into()),
            (500, "private-marker".into()),
            (200, "private-marker".into()),
            (200, "x".repeat(65537)),
        ] {
            let mut r = recv(&server);
            form(&mut r, "a");
            r.respond(
                Response::from_string(body)
                    .with_status_code(StatusCode(code))
                    .with_header(
                        tiny_http::Header::from_bytes("Location", format!("{uri}/must-not-follow"))
                            .unwrap(),
                    ),
            )
            .unwrap();
        }
        let mut r = recv(&server);
        form(&mut r, "a");
        let mut v = success("recovered");
        v.as_object_mut().unwrap().remove("token_type");
        reply(r, v);
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
