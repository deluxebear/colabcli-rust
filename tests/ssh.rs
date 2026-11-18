mod common;
use colabcli_rust::{
    Error,
    colab_api::{ApiConfig, ColabApi},
    credentials::TokenStorageMode,
    models::{Secret, SessionState},
    sessions::SessionService,
    ssh::{self, Mode},
};
use common::interrupted_stream::InterruptedTcpStream as TcpStream;
use common::*;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};
const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFixtureFixtureOnly fixture@example.invalid";
fn listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}
fn state(listener: &TcpListener) -> SessionState {
    let mut s = session("work");
    s.url = format!("http://{}/ignored/path", listener.local_addr().unwrap());
    s.token_expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    s
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let start = Instant::now();
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                return s.into();
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                assert!(start.elapsed() < Duration::from_secs(5), "no connection");
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("{e}"),
        }
    }
}
#[allow(clippy::result_large_err)]
fn websocket(listener: &TcpListener, mode: Mode, key: &str, token: &str) -> WebSocket<TcpStream> {
    tungstenite::accept_hdr(
        accept(listener),
        |request: &tungstenite::handshake::server::Request,
         response: tungstenite::handshake::server::Response| {
            assert_eq!(request.uri().path(), "/colab/ssh");
            let pairs: Vec<_> =
                url::form_urlencoded::parse(request.uri().query().unwrap().as_bytes())
                    .into_owned()
                    .collect();
            assert_eq!(
                pairs,
                [
                    ("colab-runtime-proxy-token".into(), token.into()),
                    ("colab-client-agent".into(), "colab-cli".into()),
                    (
                        "colab-client-agent".into(),
                        format!("ssh-mode-{}", mode.tag())
                    )
                ]
            );
            assert_eq!(request.headers()["x-colab-ssh-pubkey"], key);
            assert_eq!(request.headers()["x-colab-client-agent"], "colab-cli");
            assert!(!request.headers().contains_key("authorization"));
            assert!(
                !request
                    .headers()
                    .contains_key("x-colab-runtime-proxy-token")
            );
            assert!(!request.headers().contains_key("sec-websocket-protocol"));
            Ok(response)
        },
    )
    .unwrap()
}
#[test]
fn ssh_wire_has_fixed_path_duplicate_agents_and_only_selected_proxy_token() {
    for mode in [Mode::Proxy, Mode::Interactive, Mode::Command] {
        let listener = listener();
        let mut state = state(&listener);
        state.token = Secret::new("selected /?&#% 你好");
        let server = thread::spawn(move || {
            let mut socket = websocket(&listener, mode, KEY, "selected /?&#% 你好");
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
        });
        let mut socket = ssh::connect(&state, KEY, mode, Duration::from_secs(2)).unwrap();
        socket.close(None).unwrap();
        server.join().unwrap();
    }
}
#[test]
fn ssh_handshake_statuses_and_session_limits_are_typed_and_redacted() {
    for (status, body, part) in [
        (400, "missing pubkey credential-fixture", "公钥 header"),
        (400, "unsupported key type credential-fixture", "Ed25519"),
        (400, "credential-fixture", "公钥"),
        (401, "credential-fixture", "过期"),
        (403, "credential-fixture", "无权"),
        (404, "credential-fixture", "/colab/ssh"),
        (
            429,
            "{\"error\":\"already-active-session\",\"limit\":3}",
            "3/3",
        ),
        (502, "credential-fixture", "sshd"),
        (302, "credential-fixture", "拒绝"),
    ] {
        let listener = listener();
        let s = state(&listener);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut b = [0];
                stream.read_exact(&mut b).unwrap();
                head.push(b[0]);
            }
            let response = format!(
                "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\nLocation: https://credential-fixture.invalid\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let error = ssh::connect(&s, KEY, Mode::Proxy, Duration::from_secs(2))
            .err()
            .unwrap();
        assert!(matches!(error,Error::SshHandshake{status:actual,..} if actual==status));
        let text = error.to_string();
        assert!(text.contains(part), "{text}");
        assert!(!text.contains("credential-fixture"));
        server.join().unwrap();
    }
    for limit in [
        json!(true),
        json!("3"),
        json!(0),
        json!(6),
        json!(-1),
        json!(3.1),
    ] {
        let error = ssh::handshake_error(
            429,
            json!({"error":"already-active-session","limit":limit})
                .to_string()
                .as_bytes(),
        )
        .to_string();
        assert!(!error.contains('/'));
    }
    assert!(!ssh::handshake_error(429, b"[]").to_string().contains('/'));
}
struct NoInput;
impl Read for NoInput {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::ErrorKind::WouldBlock.into())
    }
}
#[cfg(unix)]
#[test]
fn ssh_bridge_preserves_binary_and_text_bytes_and_filters_control_frames_with_backpressure() {
    use std::os::unix::net::UnixStream;
    struct SlowWriter {
        bytes: Vec<u8>,
        blocked: usize,
    }
    impl Write for SlowWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.blocked > 0 {
                self.blocked -= 1;
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = bytes.len().min(127);
            self.bytes.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let listener = listener();
    let s = state(&listener);
    let payload: Vec<_> = (0..32768).map(|n| (n % 256) as u8).collect();
    let expected = payload.clone();
    let server = thread::spawn(move || {
        let mut socket = websocket(&listener, Mode::Proxy, KEY, "fixture-proxy");
        let mut got = Vec::new();
        while got.len() < expected.len() {
            match socket.read().unwrap() {
                Message::Binary(bytes) => got.extend_from_slice(&bytes),
                m => panic!("{m:?}"),
            }
        }
        assert_eq!(got, expected);
        socket.send(Message::Binary(got.into())).unwrap();
        socket.send(Message::Text("中文\r\n".into())).unwrap();
        socket
            .send(Message::Ping(b"must-not-print".to_vec().into()))
            .unwrap();
        assert!(
            matches!(socket.read().unwrap(),Message::Pong(bytes) if bytes==b"must-not-print"[..])
        );
        socket.close(None).unwrap();
    });
    let socket = ssh::connect(&s, KEY, Mode::Proxy, Duration::from_secs(2)).unwrap();
    let (mut input, mut writer) = UnixStream::pair().unwrap();
    input.set_nonblocking(true).unwrap();
    let feeder = thread::spawn(move || {
        writer.write_all(&payload).unwrap();
        writer
    });
    let mut output = SlowWriter {
        bytes: Vec::new(),
        blocked: 10,
    };
    assert_eq!(
        ssh::bridge(socket, &mut input, &mut output, &AtomicUsize::new(0)).unwrap(),
        0
    );
    let _writer = feeder.join().unwrap();
    server.join().unwrap();
    let mut expected: Vec<_> = (0..32768).map(|n| (n % 256) as u8).collect();
    expected.extend_from_slice("中文\r\n".as_bytes());
    assert_eq!(output.bytes, expected);
}
#[test]
fn ssh_bridge_stdin_eof_closes_ws_and_does_not_wait_for_remote_half_close() {
    let listener = listener();
    let s = state(&listener);
    let server = thread::spawn(move || {
        let mut socket = websocket(&listener, Mode::Proxy, KEY, "fixture-proxy");
        assert!(matches!(socket.read().unwrap(), Message::Close(_)));
    });
    let socket = ssh::connect(&s, KEY, Mode::Proxy, Duration::from_secs(2)).unwrap();
    assert_eq!(
        ssh::bridge(
            socket,
            &mut io::empty(),
            &mut Vec::new(),
            &AtomicUsize::new(0)
        )
        .unwrap(),
        0
    );
    server.join().unwrap();
}
#[test]
fn ssh_bridge_remote_loss_and_broken_pipe_are_errors_not_silent_success() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    for mode in ["lost", "broken"] {
        let listener = listener();
        let s = state(&listener);
        let server = thread::spawn(move || {
            let mut socket = websocket(&listener, Mode::Proxy, KEY, "fixture-proxy");
            if mode == "broken" {
                socket
                    .send(Message::Binary(vec![0, 255, 10].into()))
                    .unwrap();
                let _ = socket.read();
            }
        });
        let socket = ssh::connect(&s, KEY, Mode::Proxy, Duration::from_secs(2)).unwrap();
        assert!(ssh::bridge(socket, &mut NoInput, &mut Broken, &AtomicUsize::new(0)).is_err());
        server.join().unwrap();
    }
}
#[test]
fn ssh_bridge_output_stall_does_not_prevent_cancellation() {
    struct Stalled;
    impl Write for Stalled {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let listener = listener();
    let s = state(&listener);
    let flag = Arc::new(AtomicUsize::new(0));
    let server_flag = flag.clone();
    let server = thread::spawn(move || {
        let mut socket = websocket(&listener, Mode::Proxy, KEY, "fixture-proxy");
        socket
            .send(Message::Binary(vec![0; 1024 * 1024].into()))
            .unwrap();
        thread::sleep(Duration::from_millis(75));
        server_flag.store(signal_hook::consts::SIGTERM as usize, Ordering::SeqCst);
        assert!(matches!(socket.read().unwrap(), Message::Close(_)));
    });
    let socket = ssh::connect(&s, KEY, Mode::Proxy, Duration::from_secs(2)).unwrap();
    let start = Instant::now();
    assert_eq!(
        ssh::bridge(socket, &mut NoInput, &mut Stalled, &flag).unwrap(),
        143
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
}
#[test]
fn public_keys_follow_preference_identity_and_input_limits() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    fs::create_dir(home.join(".ssh")).unwrap();
    let flag = AtomicUsize::new(0);
    fs::write(home.join(".ssh/id_ecdsa.pub"), KEY).unwrap();
    assert_eq!(
        ssh::resolve_pubkey(None, Some(home), Duration::from_secs(2), &flag).unwrap(),
        KEY
    );
    let ed = format!("{KEY} ed25519-preferred");
    fs::write(home.join(".ssh/id_ed25519.pub"), &ed).unwrap();
    assert_eq!(
        ssh::resolve_pubkey(None, Some(home), Duration::from_secs(2), &flag).unwrap(),
        ed
    );
    let private = home.join("identity.key");
    let public = private.with_extension("pub");
    fs::write(&public, KEY).unwrap();
    assert_eq!(
        ssh::resolve_pubkey(Some(&private), Some(home), Duration::from_secs(2), &flag).unwrap(),
        KEY
    );
    assert_eq!(
        ssh::resolve_pubkey(Some(&public), Some(home), Duration::from_secs(2), &flag).unwrap(),
        KEY
    );
    assert_eq!(
        ssh::expand_identity(Path::new("~/.ssh/a key"), Some(home)).unwrap(),
        home.join(".ssh/a key")
    );
    fs::write(&public, format!("{KEY}\nInjected: header")).unwrap();
    assert!(ssh::resolve_pubkey(Some(&public), Some(home), Duration::from_secs(2), &flag).is_err());
    fs::write(&public, vec![b'x'; 65537]).unwrap();
    assert!(ssh::resolve_pubkey(Some(&public), Some(home), Duration::from_secs(2), &flag).is_err());
    assert!(ssh::resolve_pubkey(None, None, Duration::from_secs(2), &flag).is_err());
}
#[test]
fn private_identity_is_derived_by_real_keygen_without_storing_private_key() {
    let temp = tempfile::tempdir().unwrap();
    let private = temp.path().join("identity with spaces");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&private)
            .status()
            .unwrap()
            .success()
    );
    let public = private.with_extension("pub");
    let expected = fs::read_to_string(&public).unwrap();
    fs::remove_file(&public).unwrap();
    let key = ssh::resolve_pubkey(
        Some(&private),
        Some(temp.path()),
        Duration::from_secs(5),
        &AtomicUsize::new(0),
    )
    .unwrap();
    assert_eq!(key, expected.trim());
    assert!(!public.exists());
}
#[test]
fn openssh_pty_and_remote_shell_arguments_preserve_exact_argv() {
    for (command, tty, flag) in [
        (vec![], false, "-t"),
        (vec!["printf hi".to_owned()], false, "-T"),
        (vec!["printf hi".to_owned()], true, "-t"),
    ] {
        let args = ssh::openssh_args("fixture", None, None, &command, tty).unwrap();
        assert_eq!(args[8], flag);
        assert_eq!(args[9], "root@colab-runtime");
        assert!(
            args[10]
                .to_str()
                .unwrap()
                .starts_with("cd /content 2>/dev/null; ")
        );
    }
    let words = vec![
        "printf".into(),
        "<%s>\\n".into(),
        "space arg".into(),
        "quote'\"$HOME".into(),
        "中文\nline".into(),
    ];
    let args = ssh::openssh_args("fixture", None, None, &words, false).unwrap();
    let output = Command::new("sh")
        .args(["-c", args.last().unwrap().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "<space arg>\n<quote'\"$HOME>\n<中文\nline>\n"
    );
    let proxy = ssh::proxy_command(&[
        "/path with space/colab".into(),
        "config %h".into(),
        "quote'arg".into(),
    ])
    .unwrap();
    assert!(proxy.contains("%%h"));
    assert!(proxy.contains("'\"'\"'"));
}
fn install_token(f: &Fixture) {
    let mut token = token();
    token.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &token).unwrap();
}
fn cli(f: &Fixture, base: &str) -> Command {
    cli_with_timeout(f, base, "2")
}
fn cli_with_timeout(f: &Fixture, base: &str, timeout: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_colab"));
    c.arg("--config-dir")
        .arg(f.storage.root())
        .args([
            "--account",
            "default",
            "--token-storage",
            "file",
            "--control-plane-url",
            base,
            "--request-timeout",
            timeout,
            "--assign-retries",
            "1",
            "--retry-backoff",
            "0",
        ])
        .env_remove("COLAB_ACCOUNT")
        .env_remove("COLAB_CLI_REQUEST_TIMEOUT")
        .env_remove("COLAB_CLI_ASSIGN_RETRIES")
        .env_remove("COLAB_CLI_RETRY_BACKOFF");
    c
}
#[cfg(unix)]
fn executable(path: &Path, source: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
#[cfg(unix)]
#[test]
fn cli_command_ssh_preserves_child_exit_argv_account_and_ignores_rm_without_allocating() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    f.storage
        .save_session(&id("default"), state(&listener))
        .unwrap();
    install_token(&f);
    let bin = f.temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let args = f.temp.path().join("ssh-args");
    executable(
        &bin.join("ssh"),
        "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$FIXTURE_SSH_ARGS\"\nexit 37\n",
    );
    let output = cli(&f, "http://127.0.0.1:1")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("FIXTURE_SSH_ARGS", &args)
        .args([
            "ssh",
            "-s",
            "work",
            "--rm",
            "--gpu",
            "invalid-ignored",
            "--",
            "printf",
            "a b",
            "--flag",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--rm ignored"));
    assert!(stderr.contains("--gpu/--tpu/--high-mem ignored"));
    let args = fs::read(&args).unwrap();
    let words: Vec<_> = args
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(|b| String::from_utf8(b.to_vec()).unwrap())
        .collect();
    assert_eq!(words[8], "-T");
    assert_eq!(
        words.last().unwrap(),
        "cd /content 2>/dev/null; printf 'a b' --flag"
    );
    assert!(words[1].contains("--account default --auth oauth2 --token-storage file"));
    assert!(words[1].contains("--colab-internal-mode cmd"));
    assert!(
        f.storage
            .sessions(&id("default"))
            .unwrap()
            .contains_key("work")
    );
    listener.set_nonblocking(true).unwrap();
    assert!(listener.accept().is_err());
}
#[cfg(unix)]
#[test]
fn cli_proxy_streams_only_bytes_and_does_not_mix_selection_notices_into_stdout() {
    let f = Fixture::new(TokenStorageMode::File);
    f.add("other");
    f.accounts.use_account("other").unwrap();
    let listener = listener();
    f.storage
        .save_session(&id("default"), state(&listener))
        .unwrap();
    install_token(&f);
    let key = f.temp.path().join("key.pub");
    fs::write(&key, KEY).unwrap();
    let server = thread::spawn(move || {
        let mut socket = websocket(&listener, Mode::Proxy, KEY, "fixture-proxy");
        let request = socket.read().unwrap();
        assert!(matches!(request,Message::Binary(bytes) if bytes==[0,255,128,10][..]));
        socket
            .send(Message::Binary(vec![255, 0, 1, 128, 10].into()))
            .unwrap();
        socket.close(None).unwrap();
    });
    let mut child = cli(&f, "http://127.0.0.1:1")
        .args(["--account", "", "ssh", "--proxy-mode", "-s", "work", "-i"])
        .arg(&key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(&[0, 255, 128, 10]).unwrap();
    let output = child.wait_with_output().unwrap();
    drop(input);
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, [255, 0, 1, 128, 10]);
    assert!(output.stderr.is_empty());
}
#[test]
fn cli_ssh_proxy_command_conflict_is_rejected_before_authentication() {
    let f = Fixture::new(TokenStorageMode::File);
    let output = cli(&f, "http://127.0.0.1:1")
        .args(["ssh", "--proxy-mode", "--", "echo", "hi"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("takes no command"));
}
#[test]
fn cleanup_uses_endpoint_and_account_guards_and_never_releases_a_replacement() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let original = state(&listener);
    f.storage
        .save_session(&id("default"), original.clone())
        .unwrap();
    let other = f.add("other");
    let mut other_state = original.clone();
    other_state.endpoint = "ep-other".into();
    f.storage.save_session(&other, other_state.clone()).unwrap();
    let service = SessionService::new(
        f.storage.clone(),
        id("default"),
        ColabApi::new(ApiConfig::default(), Secret::new("never-network")).unwrap(),
    );
    let result = ssh::with_cleanup(&service, &original, true, || {
        let mut replaced = original.clone();
        replaced.endpoint = "ep-replaced".into();
        f.storage.save_session(&id("default"), replaced).unwrap();
        Err(Error::Ssh("action fixture"))
    });
    assert!(result.action.is_err());
    assert!(matches!(result.cleanup, Some(Error::KernelProtocol(_))));
    assert_eq!(
        f.storage.sessions(&id("default")).unwrap()["work"].endpoint,
        "ep-replaced"
    );
    assert_eq!(
        f.storage.sessions(&other).unwrap()["work"].endpoint,
        other_state.endpoint
    );
}

fn control_reply(request: tiny_http::Request, value: Value, status: u16) {
    request
        .respond(tiny_http::Response::from_string(value.to_string()).with_status_code(status))
        .unwrap();
}
fn control_recv(server: &tiny_http::Server, method: &str, path: &str) -> tiny_http::Request {
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("missing control request");
    assert_eq!(request.method().as_str(), method);
    assert_eq!(
        url::Url::parse(&format!("http://localhost{}", request.url()))
            .unwrap()
            .path(),
        path
    );
    request
}
#[test]
fn cli_rm_covers_pubkey_failure_after_auto_allocation_and_leaves_release_recovery_on_failure() {
    for release_status in [204, 503] {
        let f = Fixture::new(TokenStorageMode::File);
        install_token(&f);
        let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", control.server_addr());
        let server = thread::spawn(move || {
            let get = control_recv(&control, "GET", "/tun/m/assign");
            let url = url::Url::parse(&format!("http://localhost{}", get.url())).unwrap();
            let nbh = url
                .query_pairs()
                .find(|(k, _)| k == "nbh")
                .unwrap()
                .1
                .into_owned();
            control_reply(
                get,
                json!({"acc":"","nbh":nbh,"variant":"DEFAULT","token":"fixture-xsrf"}),
                200,
            );
            let post = control_recv(&control, "POST", "/tun/m/assign");
            control_reply(
                post,
                json!({"endpoint":"ep-owned","accelerator":"NONE","variant":0,"machineShape":0,"runtimeProxyInfo":{"token":"fixture-proxy","tokenExpiresInSeconds":3600,"url":"http://127.0.0.1:1"}}),
                200,
            );
            let get = control_recv(&control, "GET", "/tun/m/unassign/ep-owned");
            control_reply(get, json!({"token":"fixture-xsrf"}), 200);
            let post = control_recv(&control, "POST", "/tun/m/unassign/ep-owned");
            control_reply(post, Value::Null, release_status);
        });
        let output = cli(&f, &base)
            .args(["ssh", "--proxy-mode", "-s", "created", "--rm", "-i"])
            .arg(f.temp.path().join("no-key"))
            .output()
            .unwrap();
        server.join().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let sessions = f.storage.sessions(&id("default")).unwrap();
        let journal = f
            .storage
            .profile_path(&id("default"), "control_operation.json")
            .unwrap();
        if release_status == 204 {
            assert!(sessions.is_empty());
            assert!(!journal.exists());
        } else {
            assert!(sessions.contains_key("created"));
            let pending: Value = serde_json::from_slice(&fs::read(journal).unwrap()).unwrap();
            assert_eq!(pending["operation"], "release");
            assert_eq!(pending["endpoint"], "ep-owned");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("清理待恢复")
                    || String::from_utf8_lossy(&output.stderr).contains("failed to stop")
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn ssh_socket_backpressure_never_duplicates_or_drops_stdin_frames() {
    struct Payload {
        bytes: Vec<u8>,
        at: usize,
    }
    impl Read for Payload {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.at == self.bytes.len() {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = buffer.len().min(self.bytes.len() - self.at);
            buffer[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }
    let listener = listener();
    let s = state(&listener);
    let expected: Vec<u8> = (0..512 * 1024).map(|n| (n % 251) as u8).collect();
    let bytes = expected.clone();
    let server = thread::spawn(move || {
        let mut socket = websocket(&listener, Mode::Proxy, KEY, "fixture-proxy");
        thread::sleep(Duration::from_millis(150));
        let mut got = Vec::new();
        while got.len() < expected.len() {
            match socket.read().unwrap() {
                Message::Binary(bytes) => got.extend_from_slice(&bytes),
                m => panic!("{m:?}"),
            }
        }
        assert_eq!(got, expected);
        socket.close(None).unwrap();
    });
    let mut socket = ssh::connect(&s, KEY, Mode::Proxy, Duration::from_secs(2)).unwrap();
    use std::os::fd::AsRawFd;
    let fd = match socket.get_mut() {
        tungstenite::stream::MaybeTlsStream::Plain(s) => s.tcp().as_raw_fd(),
        _ => unreachable!(),
    };
    let size: libc::c_int = 4096;
    // SAFETY: valid socket descriptor, input buffer and length for SO_SNDBUF.
    assert_eq!(
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            )
        },
        0
    );
    let mut input = Payload { bytes, at: 0 };
    assert_eq!(
        ssh::bridge(socket, &mut input, &mut Vec::new(), &AtomicUsize::new(0)).unwrap(),
        0
    );
    assert_eq!(input.at, 512 * 1024);
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn real_openssh_proxycommand_handles_executable_config_and_identity_paths_with_spaces_and_percent()
{
    let mut f = Fixture::new(TokenStorageMode::File);
    f.storage =
        colabcli_rust::storage::Storage::new(f.temp.path().join("config %h with 空格")).unwrap();
    f.tokens = colabcli_rust::credentials::TokenStore::new(
        f.storage.clone(),
        f.backend.clone(),
        TokenStorageMode::File,
    );
    f.accounts = colabcli_rust::accounts::Accounts::new(f.storage.clone(), f.tokens.clone());
    f.accounts.bootstrap().unwrap();
    let listener = listener();
    f.storage
        .save_session(&id("default"), state(&listener))
        .unwrap();
    install_token(&f);
    let key = f.temp.path().join("identity %h key.pub");
    fs::write(&key, KEY).unwrap();
    let bin = f.temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    // Use the native SSH client, with config isolated from the developer's files.
    executable(
        &bin.join("ssh"),
        "#!/bin/sh\nexec /usr/bin/ssh -F /dev/null \"$@\"\n",
    );
    let binary_dir = tempfile::Builder::new()
        .prefix("ssh executable %h with space ")
        .tempdir_in(Path::new(env!("CARGO_BIN_EXE_colab")).parent().unwrap())
        .unwrap();
    let binary = binary_dir.path().join("colab");
    fs::hard_link(env!("CARGO_BIN_EXE_colab"), &binary).unwrap();
    let server = thread::spawn(move || {
        let mut socket = websocket(&listener, Mode::Command, KEY, "fixture-proxy");
        let first = socket.read().unwrap();
        assert!(matches!(first,Message::Binary(bytes) if bytes.starts_with(b"SSH-2.0-OpenSSH")));
        socket
            .send(Message::Binary(b"SSH-2.0-fixture\r\n".to_vec().into()))
            .unwrap();
        socket.close(None).unwrap();
    });
    let output = Command::new(&binary)
        .arg("--config-dir")
        .arg(f.storage.root())
        .args([
            "--account",
            "default",
            "--token-storage",
            "file",
            "--control-plane-url",
            "http://127.0.0.1:1",
            "--request-timeout",
            "2",
            "ssh",
            "-s",
            "work",
            "-i",
        ])
        .arg(&key)
        .args(["--", "printf", "fixture"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("COLAB_ACCOUNT", "does-not-exist")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    server.join().unwrap();
    assert_eq!(
        output.status.code(),
        Some(255),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-proxy"));
}

#[cfg(unix)]
#[test]
fn cli_proxy_rm_on_hup_term_and_int_is_clean_and_releases_only_the_selected_account() {
    for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        let s = state(&listener);
        f.storage.save_session(&id("default"), s).unwrap();
        install_token(&f);
        let other = f.add("other");
        let mut untouched = session("work");
        untouched.endpoint = "ep-untouched".into();
        f.storage.save_session(&other, untouched.clone()).unwrap();
        let key = f.temp.path().join("key.pub");
        fs::write(&key, KEY).unwrap();
        let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", control.server_addr());
        let (ready, wait) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = websocket(&listener, Mode::Proxy, KEY, "fixture-proxy");
            ready.send(()).unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
            let get = control_recv(&control, "GET", "/tun/m/unassign/ep-work");
            control_reply(get, json!({"token":"fixture-xsrf"}), 200);
            let post = control_recv(&control, "POST", "/tun/m/unassign/ep-work");
            control_reply(post, Value::Null, 204);
        });
        let mut child = cli(&f, &base)
            .args(["ssh", "--proxy-mode", "--rm", "-s", "work", "-i"])
            .arg(key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        // SAFETY: this is the owned live child; no unrelated process is targeted.
        assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("proxy did not stop on signal");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        server.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(f.storage.sessions(&id("default")).unwrap().is_empty());
        assert_eq!(
            f.storage.sessions(&other).unwrap()["work"].endpoint,
            untouched.endpoint
        );
    }
}

#[cfg(unix)]
#[test]
fn cli_command_pty_requires_both_stdin_and_stdout_to_be_terminals() {
    use std::os::fd::FromRawFd;
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    f.storage
        .save_session(&id("default"), state(&listener))
        .unwrap();
    install_token(&f);
    let bin = f.temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let args = f.temp.path().join("args");
    executable(
        &bin.join("ssh"),
        "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$FIXTURE_SSH_ARGUMENTS\"\nexit 0\n",
    );
    let (mut master_fd, mut slave_fd) = (-1, -1);
    // SAFETY: valid output pointers, optional PTY settings are null.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: openpty returned distinct owned descriptors.
    let _master = unsafe { fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { fs::File::from_raw_fd(slave_fd) };
    for (input, output) in [(false, false), (false, true), (true, false), (true, true)] {
        let status = cli(&f, "http://127.0.0.1:1")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("FIXTURE_SSH_ARGUMENTS", &args)
            .args(["ssh", "-s", "work", "--", "true"])
            .stdin(if input {
                Stdio::from(slave.try_clone().unwrap())
            } else {
                Stdio::null()
            })
            .stdout(if output {
                Stdio::from(slave.try_clone().unwrap())
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped())
            .status()
            .unwrap();
        assert!(status.success());
        let args = fs::read(&args).unwrap();
        let words: Vec<_> = args.split(|b| *b == 0).collect();
        assert_eq!(words[8], if input && output { b"-t" } else { b"-T" });
    }
}

#[cfg(unix)]
fn wait_for_child_marker(
    mut child: std::process::Child,
    marker: &std::path::Path,
) -> std::process::Child {
    // Readiness must precede the fixture request deadline, including under build load.
    let deadline = Instant::now() + Duration::from_secs(10);
    // Shell redirection creates an empty file before printf writes the PID.
    // Signalling at file creation can kill the fixture before it becomes ready.
    while !fs::read_to_string(marker)
        .ok()
        .is_some_and(|value| value.parse::<u32>().is_ok_and(|pid| pid > 0))
    {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            panic!(
                "child exited before marker: {:?}; {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "child marker deadline exceeded: {:?}; {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    child
}

#[cfg(unix)]
#[test]
fn cli_rm_cancels_and_reaps_keygen_before_releasing_on_term() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    f.storage
        .save_session(&id("default"), state(&listener))
        .unwrap();
    install_token(&f);
    let private = f.temp.path().join("private fixture");
    fs::write(&private, "synthetic fixture, never parsed").unwrap();
    let bin = f.temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let marker = f.temp.path().join("keygen.pid");
    executable(
        &bin.join("ssh-keygen"),
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$FIXTURE_KEYGEN_PID\"\nexec /bin/sleep 20\n",
    );
    let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", control.server_addr());
    let server = thread::spawn(move || {
        let get = control_recv(&control, "GET", "/tun/m/unassign/ep-work");
        control_reply(get, json!({"token":"fixture-xsrf"}), 200);
        let post = control_recv(&control, "POST", "/tun/m/unassign/ep-work");
        control_reply(post, Value::Null, 204);
    });
    let child = cli_with_timeout(&f, &base, "15")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("FIXTURE_KEYGEN_PID", &marker)
        .args(["ssh", "--proxy-mode", "--rm", "-s", "work", "-i"])
        .arg(private)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = wait_for_child_marker(child, &marker);
    // SAFETY: signal only this owned live child.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("keygen cancellation did not finish");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(f.storage.sessions(&id("default")).unwrap().is_empty());
    let pid = fs::read_to_string(marker).unwrap().parse::<i32>().unwrap();
    // SAFETY: signal 0 checks existence; it sends no terminating signal.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[cfg(unix)]
#[test]
fn cli_command_term_forwards_to_and_reaps_openssh() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    f.storage
        .save_session(&id("default"), state(&listener))
        .unwrap();
    install_token(&f);
    let bin = f.temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let marker = f.temp.path().join("ssh.pid");
    executable(
        &bin.join("ssh"),
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$FIXTURE_SSH_PID\"\nexec /bin/sleep 20\n",
    );
    let child = cli_with_timeout(&f, "http://127.0.0.1:1", "15")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("FIXTURE_SSH_PID", &marker)
        .args(["ssh", "-s", "work", "--", "sleep 20"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = wait_for_child_marker(child, &marker);
    // SAFETY: this is the owned live child, not an external process.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("OpenSSH child cancellation did not finish");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(143));
    assert!(
        f.storage
            .sessions(&id("default"))
            .unwrap()
            .contains_key("work")
    );
    let pid = fs::read_to_string(marker).unwrap().parse::<i32>().unwrap();
    // SAFETY: existence check only; no terminating signal is sent.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn ssh_proxy_child_rejects_replaced_or_missing_endpoint_without_auto_creation() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    f.storage
        .save_session(&id("default"), state(&listener))
        .unwrap();
    install_token(&f);
    for (session, code) in [("work", 1), ("missing", 2)] {
        for mode in ["proxy", "command", "interactive"] {
            let mut command = cli(&f, "http://127.0.0.1:1");
            command.args([
                "ssh",
                "-s",
                session,
                "--colab-expected-endpoint",
                "ep-original",
            ]);
            if mode == "proxy" {
                command.args(["--proxy-mode", "--colab-internal-mode", "interactive"]);
            } else if mode == "command" {
                command.args(["--", "printf", "must-not-run"]);
            }
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(code));
            assert!(output.stdout.is_empty());
            assert_eq!(
                f.storage.sessions(&id("default")).unwrap()["work"].endpoint,
                "ep-work"
            );
            assert!(
                !f.storage
                    .profile_path(&id("default"), "control_operation.json")
                    .unwrap()
                    .exists()
            );
        }
    }
    listener.set_nonblocking(true).unwrap();
    assert!(listener.accept().is_err());
}
