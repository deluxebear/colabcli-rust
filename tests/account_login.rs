use colabcli_rust::drive_credentials::{DRIVEFS_CLIENT_ID, MINIMAL_SCOPES};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};
use tiny_http::{Response, Server};
use url::Url;

struct Fixture {
    temp: tempfile::TempDir,
    root: std::path::PathBuf,
    client: std::path::PathBuf,
}
impl Fixture {
    fn new(base: &str, client_id: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("config");
        let client = temp.path().join("client.json");
        fs::write(&client,json!({"installed":{"client_id":client_id,"client_secret":"fixture-secret","auth_uri":format!("{base}/authorize"),"token_uri":format!("{base}/token")}}).to_string()).unwrap();
        Self { temp, root, client }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_colab"));
        cmd.env_clear()
            .env("HOME", self.temp.path())
            .args(["--token-storage", "file", "--config-dir"])
            .arg(&self.root)
            .arg("-c")
            .arg(&self.client);
        cmd
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
    fn input(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
    fn read(&self, path: &str) -> Value {
        serde_json::from_slice(&fs::read(self.root.join(path)).unwrap()).unwrap()
    }
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn server() -> (Server, String) {
    let s = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", s.server_addr());
    (s, base)
}
fn token_server(
    s: Server,
    count: usize,
    failure: bool,
) -> thread::JoinHandle<Vec<BTreeMap<String, String>>> {
    thread::spawn(move || {
        let mut forms = Vec::new();
        for _ in 0..count {
            let mut req = s
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .expect("expected token exchange");
            assert_eq!(req.url(), "/token");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let form: BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect();
            let drive = form.get("code").is_some_and(|code| code == "drive-code");
            let payload = if failure {
                json!({"error":"invalid_grant","private":"must-not-display"})
            } else {
                json!({"access_token":if drive {"drive-access"}else{"compute-access"},"refresh_token":if drive {"drive-refresh"}else{"compute-refresh"},"expires_in":3600,"token_type":"Bearer","scope":if drive {MINIMAL_SCOPES.join(" ")}else{"openid".into()}})
            };
            req.respond(
                Response::from_string(payload.to_string()).with_status_code(if failure {
                    400
                } else {
                    200
                }),
            )
            .unwrap();
            forms.push(form);
        }
        forms
    })
}

#[test]
fn add_url_only_switches_active_after_pending_and_no_use_keeps_previous_active() {
    let f = Fixture::new("http://127.0.0.1:1", "fixture-client");
    let output = f.run(&[
        "account",
        "add",
        "work",
        "--url-only",
        "--email",
        "work@example.invalid",
    ]);
    success(&output);
    let url = Url::parse(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "login_hint" && value == "work@example.invalid")
    );
    assert_eq!(f.read("accounts.json")["active"], "work");
    assert_eq!(
        f.read("accounts/work/oauth_pending.json")["account"],
        "work"
    );
    assert!(!f.root.join("accounts/work/dfs_credentials.json").exists());
    success(&f.run(&[
        "acct",
        "add",
        "other",
        "--url-only",
        "--no-use",
        "--with-dfs",
    ]));
    assert_eq!(f.read("accounts.json")["active"], "work");
    assert!(f.root.join("accounts/other/oauth_pending.json").exists());
    assert!(!f.root.join("accounts/other/dfs_pending.json").exists());
}
#[test]
fn configure_only_is_offline_and_binding_is_resolved_before_registration() {
    let f = Fixture::new("http://127.0.0.1:1", "fixture-client");
    success(&f.run(&["account", "add", "storage", "--configure-only"]));
    success(&f.run(&[
        "account",
        "add",
        "work",
        "--configure-only",
        "--drive-account",
        "storage",
    ]));
    let r = f.read("accounts.json");
    assert_eq!(r["active"], "default");
    assert_eq!(r["accounts"]["work"]["drive_account"], "storage");
    assert!(!f.root.join("accounts/work/oauth_pending.json").exists());
    assert!(
        !f.run(&[
            "account",
            "add",
            "rejected",
            "--drive-account",
            "missing",
            "--url-only"
        ])
        .status
        .success()
    );
    assert!(
        f.read("accounts.json")["accounts"]
            .get("rejected")
            .is_none()
    );
    assert!(
        !f.run(&["account", "add", "work", "--url-only"])
            .status
            .success()
    );
    assert_eq!(f.read("accounts.json"), r);
}
#[test]
fn named_login_creates_only_on_begin_and_email_hint_does_not_change_active() {
    let (s, base) = server();
    let f = Fixture::new(&base, "fixture-client");
    assert!(
        !f.run(&["account", "login", "missing", "--code", "compute-code"])
            .status
            .success()
    );
    assert!(f.read("accounts.json")["accounts"].get("missing").is_none());
    let output = f.run(&[
        "account",
        "login",
        "work",
        "--email",
        "hint@example.invalid",
        "--url-only",
    ]);
    success(&output);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("login_hint=hint%40example.invalid")
    );
    let worker = token_server(s, 1, false);
    success(&f.run(&["account", "login", "work", "--code", "compute-code"]));
    assert_eq!(worker.join().unwrap()[0]["code"], "compute-code");
    assert_eq!(f.read("accounts.json")["active"], "default");
    assert!(!f.root.join("accounts/work/oauth_pending.json").exists());
    assert_eq!(
        f.read("accounts/work/oauth_token.json")["token"],
        "compute-access"
    );
}
#[test]
fn add_warns_on_drive_failure_but_login_returns_failure_and_keeps_compute_authorization() {
    for add in [true, false] {
        let (s, base) = server();
        let f = Fixture::new(&base, "fixture-client");
        let worker = token_server(s, 1, false);
        let output = f.input(
            &[
                "account",
                if add { "add" } else { "login" },
                "work",
                "--with-drive",
                "--no-browser",
            ],
            "compute-code\n",
        );
        assert_eq!(output.status.success(), add);
        assert_eq!(worker.join().unwrap().len(), 1);
        assert_eq!(
            f.read("accounts/work/oauth_token.json")["token"],
            "compute-access"
        );
        assert_eq!(
            f.read("accounts.json")["active"],
            if add { "work" } else { "default" }
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("DriveFS OAuth client"));
        assert!(!f.root.join("accounts/work/dfs_credentials.json").exists());
        assert!(!f.root.join("accounts/work/oauth_pending.json").exists());
    }
}
#[test]
fn drive_compatibility_switches_override_primary_switches_and_binding_skips_consent() {
    for (flags, attempted) in [
        (&["--with-drive", "--no-dfs"][..], false),
        (&["--no-drive", "--with-dfs"][..], true),
        (&["--drive-account", "default"][..], false),
    ] {
        let (s, base) = server();
        let f = Fixture::new(&base, "fixture-client");
        let worker = token_server(s, 1, false);
        let mut args = vec!["account", "add", "work", "--no-browser"];
        args.extend_from_slice(flags);
        let output = f.input(&args, "compute-code\n");
        success(&output);
        worker.join().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).contains("DriveFS OAuth client"),
            attempted
        );
        assert_eq!(f.read("accounts.json")["active"], "work");
    }
}
#[test]
fn failed_compute_exchange_retains_pending_and_never_changes_active_or_starts_drive() {
    let (s, base) = server();
    let f = Fixture::new(&base, "fixture-client");
    let worker = token_server(s, 1, true);
    let output = f.input(
        &["account", "add", "work", "--no-browser"],
        "compute-code\n",
    );
    assert!(!output.status.success());
    worker.join().unwrap();
    assert_eq!(f.read("accounts.json")["active"], "default");
    assert_eq!(
        f.read("accounts/work/oauth_pending.json")["exchange_started"],
        true
    );
    assert!(!f.root.join("accounts/work/oauth_token.json").exists());
    assert!(!f.root.join("accounts/work/dfs_pending.json").exists());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("must-not-display"));
}
#[test]
fn adc_add_verifies_before_activation_and_does_not_start_drive_consent() {
    for fail in [true, false] {
        let (s, base) = server();
        let f = Fixture::new(&base, "fixture-client");
        let adc = f.temp.path().join("adc.json");
        fs::write(&adc,json!({"type":"authorized_user","client_id":"adc-fixture","refresh_token":"adc-refresh","token_uri":format!("{base}/token")}).to_string()).unwrap();
        let worker = token_server(s, 1, fail);
        let output = f
            .command()
            .args(["--auth", "oauth2", "account", "add", "cloud", "--adc-file"])
            .arg(&adc)
            .output()
            .unwrap();
        assert_eq!(output.status.success(), !fail);
        let forms = worker.join().unwrap();
        assert_eq!(forms[0]["grant_type"], "refresh_token");
        let r = f.read("accounts.json");
        assert_eq!(r["accounts"]["cloud"]["auth_provider"], "adc");
        assert_eq!(r["active"], if fail { "default" } else { "cloud" });
        assert!(!f.root.join("accounts/cloud/oauth_pending.json").exists());
        assert!(!f.root.join("accounts/cloud/dfs_pending.json").exists());
    }
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn add_completes_compute_then_loopback_drive_consent_in_the_same_explicit_account() {
    let (s, base) = server();
    let f = Fixture::new(&base, DRIVEFS_CLIENT_ID);
    let worker = token_server(s, 2, false);
    let mut child = ChildGuard(
        f.command()
            .env("COLAB_DRIVE_USER_ID", "gaia-fixture")
            .args(["account", "add", "work", "--no-browser"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    child
        .0
        .stdin
        .take()
        .unwrap()
        .write_all(b"compute-code\n")
        .unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let url = loop {
        let line = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("Drive authorization URL");
        if line.starts_with("http://") {
            break Url::parse(&line).unwrap();
        }
    };
    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert!(
        query["scope"]
            .split_whitespace()
            .any(|scope| scope == "https://www.googleapis.com/auth/drive")
    );
    let mut callback = Url::parse(&query["redirect_uri"]).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("code", "drive-code")
        .append_pair("state", &query["state"]);
    assert!(
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(callback)
            .send()
            .unwrap()
            .status()
            .is_success()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(std::time::Instant::now() < deadline, "child did not finish");
        thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "{stderr}");
    reader.join().unwrap();
    let forms = worker.join().unwrap();
    assert_eq!(forms[0]["code"], "compute-code");
    assert_eq!(forms[1]["code"], "drive-code");
    assert_eq!(
        f.read("accounts/work/oauth_token.json")["token"],
        "compute-access"
    );
    assert_eq!(
        f.read("accounts/work/dfs_credentials.json")["refresh_token"],
        "drive-refresh"
    );
    assert_eq!(f.read("accounts.json")["active"], "work");
    assert!(!f.root.join("accounts/default/oauth_token.json").exists());
    assert!(!f.root.join("accounts/work/oauth_pending.json").exists());
    assert!(!f.root.join("accounts/work/dfs_pending.json").exists());
}
