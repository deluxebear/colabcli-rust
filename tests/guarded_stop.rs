mod common;
use colabcli_rust::{credentials::TokenStorageMode, models::Secret};
use common::{Fixture, id};
use serde_json::json;
use std::{fs, process::Command, thread, time::Duration};
use tiny_http::{Response, Server};
fn command(f: &Fixture, base: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_colab"));
    command
        .env_clear()
        .env("HOME", f.temp.path())
        .env("NO_PROXY", "*")
        .arg("--config-dir")
        .arg(f.temp.path())
        .args([
            "--token-storage",
            "file",
            "--account",
            "default",
            "--control-plane-url",
            base,
        ]);
    command
}
fn credentials(f: &Fixture) {
    let mut token = common::token();
    token.token = Secret::new("guarded-oauth-fixture");
    token.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &token).unwrap();
}

#[test]
fn implicit_stop_selects_and_shuts_down_a_kernel_with_one_inventory_request() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let control = Server::http("127.0.0.1:0").unwrap();
    let runtime = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", control.server_addr());
    let empty = command(&f, &base).arg("stop").output().unwrap();
    assert_eq!(empty.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&empty.stdout).contains("No active sessions found"));
    assert!(control.try_recv().unwrap().is_none());

    credentials(&f);
    let mut work = common::session("work");
    work.kernel_id = Some("kernel-1".into());
    f.storage.save_session(&profile, work).unwrap();
    f.storage
        .save_session(&profile, common::session("stale"))
        .unwrap();
    let handler = thread::spawn(move || {
        let recv = |server: &Server| {
            server
                .recv_timeout(Duration::from_secs(4))
                .unwrap()
                .unwrap()
        };
        let request = recv(&control);
        assert_eq!(request.method().as_str(), "GET");
        assert!(request.url().starts_with("/tun/m/assignments?"));
        let url = format!("http://{}/runtime/", runtime.server_addr());
        request.respond(Response::from_string(json!({"assignments":[{
            "endpoint":"ep-work","variant":0,"accelerator":"NONE","machineShape":0,
            "runtimeProxyInfo":{"url":url,"token":"fresh-proxy","tokenExpiresInSeconds":3600}
        }]}).to_string())).unwrap();

        let request = recv(&runtime);
        assert_eq!(request.method().as_str(), "DELETE");
        assert_eq!(request.url(), "/runtime/api/kernels/kernel-1");
        assert!(request.headers().iter().any(|h| h.field.equiv("Authorization") && h.value.as_str() == "Bearer fresh-proxy"));
        request.respond(Response::empty(204)).unwrap();

        let request = recv(&control);
        assert_eq!(request.method().as_str(), "GET");
        assert!(request.url().starts_with("/tun/m/unassign/ep-work?"));
        request
            .respond(Response::from_string("{\"token\":\"xsrf\"}"))
            .unwrap();
        let request = recv(&control);
        assert_eq!(request.method().as_str(), "POST");
        assert!(request.url().starts_with("/tun/m/unassign/ep-work?"));
        request.respond(Response::empty(204)).unwrap();
    });
    let output = command(&f, &base).arg("stop").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "[colab] Pruned 1 stale local session(s).\n[colab] Using unique session 'work'.\n[colab] Stopping session 'work'...\n[colab] Session terminated.\n"
    );
    handler.join().unwrap();
    assert!(f.storage.sessions(&profile).unwrap().is_empty());
}

#[test]
fn implicit_stop_rejects_a_release_intent_for_a_replaced_endpoint() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    credentials(&f);
    let mut replacement = common::session("work");
    replacement.endpoint = "ep-replacement".into();
    f.storage.save_session(&profile, replacement).unwrap();
    let path = f
        .storage
        .profile_path(&profile, "control_operation.json")
        .unwrap();
    f.storage.write(&path, &json!({"operation":"release","name":"work","endpoint":"ep-work","attempted":true,"event_id":"old-release"})).unwrap();
    let original = fs::read(&path).unwrap();
    let control = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", control.server_addr());
    let handler = thread::spawn(move || {
        let request = control
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert_eq!(request.method().as_str(), "GET");
        assert!(request.url().starts_with("/tun/m/assignments?"));
        let assignments: Vec<_> = ["ep-work", "ep-replacement"].into_iter().map(|endpoint| json!({
            "endpoint":endpoint,"variant":0,"accelerator":"NONE","machineShape":0,
            "runtimeProxyInfo":{"url":"https://runtime.invalid","token":"proxy-fixture","tokenExpiresInSeconds":3600}
        })).collect();
        request
            .respond(Response::from_string(
                json!({"assignments":assignments}).to_string(),
            ))
            .unwrap();
        assert!(
            control
                .recv_timeout(Duration::from_millis(200))
                .unwrap()
                .is_none()
        );
    });
    let output = command(&f, &base).arg("stop").output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("会话已被替换"), "{stderr}");
    handler.join().unwrap();
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(
        f.storage.sessions(&profile).unwrap()["work"].endpoint,
        "ep-replacement"
    );
}

