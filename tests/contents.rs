mod common;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use colabcli_rust::{
    Error,
    colab_api::{ApiConfig, ColabApi},
    contents::{
        ContentsClient, MAX_FILE_BYTES, edit_with, edit_with_access, read_local_file,
        validate_remote_path,
    },
    credentials::TokenStorageMode,
    models::{Secret, SessionState},
    sessions::SessionService,
};
use common::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, process::Command, thread, time::Duration};
use tiny_http::{Request, Response, Server, StatusCode};
use url::Url;

fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn state(server: &Server) -> SessionState {
    let mut s = session("files");
    s.url = format!("http://{}/runtime/base/", server.server_addr());
    s.token_expires_at = Some(Utc::now() + chrono::Duration::hours(1));
    s
}
fn client(server: &Server) -> ContentsClient {
    ContentsClient::new(&state(server), Duration::from_secs(2)).unwrap()
}
fn recv(server: &Server, method: &str, path: &str) -> Request {
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("expected Contents request");
    assert_eq!(request.method().as_str(), method);
    let url = Url::parse(&format!("http://localhost{}", request.url())).unwrap();
    assert_eq!(url.path(), path);
    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query["authuser"], "0");
    assert_eq!(query["colab-runtime-proxy-token"], "fixture-proxy");
    assert!(
        !request
            .headers()
            .iter()
            .any(|h| h.field.equiv("Authorization") || h.field.equiv("X-Goog-Colab-Token"))
    );
    request
}
fn entry(content: Value, format: &str) -> Value {
    json!({"name":"file.bin","path":"content/file.bin","type":"file","format":format,"content":content})
}
fn reply(request: Request, value: Value) {
    request
        .respond(Response::from_string(value.to_string()))
        .unwrap();
}

