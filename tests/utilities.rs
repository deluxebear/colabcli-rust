mod common;
use colabcli_rust::{credentials::TokenStorageMode, models::Secret, utility};
use common::*;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
    thread,
    time::Duration,
};
use tiny_http::{Response, Server, StatusCode};
use url::Url;
fn command(root: &Path) -> Command {
    command_at(root, "http://127.0.0.1:1")
}
fn command_at(root: &Path, base: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_colab"));
    c.arg("--config-dir")
        .arg(root)
        .args(["--token-storage", "file", "--control-plane-url", base]);
    for key in [
        "COLAB_ACCOUNT",
        "COLAB_DRIVE_ACCOUNT",
        "COLAB_DFS_ACCOUNT",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "CLOUDSDK_CONFIG",
    ] {
        c.env_remove(key);
    }
    c
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn recv(s: &Server, method: &str, path: &str) -> tiny_http::Request {
    let r = s.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    assert_eq!(r.method().as_str(), method);
    assert_eq!(
        Url::parse(&format!("http://localhost{}", r.url()))
            .unwrap()
            .path(),
        path
    );
    r
}
fn form(r: &mut tiny_http::Request) -> std::collections::BTreeMap<String, String> {
    let mut body = String::new();
    r.as_reader().read_to_string(&mut body).unwrap();
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}
fn send(r: tiny_http::Request, value: Value) {
    r.respond(Response::from_string(value.to_string())).unwrap();
}
#[test]
fn connection_url_encodes_identity_endpoint_and_validates_origins_without_exposing_tokens() {
    let ordinary =
        utility::connection_url("https://colab.research.google.com/", "ep-work", None).unwrap();
    assert_eq!(
        ordinary,
        "https://colab.research.google.com/notebooks/empty.ipynb?dbu=%2Ftun%2Fm%2Fep-work#datalabBackendUrl=https://colab.research.google.com/tun/m/ep-work"
    );
    let url = utility::connection_url(
        "https://sandbox.example.invalid",
        "ep/a?b#c 中文",
        Some("compute+test@example.invalid"),
    )
    .unwrap();
    assert!(url.contains(
        "?authuser=compute%2Btest%40example.invalid&dbu=%2Ftun%2Fm%2Fep%252Fa%253Fb%2523c%2520"
    ));
    assert!(url.ends_with("#datalabBackendUrl=https://sandbox.example.invalid/tun/m/ep%2Fa%3Fb%23c%20%E4%B8%AD%E6%96%87"));
    for host in [
        "javascript:alert(1)",
        "https://name:private@example.invalid",
        "https://example.invalid/path",
        "https://example.invalid?x=y",
        "https://example.invalid#frag",
        "https://exam\nple.invalid",
    ] {
        assert!(utility::connection_url(host, "ep", None).is_err());
    }
    for endpoint in ["", ".", "..", "ep\nprivate"] {
        assert!(utility::connection_url("https://example.invalid", endpoint, None).is_err());
    }
}
#[test]
fn url_routes_existing_session_and_computing_email_without_auth_or_state_changes() {
    let f = Fixture::new(TokenStorageMode::File);
    let compute = f.add("compute");
    let drive = f.add("drive");
    f.accounts.set_drive(Some(&compute), Some(&drive)).unwrap();
    f.accounts
        .update(|r| {
            r.accounts.get_mut(&compute).unwrap().email =
                Some("compute+test@example.invalid".into());
            r.accounts.get_mut(&drive).unwrap().email = Some("drive@example.invalid".into());
            Ok(())
        })
        .unwrap();
    f.storage.save_session(&compute, session("work")).unwrap();
    let registry = fs::read(f.storage.path("accounts.json").unwrap()).unwrap();
    let path = f.storage.profile_path(&compute, "sessions.json").unwrap();
    let before = fs::read(&path).unwrap();
    let output = command(f.storage.root())
        .args(["url", "-s", "work"])
        .env("PATH", "")
        .output()
        .unwrap();
    assert_eq!(
        output.stderr,
        b"[colab] Routing session 'work' -> account 'compute'.\n"
    );
    let value = success(output);
    assert_eq!(value.lines().count(), 1);
    assert!(value.contains("authuser=compute%2Btest%40example.invalid"));
    assert!(!value.contains("drive%40"));
    assert!(!value.contains("fixture-proxy"));
    assert!(
        !f.storage
            .profile_path(&compute, "oauth_token.json")
            .unwrap()
            .exists()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        fs::read(f.storage.path("accounts.json").unwrap()).unwrap(),
        registry
    );
    let denied = command(f.storage.root())
        .args(["-a", "default", "url", "-s", "work"])
        .output()
        .unwrap();
    assert_eq!(denied.status.code(), Some(1));
    assert!(denied.stdout.is_empty());
    assert_eq!(denied.stderr, b"[colab] Session 'work' not found.\n");
    f.storage.save_session(&drive, session("work")).unwrap();
    assert!(
        !command(f.storage.root())
            .args(["url", "-s", "work"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn implicit_url_syncs_named_sessions_before_printing_a_single_link() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let url = || {
        command_at(f.storage.root(), &base)
            .arg("url")
            .output()
            .unwrap()
    };
    let empty = url();
    assert_eq!(empty.status.code(), Some(1));
    assert!(empty.stdout.is_empty());
    assert_eq!(
        empty.stderr,
        b"[colab] Error: No active sessions found. Create one with 'colab new'.\n"
    );
    assert!(server.try_recv().unwrap().is_none());

    let mut credentials = token();
    credentials.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&profile, &credentials).unwrap();
    for name in ["stale", "live"] {
        f.storage.save_session(&profile, session(name)).unwrap();
    }
    let handler = thread::spawn(move || {
        let assignment = |endpoint| {
            json!({"endpoint":endpoint,"variant":0,"accelerator":"NONE","machineShape":0,
            "runtimeProxyInfo":{"url":"https://runtime.invalid","token":"fresh-proxy","tokenExpiresInSeconds":3600}})
        };
        let req = recv(&server, "GET", "/tun/m/assignments");
        assert!(
            req.headers()
                .iter()
                .any(|h| h.field.equiv("Authorization")
                    && h.value.as_str() == "Bearer fixture-access")
        );
        send(
            req,
            json!({"assignments":[assignment("ep-stale"), assignment("ep-live"), assignment("orphan")]}),
        );
        send(
            recv(&server, "GET", "/tun/m/assignments"),
            json!({"assignments":[assignment("ep-live"), assignment("orphan")]}),
        );
        recv(&server, "GET", "/tun/m/assignments")
            .respond(Response::empty(503))
            .unwrap();
        send(
            recv(&server, "GET", "/tun/m/assignments"),
            json!({"assignments":[assignment("orphan")]}),
        );
    });

    let ambiguous = url();
    assert_eq!(ambiguous.status.code(), Some(1));
    assert!(ambiguous.stdout.is_empty());
    assert_eq!(
        ambiguous.stderr,
        b"[colab] Error: Multiple active sessions found. Specify one with -s: live, stale\n"
    );
    let actual = success(url());
    assert_eq!(
        actual,
        format!(
            "{}\n",
            utility::connection_url("https://colab.research.google.com", "ep-live", None).unwrap()
        )
    );
    let state = f.storage.sessions(&profile).unwrap();
    assert_eq!(state.len(), 1);
    assert_eq!(state["live"].token.expose(), "fresh-proxy");
    let failed = url();
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty());
    assert!(f.storage.sessions(&profile).unwrap().contains_key("live"));
    let stale = url();
    assert_eq!(stale.status.code(), Some(1));
    assert!(stale.stdout.is_empty());
    assert_eq!(stale.stderr, empty.stderr);
    assert!(f.storage.sessions(&profile).unwrap().is_empty());
    handler.join().unwrap();
}
#[test]
fn information_resources_and_pay_url_only_do_not_create_configuration_or_run_helpers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("not-created");
    for name in ["readme", "README"] {
        let text = success(command(&root).arg(name).env("PATH", "").output().unwrap());
        assert!(text.contains("Colab"));
        assert!(!root.exists());
    }
    let lower = success(
        command(&root)
            .arg("skill")
            .env("PATH", "")
            .output()
            .unwrap(),
    );
    assert_eq!(
        lower,
        success(
            command(&root)
                .arg("SKILL")
                .env("PATH", "")
                .output()
                .unwrap()
        )
    );
    for required in [
        "account",
        "drive",
        "automation-cleanup",
        "whoami",
        "exec",
        "run --keep",
    ] {
        assert!(lower.contains(required));
    }
    assert_eq!(
        success(
            command(&root)
                .args(["pay", "--url-only"])
                .env("PATH", "")
                .output()
                .unwrap()
        ),
        "https://colab.research.google.com/signup\n"
    );
    assert!(!root.exists());
}
#[cfg(unix)]
#[test]
fn browser_commands_pass_one_url_without_shell_or_stdout_pollution_and_report_failure() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(TokenStorageMode::File);
    f.storage
        .save_session(&id("default"), session("work"))
        .unwrap();
    let bin = f.temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let output = f.temp.path().join("browser-args");
    let name = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let launcher = bin.join(name);
    fs::write(&launcher,b"#!/bin/sh\nprintf '%s\\n' \"$#\" \"$1\" > \"$BROWSER_ARGS\"\nprintf 'must-not-appear-in-stdout'\n").unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let url = success(
        command(f.storage.root())
            .args(["url", "-s", "work", "--open"])
            .env("PATH", &bin)
            .env("BROWSER_ARGS", &output)
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_to_string(&output).unwrap(), format!("1\n{url}"));
    assert!(!url.contains("must-not"));
    success(
        command(f.storage.root())
            .arg("pay")
            .env("PATH", &bin)
            .env("BROWSER_ARGS", &output)
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read_to_string(&output).unwrap(),
        "1\nhttps://colab.research.google.com/signup\n"
    );
    fs::write(&launcher, b"#!/bin/sh\nexit 1\n").unwrap();
    let failed = command(f.storage.root())
        .args(["url", "-s", "work", "--open"])
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert_eq!(String::from_utf8(failed.stdout).unwrap().lines().count(), 1);
}
#[test]
fn whoami_refreshes_valid_cache_persists_rotation_on_identity_failure_and_isolates_accounts() {
    let f = Fixture::new(TokenStorageMode::File);
    let compute = f.add("compute");
    let other = f.add("other");
    let s = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", s.server_addr());
    let mut current = token();
    current.token = Secret::new("private-still-valid");
    current.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    current.token_uri = format!("{base}/token");
    f.tokens.save(&compute, &current).unwrap();
    f.tokens.save(&other, &token()).unwrap();
    let other_path = f.storage.profile_path(&other, "oauth_token.json").unwrap();
    let other_before = fs::read(&other_path).unwrap();
    let server = thread::spawn(move || {
        for (refresh, next, status) in [
            ("fixture-refresh", "private-rotation-one", 400),
            ("private-rotation-one", "private-rotation-two", 200),
        ] {
            let mut r = recv(&s, "POST", "/token");
            assert_eq!(form(&mut r)["refresh_token"], refresh);
            send(
                r,
                json!({"access_token":"private-access-token","refresh_token":next,"expires_in":3600,"token_type":"Bearer"}),
            );
            let r = recv(&s, "GET", "/tokeninfo");
            assert!(!r.headers().iter().any(|h| h.field.equiv("Authorization")));
            let url = Url::parse(&format!("http://localhost{}", r.url())).unwrap();
            assert_eq!(
                url.query_pairs()
                    .find(|(k, _)| k == "access_token")
                    .unwrap()
                    .1,
                "private-access-token"
            );
            if status == 400 {
                r.respond(
                    Response::from_string("private-access-token raw private-error")
                        .with_status_code(StatusCode(400)),
                )
                .unwrap();
            } else {
                send(
                    r,
                    json!({"email":"verified@example.invalid","aud":"public-client","expires_in":"3599","scope":"z-scope a-scope","private":"private-response"}),
                );
            }
        }
    });
    let endpoint = format!("{base}/tokeninfo");
    let failed = command(f.storage.root())
        .args([
            "-a",
            "compute",
            "whoami",
            "--json",
            "--tokeninfo-url",
            &endpoint,
        ])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains("private-"));
    assert_eq!(
        f.tokens
            .load(&compute)
            .unwrap()
            .unwrap()
            .refresh_token
            .unwrap()
            .expose(),
        "private-rotation-one"
    );
    let response = command(f.storage.root())
        .args(["whoami", "--json", "--tokeninfo-url", &endpoint])
        .env("COLAB_ACCOUNT", "compute")
        .output()
        .unwrap();
    let text = success(response);
    assert!(!text.contains("private-"));
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["account"], "compute");
    assert_eq!(value["identity"]["scopes"], json!(["a-scope", "z-scope"]));
    server.join().unwrap();
    assert_eq!(fs::read(&other_path).unwrap(), other_before);
    assert_eq!(
        f.accounts.registry().unwrap().accounts[&compute]
            .email
            .as_deref(),
        Some("verified@example.invalid")
    );
    assert_eq!(
        f.tokens
            .load(&compute)
            .unwrap()
            .unwrap()
            .refresh_token
            .unwrap()
            .expose(),
        "private-rotation-two"
    );
}
#[test]
fn tokeninfo_rejects_redirects_oversize_and_token_echo_without_leaking_raw_diagnostics() {
    let s = Server::http("127.0.0.1:0").unwrap();
    let endpoint = Url::parse(&format!("http://{}/tokeninfo", s.server_addr())).unwrap();
    let server = thread::spawn(move || {
        let r = recv(&s, "GET", "/tokeninfo");
        r.respond(Response::empty(302).with_header(
            tiny_http::Header::from_bytes("Location", "https://private.invalid/secret").unwrap(),
        ))
        .unwrap();
        let r = recv(&s, "GET", "/tokeninfo");
        send(r, json!({"email":"private-token"}));
        let r = recv(&s, "GET", "/tokeninfo");
        r.respond(Response::from_string("private-token".repeat(6000)))
            .unwrap();
        let r = recv(&s, "GET", "/tokeninfo");
        send(r, json!({"scope":"private-token"}));
    });
    for _ in 0..4 {
        let error = utility::token_identity(
            &endpoint,
            &Secret::new("private-token"),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("private-"));
    }
    server.join().unwrap();
}
#[test]
fn whoami_adc_uses_forced_refresh_without_touching_oauth_or_adc_files() {
    let f = Fixture::new(TokenStorageMode::File);
    f.tokens.save(&id("default"), &token()).unwrap();
    let oauth = f
        .storage
        .profile_path(&id("default"), "oauth_token.json")
        .unwrap();
    let before = fs::read(&oauth).unwrap();
    let s = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", s.server_addr());
    let adc = f.storage.path("adc.json").unwrap();
    f.storage.write(&adc,&json!({"type":"authorized_user","client_id":"adc-client","refresh_token":"private-adc-refresh","token":"private-cached-adc","expiry":"2099-01-01T00:00:00Z","token_uri":format!("{base}/token")})).unwrap();
    let adc_before = fs::read(&adc).unwrap();
    let server = thread::spawn(move || {
        let mut r = recv(&s, "POST", "/token");
        assert_eq!(form(&mut r)["client_id"], "adc-client");
        send(
            r,
            json!({"access_token":"private-adc-access","expires_in":3600,"token_type":"Bearer"}),
        );
        let r = recv(&s, "GET", "/tokeninfo");
        send(r, json!({"email":"adc@example.invalid","scope":"openid"}));
    });
    let text = success(
        command(f.storage.root())
            .args([
                "--auth",
                "adc",
                "whoami",
                "--json",
                "--tokeninfo-url",
                &format!("{base}/tokeninfo"),
            ])
            .env("GOOGLE_APPLICATION_CREDENTIALS", &adc)
            .output()
            .unwrap(),
    );
    assert!(!text.contains("private-"));
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["provider"],
        "adc"
    );
    server.join().unwrap();
    assert_eq!(fs::read(&oauth).unwrap(), before);
    assert_eq!(fs::read(&adc).unwrap(), adc_before);
    assert!(
        f.accounts.registry().unwrap().accounts[&id("default")]
            .email
            .is_none()
    );
}