#[test]
fn guarded_cli_stop_requires_matching_endpoint_and_isolates_other_accounts() {
    let f = Fixture::new(TokenStorageMode::File);
    credentials(&f);
    let other = f.add("other");
    f.storage
        .save_session(&id("default"), common::session("work"))
        .unwrap();
    f.storage
        .save_session(&other, common::session("work"))
        .unwrap();
    let path = f
        .storage
        .profile_path(&id("default"), "sessions.json")
        .unwrap();
    let other_path = f.storage.profile_path(&other, "sessions.json").unwrap();
    let before = fs::read(&path).unwrap();
    let other_before = fs::read(other_path).unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let output = command(&f, &base)
        .args(["stop", "-s", "work", "--colab-expected-endpoint", "ep-old"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Session terminated"));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(
        !f.storage
            .profile_path(&id("default"), "control_operation.json")
            .unwrap()
            .exists()
    );
    assert!(
        server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
    let handle = thread::spawn(move || {
        for method in ["GET", "POST"] {
            let request = server
                .recv_timeout(Duration::from_secs(4))
                .unwrap()
                .unwrap();
            assert_eq!(request.method().as_str(), method);
            assert!(request.url().starts_with("/tun/m/unassign/ep-work?"));
            assert_eq!(
                request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .unwrap()
                    .value
                    .as_str(),
                "Bearer guarded-oauth-fixture"
            );
            if method == "GET" {
                request
                    .respond(Response::from_string("{\"token\":\"xsrf\"}"))
                    .unwrap();
            } else {
                assert_eq!(
                    request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("X-Goog-Colab-Token"))
                        .unwrap()
                        .value
                        .as_str(),
                    "xsrf"
                );
                request.respond(Response::empty(204)).unwrap();
            }
        }
    });
    let output = command(&f, &base)
        .args(["stop", "-s", "work", "--colab-expected-endpoint", "ep-work"])
        .output()
        .unwrap();
    assert!(output.status.success());
    handle.join().unwrap();
    assert!(f.storage.sessions(&id("default")).unwrap().is_empty());
    assert_eq!(
        fs::read(f.storage.profile_path(&other, "sessions.json").unwrap()).unwrap(),
        other_before
    );
}
#[test]
fn guarded_cli_stop_validates_before_auth_and_does_not_claim_missing_session_success() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    for args in [
        vec!["stop", "--colab-expected-endpoint", "ep"],
        vec!["stop", "-s", "work", "--colab-expected-endpoint", ""],
    ] {
        let output = command(&f, &base).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
    let output = command(&f, &base)
        .args(["stop", "-s", "missing", "--colab-expected-endpoint", "ep"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Session terminated"));
    let normal = command(&f, &base)
        .args(["stop", "-s", "missing"])
        .output()
        .unwrap();
    assert!(normal.status.success());
    assert!(
        server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
    assert!(
        !f.storage
            .profile_path(&id("default"), "control_operation.json")
            .unwrap()
            .exists()
    );
}
#[test]
fn guarded_cli_recovery_protects_pending_endpoint_and_preserves_a_replacement_vm() {
    let f = Fixture::new(TokenStorageMode::File);
    credentials(&f);
    let mut replacement = common::session("work");
    replacement.endpoint = "ep-replacement".into();
    f.storage.save_session(&id("default"), replacement).unwrap();
    let path = f
        .storage
        .profile_path(&id("default"), "control_operation.json")
        .unwrap();
    f.storage.write(&path,&json!({"operation":"release","name":"work","endpoint":"ep-owned","attempted":true,"event_id":"owned-release"})).unwrap();
    let before = fs::read(&path).unwrap();
    let sessions_before = fs::read(
        f.storage
            .profile_path(&id("default"), "sessions.json")
            .unwrap(),
    )
    .unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let refused = command(&f, &base)
        .args([
            "stop",
            "-s",
            "work",
            "--colab-expected-endpoint",
            "ep-replacement",
        ])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(
        server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
    let worker = thread::spawn(move || {
        let request = server
            .recv_timeout(Duration::from_secs(4))
            .unwrap()
            .unwrap();
        assert_eq!(request.method().as_str(), "GET");
        assert!(request.url().starts_with("/tun/m/assignments?"));
        request.respond(Response::from_string(json!({"assignments":[{"endpoint":"ep-replacement","accelerator":"NONE","variant":0,"machineShape":0,"runtimeProxyInfo":{"token":"proxy-fixture","tokenExpiresInSeconds":3600,"url":"https://runtime.invalid"}}]}).to_string())).unwrap();
        assert!(
            server
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
    });
    let output = command(&f, &base)
        .args([
            "stop",
            "-s",
            "work",
            "--colab-expected-endpoint",
            "ep-owned",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    worker.join().unwrap();
    assert!(!path.exists());
    assert_eq!(
        fs::read(
            f.storage
                .profile_path(&id("default"), "sessions.json")
                .unwrap()
        )
        .unwrap(),
        sessions_before
    );
}

#[test]
fn runtime_cli_endpoint_condition_blocks_exec_repl_files_and_drive_before_network() {
    let f = Fixture::new(TokenStorageMode::File);
    let path = f
        .storage
        .profile_path(&id("default"), "sessions.json")
        .unwrap();
    f.storage
        .save_session(&id("default"), common::session("work"))
        .unwrap();
    let credential = f
        .storage
        .profile_path(&id("default"), "dfs_credentials.json")
        .unwrap();
    f.storage.write(&credential, &json!({"flavor":"custom","client_id":"fixture-client", "client_secret":"fixture-secret",
        "refresh_token":"fixture-refresh","user_id":"fixture-user","scopes":["https://www.googleapis.com/auth/drive"]})).unwrap();
    let job = f.temp.path().join("job.py");
    fs::write(&job, "pass").unwrap();
    let local = f.temp.path().join("download.bin");
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let before = fs::read(&path).unwrap();
    let credential_before = fs::read(&credential).unwrap();
    let cases = [
        vec!["exec", "-s", "work", "--json", "-f", job.to_str().unwrap()],
        vec!["repl", "-s", "work"],
        vec!["restart-kernel", "-s", "work"],
        vec!["ls", "-s", "work", "--json"],
        vec![
            "download",
            "content/file",
            local.to_str().unwrap(),
            "-s",
            "work",
        ],
        vec!["drive", "mount", "-s", "work", "--json"],
        vec!["drive", "umount", "-s", "work", "--json"],
        vec!["drive", "status", "-s", "work", "--json"],
    ];
    for args in cases {
        let output = command(&f, &base)
            .args(["--colab-runtime-endpoint", "ep-old"])
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("预期不匹配"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(&credential).unwrap(), credential_before);
    }
    assert!(!local.exists());
    assert!(
        server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
}

#[test]
fn runtime_cli_endpoint_condition_requires_supported_explicit_session_before_bootstrap() {
    let f = Fixture::new(TokenStorageMode::File);
    let missing = f.temp.path().join("must-not-create");
    for args in [
        vec!["--colab-runtime-endpoint", "ep", "exec"],
        vec!["--colab-runtime-endpoint", "ep", "repl"],
        vec!["--colab-runtime-endpoint", "ep", "drive", "mount"],
        vec!["--colab-runtime-endpoint", "ep", "new", "-s", "work"],
        vec!["--colab-runtime-endpoint", "ep", "stop", "-s", "work"],
        vec!["--colab-runtime-endpoint", "", "exec", "-s", "work"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_colab"))
            .env_clear()
            .env("HOME", f.temp.path())
            .arg("--config-dir")
            .arg(&missing)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(!missing.exists());
    }
}
