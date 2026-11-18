mod common;
use chrono::Utc;
use colabcli_rust::{
    credentials::TokenStorageMode,
    models::{Account, AuthProvider, Secret},
};
use common::*;
use serde_json::{Value, json};
use std::{
    process::{Command, Output},
    thread,
    time::Duration,
};
use tiny_http::{Response, Server, StatusCode};

fn command(f: &Fixture, base: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_colab"));
    command
        .arg("--config-dir")
        .arg(f.temp.path())
        .args([
            "--token-storage",
            "file",
            "--control-plane-url",
            base,
            "--request-timeout",
            "2",
            "--assign-retries",
            "0",
        ])
        .env("HOME", f.temp.path())
        .env("COLAB_ACCOUNT", "default")
        .env_remove("COLAB_DRIVE_ACCOUNT")
        .env_remove("COLAB_DFS_ACCOUNT")
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .env_remove("CLOUDSDK_CONFIG")
        .env_remove("COLAB_CLI_RETRY_BACKOFF");
    command
}
fn rows(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    for secret in [
        "private-response",
        "fixture-access",
        "fixture-refresh",
        "access-other",
        "access-adc",
        "fixture-proxy",
    ] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
    serde_json::from_str(&text).unwrap()
}
fn cached(f: &Fixture, profile: &str, bearer: &str) {
    let mut credentials = token();
    credentials.token = Secret::new(bearer);
    credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id(profile), &credentials).unwrap();
}
fn receive(server: &Server) -> tiny_http::Request {
    server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("expected request")
}
fn authorization(request: &tiny_http::Request) -> &str {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .unwrap()
        .value
        .as_str()
}

#[test]
fn account_status_queries_all_accounts_and_preserves_names_orphans_and_drive_bindings() {
    let f = Fixture::new(TokenStorageMode::File);
    f.add("other");
    f.add("storage");
    cached(&f, "default", "fixture-access");
    cached(&f, "other", "access-other");
    f.accounts
        .set_drive(Some(&id("default")), Some(&id("storage")))
        .unwrap();
    // Readiness means file presence, not loading or exposing Drive credentials.
    let drive_path = f
        .storage
        .profile_path(&id("storage"), "dfs_credentials.json")
        .unwrap();
    f.storage
        .write(
            &drive_path,
            &json!({"private-response":"not-a-valid-credential"}),
        )
        .unwrap();
    f.storage
        .save_session(&id("default"), session("work"))
        .unwrap();
    f.storage
        .save_session(&id("default"), session("stale"))
        .unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let handler = thread::spawn(move || {
        let mut counts = std::collections::BTreeMap::new();
        for _ in 0..4 {
            let request = receive(&server);
            let bearer = authorization(&request).to_owned();
            assert!(["Bearer fixture-access", "Bearer access-other"].contains(&bearer.as_str()));
            *counts.entry(bearer.clone()).or_insert(0) += 1;
            let value = if request.url().starts_with("/tun/m/ccu-info") {
                json!({"currentBalance":if bearer.ends_with("access-other") {2.0} else {8.0},"consumptionRateHourly":0.5,"assignmentsCount":2})
            } else {
                assert!(request.url().starts_with("/tun/m/assignments"));
                let endpoints = if bearer.ends_with("access-other") {
                    vec!["ep-orphan"]
                } else {
                    vec!["ep-work"]
                };
                json!({"assignments":endpoints.into_iter().map(|endpoint| json!({"endpoint":endpoint,"accelerator":"T4","variant":1,"machineShape":1,"runtimeProxyInfo":{"token":"fixture-proxy","tokenExpiresInSeconds":3600,"url":"https://runtime.invalid"}})).collect::<Vec<_>>()})
            };
            request
                .respond(Response::from_string(value.to_string()))
                .unwrap();
        }
        assert!(counts.values().all(|n| *n == 2));
    });
    let before = f.accounts.registry().unwrap();
    let output = command(&f, &base)
        .args(["-a", "default", "acct", "status", "--json"])
        .output()
        .unwrap();
    let result = rows(output);
    handler.join().unwrap();
    assert_eq!(result.as_array().unwrap().len(), 3);
    assert_eq!(result[0]["account"], "default");
    assert_eq!(result[0]["active"], true);
    assert_eq!(result[0]["usage"]["balance"], 8.0);
    assert_eq!(result[0]["drive_account"], "storage");
    assert_eq!(result[0]["drive_ready"], true);
    assert_eq!(
        result[0]["assignments"][0],
        json!({"name":"work","endpoint":"ep-work","hardware":"T4","accelerator":"T4","shape":"High-RAM","machine_shape":"HIGH_RAM","variant":"GPU"})
    );
    assert_eq!(result[1]["usage"]["balance"], 2.0);
    assert_eq!(result[1]["assignments"][0]["name"], "?");
    assert_eq!(result[2]["account"], "storage");
    assert!(result[2]["error"].as_str().unwrap().contains("授权"));
    assert!(result[2]["usage"].is_null());
    assert!(
        !f.storage
            .sessions(&id("default"))
            .unwrap()
            .contains_key("stale")
    );
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(f.accounts.registry().unwrap()).unwrap()
    );
}