#[test]
fn whoami_does_not_backfill_identity_after_the_stored_oauth_is_replaced() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", s.server_addr());
    let mut old = token();
    old.token_uri = format!("{base}/token");
    f.tokens.save(&id("default"), &old).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let r = recv(&s, "POST", "/token");
        send(
            r,
            json!({"access_token":"old-query-token","refresh_token":"rotation","expires_in":3600,"token_type":"Bearer"}),
        );
        let r = recv(&s, "GET", "/tokeninfo");
        ready_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        send(r, json!({"email":"old-identity@example.invalid"}));
    });
    let mut child = command(f.storage.root());
    child.args([
        "whoami",
        "--json",
        "--tokeninfo-url",
        &format!("{base}/tokeninfo"),
    ]);
    let query = thread::spawn(move || child.output().unwrap());
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut replacement = token();
    replacement.token = Secret::new("new-identity-token");
    f.tokens.save(&id("default"), &replacement).unwrap();
    done_tx.send(()).unwrap();
    let result = success(query.join().unwrap());
    assert!(result.contains("old-identity@example.invalid"));
    server.join().unwrap();
    assert!(
        f.accounts.registry().unwrap().accounts[&id("default")]
            .email
            .is_none()
    );
    assert_eq!(
        f.tokens
            .load(&id("default"))
            .unwrap()
            .unwrap()
            .token
            .expose(),
        "new-identity-token"
    );
}

#[test]
fn tokeninfo_uses_nonempty_audience_alias_and_preserves_zero_expiry_and_scope_multiplicity() {
    let s = Server::http("127.0.0.1:0").unwrap();
    let endpoint = Url::parse(&format!("http://{}/tokeninfo", s.server_addr())).unwrap();
    let server = thread::spawn(move || {
        let r = recv(&s, "GET", "/tokeninfo");
        send(
            r,
            json!({"audience":"","aud":"public-client","expires_in":0,"scope":"z-scope a-scope z-scope"}),
        );
        let r = recv(&s, "GET", "/tokeninfo");
        send(r, json!({}));
    });
    let info = utility::token_identity(
        &endpoint,
        &Secret::new("private-bearer"),
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(info.audience.as_deref(), Some("public-client"));
    assert_eq!(info.expires_in, Some(0));
    assert_eq!(info.scopes, ["a-scope", "z-scope", "z-scope"]);
    let info = utility::token_identity(
        &endpoint,
        &Secret::new("private-bearer"),
        Duration::from_secs(2),
    )
    .unwrap();
    assert!(info.email.is_none());
    assert!(info.audience.is_none());
    assert!(info.expires_in.is_none());
    assert!(info.scopes.is_empty());
    server.join().unwrap();
}
