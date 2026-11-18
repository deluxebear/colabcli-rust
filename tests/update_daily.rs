use chrono::{Duration as Delta, Utc};
use colabcli_rust::{
    storage::Storage,
    update::{self, daily},
};
use serde_json::{Value, json};
use std::{process::Command, thread, time::Duration};
use tiny_http::{Response, Server};
use url::Url;

fn fixture() -> (tempfile::TempDir, Storage, Server, Url) {
    let temp = tempfile::tempdir().unwrap();
    let storage = Storage::new(temp.path()).unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let uri = Url::parse(&format!("http://{}/manifest", server.server_addr())).unwrap();
    (temp, storage, server, uri)
}
fn manifest(uri: &Url, version: &str) -> Value {
    json!({"schema_version":1,"package":"colabcli-rust","version":version,
        "artifacts":{update::current_target().unwrap():{
            "url":uri.join("binary").unwrap(),"size":1,"sha256":"0".repeat(64)}}})
}
fn serve(server: &Server, doc: Value) {
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(request.url(), "/manifest");
    assert_eq!(request.method().as_str(), "GET");
    assert!(
        !request
            .headers()
            .iter()
            .any(|h| h.field.equiv("Authorization") || h.field.equiv("Cookie"))
    );
    request
        .respond(Response::from_string(doc.to_string()))
        .unwrap();
}
#[test]
fn caches_daily_preserves_python_settings_and_never_downloads() {
    let (_temp, storage, server, uri) = fixture();
    storage
        .update_settings(|settings| {
            settings.latest_version = Some("999.0.0".into());
            settings
                .extra
                .insert("unrelated".into(), json!({"keep":true}));
            Ok(())
        })
        .unwrap();
    let now = Utc::now();
    thread::scope(|scope| {
        scope.spawn(|| serve(&server, manifest(&uri, "0.2.0")));
        assert_eq!(
            daily::notice(&storage, &uri, "0.1.0", now).as_deref(),
            Some("0.2.0")
        );
    });
    assert_eq!(
        daily::notice(&storage, &uri, "0.1.0", now + Delta::hours(23)).as_deref(),
        Some("0.2.0")
    );
    assert!(daily::notice(&storage, &uri, "0.2.0+build", now).is_none());
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
    let settings = storage.settings().unwrap();
    assert_eq!(settings.latest_version.as_deref(), Some("999.0.0"));
    assert!(settings.last_check.is_none());
    assert_eq!(settings.extra["unrelated"], json!({"keep":true}));
}
#[test]
fn expiry_rechecks_without_downgrading_and_source_change_discards_old_cache() {
    let (_temp, storage, server, uri) = fixture();
    let now = Utc::now();
    for (time, observed) in [(now, "0.3.0"), (now + Delta::days(1), "0.2.0")] {
        thread::scope(|scope| {
            scope.spawn(|| serve(&server, manifest(&uri, observed)));
            assert_eq!(
                daily::notice(&storage, &uri, "0.1.0", time).as_deref(),
                Some("0.3.0")
            );
        });
    }
    let changed = uri.join("other").unwrap();
    thread::scope(|scope| {
        scope.spawn(|| {
            let req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(req.url(), "/other");
            req.respond(Response::from_string("offline").with_status_code(503))
                .unwrap();
        });
        assert!(daily::notice(&storage, &changed, "0.1.0", now + Delta::days(1)).is_none());
    });
}
#[test]
fn failed_checks_are_silent_throttled_and_retried_after_one_day() {
    let (_temp, storage, server, uri) = fixture();
    let now = Utc::now();
    thread::scope(|scope| {
        scope.spawn(|| serve(&server, json!({"secret_response":"must not surface"})));
        assert!(daily::notice(&storage, &uri, "0.1.0", now).is_none());
    });
    assert!(daily::notice(&storage, &uri, "0.1.0", now + Delta::hours(1)).is_none());
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
    thread::scope(|scope| {
        scope.spawn(|| serve(&server, manifest(&uri, "0.2.0")));
        assert!(daily::notice(&storage, &uri, "0.1.0", now + Delta::days(1)).is_some());
    });
}
#[test]
fn disabled_checks_and_contended_lock_never_request() {
    let (_temp, storage, server, uri) = fixture();
    storage
        .update_settings(|s| {
            s.enable_update_check = false;
            Ok(())
        })
        .unwrap();
    let before = std::fs::read(storage.path("settings.json").unwrap()).unwrap();
    assert!(daily::notice(&storage, &uri, "0.1.0", Utc::now()).is_none());
    assert_eq!(
        before,
        std::fs::read(storage.path("settings.json").unwrap()).unwrap()
    );
    storage
        .update_settings(|s| {
            s.enable_update_check = true;
            Ok(())
        })
        .unwrap();
    let _lock = storage
        .lock(&storage.path("native-update-check.rust.lock").unwrap())
        .unwrap();
    let start = std::time::Instant::now();
    assert!(daily::notice(&storage, &uri, "0.1.0", Utc::now()).is_none());
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
}
#[test]
fn clock_rollback_rechecks_and_invalid_settings_remain_untouched() {
    let (_temp, storage, server, uri) = fixture();
    let now = Utc::now();
    for time in [now, now - Delta::hours(1)] {
        thread::scope(|scope| {
            scope.spawn(|| serve(&server, manifest(&uri, "0.2.0")));
            assert!(daily::notice(&storage, &uri, "0.1.0", time).is_some());
        });
    }
    std::fs::write(storage.path("settings.json").unwrap(), b"broken").unwrap();
    assert!(daily::notice(&storage, &uri, "0.1.0", now).is_none());
    assert_eq!(
        std::fs::read(storage.path("settings.json").unwrap()).unwrap(),
        b"broken"
    );
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
}
#[test]
fn redirected_cli_json_and_information_commands_skip_update_http() {
    let (temp, _storage, server, uri) = fixture();
    for args in [
        vec!["account", "list", "--json"],
        vec!["account", "list"],
        vec!["version"],
        vec!["readme"],
        vec!["update", "--status"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_colab"))
            .env_clear()
            .env("HOME", temp.path())
            .env("COLAB_CLI_UPDATE_MANIFEST", uri.as_str())
            .args([
                "--config-dir",
                temp.path().to_str().unwrap(),
                "--token-storage",
                "file",
            ])
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("新版本"));
        if args.contains(&"--json") {
            serde_json::from_slice::<Value>(&output.stdout).unwrap();
        }
    }
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
    assert!(!temp.path().join("settings.json").exists());
}