#[test]
fn special_characters_are_encoded_once_and_binary_bytes_round_trip() {
    let server = server();
    let client = client(&server);
    let bytes = vec![0, 1, 255, 0, 128, b'\n'];
    let expected = bytes.clone();
    let handle = thread::spawn(move || {
        let mut request = recv(
            &server,
            "PUT",
            "/runtime/base/api/contents/content/%E4%B8%AD%20%23%3F%25%2520.bin",
        );
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["name"], "中 #?%%20.bin");
        assert_eq!(body["path"], "content/中 #?%%20.bin");
        assert_eq!(body["chunk"], 1);
        assert_eq!(body["type"], "file");
        assert_eq!(body["format"], "base64");
        assert_eq!(
            STANDARD.decode(body["content"].as_str().unwrap()).unwrap(),
            expected
        );
        reply(request, entry(json!(null), "base64"));
        let request = recv(
            &server,
            "GET",
            "/runtime/base/api/contents/content/%E4%B8%AD%20%23%3F%25%2520.bin",
        );
        assert!(request.url().contains("content=1"));
        reply(request, entry(json!(STANDARD.encode(expected)), "base64"));
    });
    client
        .upload_bytes("/content/中 #?%%20.bin/", &bytes)
        .unwrap();
    assert_eq!(
        client.download_bytes("content/中 #?%%20.bin").unwrap(),
        bytes
    );
    handle.join().unwrap();
}
#[test]
fn listing_sorts_directories_then_files_and_accepts_single_file() {
    let server = server();
    let client = client(&server);
    let handle = thread::spawn(move || {
        let r = recv(&server, "GET", "/runtime/base/api/contents/content");
        reply(
            r,
            json!({"name":"content","path":"content","type":"directory","content":[
            {"name":"z","path":"content/z","type":"file"},
            {"name":"b","path":"content/b","type":"directory"},
            {"name":"a","path":"content/a","type":"directory"},
            {"name":"a.txt","path":"content/a.txt","type":"file"}]}),
        );
        let r = recv(
            &server,
            "GET",
            "/runtime/base/api/contents/content/file.bin",
        );
        reply(r, entry(json!(null), "text"));
    });
    let names: Vec<_> = client
        .list("content")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["a", "b", "a.txt", "z"]);
    assert_eq!(client.list("content/file.bin").unwrap().len(), 1);
    handle.join().unwrap();
}
#[test]
fn download_supports_empty_binary_text_and_notebook_json() {
    for (format, content, expected) in [
        ("base64", json!(""), Vec::new()),
        ("text", json!("中文\n"), "中文\n".as_bytes().to_vec()),
        (
            "json",
            json!({"cells":[],"nbformat":4}),
            serde_json::to_vec_pretty(&json!({"cells":[],"nbformat":4})).unwrap(),
        ),
    ] {
        let server = server();
        let client = client(&server);
        let handle = thread::spawn(move || {
            let r = recv(
                &server,
                "GET",
                "/runtime/base/api/contents/content/file.bin",
            );
            let mut e = entry(content, format);
            if format == "json" {
                e["type"] = json!("notebook");
            }
            reply(r, e);
        });
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("file.bin");
        fs::write(&local, b"previous").unwrap();
        client.download("content/file.bin", &local).unwrap();
        assert_eq!(fs::read(&local).unwrap(), expected);
        handle.join().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(local).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
#[test]
fn failed_download_never_replaces_existing_file_or_leaves_tempfile() {
    for (status, body) in [
        (200, entry(json!("bad-base64!"), "base64").to_string()),
        (
            200,
            json!({"name":"dir","path":"dir","type":"directory","content":[]}).to_string(),
        ),
        (401, "credential-response-secret".into()),
        (404, "missing".into()),
        (200, "<html>login</html>".into()),
        (200, entry(json!(null), "text").to_string()),
        (200, entry(json!("x"), "future").to_string()),
    ] {
        let server = server();
        let client = client(&server);
        let handle = thread::spawn(move || {
            let r = recv(&server, "GET", "/runtime/base/api/contents/file");
            r.respond(Response::from_string(body).with_status_code(StatusCode(status)))
                .unwrap();
        });
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("keep");
        fs::write(&file, b"keep").unwrap();
        let error = client.download("file", &file).unwrap_err();
        handle.join().unwrap();
        assert!(!error.to_string().contains("credential-response-secret"));
        assert!(!error.to_string().contains("fixture-proxy"));
        assert_eq!(fs::read(&file).unwrap(), b"keep");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
#[test]
fn delete_accepts_empty_body_and_never_follows_redirect() {
    let server = server();
    let client = client(&server);
    let handle =
        thread::spawn(move || {
            let r = recv(&server, "DELETE", "/runtime/base/api/contents/file");
            r.respond(Response::empty(StatusCode(204))).unwrap();
            let r = recv(&server, "DELETE", "/runtime/base/api/contents/file");
            r.respond(Response::empty(StatusCode(302)).with_header(
                tiny_http::Header::from_bytes("Location", "/would-leak-proxy").unwrap(),
            ))
            .unwrap();
            assert!(
                server
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .is_none()
            );
        });
    client.remove("file").unwrap();
    assert!(matches!(
        client.remove("file"),
        Err(Error::RuntimeStatus(302))
    ));
    handle.join().unwrap();
}
#[test]
fn remote_validation_rejects_traversal_and_root_mutations_before_network() {
    for path in ["/", "", "../x", "a/../b", "./x", "a\\b", "a\n"] {
        assert!(validate_remote_path(path, true).is_err(), "{path}");
    }
    assert!(validate_remote_path("/", false).is_ok());
    assert!(validate_remote_path("a/%2e%2e/b", true).is_ok()); // A literal percent string, encoded again on the wire.
    let server = server();
    let client = client(&server);
    assert!(client.remove("/").is_err());
    assert!(client.upload_bytes("..", b"x").is_err());
    assert!(client.download_bytes("a/../b").is_err());
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
}
#[test]
fn local_input_and_file_size_are_validated_without_a_request() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("large");
    fs::File::create(&file)
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    assert!(matches!(
        read_local_file(&file),
        Err(Error::FileTooLarge(_))
    ));
    assert!(read_local_file(tmp.path()).is_err());
    assert!(read_local_file(&tmp.path().join("absent")).is_err());
    let server = server();
    let client = client(&server);
    assert!(client.upload(&file, "file").is_err());
    assert!(client.download("file", tmp.path()).is_err());
    assert!(
        client
            .download("file", &tmp.path().join("missing-parent/file"))
            .is_err()
    );
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
}
#[cfg(unix)]
#[test]
fn download_refuses_local_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("real");
    let link = tmp.path().join("link");
    fs::write(&target, b"original").unwrap();
    symlink(&target, &link).unwrap();
    let server = server();
    let client = client(&server);
    assert!(client.download("file", &link).is_err());
    assert_eq!(fs::read(target).unwrap(), b"original");
    assert!(
        server
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
}
#[test]
fn edit_uploads_only_changed_content_and_preserves_extension() {
    let server = server();
    let client = client(&server);
    let handle = thread::spawn(move || {
        let r = recv(&server, "GET", "/runtime/base/api/contents/code.py");
        reply(r, entry(json!("before"), "text"));
        let mut r = recv(&server, "PUT", "/runtime/base/api/contents/code.py");
        let mut body = String::new();
        r.as_reader().read_to_string(&mut body).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            STANDARD.decode(v["content"].as_str().unwrap()).unwrap(),
            b"after"
        );
        reply(r, entry(json!(null), "text"));
    });
    assert!(
        edit_with(&client, "code.py", |path| {
            assert_eq!(path.extension().unwrap(), "py");
            assert_eq!(fs::read(path).unwrap(), b"before");
            fs::write(path, b"after").unwrap();
            Ok(())
        })
        .unwrap()
    );
    handle.join().unwrap();
}
#[test]
fn edit_only_404_starts_empty_and_unchanged_content_is_not_uploaded() {
    for status in [404, 403] {
        let server = server();
        let client = client(&server);
        let handle = thread::spawn(move || {
            let r = recv(&server, "GET", "/runtime/base/api/contents/new.py");
            r.respond(Response::empty(StatusCode(status))).unwrap();
            assert!(
                server
                    .recv_timeout(Duration::from_millis(80))
                    .unwrap()
                    .is_none()
            );
        });
        let mut edited = false;
        let result = edit_with(&client, "new.py", |path| {
            edited = true;
            assert!(fs::read(path).unwrap().is_empty());
            Ok(())
        });
        if status == 404 {
            assert!(!result.unwrap());
            assert!(edited);
        } else {
            assert!(result.is_err());
            assert!(!edited);
        }
        handle.join().unwrap();
    }
}
#[test]
fn editing_failure_never_uploads_and_failed_upload_keeps_the_local_edit() {
    let result = edit_with_access(
        "file",
        || Ok(b"before".to_vec()),
        |_| panic!("must not upload"),
        |_| Err(Error::EditorFailed),
    );
    assert!(matches!(result, Err(Error::EditorFailed)));
    let result = edit_with_access(
        "file",
        || Ok(b"before".to_vec()),
        |_| Err(Error::RuntimeStatus(503)),
        |path| {
            fs::write(path, b"recover-me").unwrap();
            Ok(())
        },
    );
    let Err(Error::EditPending { path, source }) = result else {
        panic!("expected retained edit");
    };
    assert!(matches!(*source, Error::RuntimeStatus(503)));
    assert_eq!(fs::read(&path).unwrap(), b"recover-me");
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
#[test]
fn session_access_refreshes_proxy_before_file_request_and_preserves_account_isolation() {
    let f = Fixture::new(TokenStorageMode::File);
    let other = f.add("other");
    let runtime = server();
    let mut s = state(&runtime);
    s.token = Secret::new("old-proxy");
    s.token_expires_at = None;
    f.storage.save_session(&other, s.clone()).unwrap();
    f.storage
        .save_session(&id("default"), session("files"))
        .unwrap();
    let control = server();
    let api = ColabApi::new(
        ApiConfig {
            base_url: Url::parse(&format!("http://{}", control.server_addr())).unwrap(),
            ..ApiConfig::default()
        },
        Secret::new("control-oauth"),
    )
    .unwrap();
    let svc = SessionService::new(f.storage.clone(), other.clone(), api);
    let handle = thread::spawn(move || {
        let r = control
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(r.url().starts_with("/tun/m/assignments?"));
        assert!(
            r.headers()
                .iter()
                .any(|h| h.field.equiv("Authorization")
                    && h.value.as_str() == "Bearer control-oauth")
        );
        reply(
            r,
            json!({"assignments":[{"endpoint":s.endpoint,"accelerator":"NONE","variant":0,"machineShape":0,"runtimeProxyInfo":{
            "url":s.url,"token":"fixture-proxy","tokenExpiresInSeconds":3600}}]}),
        );
        let r = recv(&runtime, "GET", "/runtime/base/api/contents/file");
        reply(r, entry(json!("fresh"), "text"));
    });
    assert_eq!(
        svc.with_session(None, |s| ContentsClient::new(s, Duration::from_secs(2))?
            .download_bytes("file"))
            .unwrap(),
        b"fresh"
    );
    handle.join().unwrap();
    assert_eq!(
        f.storage.sessions(&other).unwrap()["files"].token.expose(),
        "fixture-proxy"
    );
    assert_eq!(
        f.storage.sessions(&id("default")).unwrap()["files"].url,
        "https://runtime.invalid"
    );
}
#[test]
fn missing_or_ambiguous_session_never_creates_a_vm() {
    let f = Fixture::new(TokenStorageMode::File);
    let control = server();
    let api = ColabApi::new(
        ApiConfig {
            base_url: Url::parse(&format!("http://{}", control.server_addr())).unwrap(),
            ..ApiConfig::default()
        },
        Secret::new("oauth"),
    )
    .unwrap();
    let svc = SessionService::new(f.storage.clone(), id("default"), api);
    assert!(matches!(
        svc.with_session(None, |_| Ok(())),
        Err(Error::SessionMissing(_))
    ));
    f.storage
        .save_session(&id("default"), session("one"))
        .unwrap();
    f.storage
        .save_session(&id("default"), session("two"))
        .unwrap();
    assert!(matches!(
        svc.with_session(None, |_| Ok(())),
        Err(Error::Ambiguous(_))
    ));
    assert!(
        control
            .recv_timeout(Duration::from_millis(30))
            .unwrap()
            .is_none()
    );
}
#[test]
fn cli_file_commands_use_session_owner_and_do_not_expose_credentials() {
    let f = Fixture::new(TokenStorageMode::File);
    let other = f.add("other");
    let runtime = server();
    f.storage.save_session(&other, state(&runtime)).unwrap();
    let mut credentials = token();
    credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&other, &credentials).unwrap();
    let local = f.temp.path().join("local.bin");
    let downloaded = f.temp.path().join("download.bin");
    fs::write(&local, b"file-data").unwrap();
    let handle = thread::spawn(move || {
        let r = recv(&runtime, "PUT", "/runtime/base/api/contents/file");
        reply(r, entry(json!(null), "base64"));
        let r = recv(&runtime, "GET", "/runtime/base/api/contents/content");
        reply(
            r,
            json!({"name":"content","path":"content","type":"directory","content":[
            {"name":"data","path":"content/data","type":"file"},{"name":"dir","path":"content/dir","type":"directory"}]}),
        );
        let r = recv(&runtime, "GET", "/runtime/base/api/contents/file");
        reply(r, entry(json!(STANDARD.encode(b"file-data")), "base64"));
        let r = recv(&runtime, "DELETE", "/runtime/base/api/contents/file");
        r.respond(Response::empty(StatusCode(204))).unwrap();
    });
    let invoke = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_colab"))
            .arg("--config-dir")
            .arg(f.temp.path())
            .args(["--token-storage", "file"])
            .env_remove("COLAB_ACCOUNT")
            .env_remove("COLAB_CLI_REQUEST_TIMEOUT")
            .env_remove("COLAB_CLI_ASSIGN_RETRIES")
            .env_remove("COLAB_CLI_RETRY_BACKOFF")
            .env_remove("COLAB_CLI_CONTROL_PLANE_URL")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture"));
        output.stdout
    };
    invoke(&["upload", local.to_str().unwrap(), "file", "-s", "files"]);
    assert_eq!(
        String::from_utf8(invoke(&["ls", "-s", "files"])).unwrap(),
        "dir/\ndata\n"
    );
    invoke(&[
        "download",
        "file",
        downloaded.to_str().unwrap(),
        "-s",
        "files",
    ]);
    assert_eq!(fs::read(downloaded).unwrap(), b"file-data");
    invoke(&["rm", "file", "-s", "files"]);
    handle.join().unwrap();
    assert_eq!(f.accounts.registry().unwrap().active, id("default"));
}
#[cfg(unix)]
#[test]
fn cli_editor_command_receives_an_exact_file_argument() {
    let f = Fixture::new(TokenStorageMode::File);
    let runtime = server();
    f.storage
        .save_session(&id("default"), state(&runtime))
        .unwrap();
    let mut credentials = token();
    credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &credentials).unwrap();
    let script = f.temp.path().join("editor with spaces.sh");
    fs::write(
        &script,
        "test -f \"$1\" || exit 12\nprintf 'edited' > \"$1\"\n",
    )
    .unwrap();
    let handle = thread::spawn(move || {
        let r = recv(&runtime, "GET", "/runtime/base/api/contents/code.py");
        reply(r, entry(json!("initial"), "text"));
        let mut r = recv(&runtime, "PUT", "/runtime/base/api/contents/code.py");
        let mut body = String::new();
        r.as_reader().read_to_string(&mut body).unwrap();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            STANDARD.decode(body["content"].as_str().unwrap()).unwrap(),
            b"edited"
        );
        reply(r, entry(json!(null), "text"));
    });
    let output = Command::new(env!("CARGO_BIN_EXE_colab"))
        .arg("--config-dir")
        .arg(f.temp.path())
        .args(["--token-storage", "file", "edit", "code.py", "-s", "files"])
        .env_remove("COLAB_ACCOUNT")
        .env_remove("VISUAL")
        .env("EDITOR", format!("sh '{}'", script.display()))
        .output()
        .unwrap();
    handle.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn cli_edit_refuses_replaced_vm_and_retains_the_edited_file() {
    let f = Fixture::new(TokenStorageMode::File);
    let runtime = server();
    let mut original = state(&runtime);
    f.storage
        .save_session(&id("default"), original.clone())
        .unwrap();
    let mut credentials = token();
    credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &credentials).unwrap();
    original.endpoint = "ep-replacement".into();
    let replacement = f.temp.path().join("replacement.json");
    fs::write(
        &replacement,
        serde_json::to_vec(&json!({"files":original})).unwrap(),
    )
    .unwrap();
    let sessions_file = f
        .storage
        .profile_path(&id("default"), "sessions.json")
        .unwrap();
    let script = f.temp.path().join("editor.sh");
    fs::write(
        &script,
        "printf 'valuable-edit' > \"$3\"\ncp \"$1\" \"$2\"\n",
    )
    .unwrap();
    let handler = thread::spawn(move || {
        let r = recv(&runtime, "GET", "/runtime/base/api/contents/code.py");
        reply(r, entry(json!("original"), "text"));
        assert!(
            runtime
                .recv_timeout(Duration::from_millis(200))
                .unwrap()
                .is_none(),
            "replacement VM must never receive the old edit"
        );
    });
    let output = Command::new(env!("CARGO_BIN_EXE_colab"))
        .arg("--config-dir")
        .arg(f.temp.path())
        .args(["--token-storage", "file", "edit", "code.py", "-s", "files"])
        .env_remove("COLAB_ACCOUNT")
        .env_remove("VISUAL")
        .env(
            "EDITOR",
            format!(
                "sh '{}' '{}' '{}'",
                script.display(),
                replacement.display(),
                sessions_file.display()
            ),
        )
        .output()
        .unwrap();
    handler.join().unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("编辑期间会话已被替换"));
    let path = std::path::PathBuf::from(
        error
            .split("已保留本地文件 ")
            .nth(1)
            .unwrap()
            .split(": ")
            .next()
            .unwrap(),
    );
    assert_eq!(fs::read(&path).unwrap(), b"valuable-edit");
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
