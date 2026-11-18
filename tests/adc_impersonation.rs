mod common;
use chrono::{DateTime, Duration as Delta, Utc};
use colabcli_rust::{
    Error,
    auth::{AdcProvider, PUBLIC_SCOPES, TokenClient},
    credentials::TokenStorageMode,
    models::{Account, AuthProvider},
};
use common::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, sync::Arc, thread, time::Duration};
use tiny_http::{Request, Response, Server, StatusCode};
use url::Url;
fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn base(server: &Server) -> String {
    format!("http://{}", server.server_addr())
}
fn iam(base: &str, target: &str) -> String {
    format!("{base}/v1/projects/-/serviceAccounts/{target}:generateAccessToken")
}
fn source(base: &str, name: &str, cached: bool) -> Value {
    let mut source = json!({"type":"authorized_user","client_id":name,"client_secret":"source-secret-fixture","refresh_token":format!("refresh-{name}"),"token_uri":format!("{base}/source-token"),"scopes":"https://www.googleapis.com/auth/cloud-platform"});
    if cached {
        source["token"] = json!(format!("source-{name}"));
        source["expiry"] = json!("2099-01-01T00:00:00Z");
    }
    source
}
fn document(base: &str, target: &str, source: Value) -> Value {
    json!({"type":"impersonated_service_account","service_account_impersonation_url":iam(base,target),"source_credentials":source})
}
fn recv(server: &Server) -> Request {
    server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("expected request")
}
fn form(request: &mut Request) -> BTreeMap<String, String> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).unwrap();
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}
fn payload(request: &mut Request, bearer: &str) -> Value {
    assert_eq!(request.method().as_str(), "POST");
    assert_eq!(
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .unwrap()
            .value
            .as_str(),
        format!("Bearer {bearer}")
    );
    assert!(
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Content-Type"))
            .unwrap()
            .value
            .as_str()
            .starts_with("application/json")
    );
    let mut text = String::new();
    request.as_reader().read_to_string(&mut text).unwrap();
    serde_json::from_str(&text).unwrap()
}
fn reply(request: Request, value: Value) {
    request
        .respond(Response::from_string(value.to_string()))
        .unwrap();
}
fn success(token: &str, expiry: DateTime<Utc>) -> Value {
    json!({"accessToken":token,"expireTime":expiry.to_rfc3339_opts(chrono::SecondsFormat::Nanos,true),"unknown":"private-marker"})
}
fn provider(path: &std::path::Path) -> AdcProvider {
    AdcProvider::new(path.into(), TokenClient::new().unwrap())
}
#[test]
fn impersonation_profiles_share_only_their_own_target_cache_and_preserve_delegates() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let base = base(&server);
    let mut providers = Vec::new();
    let mut files = Vec::new();
    for name in ["a", "b"] {
        let path = temp.path().join(format!("{name}.json"));
        let mut doc = document(
            &base,
            &format!("{name}@example.invalid"),
            source(&base, name, true),
        );
        if name == "b" {
            doc["service_account_impersonation_url"] = json!(iam(&base, "b%40example.invalid"));
        }
        doc["delegates"] = json!([format!(
            "projects/-/serviceAccounts/delegate-{name}@example.invalid"
        )]);
        let bytes = serde_json::to_vec(&doc).unwrap();
        fs::write(&path, &bytes).unwrap();
        providers.push(Arc::new(provider(&path)));
        files.push((path, bytes));
    }
    let env_before = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS");
    let handler = thread::spawn(move || {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..2 {
            let mut request = recv(&server);
            let name = if request.url().contains("/a@example.invalid:") {
                "a"
            } else {
                "b"
            };
            assert!(seen.insert(name));
            let value = payload(&mut request, &format!("source-{name}"));
            assert_eq!(
                value,
                json!({"scope":PUBLIC_SCOPES,"lifetime":"3600s","delegates":[format!("projects/-/serviceAccounts/delegate-{name}@example.invalid")]})
            );
            reply(
                request,
                success(&format!("target-{name}"), now() + Delta::hours(1)),
            );
        }
        assert!(
            server
                .recv_timeout(Duration::from_millis(150))
                .unwrap()
                .is_none()
        );
    });
    let workers: Vec<_> = (0..8)
        .map(|n| {
            let p = providers[n % 2].clone();
            thread::spawn(move || (n % 2, p.access_token(now()).unwrap()))
        })
        .collect();
    for worker in workers {
        let (n, token) = worker.join().unwrap();
        assert_eq!(token.expose(), if n == 0 { "target-a" } else { "target-b" });
    }
    handler.join().unwrap();
    for (path, bytes) in files {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert_eq!(
        std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS"),
        env_before
    );
}
#[test]
fn impersonation_target_refresh_keeps_source_rotation_and_previous_valid_target_after_iam_failure()
{
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    let server = server();
    let base = base(&server);
    let bytes = serde_json::to_vec(&document(
        &base,
        "target@example.invalid",
        source(&base, "a", false),
    ))
    .unwrap();
    fs::write(&path, &bytes).unwrap();
    let p = provider(&path);
    let handler = thread::spawn(move || {
        for (round, seconds) in [(0, 0), (1, 500), (2, 4042)] {
            let mut request = recv(&server);
            assert!(request.url().starts_with("/source-token"));
            let fields = form(&mut request);
            assert_eq!(
                fields["refresh_token"],
                if round == 0 {
                    "refresh-a".to_owned()
                } else {
                    format!("rotated-{}", round - 1)
                }
            );
            reply(
                request,
                json!({"access_token":format!("source-{round}"),"refresh_token":format!("rotated-{round}"),"expires_in":if round==0 {120} else {3600}}),
            );
            let mut request = recv(&server);
            assert!(request.url().contains(":generateAccessToken"));
            assert_eq!(
                payload(&mut request, &format!("source-{round}")),
                json!({"scope":PUBLIC_SCOPES,"lifetime":"3600s"})
            );
            if round == 1 {
                request
                    .respond(
                        Response::from_string("private-marker source-1")
                            .with_status_code(StatusCode(403)),
                    )
                    .unwrap();
                let mut request = recv(&server);
                assert!(request.url().contains(":generateAccessToken"));
                payload(&mut request, "source-1");
                reply(request, success("target-1", now() + Delta::seconds(4102)));
            } else {
                reply(
                    request,
                    success(
                        &format!("target-{round}"),
                        now() + Delta::seconds(seconds + 3600),
                    ),
                );
            }
        }
    });
    assert_eq!(p.access_token(now()).unwrap().expose(), "target-0");
    assert_eq!(
        p.access_token(now() + Delta::seconds(499))
            .unwrap()
            .expose(),
        "target-0"
    );
    let error = p
        .refresh_access_token(now() + Delta::seconds(500))
        .unwrap_err();
    assert!(matches!(error, Error::OAuthStatus(403)));
    assert!(!error.to_string().contains("private-marker"));
    assert_eq!(
        p.access_token(now() + Delta::seconds(501))
            .unwrap()
            .expose(),
        "target-0"
    );
    assert_eq!(
        p.refresh_access_token(now() + Delta::seconds(502))
            .unwrap()
            .expose(),
        "target-1"
    );
    assert_eq!(
        p.access_token(now() + Delta::seconds(4041))
            .unwrap()
            .expose(),
        "target-1"
    );
    assert_eq!(
        p.access_token(now() + Delta::seconds(4042))
            .unwrap()
            .expose(),
        "target-2"
    );
    handler.join().unwrap();
    assert_eq!(fs::read(path).unwrap(), bytes);
}
#[test]
fn impersonation_accepts_service_account_source_and_uses_only_exchanged_source_bearer() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    let server = server();
    let base = base(&server);
    let src =
        service_account::document(0, "source@example.invalid", &format!("{base}/source-token"));
    let mut doc = document(&base, "123456789012345678901", src);
    doc["delegates"] = Value::Null;
    fs::write(&path, doc.to_string()).unwrap();
    let handler = thread::spawn(move || {
        let mut request = recv(&server);
        assert_eq!(request.url(), "/source-token");
        let fields = form(&mut request);
        assert_eq!(
            fields["grant_type"],
            "urn:ietf:params:oauth:grant-type:jwt-bearer"
        );
        service_account::verify(&fields["assertion"], 0, "source@example.invalid", now());
        reply(
            request,
            json!({"access_token":"source-service","expires_in":3600}),
        );
        let mut request = recv(&server);
        assert!(request.url().contains("/123456789012345678901:"));
        payload(&mut request, "source-service");
        reply(request, success("target-service", now() + Delta::hours(1)));
    });
    assert_eq!(
        provider(&path).access_token(now()).unwrap().expose(),
        "target-service"
    );
    handler.join().unwrap();
}
#[test]
fn impersonation_rejects_bad_resources_nested_or_external_sources_before_network() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    let server = server();
    let base = base(&server);
    let original = document(&base, "target@example.invalid", source(&base, "a", false));
    for (field, replacement) in [
        (
            "service_account_impersonation_url",
            json!(
                "https://private-marker.invalid/v1/projects/-/serviceAccounts/a@example.invalid:generateAccessToken"
            ),
        ),
        (
            "service_account_impersonation_url",
            json!(iam("http://private-marker.invalid", "a@example.invalid")),
        ),
        (
            "service_account_impersonation_url",
            json!(format!(
                "{}?private-marker=x",
                iam(&base, "a@example.invalid")
            )),
        ),
        (
            "service_account_impersonation_url",
            json!(iam(&base, "a%2Fb@example.invalid")),
        ),
        (
            "service_account_impersonation_url",
            json!(iam(&base, "a%0Ab@example.invalid")),
        ),
        (
            "service_account_impersonation_url",
            json!(format!(
                "{base}/v1/projects/project-id/serviceAccounts/a@example.invalid:generateAccessToken"
            )),
        ),
        (
            "delegates",
            json!(["projects/project-id/serviceAccounts/private-marker@example.invalid"]),
        ),
        (
            "delegates",
            json!(["projects/-/serviceAccounts/../../private-marker"]),
        ),
        (
            "source_credentials",
            json!({"type":"external_account","credential_source":{"executable":{"command":"private-marker-must-not-run"}}}),
        ),
        ("source_credentials", original.clone()),
    ] {
        let mut doc = original.clone();
        doc[field] = replacement;
        let bytes = serde_json::to_vec(&doc).unwrap();
        fs::write(&path, &bytes).unwrap();
        let error = provider(&path).access_token(now()).unwrap_err();
        assert!(!error.to_string().contains("private-marker"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    assert!(
        server
            .recv_timeout(Duration::from_millis(150))
            .unwrap()
            .is_none()
    );
}
#[test]
fn impersonation_rejects_redirects_or_invalid_iam_responses_without_revealing_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    for (status, body, expected) in [
        (302, "private-marker".to_owned(), "302"),
        (401, "private-marker".to_owned(), "401"),
        (403, "private-marker".to_owned(), "403"),
        (200, "x".repeat(65537), "响应过大"),
        (200, "private-marker".to_owned(), "响应无效"),
        (
            200,
            json!({"accessToken":"private-marker"}).to_string(),
            "响应无效",
        ),
        (
            200,
            json!({"accessToken":"private-marker","expireTime":"invalid"}).to_string(),
            "到期时间无效",
        ),
        (
            200,
            success("private-marker", now()).to_string(),
            "token 或到期时间无效",
        ),
        (
            200,
            success("", now() + Delta::hours(1)).to_string(),
            "token 或到期时间无效",
        ),
        (
            200,
            success("private-marker\n", now() + Delta::hours(1)).to_string(),
            "token 或到期时间无效",
        ),
    ] {
        let server = server();
        let base = base(&server);
        let doc = document(&base, "target@example.invalid", source(&base, "a", true));
        fs::write(&path, doc.to_string()).unwrap();
        let redirect = iam(&base, "must-not-follow@example.invalid");
        let handler = thread::spawn(move || {
            let mut request = recv(&server);
            payload(&mut request, "source-a");
            request
                .respond(
                    Response::from_string(body)
                        .with_status_code(StatusCode(status))
                        .with_header(tiny_http::Header::from_bytes("Location", redirect).unwrap()),
                )
                .unwrap();
            assert!(
                server
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .is_none()
            );
        });
        let error = provider(&path).access_token(now()).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        for secret in ["private-marker", "source-a", "http://"] {
            assert!(!error.to_string().contains(secret));
        }
        handler.join().unwrap();
    }
}
#[test]
fn impersonation_source_failure_never_calls_iam_and_explicit_retry_keeps_original_source() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    let server = server();
    let base = base(&server);
    fs::write(
        &path,
        document(&base, "target@example.invalid", source(&base, "a", false)).to_string(),
    )
    .unwrap();
    let p = provider(&path);
    let handler = thread::spawn(move || {
        let mut request = recv(&server);
        assert_eq!(request.url(), "/source-token");
        assert_eq!(form(&mut request)["refresh_token"], "refresh-a");
        request
            .respond(Response::from_string("private-marker").with_status_code(StatusCode(500)))
            .unwrap();
        let mut request = recv(&server);
        assert_eq!(request.url(), "/source-token");
        assert_eq!(form(&mut request)["refresh_token"], "refresh-a");
        reply(
            request,
            json!({"access_token":"source-recovered","expires_in":3600}),
        );
        let mut request = recv(&server);
        assert!(request.url().contains(":generateAccessToken"));
        payload(&mut request, "source-recovered");
        reply(
            request,
            success("target-recovered", now() + Delta::hours(1)),
        );
    });
    assert!(matches!(
        p.access_token(now()),
        Err(Error::OAuthStatus(500))
    ));
    assert_eq!(p.access_token(now()).unwrap().expose(), "target-recovered");
    handler.join().unwrap();
}

