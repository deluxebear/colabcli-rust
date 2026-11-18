mod common;
use colabcli_rust::{
    Error,
    jupyter::{
        ExecutionTimeout, KernelRest, KernelRuntime,
        codec::{self, Frame, KernelMessage},
    },
    ssh::{self, Mode},
    websocket_transport::{self as transport, ProxyEnvironment},
};
use common::interrupted_stream::InterruptedTcpStream as TcpStream;
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};
use tungstenite::Message;
use url::Url;

fn policy(values: &[(&str, &str)]) -> ProxyEnvironment {
    ProxyEnvironment::from_values(|name| {
        values
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    })
}
fn listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}
fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream.into();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
fn header(stream: &mut TcpStream) -> String {
    let mut data = Vec::new();
    while !data.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        data.push(byte[0]);
        assert!(data.len() < 16384);
    }
    String::from_utf8(data).unwrap()
}
fn proxy_url(listener: &TcpListener) -> String {
    format!("http://{}", listener.local_addr().unwrap())
}
fn send(
    socket: &mut tungstenite::WebSocket<TcpStream>,
    kind: &str,
    parent: &str,
    content: serde_json::Value,
) {
    let mut message = KernelMessage::new(
        kind,
        if kind == "execute_reply" {
            "shell"
        } else {
            "iopub"
        },
        "server",
        content,
    );
    message.envelope["parent_header"] = json!({"msg_id":parent});
    match codec::encode(&message).unwrap() {
        Frame::Text(text) => socket.send(Message::Text(text.into())).unwrap(),
        Frame::Binary(bytes) => socket.send(Message::Binary(bytes.into())).unwrap(),
    }
}
fn finish(socket: &mut tungstenite::WebSocket<TcpStream>, id: &str) {
    send(
        socket,
        "execute_reply",
        id,
        json!({"status":"ok","execution_count":1}),
    );
    send(socket, "status", id, json!({"execution_state":"idle"}));
}
fn read_message(socket: &mut tungstenite::WebSocket<TcpStream>) -> KernelMessage {
    match socket.read().unwrap() {
        Message::Text(text) => codec::decode_text(&text).unwrap(),
        other => panic!("{other:?}"),
    }
}
#[test]
fn environment_priority_and_no_proxy_use_the_locked_http_matching_rules() {
    let proxy = "http://127.0.0.1:1";
    for (no, host, bypass) in [
        ("example.com", "www.example.com", true),
        (".example.com", "example.com", true),
        ("example.com", "notexample.com", false),
        ("10.0.0.0/8", "10.2.3.4", true),
        ("10.0.0.0/8", "11.2.3.4", false),
        ("::1", "[::1]", true),
        ("2001:db8::/32", "[2001:db8::1]", true),
        ("*", "else.invalid", true),
    ] {
        assert_eq!(
            policy(&[("HTTP_PROXY", proxy), ("NO_PROXY", no)])
                .uses_proxy(&Url::parse(&format!("ws://{host}:80/path")).unwrap())
                .unwrap(),
            !bypass,
            "{no} {host}"
        );
    }
    assert!(
        policy(&[("http_proxy", proxy)])
            .uses_proxy(&Url::parse("ws://host.invalid/").unwrap())
            .unwrap()
    );
    assert!(
        !policy(&[("HTTP_PROXY", proxy)])
            .uses_proxy(&Url::parse("wss://host.invalid/").unwrap())
            .unwrap()
    );
    assert!(
        policy(&[
            ("HTTP_PROXY", proxy),
            ("http_proxy", "socks4://bad.invalid")
        ])
        .uses_proxy(&Url::parse("ws://host.invalid/").unwrap())
        .unwrap()
    );
    assert!(
        policy(&[
            ("HTTPS_PROXY", "socks4://private:secret@bad.invalid"),
            ("ALL_PROXY", proxy)
        ])
        .uses_proxy(&Url::parse("wss://host.invalid/").unwrap())
        .is_err()
    );
    assert!(
        policy(&[("HTTP_PROXY", proxy), ("NO_PROXY", ""), ("no_proxy", "*")])
            .uses_proxy(&Url::parse("ws://host.invalid/").unwrap())
            .unwrap()
    );
    assert!(
        !policy(&[
            ("HTTP_PROXY", proxy),
            ("HTTPS_PROXY", proxy),
            ("REQUEST_METHOD", "GET")
        ])
        .uses_proxy(&Url::parse("wss://host.invalid/").unwrap())
        .unwrap()
    );
    for value in [
        "ftp://private:secret@proxy.invalid/",
        "socks4://proxy.invalid/",
        "http://proxy.invalid/path?private=secret",
    ] {
        let error = policy(&[("ALL_PROXY", value)])
            .uses_proxy(&Url::parse("wss://host.invalid/").unwrap())
            .unwrap_err()
            .to_string();
        assert!(!error.contains("private"));
        assert!(!error.contains("secret"));
    }
}
#[test]
fn connect_decodes_proxy_credentials_only_for_connect_and_does_not_read_tunnel_bytes() {
    let listener = listener();
    let proxy = proxy_url(&listener).replace("http://", "http://u%40ser:p%3Ass@");
    let handler = thread::spawn(move || {
        let mut stream = accept(&listener);
        let request = header(&mut stream);
        assert!(request.starts_with("CONNECT [::1]:1234 HTTP/1.1\r\nHost: [::1]:1234\r\n"));
        assert!(request.contains("Proxy-Authorization: Basic dUBzZXI6cDpzcw==\r\n"));
        assert!(!request.contains("runtime-secret"));
        assert!(!request.contains("endpoint-path"));
        stream
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.0 204 Tunnel\r\n\r\nX")
            .unwrap();
        let mut body = [0; 7];
        stream.read_exact(&mut body).unwrap();
        assert_eq!(&body, b"payload");
    });
    let uri = Url::parse("ws://[::1]:1234/endpoint-path?token=runtime-secret").unwrap();
    let mut stream = transport::connect(
        &uri,
        Instant::now() + Duration::from_secs(2),
        &policy(&[("ALL_PROXY", &proxy)]),
    )
    .unwrap();
    let mut byte = [0];
    stream.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"X");
    stream.write_all(b"payload").unwrap();
    handler.join().unwrap();
}
// tungstenite fixes the callback error type; it cannot be boxed by this caller.
#[allow(clippy::result_large_err)]
#[test]
fn jupyter_channels_execute_through_connect_without_forwarding_proxy_authorization() {
    for socks in [false, true] {
        let listener = listener();
        let proxy = if socks {
            format!("socks5h://user:pass@{}", listener.local_addr().unwrap())
        } else {
            proxy_url(&listener).replace("http://", "http://user:pass@")
        };
        let handler = thread::spawn(move || {
            let mut stream = accept(&listener);
            if socks {
                assert_eq!(
                    socks_request(&mut stream, Some((b"user", b"pass"))),
                    [1, 127, 0, 0, 1, 0, 1]
                );
                stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
            } else {
                let request = header(&mut stream);
                assert!(request.starts_with("CONNECT 127.0.0.1:1 HTTP/1.1"));
                assert!(!request.contains("fixture-proxy"));
                stream
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .unwrap();
            }
            let mut socket = tungstenite::accept_hdr(
                stream,
                |request: &tungstenite::handshake::server::Request,
                 response: tungstenite::handshake::server::Response| {
                    assert_eq!(
                        request.uri().path(),
                        "/prefix/api/kernels/kernel-1/channels"
                    );
                    assert_eq!(request.headers()["Authorization"], "Bearer fixture-proxy");
                    assert!(!request.headers().contains_key("Proxy-Authorization"));
                    Ok(response)
                },
            )
            .unwrap();
            let request = read_message(&mut socket);
            assert_eq!(request.content()["code"], "print('tunnel')");
            send(
                &mut socket,
                "stream",
                request.id(),
                json!({"name":"stdout","text":"tunnel\n"}),
            );
            finish(&mut socket, request.id());
        });
        let mut state = common::session("work");
        state.url = "http://127.0.0.1:1/prefix/".into();
        let rest = KernelRest::new(&state, Duration::from_secs(2)).unwrap();
        let mut runtime = KernelRuntime::connect_with_proxy(
            &rest,
            "kernel-1",
            "session-1",
            &policy(&[("HTTP_PROXY", &proxy)]),
        )
        .unwrap();
        let result = runtime
            .execute(
                "print('tunnel')",
                ExecutionTimeout::Idle(Some(Duration::from_secs(2))),
                |_| Ok(()),
            )
            .unwrap();
        assert!(result.succeeded());
        assert_eq!(result.outputs[0].data["text"], "tunnel\n");
        runtime.close();
        handler.join().unwrap();
    }
}

