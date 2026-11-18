mod common;
use chrono::Utc;
use colabcli_rust::credentials::TokenStorageMode;
use common::*;
use serde_json::{Value, json};
use std::{
    process::{Command, Output},
    thread,
    time::Duration,
};
use tiny_http::{Response, Server};

fn command(f: &Fixture, base: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_colab"));
    command
        .env_clear()
        .env("HOME", f.temp.path())
        .arg("--config-dir")
        .arg(f.storage.root())
        .args([
            "--token-storage",
            "file",
            "--control-plane-url",
            base,
            "--request-timeout",
            "2",
            "--assign-retries",
            "0",
        ]);
    command
}
fn row(output: Output) -> Value {
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
        "fixture-proxy",
        "fixture-private",
    ] {
        assert!(!text.contains(secret));
    }
    serde_json::from_str(&text).unwrap()
}
fn cached(f: &Fixture) {
    let mut credentials = token();
    credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &credentials).unwrap();
}

#[test]
fn single_sessions_json_uses_merged_row_and_only_prunes_selected_profile() {
    let f = Fixture::new(TokenStorageMode::File);
    cached(&f);
    f.add("other");
    f.storage
        .save_session(&id("default"), session("owned"))
        .unwrap();
    f.storage
        .save_session(&id("default"), session("stale"))
        .unwrap();
    f.storage
        .save_session(&id("other"), session("other-owned"))
        .unwrap();
    let other = f
        .storage
        .profile_path(&id("other"), "sessions.json")
        .unwrap();
    let before = std::fs::read(&other).unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let url = base.clone();
    let handler = thread::spawn(move || {
        let request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(request.url(), "/tun/m/assignments?authuser=0");
        assert!(
            request
                .headers()
                .iter()
                .any(|header| header.field.equiv("Authorization")
                    && header.value.as_str() == "Bearer fixture-access")
        );
        request.respond(Response::from_string(json!({"assignments":[{"endpoint":"ep-owned","accelerator":"T4","machineShape":1,"variant":1,"runtimeProxyInfo":{"token":"fixture-proxy","tokenExpiresInSeconds":3600,"url":url}}]}).to_string())).unwrap();
        assert!(
            server
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
    });
    let value = row(command(&f, &base)
        .args(["sessions", "--json"])
        .output()
        .unwrap());
    handler.join().unwrap();
    assert_eq!(value["account"], "default");
    assert!(value["usage"].is_null());
    assert!(value["error"].is_null());
    assert_eq!(value["assignments"][0]["name"], "owned");
    assert_eq!(value["assignments"][0]["shape"], "High-RAM");
    assert_eq!(value["assignments"][0]["hardware"], "T4");
    assert_eq!(f.storage.sessions(&id("default")).unwrap().len(), 1);
    assert_eq!(std::fs::read(other).unwrap(), before);
}

#[test]
fn single_usage_json_keeps_http_error_in_row_and_does_not_touch_sessions() {
    let f = Fixture::new(TokenStorageMode::File);
    cached(&f);
    f.storage
        .save_session(&id("default"), session("owned"))
        .unwrap();
    let path = f
        .storage
        .profile_path(&id("default"), "sessions.json")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let handler = thread::spawn(move || {
        let request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(request.url(), "/tun/m/ccu-info?authuser=0");
        request
            .respond(Response::from_string("fixture-private").with_status_code(503))
            .unwrap();
        assert!(
            server
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
    });
    let value = row(command(&f, &base)
        .args(["usage", "--json"])
        .output()
        .unwrap());
    handler.join().unwrap();
    assert_eq!(value["account"], "default");
    assert!(value["usage"].is_null());
    assert_eq!(value["assignments"], json!([]));
    assert!(value["error"].as_str().unwrap().contains("503"));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn merged_queries_reject_invalid_global_account_before_any_http_request() {
    let f = Fixture::new(TokenStorageMode::File);
    cached(&f);
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    for arguments in [
        vec!["account", "status", "--json"],
        vec!["sessions", "--all", "--json"],
        vec!["usage", "--all", "--json"],
    ] {
        let output = command(&f, &base)
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
    }
    assert!(
        server
            .recv_timeout(Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
}
