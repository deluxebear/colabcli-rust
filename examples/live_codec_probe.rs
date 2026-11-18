//! Development-only raw DEFAULT frame capture/replay. No allocation or credential lookup.
use base64::{Engine, engine::general_purpose::STANDARD};
use colabcli_rust::{
    Error, Result,
    jupyter::{
        KernelRest,
        codec::{self, Frame, KernelMessage},
    },
    models::SessionState,
    websocket_transport::{self, ProxyEnvironment},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, Read},
    time::{Duration, Instant},
};
use tungstenite::{Message, client::IntoClientRequest, stream::MaybeTlsStream};
use url::Url;

const CODE: &str = "print('COLABCLI_CODEC_TEXT')\n_ip = get_ipython()\n_ip.kernel.session.send(_ip.kernel.iopub_socket, 'display_data', {'data': {'text/plain': 'COLABCLI_CODEC_BINARY'}, 'metadata': {}}, parent=_ip.kernel.get_parent(), buffers=[bytes([0, 255, 128, 65]), b''])\n";

#[derive(Deserialize)]
struct Input {
    state: SessionState,
    expected_endpoint: String,
    expected_kernel: String,
}
fn decode(kind: &str, bytes: &[u8]) -> Result<KernelMessage> {
    match kind {
        "text" => codec::decode_text(
            std::str::from_utf8(bytes).map_err(|_| Error::KernelProtocol("probe UTF-8"))?,
        ),
        "binary" => codec::decode_binary(bytes),
        _ => Err(Error::KernelProtocol("probe frame type")),
    }
}
fn replay(value: Value) -> Result<Value> {
    let samples = value["samples"]
        .as_array()
        .ok_or(Error::KernelProtocol("probe samples"))?;
    if samples.is_empty() || samples.len() > 8 {
        return Err(Error::KernelProtocol("probe sample count"));
    }
    for sample in samples {
        let raw = STANDARD
            .decode(sample["wire_base64"].as_str().unwrap_or(""))
            .map_err(|_| Error::KernelProtocol("probe base64"))?;
        let message = decode(sample["wire_type"].as_str().unwrap_or(""), &raw)?;
        if message.envelope != sample["expected_envelope"] {
            return Err(Error::KernelProtocol("probe envelope mismatch"));
        }
        let expected = sample["buffers_base64"]
            .as_array()
            .ok_or(Error::KernelProtocol("probe buffers"))?;
        let buffers: Vec<String> = message.buffers.iter().map(|b| STANDARD.encode(b)).collect();
        if json!(buffers) != json!(expected) {
            return Err(Error::KernelProtocol("probe buffer mismatch"));
        }
        let roundtrip = match codec::encode(&message)? {
            Frame::Text(s) => codec::decode_text(&s)?,
            Frame::Binary(b) => codec::decode_binary(&b)?,
        };
        if message != roundtrip {
            return Err(Error::KernelProtocol("probe roundtrip mismatch"));
        }
    }
    Ok(json!({"passed": true, "replayed_samples": samples.len()}))
}
fn capture(input: Input) -> Result<Value> {
    let state = input.state;
    if input.expected_endpoint != state.endpoint
        || state.kernel_id.as_deref() != Some(&input.expected_kernel)
    {
        return Err(Error::KernelProtocol("probe identity mismatch"));
    }
    let rest = KernelRest::new(&state, Duration::from_secs(20))?;
    if rest.get(&input.expected_kernel)?.id != input.expected_kernel {
        return Err(Error::KernelProtocol("probe kernel mismatch"));
    }
    let session = codec::session_id();
    let mut url = Url::parse(&state.url).map_err(|_| Error::KernelProtocol("probe URL"))?;
    url.path_segments_mut()
        .map_err(|_| Error::KernelProtocol("probe URL"))?
        .pop_if_empty()
        .push("api")
        .push("kernels")
        .push(&input.expected_kernel)
        .push("channels");
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| Error::KernelProtocol("probe scheme"))?;
    url.query_pairs_mut()
        .append_pair("session_id", &session)
        .append_pair("token", state.token.expose());
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| Error::WebSocketTransport)?;
    for (name, value) in [
        ("Authorization", format!("Bearer {}", state.token.expose())),
        (
            "X-Colab-Runtime-Proxy-Token",
            state.token.expose().to_owned(),
        ),
        ("X-Colab-Client-Agent", "colab-cli".into()),
    ] {
        request.headers_mut().insert(
            tungstenite::http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().map_err(|_| Error::WebSocketTransport)?,
        );
    }
    let stream = websocket_transport::connect(
        &url,
        Instant::now() + Duration::from_secs(20),
        &ProxyEnvironment::capture(),
    )?;
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(codec::MAX_MESSAGE_BYTES))
        .max_frame_size(Some(codec::MAX_MESSAGE_BYTES));
    let (mut socket, _) = tungstenite::client_tls_with_config(request, stream, Some(config), None)
        .map_err(|_| Error::WebSocketTransport)?;
    let tcp = match socket.get_mut() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => &mut s.sock,
        _ => return Err(Error::WebSocketTransport),
    };
    tcp.set_read_timeout(Some(Duration::from_secs(2)))
        .and_then(|_| tcp.set_write_timeout(Some(Duration::from_secs(20))))
        .map_err(|_| Error::WebSocketTransport)?;
    let execute = KernelMessage::new(
        "execute_request",
        "shell",
        &session,
        json!({"code":CODE,"silent":false,"store_history":false,"user_expressions":{},"allow_stdin":false,"stop_on_error":true}),
    );
    let id = execute.id().to_owned();
    let Frame::Text(text) = codec::encode(&execute)? else {
        return Err(Error::KernelProtocol("probe request frame"));
    };
    socket
        .send(Message::Text(text.into()))
        .map_err(|_| Error::WebSocketTransport)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut text_seen, mut binary_seen, mut reply, mut idle) = (false, false, false, false);
    let mut samples = Vec::new();
    let mut received = 0;
    while Instant::now() < deadline && !(text_seen && binary_seen && reply && idle) {
        let (kind, raw) = match socket.read() {
            Ok(Message::Text(s)) => ("text", s.as_bytes().to_vec()),
            Ok(Message::Binary(b)) => ("binary", b.to_vec()),
            Ok(Message::Ping(_)) => {
                socket.flush().map_err(|_| Error::WebSocketTransport)?;
                continue;
            }
            Ok(Message::Pong(_)) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            _ => return Err(Error::WebSocketTransport),
        };
        received += 1;
        if received > 128 {
            return Err(Error::KernelProtocol("probe frame budget"));
        }
        let message = decode(kind, &raw)?;
        if message.parent() != Some(&id) {
            continue;
        }
        if message.kind() == "execute_reply" && message.channel() == "shell" {
            if message.content()["status"] != "ok" {
                return Err(Error::KernelProtocol("probe remote failed"));
            }
            reply = true;
        }
        if message.kind() == "status"
            && message.channel() == "iopub"
            && message.content()["execution_state"] == "idle"
        {
            idle = true;
        }
        let is_text = message.kind() == "stream"
            && message.channel() == "iopub"
            && message.content() == &json!({"name":"stdout","text":"COLABCLI_CODEC_TEXT\n"})
            && message.buffers.is_empty();
        let is_binary = message.kind() == "display_data"
            && message.channel() == "iopub"
            && message.content()
                == &json!({"data":{"text/plain":"COLABCLI_CODEC_BINARY"},"metadata":{}})
            && message.buffers == vec![vec![0, 255, 128, 65], vec![]];
        if (is_text && !text_seen) || (is_binary && !binary_seen) {
            if is_text {
                text_seen = true;
            } else {
                binary_seen = true;
            }
            samples.push(json!({"wire_type":kind,"wire_base64":STANDARD.encode(raw),"expected_envelope":message.envelope,"buffers_base64":message.buffers.iter().map(|b|STANDARD.encode(b)).collect::<Vec<_>>()}));
        }
    }
    let _ = socket.close(None);
    if !(text_seen && binary_seen && reply && idle) {
        return Err(Error::KernelProtocol("probe incomplete"));
    }
    Ok(
        json!({"schema_version":1,"passed":true,"text_seen":text_seen,"binary_seen":binary_seen,"reply_seen":reply,"idle_seen":idle,"samples":samples}),
    )
}
fn run() -> Result<Value> {
    let modes: Vec<_> = std::env::args().skip(1).collect();
    if modes.len() != 1 || !matches!(modes[0].as_str(), "--capture" | "--replay") {
        return Err(Error::KernelProtocol("explicit probe mode required"));
    }
    let mut bytes = Vec::new();
    io::stdin()
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::KernelProtocol("probe input"))?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(Error::KernelProtocol("probe input limit"));
    }
    if modes[0] == "--replay" {
        replay(serde_json::from_slice(&bytes).map_err(|_| Error::KernelProtocol("probe JSON"))?)
    } else {
        capture(serde_json::from_slice(&bytes).map_err(|_| Error::KernelProtocol("probe JSON"))?)
    }
}
fn main() {
    match run() {
        Ok(value) => println!("{value}"),
        Err(_) => {
            eprintln!("live codec probe failed; no credential or raw diagnostic emitted");
            std::process::exit(1);
        }
    }
}
