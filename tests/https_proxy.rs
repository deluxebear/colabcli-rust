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
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tungstenite::Message;
use url::Url;

struct TlsFixture {
    root: CertificateDer<'static>,
    server: Arc<ServerConfig>,
}
impl TlsFixture {
    fn new(host: &str, expired: bool) -> Self {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(2);
        params.not_after = now + time::Duration::days(2);
        let ca_key = KeyPair::generate().unwrap();
        let ca = params.self_signed(&ca_key).unwrap();
        let mut params = CertificateParams::new(vec![host.to_owned()]).unwrap();
        params.not_before = now - time::Duration::days(2);
        params.not_after = now + time::Duration::days(1);
        if expired {
            params.not_after = now - time::Duration::days(1);
        }
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &ca, &ca_key).unwrap();
        let server = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone(), ca.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        Self {
            root: ca.der().clone(),
            server: Arc::new(server),
        }
    }
    fn client(&self) -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        roots.add(self.root.clone()).unwrap();
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }
    fn environment(&self, listener: &TcpListener) -> ProxyEnvironment {
        let proxy = format!("https://private:secret@{}", listener.local_addr().unwrap());
        ProxyEnvironment::from_values(|name| {
            if name == "ALL_PROXY" {
                Some(proxy.clone())
            } else {
                None
            }
        })
        .with_proxy_root_certificate(self.root.to_vec())
        .unwrap()
    }
}
fn listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}
fn accept(listener: TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
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
fn header(stream: &mut impl Read) -> String {
    let mut bytes = vec![];
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 16384);
    }
    String::from_utf8(bytes).unwrap()
}
fn tls(stream: TcpStream, config: Arc<ServerConfig>) -> StreamOwned<ServerConnection, TcpStream> {
    StreamOwned::new(ServerConnection::new(config).unwrap(), stream)
}
fn connect_header(stream: &mut (impl Read + Write), authority: &str) {
    let request = header(stream);
    assert!(request.starts_with(&format!("CONNECT {authority} HTTP/1.1\r\n")));
    assert!(request.contains("Proxy-Authorization: Basic cHJpdmF0ZTpzZWNyZXQ="));
    assert!(
        !request.contains("fixture-proxy")
            && !request.contains("runtime-token")
            && !request.contains("ssh-ed25519")
    );
    stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
    stream.flush().unwrap();
}
fn send<S: Read + Write>(
    socket: &mut tungstenite::WebSocket<S>,
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
#[test]
#[allow(clippy::result_large_err)]
fn jupyter_executes_after_verified_proxy_tls_and_encrypted_connect() {
    let fixture = TlsFixture::new("localhost", false);
    let listener = listener();
    let proxy = format!(
        "https://private:secret@localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let environment = ProxyEnvironment::from_values(|name| {
        if name == "ALL_PROXY" {
            Some(proxy.clone())
        } else {
            None
        }
    })
    .with_proxy_root_certificate(fixture.root.to_vec())
    .unwrap();
    let handler = thread::spawn(move || {
        let mut stream = tls(accept(listener), fixture.server);
        connect_header(&mut stream, "127.0.0.1:1");
        assert_eq!(stream.conn.server_name(), Some("localhost"));
        let mut socket = tungstenite::accept_hdr(
            stream,
            |request: &tungstenite::handshake::server::Request,
             response: tungstenite::handshake::server::Response| {
                assert_eq!(request.headers()["Authorization"], "Bearer fixture-proxy");
                assert!(!request.headers().contains_key("Proxy-Authorization"));
                Ok(response)
            },
        )
        .unwrap();
        let message = match socket.read().unwrap() {
            Message::Text(text) => codec::decode_text(&text).unwrap(),
            other => panic!("{other:?}"),
        };
        assert_eq!(message.content()["code"], "print('https-proxy')");
        send(
            &mut socket,
            "stream",
            message.id(),
            json!({"name":"stdout","text":"https-proxy\n"}),
        );
        send(
            &mut socket,
            "execute_reply",
            message.id(),
            json!({"status":"ok","execution_count":1}),
        );
        send(
            &mut socket,
            "status",
            message.id(),
            json!({"execution_state":"idle"}),
        );
    });
    let mut state = common::session("work");
    state.url = "http://127.0.0.1:1/prefix/".into();
    let rest = KernelRest::new(&state, Duration::from_secs(3)).unwrap();
    let mut runtime =
        KernelRuntime::connect_with_proxy(&rest, "kernel-1", "session-1", &environment).unwrap();
    let result = runtime
        .execute(
            "print('https-proxy')",
            ExecutionTimeout::Idle(Some(Duration::from_secs(3))),
            |_| Ok(()),
        )
        .unwrap();
    assert!(result.succeeded());
    assert_eq!(result.outputs[0].data["text"], "https-proxy\n");
    runtime.close();
    handler.join().unwrap();
}
#[test]
#[allow(clippy::result_large_err)]
fn ssh_preserves_opaque_bytes_and_endpoint_authentication_inside_proxy_tls() {
    let fixture = TlsFixture::new("127.0.0.1", false);
    let listener = listener();
    let environment = fixture.environment(&listener);
    let payload: Vec<u8> = (0..2 * 1024 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();
    let expected = payload.clone();
    let handler = thread::spawn(move || {
        let mut stream = tls(accept(listener), fixture.server);
        connect_header(&mut stream, "127.0.0.1:1");
        let mut socket = tungstenite::accept_hdr(
            stream,
            |request: &tungstenite::handshake::server::Request,
             response: tungstenite::handshake::server::Response| {
                assert_eq!(
                    request.headers()["X-Colab-Ssh-Pubkey"],
                    "ssh-ed25519 AAAA fixture"
                );
                assert!(!request.headers().contains_key("Proxy-Authorization"));
                assert!(request.uri().query().unwrap().contains("fixture-proxy"));
                Ok(response)
            },
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(socket.read().unwrap().into_data().as_ref() == expected.as_slice());
        socket
            .send(Message::Binary(b"reply\0bytes".to_vec().into()))
            .unwrap();
    });
    let mut state = common::session("work");
    state.url = "http://127.0.0.1:1/prefix/".into();
    let mut socket = ssh::connect_with_proxy(
        &state,
        "ssh-ed25519 AAAA fixture",
        Mode::Proxy,
        Duration::from_secs(3),
        &environment,
    )
    .unwrap();
    if let Err(error) = socket.send(Message::Binary(payload.into())) {
        assert!(
            matches!(error,tungstenite::Error::Io(ref io) if io.kind()==std::io::ErrorKind::WouldBlock)
        );
    }
    let flush_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match socket.flush() {
            Ok(()) => break,
            Err(tungstenite::Error::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                assert!(Instant::now() < flush_deadline);
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match socket.read() {
            Ok(message) => {
                assert_eq!(message.into_data(), b"reply\0bytes".as_slice());
                break;
            }
            Err(tungstenite::Error::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    }
    handler.join().unwrap();
}
#[test]
fn proxy_certificate_chain_hostname_and_expiry_fail_before_connect_or_direct_fallback() {
    for (trusted, host, expired) in [
        (false, "127.0.0.1", false),
        (true, "wrong.invalid", false),
        (true, "127.0.0.1", true),
    ] {
        let target = listener();
        let destination = Url::parse(&format!(
            "ws://{}/runtime-token",
            target.local_addr().unwrap()
        ))
        .unwrap();
        let fixture = TlsFixture::new(host, expired);
        let listener = listener();
        let proxy = format!("https://private:secret@{}", listener.local_addr().unwrap());
        let environment = if trusted {
            fixture.environment(&listener)
        } else {
            ProxyEnvironment::from_values(|name| {
                if name == "ALL_PROXY" {
                    Some(proxy.clone())
                } else {
                    None
                }
            })
        };
        let handler = thread::spawn(move || {
            let mut stream = tls(accept(listener), fixture.server);
            let mut bytes = [0; 2048];
            assert!(stream.read(&mut bytes).is_err());
        });
        let error = transport::connect(
            &destination,
            Instant::now() + Duration::from_secs(2),
            &environment,
        )
        .unwrap_err();
        assert!(matches!(error, Error::WebSocketProxyTls));
        let text = error.to_string();
        assert!(
            !text.contains("private")
                && !text.contains("secret")
                && !text.contains("runtime-token")
        );
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        handler.join().unwrap();
    }
    assert!(
        ProxyEnvironment::default()
            .with_proxy_root_certificate(b"invalid".to_vec())
            .is_err()
    );
}
#[test]
fn endpoint_tls_inside_proxy_tls_keeps_independent_certificate_validation() {
    for failure in [None, Some("untrusted"), Some("hostname")] {
        let proxy = TlsFixture::new("127.0.0.1", false);
        let endpoint = TlsFixture::new(
            if failure == Some("hostname") {
                "wrong.invalid"
            } else {
                "runtime.invalid"
            },
            false,
        );
        let client = if failure == Some("untrusted") {
            TlsFixture::new("runtime.invalid", false).client()
        } else {
            endpoint.client()
        };
        let listener = listener();
        let environment = proxy.environment(&listener);
        let handler = thread::spawn(move || {
            let mut stream = tls(accept(listener), proxy.server);
            connect_header(&mut stream, "runtime.invalid:443");
            let mut nested =
                StreamOwned::new(ServerConnection::new(endpoint.server).unwrap(), stream);
            if failure.is_some() {
                let mut byte = [0];
                assert!(nested.read(&mut byte).is_err());
                return;
            }
            let mut socket = tungstenite::accept(nested).unwrap();
            assert_eq!(
                socket.read().unwrap().into_data(),
                b"nested\0tls".as_slice()
            );
            socket
                .send(Message::Binary(b"verified".to_vec().into()))
                .unwrap();
        });
        let destination = Url::parse("wss://runtime.invalid/runtime-token").unwrap();
        let stream = transport::connect(
            &destination,
            Instant::now() + Duration::from_secs(3),
            &environment,
        )
        .unwrap();
        let result = tungstenite::client_tls_with_config(
            destination.as_str(),
            stream,
            None,
            Some(tungstenite::Connector::Rustls(client)),
        );
        if failure.is_some() {
            assert!(result.is_err());
        } else {
            let (mut socket, _) = result.unwrap();
            socket
                .send(Message::Binary(b"nested\0tls".to_vec().into()))
                .unwrap();
            assert_eq!(socket.read().unwrap().into_data(), b"verified".as_slice());
        }
        handler.join().unwrap();
    }
}
#[test]
fn encrypted_connect_preserves_tunnel_bytes_and_redacts_proxy_rejections() {
    for status in [200, 407, 302] {
        let fixture = TlsFixture::new("127.0.0.1", false);
        let listener = listener();
        let environment = fixture.environment(&listener);
        let handler = thread::spawn(move || {
            let mut stream = tls(accept(listener), fixture.server);
            assert!(header(&mut stream).starts_with("CONNECT 127.0.0.1:1 HTTP/1.1"));
            write!(
                stream,
                "HTTP/1.1 {status} response\r\n\r\nprivate-secret-body"
            )
            .unwrap();
            stream.flush().unwrap();
        });
        let result = transport::connect(
            &Url::parse("ws://127.0.0.1:1/").unwrap(),
            Instant::now() + Duration::from_secs(3),
            &environment,
        );
        if status == 200 {
            let mut stream = result.unwrap();
            let mut bytes = [0; 19];
            stream.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"private-secret-body");
        } else {
            let error = result.unwrap_err();
            assert!(matches!(error,Error::WebSocketProxyStatus(code)if code==status));
            assert!(
                !error.to_string().contains("private") && !error.to_string().contains("secret")
            );
        }
        handler.join().unwrap();
    }
}
#[test]
fn dripping_proxy_tls_records_cannot_extend_the_negotiation_deadline() {
    let listener = listener();
    let proxy = format!("https://private:secret@{}", listener.local_addr().unwrap());
    let environment = ProxyEnvironment::from_values(|name| {
        if name == "ALL_PROXY" {
            Some(proxy.clone())
        } else {
            None
        }
    });
    let handler = thread::spawn(move || {
        let mut stream = accept(listener);
        // read_exact retries EINTR. Require a real ClientHello byte before
        // dripping an incomplete record, including under CPU translation.
        stream.read_exact(&mut [0; 1]).unwrap();
        for byte in [22, 3, 3, 0, 100].into_iter().chain([0; 100]) {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let start = Instant::now();
    assert!(matches!(
        transport::connect(
            &Url::parse("ws://127.0.0.1:1/").unwrap(),
            start + Duration::from_millis(750),
            &environment
        ),
        Err(Error::WebSocketProxyTls)
    ));
    assert!(start.elapsed() < Duration::from_millis(1500));
    handler.join().unwrap();
}

#[test]
fn proxy_tls_and_encrypted_connect_share_the_same_deadline() {
    let fixture = TlsFixture::new("127.0.0.1", false);
    let listener = listener();
    let environment = fixture.environment(&listener);
    let handler = thread::spawn(move || {
        let socket = accept(listener);
        thread::sleep(Duration::from_millis(70));
        let mut stream = tls(socket, fixture.server);
        assert!(header(&mut stream).starts_with("CONNECT 127.0.0.1:1 HTTP/1.1"));
        for byte in b"HTTP/1.1 200 OK\r\n\r\n" {
            if stream
                .write_all(&[*byte])
                .and_then(|()| stream.flush())
                .is_err()
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let start = Instant::now();
    assert!(matches!(
        transport::connect(
            &Url::parse("ws://127.0.0.1:1/").unwrap(),
            start + Duration::from_millis(200),
            &environment
        ),
        Err(Error::WebSocketTransport)
    ));
    assert!(start.elapsed() < Duration::from_millis(800));
    handler.join().unwrap();
}
#[test]
fn no_proxy_bypasses_https_proxy_without_opening_tls_or_sending_proxy_credentials() {
    let direct = listener();
    let proxy = listener();
    let destination = Url::parse(&format!("ws://{}/", direct.local_addr().unwrap())).unwrap();
    let value = format!("https://private:secret@{}", proxy.local_addr().unwrap());
    let environment = ProxyEnvironment::from_values(|name| match name {
        "ALL_PROXY" => Some(value.clone()),
        "NO_PROXY" => Some("127.0.0.0/8".into()),
        _ => None,
    });
    let handler = thread::spawn(move || {
        let mut stream = accept(direct);
        let mut bytes = [0; 6];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"direct");
        stream.write_all(b"X").unwrap();
    });
    let mut stream = transport::connect(
        &destination,
        Instant::now() + Duration::from_secs(2),
        &environment,
    )
    .unwrap();
    stream.write_all(b"direct").unwrap();
    let mut byte = [0];
    stream.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"X");
    handler.join().unwrap();
    assert_eq!(
        proxy.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