// tungstenite fixes the callback error type; it cannot be boxed by this caller.
#[allow(clippy::result_large_err)]
#[test]
fn ssh_tunnel_keeps_endpoint_key_and_runtime_token_out_of_connect_headers() {
    for socks in [false, true] {
        let listener = listener();
        let proxy = if socks {
            format!("socks5h://{}", listener.local_addr().unwrap())
        } else {
            proxy_url(&listener)
        };
        let handler = thread::spawn(move || {
            let mut stream = accept(&listener);
            if socks {
                socks_accept(&mut stream);
            } else {
                let connect = header(&mut stream);
                assert!(!connect.contains("fixture-proxy"));
                assert!(!connect.contains("ssh-ed25519"));
                stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
            }
            let mut socket = tungstenite::accept_hdr(
                stream,
                |request: &tungstenite::handshake::server::Request,
                 response: tungstenite::handshake::server::Response| {
                    assert_eq!(request.uri().path(), "/colab/ssh");
                    assert_eq!(
                        request.headers()["X-Colab-Ssh-Pubkey"],
                        "ssh-ed25519 AAAA fixture"
                    );
                    assert!(!request.headers().contains_key("Authorization"));
                    assert!(!request.headers().contains_key("Proxy-Authorization"));
                    assert!(request.uri().query().unwrap().contains("fixture-proxy"));
                    Ok(response)
                },
            )
            .unwrap();
            assert_eq!(
                socket.read().unwrap().into_data(),
                b"opaque\0ssh".as_slice()
            );
            socket
                .send(Message::Binary(b"reply".to_vec().into()))
                .unwrap();
        });
        let mut state = common::session("work");
        state.url = "http://127.0.0.1:1/prefix".into();
        let mut socket = ssh::connect_with_proxy(
            &state,
            "ssh-ed25519 AAAA fixture",
            Mode::Proxy,
            Duration::from_secs(2),
            &policy(&[("ALL_PROXY", &proxy)]),
        )
        .unwrap();
        socket
            .send(Message::Binary(b"opaque\0ssh".to_vec().into()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let reply = loop {
            match socket.read() {
                Ok(message) => break message,
                Err(tungstenite::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        assert_eq!(reply.into_data(), b"reply".as_slice());
        handler.join().unwrap();
    }
}

#[test]
fn proxy_refusals_bad_headers_and_redirects_never_fall_back_to_direct_or_leak_bodies() {
    for response in [
        "HTTP/1.1 407 Private error\r\n\r\nprivate-body",
        "HTTP/1.1 302 Found\r\nLocation: http://private:secret@else/\r\n\r\n",
        "garbage private-body\r\n\r\n",
        &format!("HTTP/1.1 200 OK\r\nX: {}\r\n\r\n", "x".repeat(17000)),
    ] {
        let proxy_listener = listener();
        let direct = listener();
        let uri = Url::parse(&format!("ws://{}/", direct.local_addr().unwrap())).unwrap();
        let proxy = proxy_url(&proxy_listener);
        let response = response.to_owned();
        let handler = thread::spawn(move || {
            let mut stream = accept(&proxy_listener);
            header(&mut stream);
            let _ = stream.write_all(response.as_bytes());
        });
        let error = transport::connect(
            &uri,
            Instant::now() + Duration::from_secs(2),
            &policy(&[("HTTP_PROXY", &proxy)]),
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("private"));
        assert!(!error.contains("secret"));
        handler.join().unwrap();
        assert_eq!(
            direct.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
#[test]
fn drip_headers_cannot_extend_the_connect_deadline() {
    let listener = listener();
    let proxy = proxy_url(&listener);
    let handler = thread::spawn(move || {
        let mut stream = accept(&listener);
        header(&mut stream);
        for byte in b"HTTP/1.1 200 OK\r\nX: aaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\r\n" {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let start = Instant::now();
    assert!(
        transport::connect(
            &Url::parse("ws://127.0.0.1:1/").unwrap(),
            start + Duration::from_millis(150),
            &policy(&[("HTTP_PROXY", &proxy)])
        )
        .is_err()
    );
    assert!(start.elapsed() < Duration::from_millis(800));
    handler.join().unwrap();
}
#[test]
fn secure_channels_start_tls_inside_the_tunnel_before_any_runtime_headers() {
    for socks in [false, true] {
        let listener = listener();
        let proxy = if socks {
            format!("socks5h://{}", listener.local_addr().unwrap())
        } else {
            proxy_url(&listener)
        };
        let handler = thread::spawn(move || {
            let mut stream = accept(&listener);
            if socks {
                socks_accept(&mut stream);
            } else {
                assert!(header(&mut stream).starts_with("CONNECT 127.0.0.1:1 HTTP/1.1"));
                stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
            }
            let mut bytes = [0; 4096];
            let count = stream.read(&mut bytes).unwrap();
            assert!(count > 0);
            assert_eq!(bytes[0], 0x16);
            assert!(
                !bytes[..count]
                    .windows(b"fixture-proxy".len())
                    .any(|v| v == b"fixture-proxy")
            );
        });
        let mut state = common::session("work");
        state.url = "https://127.0.0.1:1/".into();
        let rest = KernelRest::new(&state, Duration::from_secs(2)).unwrap();
        assert!(matches!(
            KernelRuntime::connect_with_proxy(
                &rest,
                "kernel-1",
                "session-1",
                &policy(&[("HTTPS_PROXY", &proxy)])
            ),
            Err(Error::WebSocketTransport)
        ));
        handler.join().unwrap();
    }
}

#[test]
fn no_proxy_connects_directly_and_never_touches_the_configured_proxy() {
    let direct = listener();
    let proxy_listener = listener();
    let proxy = proxy_url(&proxy_listener);
    let uri = Url::parse(&format!("ws://{}/", direct.local_addr().unwrap())).unwrap();
    let handler = thread::spawn(move || {
        let mut stream = accept(&direct);
        let mut bytes = [0; 6];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"direct");
    });
    transport::connect(
        &uri,
        Instant::now() + Duration::from_secs(2),
        &policy(&[("HTTP_PROXY", &proxy), ("NO_PROXY", "127.0.0.0/8")]),
    )
    .unwrap()
    .write_all(b"direct")
    .unwrap();
    handler.join().unwrap();
    assert_eq!(
        proxy_listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn cli_reads_proxy_environment_and_reuses_kernel_after_connect_rejection() {
    use colabcli_rust::credentials::TokenStorageMode;
    use std::{
        fs,
        process::{Command, Stdio},
    };
    for socks in [false, true] {
        for reject in [false, true] {
            let fixture = common::Fixture::new(TokenStorageMode::File);
            let mut state = common::session("work");
            state.url = "http://127.0.0.1:1/prefix/".into();
            state.token_expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(1));
            fixture
                .storage
                .save_session(&common::id("default"), state)
                .unwrap();
            let mut token = common::token();
            token.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
            fixture.tokens.save(&common::id("default"), &token).unwrap();
            let client = fixture.temp.path().join("client.json");
            fs::write(&client,json!({"installed":{"client_id":"fixture-client","client_secret":"fixture-secret","auth_uri":"http://127.0.0.1:1/authorize","token_uri":"http://127.0.0.1:1/token","redirect_uris":["http://localhost"]}}).to_string()).unwrap();
            let listener = listener();
            let proxy = if socks {
                format!("socks5h://{}", listener.local_addr().unwrap())
            } else {
                proxy_url(&listener)
            };
            let handler = thread::spawn(move || {
                let mut methods = Vec::new();
                for attempt in 0..if reject { 2 } else { 1 } {
                    let mut stream = accept(&listener);
                    if socks {
                        socks_accept(&mut stream);
                    }
                    let request = header(&mut stream);
                    let method = if attempt == 0 { "POST" } else { "GET" };
                    assert!(
                        request.starts_with(&format!(
                            "{method} {}/prefix/api/kernels",
                            if socks { "" } else { "http://127.0.0.1:1" }
                        )),
                        "{request}"
                    );
                    methods.push(method.to_owned());
                    let count = request
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0; count];
                    stream.read_exact(&mut body).unwrap();
                    let response = b"{\"id\":\"kernel-1\"}";
                    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",response.len()).unwrap();
                    stream.write_all(response).unwrap();
                    drop(stream);
                    let mut stream = accept(&listener);
                    if socks {
                        let address = socks_request(&mut stream, None);
                        assert_eq!(address, [1, 127, 0, 0, 1, 0, 1]);
                        stream
                            .write_all(&[5, if reject { 2 } else { 0 }, 0, 1, 0, 0, 0, 0, 0, 0])
                            .unwrap();
                    } else {
                        let request = header(&mut stream);
                        assert!(request.starts_with("CONNECT 127.0.0.1:1 HTTP/1.1"));
                        stream
                            .write_all(if reject {
                                b"HTTP/1.1 407 Private response\r\n\r\nprivate-proxy-body"
                            } else {
                                b"HTTP/1.1 200 OK\r\n\r\n"
                            })
                            .unwrap();
                    }
                    methods.push("CONNECT".into());
                    if reject {
                        continue;
                    }
                    let mut socket = tungstenite::accept(stream).unwrap();
                    let setup = read_message(&mut socket);
                    finish(&mut socket, setup.id());
                    let request = read_message(&mut socket);
                    assert_eq!(request.content()["code"], "print('proxied')");
                    send(
                        &mut socket,
                        "stream",
                        request.id(),
                        json!({"name":"stdout","text":"proxied\n"}),
                    );
                    finish(&mut socket, request.id());
                }
                methods
            });
            for _ in 0..if reject { 2 } else { 1 } {
                let mut child = Command::new(env!("CARGO_BIN_EXE_colab"))
                    .env_clear()
                    .env("HOME", fixture.temp.path())
                    .env("HTTP_PROXY", &proxy)
                    .env("NO_PROXY", "")
                    .arg("--config-dir")
                    .arg(fixture.temp.path())
                    .args(["--token-storage", "file", "--request-timeout", "3", "-c"])
                    .arg(&client)
                    .args(["exec", "-s", "work"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"print('proxied')")
                    .unwrap();
                let output = child.wait_with_output().unwrap();
                if reject {
                    assert!(!output.status.success());
                    let error = String::from_utf8(output.stderr).unwrap();
                    assert!(
                        error.contains(if socks { "状态 2" } else { "HTTP 407" }),
                        "{error}"
                    );
                    assert!(!error.contains("private-proxy-body"));
                } else {
                    assert!(
                        output.status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert_eq!(output.stdout, b"proxied\n");
                }
                let sessions = fixture.storage.sessions(&common::id("default")).unwrap();
                assert_eq!(sessions["work"].kernel_id.as_deref(), Some("kernel-1"));
                assert!(sessions["work"].running.is_none());
                assert!(!sessions["work"].extra.contains_key("execution_pending"));
            }
            assert_eq!(
                handler.join().unwrap(),
                if reject {
                    vec!["POST", "CONNECT", "GET", "CONNECT"]
                } else {
                    vec!["POST", "CONNECT"]
                }
            );
        }
    }
}

#[test]
fn cli_ssh_proxy_mode_uses_inherited_proxy_environment_and_preserves_opaque_bytes() {
    use colabcli_rust::credentials::TokenStorageMode;
    use std::{
        fs,
        process::{Command, Stdio},
    };
    for socks in [false, true] {
        let fixture = common::Fixture::new(TokenStorageMode::File);
        let mut state = common::session("work");
        state.url = "http://127.0.0.1:1/prefix/".into();
        state.token_expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(1));
        fixture
            .storage
            .save_session(&common::id("default"), state)
            .unwrap();
        let mut token = common::token();
        token.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
        fixture.tokens.save(&common::id("default"), &token).unwrap();
        let client = fixture.temp.path().join("client.json");
        fs::write(&client,json!({"installed":{"client_id":"fixture-client","client_secret":"fixture-secret","auth_uri":"http://127.0.0.1:1/authorize","token_uri":"http://127.0.0.1:1/token","redirect_uris":["http://localhost"]}}).to_string()).unwrap();
        let key = fixture.temp.path().join("id.pub");
        fs::write(&key, "ssh-ed25519 AAAA fixture\n").unwrap();
        let listener = listener();
        let proxy = if socks {
            format!("socks5://{}", listener.local_addr().unwrap())
        } else {
            proxy_url(&listener)
        };
        let handler = thread::spawn(move || {
            let mut stream = accept(&listener);
            if socks {
                socks_accept(&mut stream);
            } else {
                assert!(header(&mut stream).starts_with("CONNECT 127.0.0.1:1 HTTP/1.1"));
                stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
            }
            let mut socket = tungstenite::accept(stream).unwrap();
            assert_eq!(
                socket.read().unwrap().into_data(),
                b"opaque\0ssh".as_slice()
            );
            socket
                .send(Message::Binary(b"reply\0bytes".to_vec().into()))
                .unwrap();
            socket.close(None).unwrap();
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_colab"))
            .env_clear()
            .env("HOME", fixture.temp.path())
            .env("all_proxy", proxy)
            .env("NO_PROXY", "")
            .arg("--config-dir")
            .arg(fixture.temp.path())
            .args(["--token-storage", "file", "--request-timeout", "3", "-c"])
            .arg(&client)
            .args(["ssh", "--proxy-mode", "-s", "work", "-i"])
            .arg(&key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"opaque\0ssh").unwrap();
        let output = child.wait_with_output().unwrap();
        drop(stdin);
        handler.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"reply\0bytes");
        assert_eq!(
            fixture.storage.sessions(&common::id("default")).unwrap()["work"].endpoint,
            "ep-work"
        );
    }
}

// A bounded loopback SOCKS server. It returns the requested address without
// opening the target, so a successful caller cannot have bypassed the proxy.
fn socks_request(stream: &mut TcpStream, auth: Option<(&[u8], &[u8])>) -> Vec<u8> {
    let method = if auth.is_some() { 2 } else { 0 };
    let mut greeting = [0; 3];
    stream.read_exact(&mut greeting).unwrap();
    assert_eq!(greeting, [5, 1, method]);
    stream.write_all(&[5, method]).unwrap();
    if let Some((user, password)) = auth {
        let mut head = [0; 2];
        stream.read_exact(&mut head).unwrap();
        assert_eq!(head, [1, user.len() as u8]);
        let mut actual = vec![0; user.len()];
        stream.read_exact(&mut actual).unwrap();
        assert_eq!(actual, user);
        let mut length = [0];
        stream.read_exact(&mut length).unwrap();
        assert_eq!(length[0] as usize, password.len());
        let mut actual = vec![0; password.len()];
        stream.read_exact(&mut actual).unwrap();
        assert_eq!(actual, password);
        stream.write_all(&[1, 0]).unwrap();
    }
    let mut head = [0; 4];
    stream.read_exact(&mut head).unwrap();
    assert_eq!(&head[..3], &[5, 1, 0]);
    let mut address = vec![head[3]];
    let length = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0];
            stream.read_exact(&mut length).unwrap();
            address.push(length[0]);
            length[0] as usize
        }
        kind => panic!("unexpected address type {kind}"),
    };
    let mut bytes = vec![0; length + 2];
    stream.read_exact(&mut bytes).unwrap();
    address.extend(bytes);
    address
}
fn socks_accept(stream: &mut TcpStream) {
    let address = socks_request(stream, None);
    assert_eq!(address, [1, 127, 0, 0, 1, 0, 1]);
    stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).unwrap();
}
#[test]
fn socks_address_modes_and_bound_replies_preserve_tunnel_bytes() {
    for (scheme, destination, expected, bound) in [
        (
            "socks5",
            "ws://127.0.0.1:81/path?private",
            vec![1, 127, 0, 0, 1, 0, 81],
            vec![1, 0, 0, 0, 0, 0, 0],
        ),
        (
            "socks5h",
            "wss://unresolvable.invalid/path",
            {
                let mut value = vec![3, 20];
                value.extend(b"unresolvable.invalid");
                value.extend([1, 187]);
                value
            },
            vec![3, 1, b'x', 0, 0],
        ),
        (
            "socks5h",
            "ws://[::1]:82/path",
            {
                let mut value = vec![4];
                value.extend([0; 15]);
                value.extend([1, 0, 82]);
                value
            },
            {
                let mut value = vec![4];
                value.extend([0; 18]);
                value
            },
        ),
    ] {
        let listener = listener();
        let proxy = format!("{scheme}://{}", listener.local_addr().unwrap());
        let handler = thread::spawn(move || {
            let mut stream = accept(&listener);
            assert_eq!(socks_request(&mut stream, None), expected);
            let mut reply = vec![5, 0, 0];
            reply.extend(bound);
            reply.push(b'X');
            stream.write_all(&reply).unwrap();
        });
        let mut stream = transport::connect(
            &Url::parse(destination).unwrap(),
            Instant::now() + Duration::from_secs(2),
            &policy(&[("ALL_PROXY", &proxy)]),
        )
        .unwrap();
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'X']);
        handler.join().unwrap();
    }
}
#[test]
fn socks_local_dns_resolves_localhost_and_percent_credentials_are_utf8_bytes() {
    let listener = listener();
    let proxy = format!(
        "socks5://us%40er:p%C3%A4ss@{}",
        listener.local_addr().unwrap()
    );
    let handler = thread::spawn(move || {
        let mut stream = accept(&listener);
        let address = socks_request(&mut stream, Some((b"us@er", "päss".as_bytes())));
        assert!(matches!(address[0], 1 | 4));
        assert_eq!(&address[address.len() - 2..], &[0, 80]);
        stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
    });
    transport::connect(
        &Url::parse("ws://localhost/path").unwrap(),
        Instant::now() + Duration::from_secs(2),
        &policy(&[("HTTP_PROXY", &proxy)]),
    )
    .unwrap();
    handler.join().unwrap();
}
#[test]
fn socks_rejections_are_redacted_and_do_not_fall_back_to_direct_target() {
    for reply in [
        vec![4, 0, 0, 1],
        vec![5, 0, 1, 1],
        vec![5, 5, 0, 1],
        vec![5, 0, 0, 9],
        vec![5, 0, 0, 3, 0],
        vec![5, 0, 0, 1, 0],
    ] {
        let target = listener();
        let destination =
            Url::parse(&format!("ws://{}/private", target.local_addr().unwrap())).unwrap();
        let listener = listener();
        let proxy = format!(
            "socks5h://private:secret@{}",
            listener.local_addr().unwrap()
        );
        let handler = thread::spawn(move || {
            let mut stream = accept(&listener);
            socks_request(&mut stream, Some((b"private", b"secret")));
            stream.write_all(&reply).unwrap();
        });
        let error = transport::connect(
            &destination,
            Instant::now() + Duration::from_secs(2),
            &policy(&[("HTTP_PROXY", &proxy)]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            !error.contains("private") && !error.contains("secret") && !error.contains("socks5"),
            "{error}"
        );
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        handler.join().unwrap();
    }
}
#[test]
fn socks_authentication_cannot_downgrade_and_rejects_bad_versions_or_passwords() {
    for (selected, auth_reply) in [
        ([5, 0], None),
        ([5, 255], None),
        ([4, 2], None),
        ([5, 2], Some([1, 1])),
        ([5, 2], Some([2, 0])),
    ] {
        let listener = listener();
        let proxy = format!("socks5://private:secret@{}", listener.local_addr().unwrap());
        let handler = thread::spawn(move || {
            let mut stream = accept(&listener);
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).unwrap();
            assert_eq!(greeting, [5, 1, 2]);
            stream.write_all(&selected).unwrap();
            if let Some(reply) = auth_reply {
                let mut auth = [0; 16];
                stream.read_exact(&mut auth).unwrap();
                assert_eq!(&auth, b"\x01\x07private\x06secret");
                stream.write_all(&reply).unwrap();
            }
        });
        let error = transport::connect(
            &Url::parse("ws://127.0.0.1:1/").unwrap(),
            Instant::now() + Duration::from_secs(2),
            &policy(&[("HTTP_PROXY", &proxy)]),
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("private") && !error.contains("secret"));
        handler.join().unwrap();
    }
}
#[test]
fn socks_invalid_credentials_send_no_greeting() {
    for auth in [
        "user:".to_string(),
        ":secret".to_string(),
        format!("{}:secret", "x".repeat(256)),
        "user:%FF".to_string(),
    ] {
        let listener = listener();
        let proxy = format!("socks5://{auth}@{}", listener.local_addr().unwrap());
        let handler = thread::spawn(move || {
            let mut stream = accept(&listener);
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        assert!(
            transport::connect(
                &Url::parse("ws://127.0.0.1:1/").unwrap(),
                Instant::now() + Duration::from_secs(2),
                &policy(&[("HTTP_PROXY", &proxy)])
            )
            .is_err()
        );
        handler.join().unwrap();
    }
}
#[test]
fn socks_negotiation_uses_one_deadline_despite_dripping_reply_bytes() {
    let listener = listener();
    let proxy = format!("socks5://{}", listener.local_addr().unwrap());
    let handler = thread::spawn(move || {
        let mut stream = accept(&listener);
        socks_request(&mut stream, None);
        for byte in [
            5, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ] {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let start = Instant::now();
    assert!(
        transport::connect(
            &Url::parse("ws://127.0.0.1:1/").unwrap(),
            start + Duration::from_millis(150),
            &policy(&[("HTTP_PROXY", &proxy)])
        )
        .is_err()
    );
    assert!(start.elapsed() < Duration::from_millis(800));
    handler.join().unwrap();
}
