#![cfg(unix)]
use chrono::{Duration as Delta, Utc};
use colabcli_rust::{
    auth::{AdcProvider, MetadataOptions, TokenClient},
    context::{AccountContext, AccountSource},
    models::{AuthProvider, ProfileId},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tiny_http::{Request, Response, Server};
const AUD: &str =
    "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p";
fn quote(p: &Path) -> String {
    format!("'{}'", p.to_str().unwrap().replace('\'', "'\\''"))
}
fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn document(s: &Server, command: &str) -> Value {
    json!({"type":"external_account","audience":AUD,"subject_token_type":"urn:ietf:params:oauth:token-type:jwt","token_url":format!("http://{}/v1/token",s.server_addr()),"credential_source":{"executable":{"command":command}}})
}
fn provider(path: &Path, doc: &Value, allowed: bool) -> AdcProvider {
    fs::write(path, doc.to_string()).unwrap();
    let c = AccountContext {
        profile: ProfileId::new("exec").unwrap(),
        source: AccountSource::Flag,
        provider: AuthProvider::Adc,
        adc_file: Some(path.into()),
        quota_project: None,
        adc_file_required: true,
        cancellation: None,
        aws: Default::default(),
        certificate: Default::default(),
        metadata: MetadataOptions {
            disabled: true,
            ..Default::default()
        },
        allow_executables: allowed,
    };
    AdcProvider::from_context(&c, TokenClient::new().unwrap())
}
fn response(token: &str) -> Value {
    json!({"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:id_token","id_token":token,"expiration_time":(Utc::now()+Delta::hours(1)).timestamp()})
}
fn body(temp: &Path, v: &Value) -> PathBuf {
    let path = temp.join("body.json");
    fs::write(&path, v.to_string()).unwrap();
    path
}
fn script(temp: &Path, text: &str) -> String {
    let path = temp.join("plugin ' quoted.sh");
    fs::write(&path, text).unwrap();
    format!("/bin/sh {}", quote(&path))
}
fn recv(s: &Server) -> Request {
    s.recv_timeout(Duration::from_secs(15))
        .unwrap()
        .expect("expected request")
}
fn sts(s: &Server) -> (Request, BTreeMap<String, String>) {
    let mut r = recv(s);
    assert_eq!(r.method().as_str(), "POST");
    assert_eq!(r.url(), "/v1/token");
    assert!(!r.headers().iter().any(|h| h.field.equiv("Authorization")));
    let mut text = String::new();
    r.as_reader().read_to_string(&mut text).unwrap();
    let fields = url::form_urlencoded::parse(text.as_bytes())
        .into_owned()
        .collect();
    (r, fields)
}
fn reply(r: Request, token: &str) {
    r.respond(Response::from_string(json!({"access_token":token,"token_type":"Bearer","issued_token_type":"urn:ietf:params:oauth:token-type:access_token","expires_in":3600}).to_string())).unwrap();
}
fn quiet(s: &Server) {
    assert!(s.recv_timeout(Duration::from_millis(75)).unwrap().is_none());
}
fn alive(pid: i32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().starts_with('Z')
}

#[test]
fn executable_opt_in_is_required_even_with_valid_cached_response() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let cache = body(temp.path(), &response("cached"));
    let marker = temp.path().join("must-not-exist");
    let command = script(temp.path(), &format!("touch {}", quote(&marker)));
    let mut doc = document(&server, &command);
    doc["credential_source"]["executable"]["output_file"] = json!(cache);
    let p = provider(&temp.path().join("adc"), &doc, false);
    assert!(p.access_token(Utc::now()).is_err());
    assert!(!marker.exists());
    quiet(&server);
}
#[test]
fn executable_quoted_arguments_hints_and_stdout_reach_sts_without_stderr_leaks() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let file = body(temp.path(), &response("plugin-subject"));
    let command = script(
        temp.path(),
        &format!(
            "test \"$1\" = 'arg with spaces' || exit 3\ntest \"$GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE\" = '{AUD}' || exit 4\ntest \"$GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE\" = 'urn:ietf:params:oauth:token-type:jwt' || exit 5\ntest \"$GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE\" = 0 || exit 6\ntest \"$GOOGLE_EXTERNAL_ACCOUNT_REVOKE\" = 0 || exit 7\ntest \"$GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES\" = 1 || exit 8\ntest -z \"${{GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL+x}}\" || exit 9\ntest -z \"${{GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE+x}}\" || exit 10\nprintf private-stderr >&2\ncat {}",
            quote(&file)
        ),
    ) + " 'arg with spaces'";
    let p = provider(&temp.path().join("adc"), &document(&server, &command), true);
    let handler = thread::spawn(move || {
        let (r, f) = sts(&server);
        assert_eq!(f["subject_token"], "plugin-subject");
        reply(r, "access");
        quiet(&server);
    });
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "access");
    handler.join().unwrap();
}
#[test]
fn executable_cache_skips_command_then_expired_cache_runs_and_malformed_cache_refuses_execution() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let cache = temp.path().join("cache.json");
    fs::write(&cache, response("cached").to_string()).unwrap();
    let fresh = body(temp.path(), &response("fresh"));
    let marker = temp.path().join("ran");
    let command = script(
        temp.path(),
        &format!("touch {}\ncat {}", quote(&marker), quote(&fresh)),
    );
    let mut doc = document(&server, &command);
    doc["credential_source"]["executable"]["output_file"] = json!(cache);
    let p = provider(&temp.path().join("adc"), &doc, true);
    let handler = thread::spawn(move || {
        for name in ["cached", "fresh"] {
            let (r, f) = sts(&server);
            assert_eq!(f["subject_token"], name);
            reply(r, name);
        }
        quiet(&server);
    });
    let now = Utc::now();
    assert_eq!(p.access_token(now).unwrap().expose(), "cached");
    assert!(!marker.exists());
    let mut expired = response("expired");
    expired["expiration_time"] = json!(now.timestamp() - 1);
    fs::write(&cache, expired.to_string()).unwrap();
    assert_eq!(p.refresh_access_token(now).unwrap().expose(), "fresh");
    assert!(marker.exists());
    fs::remove_file(&marker).unwrap();
    fs::write(&cache, "private-invalid-json").unwrap();
    assert!(p.refresh_access_token(now).is_err());
    assert!(!marker.exists());
    assert_eq!(p.access_token(now).unwrap().expose(), "fresh");
    handler.join().unwrap();
}
#[test]
fn executable_can_write_its_own_cache_and_return_empty_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let file = body(temp.path(), &response("file-subject"));
    let cache = temp.path().join("new cache.json");
    let command = script(
        temp.path(),
        &format!(
            "test \"$GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE\" = {} || exit 3\ncp {} \"$GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE\"",
            quote(&cache),
            quote(&file)
        ),
    );
    let mut doc = document(&server, &command);
    doc["credential_source"]["executable"]["output_file"] = json!(cache);
    let p = provider(&temp.path().join("adc"), &doc, true);
    let handler = thread::spawn(move || {
        let (r, f) = sts(&server);
        assert_eq!(f["subject_token"], "file-subject");
        reply(r, "access");
        quiet(&server);
    });
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "access");
    assert_eq!(fs::read(cache).unwrap(), fs::read(file).unwrap());
    handler.join().unwrap();
}
#[test]
fn executable_invalid_outputs_never_reach_sts_or_echo_sensitive_material() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let mut invalid = Vec::new();
    for (key, value) in [
        ("version", json!(2)),
        ("success", json!(false)),
        ("id_token", json!("")),
        (
            "token_type",
            json!("urn:ietf:params:oauth:token-type:saml2"),
        ),
        ("expiration_time", json!(1)),
        ("expiration_time", json!(i64::MAX)),
    ] {
        let mut v = response("private-marker");
        v[key] = value;
        v["code"] = json!("private-marker");
        v["message"] = json!("private-marker");
        invalid.push(v);
    }
    for v in invalid {
        let file = body(temp.path(), &v);
        let p = provider(
            &temp.path().join("adc"),
            &document(&server, &format!("/bin/cat {}", quote(&file))),
            true,
        );
        assert!(
            !p.access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("private-marker")
        );
    }
    for text in ["private-marker".into(), "x".repeat(65537)] {
        let file = temp.path().join("raw");
        fs::write(&file, text).unwrap();
        let p = provider(
            &temp.path().join("adc"),
            &document(&server, &format!("/bin/cat {}", quote(&file))),
            true,
        );
        assert!(
            !p.access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("private-marker")
        );
    }
    let command = script(
        temp.path(),
        "printf private-marker; printf private-marker >&2; exit 4",
    );
    let p = provider(&temp.path().join("adc"), &document(&server, &command), true);
    assert!(
        !p.access_token(Utc::now())
            .unwrap_err()
            .to_string()
            .contains("private-marker")
    );
    quiet(&server);
}
#[test]
fn executable_invalid_configurations_do_not_start_processes() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    for config in [
        json!({"command":"relative-command"}),
        json!({"command":"'/unterminated"}),
        json!({"command":"/bin/sh","timeout_millis":4999}),
        json!({"command":"/bin/sh","timeout_millis":120001}),
        json!({"command":"/bin/sh","output_file":"relative"}),
        json!({"command":"/bin/sh","interactive_timeout_millis":30000}),
    ] {
        let mut doc = document(&server, "/bin/sh");
        doc["credential_source"]["executable"] = config;
        let p = provider(&temp.path().join("adc"), &doc, true);
        assert!(p.access_token(Utc::now()).is_err());
    }
    quiet(&server);
}
#[test]
fn executable_timeout_kills_and_reaps_owned_process_group() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let marker = temp.path().join("pids");
    let command = script(
        temp.path(),
        &format!(
            "sleep 30 &\nprintf '%s %s' \"$$\" \"$!\" > {}\nwait",
            quote(&marker)
        ),
    );
    let mut doc = document(&server, &command);
    doc["credential_source"]["executable"]["timeout_millis"] = json!(5000);
    let p = provider(&temp.path().join("adc"), &doc, true);
    let start = Instant::now();
    assert!(p.access_token(Utc::now()).is_err());
    assert!(start.elapsed() < Duration::from_secs(10));
    let pids: Vec<i32> = fs::read_to_string(marker)
        .unwrap()
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 2);
    for pid in pids {
        assert!(!alive(pid), "owned plugin process remains alive");
    }
    quiet(&server);
}
#[test]
fn executable_profiles_keep_separate_subject_and_access_token_caches() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let mut providers = Vec::new();
    for name in ["a", "b"] {
        let dir = temp.path().join(name);
        fs::create_dir(&dir).unwrap();
        let file = body(&dir, &response(name));
        providers.push(Arc::new(provider(
            &dir.join("adc"),
            &document(&server, &format!("/bin/cat {}", quote(&file))),
            true,
        )));
    }
    let handler = thread::spawn(move || {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..2 {
            let (r, f) = sts(&server);
            let name = &f["subject_token"];
            assert!(seen.insert(name.clone()));
            reply(r, name);
        }
        quiet(&server);
    });
    let jobs: Vec<_> = (0..8)
        .map(|i| {
            let p = providers[i % 2].clone();
            thread::spawn(move || {
                assert_eq!(
                    p.access_token(Utc::now()).unwrap().expose(),
                    if i % 2 == 0 { "a" } else { "b" }
                )
            })
        })
        .collect();
    for j in jobs {
        j.join().unwrap();
    }
    handler.join().unwrap();
}
#[test]
fn executable_cli_cancellation_reaps_plugin_and_does_not_send_sts_request() {
    for business in [
        vec!["account", "login"],
        vec!["account", "status", "--json"],
        vec!["sessions", "--all-accounts", "--json"],
    ] {
        let temp = tempfile::tempdir().unwrap();
        let server = server();
        let marker = temp.path().join("pid");
        let command = script(
            temp.path(),
            &format!("printf '%s\\n' \"$$\" >> {}\nsleep 30", quote(&marker)),
        );
        let path = temp.path().join("adc");
        fs::write(&path, document(&server, &command).to_string()).unwrap();
        let config = temp.path().join("config");
        let expected = if business == ["account", "login"] {
            1
        } else {
            4
        };
        if expected == 4 {
            use std::os::unix::fs::PermissionsExt;
            let mut profiles = serde_json::Map::new();
            for name in ["default", "p1", "p2", "p3", "p4"] {
                let dir = config.join("accounts").join(name);
                fs::create_dir_all(&dir).unwrap();
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
                profiles.insert(name.into(), json!({"auth_provider":"adc"}));
            }
            fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(config.join("accounts"), fs::Permissions::from_mode(0o700))
                .unwrap();
            let registry = config.join("accounts.json");
            fs::write(&registry, json!({"version":1,"active":"default","default_drive_account":null,"accounts":profiles}).to_string()).unwrap();
            fs::set_permissions(&registry, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_colab"));
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("GOOGLE_APPLICATION_CREDENTIALS", &path)
            .env("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES", "1")
            .env(
                "GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL",
                "private-stale",
            )
            .env("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE", "private-stale")
            .args([
                "--config-dir",
                config.to_str().unwrap(),
                "--token-storage",
                "file",
                "--auth",
                "adc",
            ])
            .args(business)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let count = || {
            fs::read_to_string(&marker)
                .map(|s| s.split_whitespace().count())
                .unwrap_or(0)
        };
        while count() < expected && Instant::now() < deadline {
            assert!(child.try_wait().unwrap().is_none());
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(count(), expected);
        let pids: Vec<i32> = fs::read_to_string(&marker)
            .unwrap()
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("CLI cancellation did not finish");
        }
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(143));
        assert_eq!(
            count(),
            expected,
            "queued account must not start after cancellation"
        );
        for pid in pids {
            assert!(!alive(pid));
        }
        assert!(!String::from_utf8_lossy(&out.stderr).contains("private-stale"));
        quiet(&server);
    }
}

#[test]
fn successful_plugin_exit_reaps_descendants_before_draining_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let file = body(temp.path(), &response("subject"));
    let marker = temp.path().join("descendant");
    let command = script(
        temp.path(),
        &format!(
            "sleep 30 &\nprintf '%s' \"$!\" > {}\ncat {}\nexit 0",
            quote(&marker),
            quote(&file)
        ),
    );
    let p = provider(&temp.path().join("adc"), &document(&server, &command), true);
    let handler = thread::spawn(move || {
        let (r, f) = sts(&server);
        assert_eq!(f["subject_token"], "subject");
        reply(r, "access");
        quiet(&server);
    });
    let start = Instant::now();
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "access");
    assert!(start.elapsed() < Duration::from_secs(5));
    let pid: i32 = fs::read_to_string(marker).unwrap().parse().unwrap();
    assert!(!alive(pid));
    handler.join().unwrap();
}
#[test]
fn cli_executable_opt_in_and_iam_hints_override_stale_environment_without_exposing_tokens() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let file = body(temp.path(), &response("subject"));
    let marker = temp.path().join("ran");
    let command = script(
        temp.path(),
        &format!(
            "test \"$GOOGLE_EXTERNAL_ACCOUNT_ID\" = 'target@example.invalid' || exit 3\ntest \"$GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL\" = 'target@example.invalid' || exit 4\ntest \"$GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE\" = 0 || exit 5\ntest \"$GOOGLE_EXTERNAL_ACCOUNT_REVOKE\" = 0 || exit 6\ntest -z \"${{GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE+x}}\" || exit 7\ntouch {}\ncat {}",
            quote(&marker),
            quote(&file)
        ),
    );
    let mut doc = document(&server, &command);
    doc["service_account_impersonation_url"] = json!(format!(
        "http://{}/v1/projects/-/serviceAccounts/target%40example.invalid:generateAccessToken",
        server.server_addr()
    ));
    let path = temp.path().join("adc");
    fs::write(&path, doc.to_string()).unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let config = temp.path().join("config");
    let cli = |allow: &str| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_colab"));
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("GOOGLE_APPLICATION_CREDENTIALS", &path)
            .env("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES", allow)
            .env("GOOGLE_EXTERNAL_ACCOUNT_ID", "private-stale")
            .env(
                "GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL",
                "private-stale",
            )
            .env("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE", "private-stale")
            .env("GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE", "1")
            .env("GOOGLE_EXTERNAL_ACCOUNT_REVOKE", "1")
            .args([
                "--config-dir",
                config.to_str().unwrap(),
                "--token-storage",
                "file",
                "--auth",
                "adc",
                "account",
                "login",
            ])
            .stdin(Stdio::null());
        c
    };
    let out = cli("0").output().unwrap();
    assert!(!out.status.success());
    assert!(!marker.exists());
    let handler = thread::spawn(move || {
        let (r, f) = sts(&server);
        assert_eq!(f["subject_token"], "subject");
        assert_eq!(f["scope"], "https://www.googleapis.com/auth/cloud-platform");
        reply(r, "private-sts-access");
        let mut r = recv(&server);
        assert!(
            r.url()
                .contains("target%40example.invalid:generateAccessToken")
        );
        assert_eq!(
            r.headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .unwrap()
                .value
                .as_str(),
            "Bearer private-sts-access"
        );
        let mut raw = String::new();
        r.as_reader().read_to_string(&mut raw).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap(),
            json!({"scope":colabcli_rust::auth::PUBLIC_SCOPES,"lifetime":"3600s"})
        );
        r.respond(Response::from_string(json!({"accessToken":"private-target-access","expireTime":(Utc::now()+Delta::hours(1)).to_rfc3339()}).to_string())).unwrap();
        quiet(&server);
    });
    let out = cli("1").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(marker.exists());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("private-"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("private-"));
    handler.join().unwrap();
}