#[test]
fn impersonation_cli_login_sessions_whoami_and_status_use_target_only_and_keep_source_unchanged() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = server();
    let base = base(&server);
    let path = f.temp.path().join("impersonated.json");
    let bytes = serde_json::to_vec(&document(
        &base,
        "target@example.invalid",
        source(&base, "cli", true),
    ))
    .unwrap();
    fs::write(&path, &bytes).unwrap();
    f.accounts
        .add(
            id("target"),
            Account {
                auth_provider: AuthProvider::Adc,
                adc_credentials_file: Some(path.to_str().unwrap().into()),
                ..Account::default()
            },
        )
        .unwrap();
    let before = serde_json::to_value(f.accounts.registry().unwrap()).unwrap();
    let handler = thread::spawn(move || {
        let mut counts = BTreeMap::new();
        for _ in 0..8 {
            let mut request = recv(&server);
            let url = Url::parse(&format!("http://localhost{}", request.url())).unwrap();
            let kind = if url.path().ends_with(":generateAccessToken") {
                "iam"
            } else {
                url.path()
            };
            *counts.entry(kind.to_owned()).or_insert(0) += 1;
            let value = match kind {
                "iam" => {
                    assert_eq!(
                        payload(&mut request, "source-cli"),
                        json!({"scope":PUBLIC_SCOPES,"lifetime":"3600s"})
                    );
                    success("target-cli-fixture", Utc::now() + Delta::hours(1))
                }
                "/tokeninfo" => {
                    assert_eq!(
                        url.query_pairs()
                            .find(|(k, _)| k == "access_token")
                            .unwrap()
                            .1,
                        "target-cli-fixture"
                    );
                    assert!(
                        !request
                            .headers()
                            .iter()
                            .any(|h| h.field.equiv("Authorization"))
                    );
                    json!({"email":"target@example.invalid","scope":"openid"})
                }
                path => {
                    assert_eq!(
                        request
                            .headers()
                            .iter()
                            .find(|h| h.field.equiv("Authorization"))
                            .unwrap()
                            .value
                            .as_str(),
                        "Bearer target-cli-fixture"
                    );
                    match path {
                        "/tun/m/assignments" => json!({"assignments":[]}),
                        "/tun/m/ccu-info" => {
                            json!({"currentBalance":7.5,"consumptionRateHourly":0.0,"assignmentsCount":0})
                        }
                        _ => panic!("unexpected request"),
                    }
                }
            };
            reply(request, value);
        }
        assert_eq!(counts["iam"], 4);
        assert_eq!(counts["/tokeninfo"], 1);
        assert_eq!(counts["/tun/m/assignments"], 2);
        assert_eq!(counts["/tun/m/ccu-info"], 1);
    });
    for args in [
        vec!["account", "login"],
        vec!["sessions", "--json"],
        vec!["whoami", "--json", "--tokeninfo-url"],
        vec!["account", "status", "--json"],
    ] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_colab"));
        command
            .arg("--config-dir")
            .arg(f.temp.path())
            .args([
                "--token-storage",
                "file",
                "-a",
                "target",
                "--control-plane-url",
                &base,
                "--request-timeout",
                "2",
                "--assign-retries",
                "0",
            ])
            .env("HOME", f.temp.path())
            .env_remove("COLAB_ACCOUNT")
            .env_remove("COLAB_DRIVE_ACCOUNT")
            .env_remove("COLAB_DFS_ACCOUNT")
            .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
            .env_remove("CLOUDSDK_CONFIG")
            .env_remove("COLAB_CLI_RETRY_BACKOFF");
        command.args(&args);
        if args[0] == "whoami" {
            command.arg(format!("{base}/tokeninfo"));
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let stdout = String::from_utf8(output.stdout).unwrap();
        for secret in [
            "source-cli",
            "source-secret-fixture",
            "target-cli-fixture",
            "private-marker",
        ] {
            assert!(!stdout.contains(secret));
        }
        match args.as_slice() {
            ["account", "login"] => assert!(stdout.contains("尚未验证 Colab 权限")),
            ["sessions", ..] => {
                let row: Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(row["account"], "target");
                assert_eq!(row["auth_provider"], "adc");
                assert_eq!(row["assignments"], json!([]));
                assert!(row["usage"].is_null());
                assert!(row["error"].is_null());
            }
            ["whoami", ..] => assert_eq!(
                serde_json::from_str::<Value>(&stdout).unwrap()["provider"],
                "adc"
            ),
            _ => {
                let rows: Value = serde_json::from_str(&stdout).unwrap();
                assert!(rows[0]["error"].is_string());
                assert!(rows[1]["error"].is_null());
                assert_eq!(rows[1]["usage"]["balance"], 7.5);
            }
        }
    }
    handler.join().unwrap();
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(
        serde_json::to_value(f.accounts.registry().unwrap()).unwrap(),
        before
    );
    assert!(
        !f.storage
            .profile_path(&id("target"), "oauth_token.json")
            .unwrap()
            .exists()
    );
}
