use serde_json::Value;
use std::process::Command;

fn command(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_colab"));
    cmd.arg("--config-dir")
        .arg(root)
        .args(["--token-storage", "file"])
        .env_remove("COLAB_ACCOUNT")
        .env_remove("COLAB_TOKEN_STORAGE");
    cmd
}
#[test]
fn cli_can_register_select_and_inspect_accounts_offline() {
    let temp = tempfile::tempdir().unwrap();
    let output = command(temp.path())
        .args([
            "account",
            "add",
            "work",
            "--configure-only",
            "--email",
            "a@example.invalid",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        command(temp.path())
            .args(["account", "use", "work"])
            .status()
            .unwrap()
            .success()
    );
    let output = command(temp.path())
        .args(["account", "list", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["active"], "work");
    assert_eq!(
        value["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["account"] == "work")
            .unwrap()["email"],
        "a@example.invalid"
    );
    let output = command(temp.path())
        .args(["account", "current"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "work\n");
}
#[test]
fn concurrent_cli_processes_do_not_lose_registry_updates() {
    let temp = tempfile::tempdir().unwrap();
    let mut children = Vec::new();
    for n in 0..8 {
        children.push(
            command(temp.path())
                .args(["account", "add", &format!("work{n}"), "--configure-only"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let output = command(temp.path())
        .args(["account", "list", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["accounts"].as_array().unwrap().len(), 9);
}
#[test]
fn noninteractive_login_fails_without_waiting_for_input() {
    let temp = tempfile::tempdir().unwrap();
    let output = command(temp.path())
        .args(["account", "login"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--url-only"));
}
#[test]
fn invalid_profile_is_rejected_before_creating_escaped_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("config");
    let output = command(&root)
        .args(["account", "add", "../escape"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!temp.path().join("escape").exists());
}

#[test]
fn cli_headless_login_exchanges_code_using_only_loopback_http() {
    use std::{collections::BTreeMap, thread, time::Duration};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("config");
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let client_file = temp.path().join("client.json");
    std::fs::write(&client_file,serde_json::json!({"installed":{"client_id":"fixture-client","client_secret":"fixture-secret",
        "auth_uri":format!("{base}/authorize"),"token_uri":format!("{base}/token")}}).to_string()).unwrap();
    let output = command(&root)
        .arg("-c")
        .arg(&client_file)
        .args(["account", "login", "--url-only"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let url = url::Url::parse(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "code_challenge_method" && v == "S256")
    );
    let handler = thread::spawn(move || {
        let mut request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        let fields: BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(fields["code"], "fixture-code");
        assert_eq!(fields["code_verifier"].len(), 43);
        request.respond(tiny_http::Response::from_string(serde_json::json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","scope":"openid","expires_in":3600}).to_string())).unwrap();
    });
    let output = command(&root)
        .args(["account", "login", "--code", "fixture-code"])
        .output()
        .unwrap();
    handler.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-access"));
    let saved: Value = serde_json::from_slice(
        &std::fs::read(root.join("accounts/default/oauth_token.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["scopes"], serde_json::json!(["openid"]));
}

#[test]
fn global_drive_binding_can_be_cleared_without_affecting_accounts() {
    let temp = tempfile::tempdir().unwrap();
    assert!(
        command(temp.path())
            .args(["account", "set-drive", "default", "--default"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        command(temp.path())
            .args(["account", "set-drive", "--default", "--clear"])
            .status()
            .unwrap()
            .success()
    );
    let output = command(temp.path())
        .args(["account", "list", "--json"])
        .output()
        .unwrap();
    let r: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(r["default_drive_account"].is_null());
    assert_eq!(r["accounts"].as_array().unwrap().len(), 1);
}