#[test]
fn account_status_keeps_usage_when_session_query_fails_and_never_prunes_on_error() {
    let f = Fixture::new(TokenStorageMode::File);
    cached(&f, "default", "fixture-access");
    f.storage
        .save_session(&id("default"), session("work"))
        .unwrap();
    let before = std::fs::read(
        f.storage
            .profile_path(&id("default"), "sessions.json")
            .unwrap(),
    )
    .unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let handler = thread::spawn(move || {
        let request = receive(&server);
        assert!(request.url().starts_with("/tun/m/ccu-info"));
        request
            .respond(Response::from_string(
                json!({"currentBalance":4.5,"consumptionRateHourly":1.0,"assignmentsCount":1})
                    .to_string(),
            ))
            .unwrap();
        let request = receive(&server);
        assert!(request.url().starts_with("/tun/m/assignments"));
        request
            .respond(
                Response::from_string("private-response fixture-access")
                    .with_status_code(StatusCode(502)),
            )
            .unwrap();
        let request = receive(&server);
        assert!(request.url().starts_with("/tun/m/ccu-info"));
        request
            .respond(Response::from_string("private-response").with_status_code(StatusCode(403)))
            .unwrap();
        assert!(
            server
                .recv_timeout(Duration::from_millis(150))
                .unwrap()
                .is_none()
        );
    });
    let first = rows(
        command(&f, &base)
            .args(["account", "status", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(first[0]["usage"]["balance"], 4.5);
    assert!(first[0]["error"].as_str().unwrap().contains("502"));
    assert_eq!(first[0]["assignments"], json!([]));
    let second = rows(
        command(&f, &base)
            .args(["account", "status", "--json"])
            .output()
            .unwrap(),
    );
    assert!(second[0]["usage"].is_null());
    assert!(second[0]["error"].as_str().unwrap().contains("403"));
    handler.join().unwrap();
    assert_eq!(
        before,
        std::fs::read(
            f.storage
                .profile_path(&id("default"), "sessions.json")
                .unwrap()
        )
        .unwrap()
    );
}

#[test]
fn account_status_uses_account_scoped_adc_without_changing_source_or_oauth() {
    let f = Fixture::new(TokenStorageMode::File);
    cached(&f, "default", "fixture-access");
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let adc_path = f.temp.path().join("adc.json");
    let adc=json!({"type":"authorized_user","client_id":"adc-client","client_secret":"adc-secret","refresh_token":"adc-refresh","token_uri":format!("{base}/oauth/token")}).to_string();
    std::fs::write(&adc_path, &adc).unwrap();
    f.accounts
        .add(
            id("adc"),
            Account {
                auth_provider: AuthProvider::Adc,
                adc_credentials_file: Some(adc_path.to_str().unwrap().into()),
                ..Account::default()
            },
        )
        .unwrap();
    let oauth_before = std::fs::read(
        f.storage
            .profile_path(&id("default"), "oauth_token.json")
            .unwrap(),
    )
    .unwrap();
    let handler = thread::spawn(move || {
        let mut counts = std::collections::BTreeMap::new();
        for _ in 0..5 {
            let mut request = receive(&server);
            let value = if request.url() == "/oauth/token" {
                assert_eq!(request.method().as_str(), "POST");
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let form: std::collections::BTreeMap<_, _> =
                    url::form_urlencoded::parse(body.as_bytes())
                        .into_owned()
                        .collect();
                assert_eq!(form["refresh_token"], "adc-refresh");
                assert_eq!(form["client_id"], "adc-client");
                json!({"access_token":"access-adc","expires_in":3600,"token_type":"Bearer"})
            } else {
                *counts
                    .entry(authorization(&request).to_owned())
                    .or_insert(0) += 1;
                assert!(
                    ["Bearer fixture-access", "Bearer access-adc"]
                        .contains(&authorization(&request))
                );
                if request.url().starts_with("/tun/m/ccu-info") {
                    json!({"currentBalance":1.0,"consumptionRateHourly":0.0,"assignmentsCount":0})
                } else {
                    assert!(request.url().starts_with("/tun/m/assignments"));
                    json!({"assignments":[]})
                }
            };
            request
                .respond(Response::from_string(value.to_string()))
                .unwrap();
        }
        assert_eq!(counts.get("Bearer fixture-access"), Some(&2));
        assert_eq!(counts.get("Bearer access-adc"), Some(&2));
    });
    let result = rows(
        command(&f, &base)
            .args(["account", "status", "--json"])
            .output()
            .unwrap(),
    );
    handler.join().unwrap();
    // The merged probe preserves registry order, even when ADC sorts before default by name.
    assert_eq!(result[0]["account"], "default");
    assert_eq!(result[0]["auth_provider"], "oauth2");
    assert_eq!(result[1]["account"], "adc");
    assert_eq!(result[1]["auth_provider"], "adc");
    assert!(
        result
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["error"].is_null())
    );
    assert_eq!(std::fs::read_to_string(adc_path).unwrap(), adc);
    assert_eq!(
        std::fs::read(
            f.storage
                .profile_path(&id("default"), "oauth_token.json")
                .unwrap()
        )
        .unwrap(),
        oauth_before
    );
    assert!(
        !f.storage
            .profile_path(&id("adc"), "oauth_token.json")
            .unwrap()
            .exists()
    );
}

#[test]
fn account_status_text_reports_missing_auth_without_interaction_and_binding_errors_stay_in_rows() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let output = command(&f, &base)
        .args(["account", "status"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("[*default] (-) | drive:default(missing) | ERROR: "));
    assert!(text.contains("需要登录或重新授权"));
    cached(&f, "default", "fixture-access");
    let result = rows(
        command(&f, &base)
            .env("COLAB_DRIVE_ACCOUNT", "missing-binding")
            .args(["account", "status", "--json"])
            .output()
            .unwrap(),
    );
    assert!(result[0]["drive_account"].is_null());
    assert!(result[0]["usage"].is_null());
    assert!(
        result[0]["error"]
            .as_str()
            .unwrap()
            .contains("missing-binding")
    );
    assert!(
        server
            .recv_timeout(Duration::from_millis(150))
            .unwrap()
            .is_none()
    );
}
