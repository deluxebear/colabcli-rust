mod common;
use chrono::Utc;
use colabcli_rust::{Error, credentials::TokenStorageMode, inventory, models::ProfileId};
use common::*;
use serde_json::{Value, json};
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

#[test]
fn bounded_workers_preserve_order_and_individual_failures() {
    let profiles: Vec<_> = (0..13).map(|n| id(&format!("p{n:02}"))).collect();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let rows = inventory::collect(&profiles, |profile| {
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(10));
        active.fetch_sub(1, Ordering::SeqCst);
        if profile == &id("p03") {
            Err(Error::NeedsAuth)
        } else {
            Ok(profile.to_string())
        }
    });
    assert!(peak.load(Ordering::SeqCst) <= 4);
    assert!(peak.load(Ordering::SeqCst) > 1);
    assert_eq!(
        rows.iter().map(|r| r.account.clone()).collect::<Vec<_>>(),
        profiles
    );
    assert_eq!(rows[3].error.as_deref(), Some("需要登录或重新授权"));
    assert!(rows[3].result.is_none());
    assert_eq!(rows[12].result.as_deref(), Some("p12"));
}
#[test]
fn empty_registry_does_not_start_workers() {
    let rows: Vec<inventory::AccountResult<()>> =
        inventory::collect(&[], |_| panic!("no worker expected"));
    assert!(rows.is_empty());
}
#[test]
fn cli_all_keeps_authenticated_results_alongside_missing_credentials() {
    let f = Fixture::new(TokenStorageMode::File);
    f.add("other");
    let mut credentials = token();
    credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &credentials).unwrap();
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let handler = thread::spawn(move || {
        for (path, body) in [
            ("/tun/m/assignments", json!({"assignments":[]})),
            (
                "/tun/m/ccu-info",
                json!({"currentBalance":4.5,"consumptionRateHourly":0.0,"assignmentsCount":0}),
            ),
        ] {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert!(request.url().starts_with(path));
            assert!(
                request
                    .headers()
                    .iter()
                    .any(|h| h.field.equiv("Authorization")
                        && h.value.as_str() == "Bearer fixture-access")
            );
            request
                .respond(tiny_http::Response::from_string(body.to_string()))
                .unwrap();
        }
    });
    for subcommand in ["status", "usage"] {
        let output = Command::new(env!("CARGO_BIN_EXE_colab"))
            .arg("--config-dir")
            .arg(f.temp.path())
            .args([
                "--token-storage",
                "file",
                "--control-plane-url",
                &base,
                subcommand,
                "--all",
                "--json",
            ])
            .env(
                "COLAB_ACCOUNT",
                if subcommand == "usage" {
                    "default"
                } else {
                    "nonexistent-selection-must-be-ignored-for-all"
                },
            )
            .env_remove("COLAB_CLI_REQUEST_TIMEOUT")
            .env_remove("COLAB_CLI_ASSIGN_RETRIES")
            .env_remove("COLAB_CLI_RETRY_BACKOFF")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("fixture-access"));
        assert!(!text.contains("fixture-refresh"));
        let rows: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(rows[0]["account"], "default");
        assert!(rows[1]["error"].as_str().unwrap().contains("授权"));
        assert_eq!(rows[1]["account"], "other");
        if subcommand == "status" {
            assert_eq!(rows[0]["result"], json!([]));
        } else {
            assert_eq!(rows[0]["usage"]["balance"], 4.5);
        }
    }
    handler.join().unwrap();
    assert_eq!(
        f.accounts.registry().unwrap().active,
        ProfileId::default_profile()
    );
}