#[test]
fn saml_executable_preserves_assertion_bytes_and_requires_expiry_when_cache_is_configured() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let assertion = "<Response>\nopaque assertion\n</Response>";
    let v = json!({"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:saml2","saml_response":assertion});
    let file = body(temp.path(), &v);
    let mut doc = document(&server, &format!("/bin/cat {}", quote(&file)));
    doc["subject_token_type"] = json!("urn:ietf:params:oauth:token-type:saml2");
    let p = provider(&temp.path().join("adc"), &doc, true);
    let handler = thread::spawn(move || {
        let (r, f) = sts(&server);
        assert_eq!(f["subject_token"], assertion);
        assert_eq!(
            f["subject_token_type"],
            "urn:ietf:params:oauth:token-type:saml2"
        );
        reply(r, "access");
        quiet(&server);
    });
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "access");
    handler.join().unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    doc["token_url"] = json!(format!("http://{}/v1/token", server.server_addr()));
    doc["credential_source"]["executable"]["output_file"] =
        json!(temp.path().join("missing-cache"));
    let p = provider(&temp.path().join("adc-2"), &doc, true);
    assert!(p.access_token(Utc::now()).is_err());
    quiet(&server);
}

#[test]
fn executable_response_that_expires_during_execution_is_not_sent_to_sts() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let mut v = response("expiring-subject");
    v["expiration_time"] = json!(Utc::now().timestamp() + 1);
    let file = body(temp.path(), &v);
    let command = script(
        temp.path(),
        &format!("/bin/sleep 2\n/bin/cat {}", quote(&file)),
    );
    let p = provider(&temp.path().join("adc"), &document(&server, &command), true);
    let (stop, done) = std::sync::mpsc::channel();
    let handler = thread::spawn(move || {
        let mut seen = 0;
        loop {
            if done.try_recv().is_ok() {
                return seen;
            }
            if let Some(r) = server.recv_timeout(Duration::from_millis(50)).unwrap() {
                seen += 1;
                reply(r, "must-not-be-issued");
            }
        }
    });
    let result = p.access_token(Utc::now());
    stop.send(()).unwrap();
    let seen = handler.join().unwrap();
    assert!(matches!(
        result,
        Err(colabcli_rust::Error::OAuth("可执行 ADC 返回已过期令牌"))
    ));
    assert_eq!(seen, 0);
}