#[test]
fn unresponsive_server_is_bounded_and_failure_is_cached() {
    let (_temp, storage, server, uri) = fixture();
    let now = Utc::now();
    let start = std::time::Instant::now();
    thread::scope(|scope| {
        let handle = scope.spawn(|| daily::notice(&storage, &uri, "0.1.0", now));
        let _unanswered = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(handle.join().unwrap().is_none());
    });
    assert!(start.elapsed() < Duration::from_secs(4));
    assert!(daily::notice(&storage, &uri, "0.1.0", now).is_none());
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn interactive_cli_shows_cached_notice_but_json_and_information_stay_clean() {
    use std::{fs::File, io::Read, os::fd::FromRawFd, process::Stdio};
    fn interactive_result(temp: &std::path::Path, uri: &Url, args: &[&str]) -> (bool, String) {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: valid out pointers; null optional name/termios/winsize.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: openpty succeeded, returning distinct owned descriptors.
        let mut master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let mut child = Command::new(env!("CARGO_BIN_EXE_colab"))
            .env_clear()
            .env("HOME", temp)
            .env("COLAB_CLI_UPDATE_MANIFEST", uri.as_str())
            .args([
                "--config-dir",
                temp.to_str().unwrap(),
                "--token-storage",
                "file",
            ])
            .args(args)
            .stdin(Stdio::null())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave)
            .spawn()
            .unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            match master.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) => panic!("pty read: {e}"),
            }
        }
        (
            child.wait().unwrap().success(),
            String::from_utf8(bytes).unwrap(),
        )
    }
    fn interactive(temp: &std::path::Path, uri: &Url, args: &[&str]) -> String {
        let (success, output) = interactive_result(temp, uri, args);
        assert!(success, "{output}");
        output
    }
    let (temp, storage, server, uri) = fixture();
    let (success, output) =
        interactive_result(temp.path(), &uri, &["-a", "missing", "account", "list"]);
    assert!(!success);
    assert!(!output.contains("新版本"));
    assert!(!temp.path().join("settings.json").exists());
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
    thread::scope(|scope| {
        scope.spawn(|| serve(&server, manifest(&uri, "99.0.0")));
        let output = interactive(temp.path(), &uri, &["account", "list"]);
        assert!(output.contains("新版本 99.0.0"), "{output:?}");
    });
    assert!(interactive(temp.path(), &uri, &["account", "list"]).contains("新版本 99.0.0"));
    for args in [
        &["account", "list", "--json"][..],
        &["version"][..],
        &["help"][..],
    ] {
        assert!(!interactive(temp.path(), &uri, args).contains("新版本 99.0.0"));
    }
    storage
        .update_settings(|s| {
            s.enable_update_check = false;
            Ok(())
        })
        .unwrap();
    assert!(!interactive(temp.path(), &uri, &["account", "list"]).contains("新版本 99.0.0"));
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
}
