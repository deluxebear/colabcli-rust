mod common;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use colabcli_rust::{
    Error,
    colab_api::{ApiConfig, ColabApi},
    credentials::TokenStorageMode,
    execution::{self, Document, ExecuteOptions},
    jupyter::{
        ExecutionStatus, ExecutionTimeout, KernelRest, KernelRuntime,
        codec::{self, Frame, KernelMessage},
        output::{OutputChange, OutputCollector},
    },
    models::{Secret, SessionState},
    sessions::SessionService,
};
use common::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};

fn fixture_message() -> KernelMessage {
    KernelMessage::new(
        "execute_request",
        "shell",
        "test-session",
        json!({"code":"print('你好')","silent":false}),
    )
}
fn incoming(kind: &str, channel: &str, parent: &str, content: Value) -> KernelMessage {
    let mut message = KernelMessage::new(kind, channel, "server-session", content);
    message.envelope["parent_header"] = json!({"msg_id":parent});
    message
}
fn send(socket: &mut WebSocket<TcpStream>, message: KernelMessage) {
    let frame = match codec::encode(&message).unwrap() {
        Frame::Text(t) => Message::Text(t.into()),
        Frame::Binary(b) => Message::Binary(b.into()),
    };
    socket.send(frame).unwrap();
}
fn read_message(socket: &mut WebSocket<TcpStream>) -> KernelMessage {
    match socket.read().unwrap() {
        Message::Text(text) => codec::decode_text(&text).unwrap(),
        Message::Binary(bytes) => codec::decode_binary(&bytes).unwrap(),
        other => panic!("unexpected {other:?}"),
    }
}
fn finish(socket: &mut WebSocket<TcpStream>, id: &str) {
    send(
        socket,
        incoming("status", "iopub", id, json!({"execution_state":"idle"})),
    );
    send(
        socket,
        incoming(
            "execute_reply",
            "shell",
            id,
            json!({"status":"ok","execution_count":1}),
        ),
    );
}
fn user_request(socket: &mut WebSocket<TcpStream>) -> KernelMessage {
    let setup = read_message(socket);
    assert!(
        setup.content()["code"]
            .as_str()
            .unwrap()
            .contains("chdir('/content')")
    );
    finish(socket, setup.id());
    read_message(socket)
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
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "expected runtime request");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
fn state(listener: &TcpListener) -> SessionState {
    let mut state = session("work");
    state.url = format!("http://{}/prefix/", listener.local_addr().unwrap());
    state.token_expires_at = Some(Utc::now() + chrono::Duration::hours(1));
    state
}
// tungstenite fixes the handshake callback error type; it cannot be boxed by this caller.
#[allow(clippy::result_large_err)]
fn websocket(listener: &TcpListener) -> WebSocket<TcpStream> {
    tungstenite::accept_hdr(
        accept(listener),
        |request: &tungstenite::handshake::server::Request,
         response: tungstenite::handshake::server::Response| {
            assert_eq!(
                request.uri().path(),
                "/prefix/api/kernels/kernel-1/channels"
            );
            let query: BTreeMap<_, _> =
                url::form_urlencoded::parse(request.uri().query().unwrap().as_bytes())
                    .into_owned()
                    .collect();
            assert_eq!(query["token"], "fixture-proxy");
            assert!(!query["session_id"].is_empty());
            assert_eq!(request.headers()["authorization"], "Bearer fixture-proxy");
            assert_eq!(
                request.headers()["x-colab-runtime-proxy-token"],
                "fixture-proxy"
            );
            assert_eq!(request.headers()["x-colab-client-agent"], "colab-cli");
            assert!(!request.headers().contains_key("sec-websocket-protocol"));
            Ok(response)
        },
    )
    .unwrap()
}
fn http(listener: &TcpListener, method: &str, path: &str, status: u16, response: Value) -> Value {
    let mut stream = accept(listener);
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
        assert!(headers.len() < 32768);
    }
    let text = String::from_utf8(headers).unwrap();
    assert!(
        text.starts_with(&format!("{method} {path} HTTP/1.1\r\n")),
        "{text}"
    );
    let fields: BTreeMap<_, _> = text
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    assert_eq!(fields["authorization"], "Bearer fixture-proxy");
    assert_eq!(fields["x-colab-runtime-proxy-token"], "fixture-proxy");
    assert_eq!(fields["x-colab-client-agent"], "colab-cli");
    let length = fields
        .get("content-length")
        .map(|s| s.parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    let encoded = if response.is_null() {
        Vec::new()
    } else {
        serde_json::to_vec(&response).unwrap()
    };
    write!(
        stream,
        "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        encoded.len()
    )
    .unwrap();
    stream.write_all(&encoded).unwrap();
    if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    }
}
fn service(f: &Fixture, state: SessionState) -> SessionService {
    f.storage.save_session(&id("default"), state).unwrap();
    SessionService::new(
        f.storage.clone(),
        id("default"),
        ColabApi::new(
            ApiConfig::default(),
            Secret::new("never-sent-control-token"),
        )
        .unwrap(),
    )
}
fn opts() -> ExecuteOptions<'static> {
    ExecuteOptions {
        timeout: ExecutionTimeout::Idle(Some(Duration::from_secs(2))),
        http_timeout: Duration::from_secs(2),
        kernel_id: None,
        cancellation: None,
    }
}

#[test]
fn default_codec_decodes_fixed_python_fixtures_and_preserves_binary() {
    let fixture: Value =
        serde_json::from_str(include_str!("../research/kernel-codec-probe.json")).unwrap();
    let text = fixture["fixtures"]["default_text"].as_str().unwrap();
    let decoded = codec::decode_text(text).unwrap();
    assert_eq!(decoded.content()["code"], "print('你好')");
    let binary = STANDARD
        .decode(
            fixture["fixtures"]["default_binary_base64"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    let decoded_binary = codec::decode_binary(&binary).unwrap();
    assert_eq!(decoded_binary.envelope, decoded.envelope);
    assert_eq!(
        decoded_binary.buffers,
        vec![b"\x00\xff\x80hello".to_vec(), Vec::new()]
    );
    let Frame::Binary(encoded) = codec::encode(&decoded_binary).unwrap() else {
        panic!()
    };
    assert_eq!(codec::decode_binary(&encoded).unwrap(), decoded_binary);
}
#[test]
fn codec_roundtrips_text_and_multiple_empty_binary_buffers() {
    let mut message = fixture_message();
    let Frame::Text(text) = codec::encode(&message).unwrap() else {
        panic!()
    };
    assert_eq!(codec::decode_text(&text).unwrap(), message);
    message.buffers = vec![Vec::new(), vec![0, 255, 128], Vec::new()];
    let Frame::Binary(bytes) = codec::encode(&message).unwrap() else {
        panic!()
    };
    assert_eq!(u32::from_be_bytes(bytes[..4].try_into().unwrap()), 4);
    assert_eq!(codec::decode_binary(&bytes).unwrap(), message);
}
#[test]
fn codec_rejects_truncated_or_unsafe_offsets_and_unknown_channels() {
    for bytes in [
        vec![],
        vec![0; 3],
        1u32.to_be_bytes().to_vec(),
        u32::MAX.to_be_bytes().to_vec(),
        vec![0, 0, 0, 2, 0, 0, 0, 12],
        vec![0, 0, 0, 2, 0, 0, 0, 13, 0, 0, 0, 12],
        vec![0, 0, 0, 2, 0, 0, 0, 12, 0, 0, 0, 99],
    ] {
        assert!(codec::decode_binary(&bytes).is_err());
    }
    for text in ["", "{}", "[]", "{bad}"] {
        assert!(codec::decode_text(text).is_err());
    }
    let mut message = fixture_message();
    message.envelope["channel"] = json!("unexpected");
    assert!(codec::encode(&message).is_err());
    message.envelope["channel"] = json!("shell");
    message.buffers = vec![vec![]; 65];
    assert!(codec::encode(&message).is_err());
}
#[test]
fn output_accumulator_updates_display_and_respects_delayed_clear() {
    let mut collector = OutputCollector::default();
    for text in ["a", "b"] {
        collector
            .apply(&incoming(
                "display_data",
                "iopub",
                "id",
                json!({"data":{"text/plain":text},"metadata":{},"transient":{"display_id":"same"}}),
            ))
            .unwrap();
    }
    let changes=collector.apply(&incoming("update_display_data","iopub","id",json!({"data":{"image/png":"aGVsbG8=","text/plain":"updated"},"metadata":{"extra":true},"transient":{"display_id":"same"}}))).unwrap();
    assert_eq!(changes.len(), 2);
    assert_eq!(collector.outputs[1].data["data"]["text/plain"], "updated");
    collector
        .apply(&incoming(
            "clear_output",
            "iopub",
            "id",
            json!({"wait":true}),
        ))
        .unwrap();
    assert_eq!(collector.outputs.len(), 2);
    collector
        .apply(&incoming(
            "status",
            "iopub",
            "id",
            json!({"execution_state":"busy"}),
        ))
        .unwrap();
    assert_eq!(collector.outputs.len(), 2);
    let changes = collector
        .apply(&incoming(
            "stream",
            "iopub",
            "id",
            json!({"name":"stderr","text":"new"}),
        ))
        .unwrap();
    assert!(matches!(changes[0], OutputChange::Clear));
    assert_eq!(collector.outputs.len(), 1);
    collector
        .apply(&incoming(
            "clear_output",
            "iopub",
            "id",
            json!({"wait":false}),
        ))
        .unwrap();
    assert!(collector.outputs.is_empty());
}
#[test]
fn reply_error_is_synthesized_once_and_binary_output_is_retained() {
    let mut collector = OutputCollector::default();
    let mut display = incoming(
        "display_data",
        "iopub",
        "id",
        json!({"data":{"text/plain":"binary"},"metadata":{}}),
    );
    display.buffers = vec![vec![0, 255]];
    collector.apply(&display).unwrap();
    assert_eq!(collector.outputs[0].buffers, vec![vec![0, 255]]);
    let reply = json!({"status":"error","ename":"ValueError","evalue":"bad","traceback":["trace"]});
    assert_eq!(collector.synthesize_error(&reply).unwrap().len(), 1);
    assert!(collector.synthesize_error(&reply).unwrap().is_empty());
    assert_eq!(collector.outputs[1].data["ename"], "ValueError");
}
#[test]
fn runtime_waits_for_both_idle_and_reply_in_either_order() {
    for early_reply in [false, true] {
        let listener = listener();
        let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
        let handler = thread::spawn(move || {
            let mut socket = websocket(&listener);
            let request = read_message(&mut socket);
            assert_eq!(request.kind(), "execute_request");
            assert_eq!(request.content()["allow_stdin"], false);
            if early_reply {
                send(
                    &mut socket,
                    incoming(
                        "execute_reply",
                        "shell",
                        request.id(),
                        json!({"status":"ok","execution_count":7}),
                    ),
                );
            }
            send(
                &mut socket,
                incoming(
                    "stream",
                    "iopub",
                    "other-client",
                    json!({"name":"stdout","text":"not ours"}),
                ),
            );
            send(
                &mut socket,
                incoming(
                    "stream",
                    "iopub",
                    request.id(),
                    json!({"name":"stdout","text":"ours"}),
                ),
            );
            send(
                &mut socket,
                incoming(
                    "status",
                    "iopub",
                    request.id(),
                    json!({"execution_state":"idle"}),
                ),
            );
            if !early_reply {
                send(
                    &mut socket,
                    incoming(
                        "execute_reply",
                        "shell",
                        request.id(),
                        json!({"status":"ok","execution_count":7}),
                    ),
                );
            }
        });
        let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "stable-session").unwrap();
        let report = runtime
            .execute(
                "1+1",
                ExecutionTimeout::Idle(Some(Duration::from_secs(1))),
                |_| Ok(()),
            )
            .unwrap();
        assert!(report.succeeded());
        assert_eq!(report.outputs.len(), 1);
        assert_eq!(report.outputs[0].data["text"], "ours");
        assert_eq!(report.reply.unwrap()["execution_count"], 7);
        handler.join().unwrap();
    }
}
#[test]
fn continuous_relevant_output_can_exceed_idle_timeout_total_duration() {
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let handler = thread::spawn(move || {
        let mut socket = websocket(&listener);
        let request = read_message(&mut socket);
        for _ in 0..5 {
            thread::sleep(Duration::from_millis(35));
            send(
                &mut socket,
                incoming(
                    "stream",
                    "iopub",
                    request.id(),
                    json!({"name":"stdout","text":"progress"}),
                ),
            );
        }
        finish(&mut socket, request.id());
    });
    let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "s").unwrap();
    let started = Instant::now();
    let report = runtime
        .execute(
            "work()",
            ExecutionTimeout::Idle(Some(Duration::from_millis(90))),
            |_| Ok(()),
        )
        .unwrap();
    assert!(report.succeeded());
    assert!(started.elapsed() > Duration::from_millis(150));
    handler.join().unwrap();
}
#[test]
fn unrelated_traffic_does_not_extend_execution_idle_timeout() {
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let handler = thread::spawn(move || {
        let mut socket = websocket(&listener);
        let _ = read_message(&mut socket);
        for _ in 0..8 {
            thread::sleep(Duration::from_millis(25));
            if socket
                .send(Message::Text(
                    serde_json::to_string(
                        &incoming(
                            "stream",
                            "iopub",
                            "other",
                            json!({"name":"stdout","text":"noise"}),
                        )
                        .envelope,
                    )
                    .unwrap()
                    .into(),
                ))
                .is_err()
            {
                break;
            }
        }
    });
    let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "s").unwrap();
    let started = Instant::now();
    let report = runtime
        .execute(
            "silent()",
            ExecutionTimeout::Idle(Some(Duration::from_millis(80))),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(report.status, ExecutionStatus::Unknown);
    assert!(report.outputs.is_empty());
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(
        runtime
            .execute(
                "must_not_replay()",
                ExecutionTimeout::Idle(None),
                |_| Ok(())
            )
            .is_err()
    );
    drop(runtime);
    handler.join().unwrap();
}
#[test]
fn disconnect_preserves_partial_output_and_marks_result_unknown() {
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let handler = thread::spawn(move || {
        let mut socket = websocket(&listener);
        let request = read_message(&mut socket);
        send(
            &mut socket,
            incoming(
                "stream",
                "iopub",
                request.id(),
                json!({"name":"stdout","text":"partial"}),
            ),
        );
        socket.close(None).unwrap();
    });
    let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "s").unwrap();
    let report = runtime
        .execute("train()", ExecutionTimeout::Idle(None), |_| Ok(()))
        .unwrap();
    assert_eq!(report.status, ExecutionStatus::Unknown);
    assert_eq!(report.outputs[0].data["text"], "partial");
    assert!(!report.msg_id.is_empty());
    handler.join().unwrap();
}
#[test]
fn parentless_kernel_restart_aborts_waiting_and_error_reply_without_output_is_visible() {
    for restart in [true, false] {
        let listener = listener();
        let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
        let handler = thread::spawn(move || {
            let mut socket = websocket(&listener);
            let request = read_message(&mut socket);
            if restart {
                let mut message = incoming(
                    "status",
                    "iopub",
                    "",
                    json!({"execution_state":"restarting"}),
                );
                message.envelope["parent_header"] = json!({});
                send(&mut socket, message);
            } else {
                send(
                    &mut socket,
                    incoming(
                        "execute_reply",
                        "shell",
                        request.id(),
                        json!({"status":"error","ename":"Oops","evalue":"bad","traceback":[]}),
                    ),
                );
                send(
                    &mut socket,
                    incoming(
                        "status",
                        "iopub",
                        request.id(),
                        json!({"execution_state":"idle"}),
                    ),
                );
            }
        });
        let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "s").unwrap();
        let report = runtime
            .execute("bad()", ExecutionTimeout::Idle(None), |_| Ok(()))
            .unwrap();
        assert_eq!(report.status, ExecutionStatus::Failed);
        if !restart {
            assert_eq!(report.outputs[0].data["ename"], "Oops");
        }
        handler.join().unwrap();
    }
}
#[test]
fn rest_create_reuse_restart_interrupt_and_delete_use_proxy_headers() {
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let handler = thread::spawn(move || {
        assert_eq!(
            http(
                &listener,
                "POST",
                "/prefix/api/kernels",
                201,
                json!({"id":"kernel-1","execution_state":"idle"})
            ),
            json!({"name":"python3","path":"content"})
        );
        http(
            &listener,
            "GET",
            "/prefix/api/kernels/kernel-1",
            200,
            json!({"id":"kernel-1"}),
        );
        http(
            &listener,
            "GET",
            "/prefix/api/kernels",
            200,
            json!([{"id":"kernel-1"}]),
        );
        http(
            &listener,
            "POST",
            "/prefix/api/kernels/kernel-1/restart",
            200,
            json!({"id":"kernel-1"}),
        );
        http(
            &listener,
            "POST",
            "/prefix/api/kernels/kernel-1/interrupt",
            204,
            Value::Null,
        );
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/kernel-1",
            404,
            json!({"secret":"not printed"}),
        );
    });
    assert_eq!(rest.create().unwrap().id, "kernel-1");
    rest.get("kernel-1").unwrap();
    assert_eq!(rest.list().unwrap().len(), 1);
    rest.restart("kernel-1").unwrap();
    rest.interrupt("kernel-1").unwrap();
    rest.delete("kernel-1").unwrap();
    handler.join().unwrap();
}
#[test]
fn execution_persists_kernel_and_session_ids_before_socket_and_reuses_kernel() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let svc = service(&f, state(&listener));
    let storage = f.storage.clone();
    let handler = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut first = websocket(&listener);
        let saved = storage.sessions(&id("default")).unwrap();
        assert_eq!(saved["work"].kernel_id.as_deref(), Some("kernel-1"));
        let request = user_request(&mut first);
        assert!(request.content()["code"].as_str().unwrap().contains("x=7"));
        let first_session = request.envelope["header"]["session"].clone();
        assert_eq!(
            storage.sessions(&id("default")).unwrap()["work"]
                .running
                .as_deref(),
            Some(request.id())
        );
        finish(&mut first, request.id());
        drop(first);
        http(
            &listener,
            "GET",
            "/prefix/api/kernels/kernel-1",
            200,
            json!({"id":"kernel-1"}),
        );
        let mut second = websocket(&listener);
        let request = user_request(&mut second);
        assert_eq!(request.envelope["header"]["session"], first_session);
        assert!(
            request.content()["code"]
                .as_str()
                .unwrap()
                .contains("print(x)")
        );
        finish(&mut second, request.id());
    });
    let mut first = Document::from_stdin("x=7".as_bytes(), &[]).unwrap();
    assert!(
        execution::execute_document(&svc, Some("work"), &mut first, opts(), |_| Ok(())).unwrap()[0]
            .succeeded()
    );
    let mut second = Document::from_stdin("print(x)".as_bytes(), &[]).unwrap();
    execution::execute_document(&svc, Some("work"), &mut second, opts(), |_| Ok(())).unwrap();
    handler.join().unwrap();
    assert!(
        f.storage.sessions(&id("default")).unwrap()["work"]
            .running
            .is_none()
    );
}
#[test]
fn failed_kernel_creation_is_not_repeated_without_explicit_recovery() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let svc = service(&f, state(&listener));
    let handler = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            503,
            json!({"private":"hidden"}),
        );
    });
    let mut document = Document::from_stdin("pass".as_bytes(), &[]).unwrap();
    assert!(execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())).is_err());
    handler.join().unwrap();
    assert!(matches!(
        execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())),
        Err(Error::KernelStartPending)
    ));
}
#[test]
fn notebook_preserves_unknown_fields_markdown_and_partial_outputs_on_disconnect() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let svc = service(&f, state(&listener));
    let file = f.temp.path().join("source.ipynb");
    fs::write(&file,serde_json::to_vec(&json!({"nbformat":4,"nbformat_minor":5,"metadata":{"custom":7},"extra":"keep","cells":[
        {"cell_type":"markdown","source":["# Title"],"metadata":{}},{"cell_type":"code","source":["print('partial')"],"metadata":{},"outputs":[],"execution_count":null},
        {"cell_type":"code","source":"must_not_run()","metadata":{},"outputs":[],"execution_count":null}]})).unwrap()).unwrap();
    let handler = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&listener);
        let request = user_request(&mut socket);
        send(
            &mut socket,
            incoming(
                "stream",
                "iopub",
                request.id(),
                json!({"name":"stdout","text":"partial"}),
            ),
        );
        socket.close(None).unwrap();
    });
    let mut document = Document::from_file(&file, &[]).unwrap();
    let reports =
        execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())).unwrap();
    handler.join().unwrap();
    assert_eq!(reports[0].status, ExecutionStatus::Unknown);
    let saved: Value =
        serde_json::from_slice(&fs::read(document.output_path.unwrap()).unwrap()).unwrap();
    assert_eq!(saved["extra"], "keep");
    assert_eq!(saved["metadata"]["custom"], 7);
    assert_eq!(saved["cells"][0]["source"], json!(["# Title"]));
    assert_eq!(saved["cells"][1]["outputs"][0]["text"], "partial");
    assert_eq!(
        saved["cells"][1]["metadata"]["colab_cli_execution"]["status"],
        "unknown"
    );
    assert!(
        saved["cells"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["id"].is_string())
    );
    assert!(
        f.storage.sessions(&id("default")).unwrap()["work"]
            .running
            .is_some()
    );
    let mut again = Document::from_stdin("don't replay".as_bytes(), &[]).unwrap();
    assert!(matches!(
        execution::execute_document(&svc, None, &mut again, opts(), |_| Ok(())),
        Err(Error::ExecutionPending(_))
    ));
}
#[test]
fn local_document_and_env_validation_happen_before_runtime_access() {
    for env in [
        vec!["MISSING".into()],
        vec!["1BAD=x".into()],
        vec!["A-B=x".into()],
        vec!["=x".into()],
    ] {
        assert!(Document::from_stdin("pass".as_bytes(), &env).is_err());
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("invalid.ipynb");
    for notebook in [
        json!({"nbformat":3,"cells":[]}),
        json!({"nbformat":4,"cells":[{"cell_type":"code","source":23}]}),
        json!({"nbformat":4,"cells":[{"cell_type":"code","source":"pass","metadata":"bad"}]}),
    ] {
        fs::write(&path, notebook.to_string()).unwrap();
        assert!(Document::from_file(&path, &[]).is_err());
    }
    assert!(
        !Document::from_stdin(" \n".as_bytes(), &[])
            .unwrap()
            .has_code()
    );
}
#[test]
fn cli_exec_streams_stdout_stderr_and_returns_nonzero_for_kernel_errors() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    f.storage
        .save_session(&id("default"), state(&listener))
        .unwrap();
    let mut credentials = token();
    credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &credentials).unwrap();
    let handler = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&listener);
        let setup = read_message(&mut socket);
        let code = setup.content()["code"].as_str().unwrap();
        let literal = code
            .lines()
            .find(|line| line.contains("__import__('json').loads("))
            .unwrap()
            .split("__import__('json').loads(")
            .nth(1)
            .unwrap()
            .strip_suffix("))")
            .unwrap();
        let encoded: String = serde_json::from_str(literal).unwrap();
        let env: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(env["SAFE"], "quote'\\\nvalue");
        finish(&mut socket, setup.id());
        let request = read_message(&mut socket);
        assert_eq!(request.content()["code"], "print('execute')");
        send(
            &mut socket,
            incoming(
                "stream",
                "iopub",
                request.id(),
                json!({"name":"stdout","text":"hello\n"}),
            ),
        );
        send(
            &mut socket,
            incoming(
                "stream",
                "iopub",
                request.id(),
                json!({"name":"stderr","text":"warning\n"}),
            ),
        );
        send(
            &mut socket,
            incoming(
                "execute_reply",
                "shell",
                request.id(),
                json!({"status":"error","ename":"ValueError","evalue":"fixture-failure","traceback":[]}),
            ),
        );
        send(
            &mut socket,
            incoming(
                "status",
                "iopub",
                request.id(),
                json!({"execution_state":"idle"}),
            ),
        );
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_colab"))
        .arg("--config-dir")
        .arg(f.temp.path())
        .args([
            "--token-storage",
            "file",
            "exec",
            "-s",
            "work",
            "--env",
            "SAFE=quote'\\\nvalue",
        ])
        .env_remove("COLAB_ACCOUNT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"print('execute')")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    handler.join().unwrap();
    assert!(!output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "hello\n");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("warning"));
    assert!(stderr.contains("ValueError: fixture-failure"));
    assert!(!stderr.contains("fixture-proxy"));
    assert!(!stderr.contains("fixture-access"));
}

#[test]
fn iopub_after_idle_does_not_postpone_missing_shell_reply() {
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let handler = thread::spawn(move || {
        let mut socket = websocket(&listener);
        let request = read_message(&mut socket);
        send(
            &mut socket,
            incoming(
                "status",
                "iopub",
                request.id(),
                json!({"execution_state":"idle"}),
            ),
        );
        for _ in 0..8 {
            thread::sleep(Duration::from_millis(25));
            let text = serde_json::to_string(
                &incoming(
                    "status",
                    "iopub",
                    request.id(),
                    json!({"execution_state":"idle"}),
                )
                .envelope,
            )
            .unwrap();
            if socket.send(Message::Text(text.into())).is_err() {
                break;
            }
        }
    });
    let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "s").unwrap();
    let started = Instant::now();
    let result = runtime
        .execute(
            "pass",
            ExecutionTimeout::Idle(Some(Duration::from_millis(80))),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Unknown);
    assert!(started.elapsed() < Duration::from_millis(200));
    drop(runtime);
    handler.join().unwrap();
}
#[test]
fn total_deadline_expires_even_with_relevant_output() {
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let handler = thread::spawn(move || {
        let mut socket = websocket(&listener);
        let request = read_message(&mut socket);
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(20));
            let text = serde_json::to_string(
                &incoming(
                    "stream",
                    "iopub",
                    request.id(),
                    json!({"name":"stdout","text":"progress"}),
                )
                .envelope,
            )
            .unwrap();
            if socket.send(Message::Text(text.into())).is_err() {
                break;
            }
        }
    });
    let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "s").unwrap();
    let result = runtime
        .execute(
            "pass",
            ExecutionTimeout::Deadline(Some(Duration::from_millis(80))),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Unknown);
    assert!(!result.outputs.is_empty());
    drop(runtime);
    handler.join().unwrap();
}
#[test]
fn websocket_ping_is_answered_without_becoming_user_output() {
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let handler = thread::spawn(move || {
        let mut socket = websocket(&listener);
        let request = read_message(&mut socket);
        socket.send(Message::Ping(b"keep".to_vec().into())).unwrap();
        assert_eq!(
            socket.read().unwrap(),
            Message::Pong(b"keep".to_vec().into())
        );
        finish(&mut socket, request.id());
    });
    let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "s").unwrap();
    let result = runtime
        .execute("pass", ExecutionTimeout::Idle(None), |_| Ok(()))
        .unwrap();
    assert!(result.succeeded());
    assert!(result.outputs.is_empty());
    handler.join().unwrap();
}
#[test]
fn explicit_kernel_id_recovers_uncertain_creation_without_another_post() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let mut initial = state(&listener);
    initial
        .extra
        .insert("kernel_start_pending".into(), json!(true));
    let svc = service(&f, initial);
    let handler = thread::spawn(move || {
        http(
            &listener,
            "GET",
            "/prefix/api/kernels/kernel-1",
            200,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&listener);
        let request = user_request(&mut socket);
        finish(&mut socket, request.id());
    });
    let mut document =
        Document::from_stdin("from __future__ import annotations\n1+1".as_bytes(), &[]).unwrap();
    let options = ExecuteOptions {
        kernel_id: Some("kernel-1"),
        ..opts()
    };
    let results =
        execution::execute_document(&svc, None, &mut document, options, |_| Ok(())).unwrap();
    assert!(results[0].succeeded());
    handler.join().unwrap();
    let saved = f.storage.sessions(&id("default")).unwrap();
    assert!(!saved["work"].extra.contains_key("kernel_start_pending"));
}
#[test]
fn restart_clears_uncertain_execution_only_after_confirmed_success() {
    for status in [503, 200] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        let mut initial = state(&listener);
        initial.kernel_id = Some("kernel-1".into());
        initial.running = Some("uncertain-message".into());
        let svc = service(&f, initial);
        let handler = thread::spawn(move || {
            http(
                &listener,
                "POST",
                "/prefix/api/kernels/kernel-1/restart",
                status,
                json!({"id":"kernel-1"}),
            );
        });
        let result = execution::restart_kernel(&svc, None, Duration::from_secs(2));
        handler.join().unwrap();
        let saved = f.storage.sessions(&id("default")).unwrap();
        if status == 200 {
            assert!(result.is_ok());
            assert!(saved["work"].running.is_none());
            assert!(!saved["work"].extra.contains_key("kernel_restart_pending"));
        } else {
            assert!(result.is_err());
            assert_eq!(saved["work"].running.as_deref(), Some("uncertain-message"));
            assert_eq!(saved["work"].extra["kernel_restart_pending"], true);
        }
    }
}
#[test]
fn blank_cli_exec_does_not_authenticate_or_create_kernel() {
    let temp = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_colab"))
        .arg("--config-dir")
        .arg(temp.path())
        .args(["--token-storage", "file", "exec"])
        .env_remove("COLAB_ACCOUNT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b" \n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!temp.path().join("accounts/default/sessions.json").exists());
}

#[test]
fn notebook_binary_mime_outputs_use_cell_metadata_without_transient_fields() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let svc = service(&f, state(&listener));
    let path = f.temp.path().join("display.ipynb");
    fs::write(&path,json!({"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"cell_type":"code","source":"display_image()","metadata":{},"outputs":[],"execution_count":null}]}).to_string()).unwrap();
    let handler = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&listener);
        let request = user_request(&mut socket);
        let mut output = incoming(
            "display_data",
            "iopub",
            request.id(),
            json!({"data":{"image/png":STANDARD.encode(b"fake-png"),"text/plain":"plot"},"metadata":{},"transient":{"display_id":"plot-1"}}),
        );
        output.buffers = vec![vec![0, 255, 128]];
        send(&mut socket, output);
        finish(&mut socket, request.id());
    });
    let mut document = Document::from_file(&path, &[]).unwrap();
    let results =
        execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())).unwrap();
    handler.join().unwrap();
    let saved: Value =
        serde_json::from_slice(&fs::read(document.output_path.unwrap()).unwrap()).unwrap();
    assert!(saved["cells"][0]["outputs"][0].get("transient").is_none());
    assert_eq!(
        saved["cells"][0]["metadata"]["colab_cli_output_buffers"]["0"][0],
        STANDARD.encode([0, 255, 128])
    );
    assert_eq!(results[0].outputs[0].buffers, vec![vec![0, 255, 128]]);
    let image = f.temp.path().join("plot.png");
    execution::save_last_image(&results, &image).unwrap();
    assert_eq!(fs::read(image).unwrap(), b"fake-png");
}
#[test]
fn failed_ws_handshake_preserves_created_kernel_for_later_reconnect() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let svc = service(&f, state(&listener));
    let handler = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut socket = accept(&listener);
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
        }
        socket
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        drop(socket);
        http(
            &listener,
            "GET",
            "/prefix/api/kernels/kernel-1",
            200,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&listener);
        let request = user_request(&mut socket);
        finish(&mut socket, request.id());
    });
    let mut document = Document::from_stdin("pass".as_bytes(), &[]).unwrap();
    assert!(execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())).is_err());
    let state = f.storage.sessions(&id("default")).unwrap();
    assert_eq!(state["work"].kernel_id.as_deref(), Some("kernel-1"));
    assert!(state["work"].running.is_none());
    assert!(
        execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())).unwrap()[0]
            .succeeded()
    );
    handler.join().unwrap();
}
#[test]
fn frame_and_output_limits_fail_without_unbounded_growth() {
    let code = "x".repeat(codec::MAX_MESSAGE_BYTES);
    assert!(Document::from_stdin(code.as_bytes(), &[]).is_err());
    let mut collector = OutputCollector::default();
    let message = incoming(
        "stream",
        "iopub",
        "id",
        json!({"name":"stdout","text":"short"}),
    );
    for _ in 0..10000 {
        collector.apply(&message).unwrap();
    }
    assert!(collector.apply(&message).is_err());
    assert_eq!(collector.outputs.len(), 10000);
}

#[test]
fn stop_can_release_vm_while_execution_waits_and_old_writer_cannot_resurrect_it() {
    let f = Fixture::new(TokenStorageMode::File);
    let runtime_listener = listener();
    let state = state(&runtime_listener);
    let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = url::Url::parse(&format!("http://{}", control.server_addr())).unwrap();
    f.storage
        .save_session(&id("default"), state.clone())
        .unwrap();
    let make_service = || {
        SessionService::new(
            f.storage.clone(),
            id("default"),
            ColabApi::new(
                ApiConfig {
                    base_url: base.clone(),
                    ..ApiConfig::default()
                },
                Secret::new("control-oauth"),
            )
            .unwrap(),
        )
    };
    let execute_service = make_service();
    let stop_service = make_service();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let runtime_handler = thread::spawn(move || {
        http(
            &runtime_listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&runtime_listener);
        let request = user_request(&mut socket);
        send(
            &mut socket,
            incoming(
                "stream",
                "iopub",
                request.id(),
                json!({"name":"stdout","text":"running"}),
            ),
        );
        ready_tx.send(()).unwrap();
        stop_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        socket.close(None).unwrap();
    });
    let control_handler = thread::spawn(move || {
        for (method, path, body) in [
            (
                "GET",
                "/tun/m/assignments",
                json!({"assignments":[{"endpoint":state.endpoint,"accelerator":"NONE","variant":0,"machineShape":0,
            "runtimeProxyInfo":{"url":state.url,"token":"fixture-proxy","tokenExpiresInSeconds":3600}}]}),
            ),
            ("GET", "/tun/m/unassign/ep-work", json!({"token":"xsrf"})),
            ("POST", "/tun/m/unassign/ep-work", Value::Null),
        ] {
            let r = control
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert_eq!(r.method().as_str(), method);
            assert!(r.url().starts_with(path));
            assert!(
                r.headers().iter().any(|h| h.field.equiv("Authorization")
                    && h.value.as_str() == "Bearer control-oauth")
            );
            if body.is_null() {
                r.respond(tiny_http::Response::empty(204)).unwrap();
            } else {
                r.respond(tiny_http::Response::from_string(body.to_string()))
                    .unwrap();
            }
        }
    });
    let execute = thread::spawn(move || {
        let mut document = Document::from_stdin("long_work()".as_bytes(), &[]).unwrap();
        execution::execute_document(&execute_service, None, &mut document, opts(), |_| Ok(()))
    });
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    stop_service.stop("work").unwrap();
    stop_tx.send(()).unwrap();
    let result = execute.join().unwrap();
    assert!(matches!(result, Err(Error::SessionMissing(_))));
    runtime_handler.join().unwrap();
    control_handler.join().unwrap();
    assert!(f.storage.sessions(&id("default")).unwrap().is_empty());
}
#[test]
fn execution_checkpoint_preserves_concurrent_refresh_and_rejects_old_generation() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let svc = service(&f, state(&listener));
    svc.with_execution_session(None, |initial, checkpoint| {
        let mut refreshed = initial.clone();
        refreshed.token = Secret::new("new-proxy");
        refreshed.accelerator = "FUTURE".into();
        f.storage.save_session(&id("default"), refreshed).unwrap();
        let mut updated = initial.clone();
        updated.running = Some("current-message".into());
        checkpoint(updated.clone())?;
        let saved = f.storage.sessions(&id("default")).unwrap();
        assert_eq!(saved["work"].token.expose(), "new-proxy");
        assert_eq!(saved["work"].accelerator, "FUTURE");
        let mut restarted = saved["work"].clone();
        restarted.running = None;
        restarted
            .extra
            .insert("kernel_generation".into(), json!("new-generation"));
        f.storage.save_session(&id("default"), restarted).unwrap();
        assert!(checkpoint(updated).is_err());
        assert!(
            f.storage.sessions(&id("default")).unwrap()["work"]
                .running
                .is_none()
        );
        Ok(())
    })
    .unwrap();
}

fn run_control_server(
    server: tiny_http::Server,
    runtime_url: String,
    release_status: u16,
    expected_accelerator: &str,
) {
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(request.method().as_str(), "GET");
    assert!(request.url().starts_with("/tun/m/assign?"));
    let url = url::Url::parse(&format!("http://localhost{}", request.url())).unwrap();
    let pairs: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs["accelerator"], expected_accelerator);
    request.respond(tiny_http::Response::from_string(json!({"endpoint":"ep-run","runtimeProxyInfo":{"url":runtime_url,"token":"fixture-proxy","tokenExpiresInSeconds":3600}}).to_string())).unwrap();
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert!(request.url().starts_with("/tun/m/assignments?"));
    request.respond(tiny_http::Response::from_string(json!({"assignments":[{"endpoint":"ep-run","accelerator":"NONE","variant":0,"machineShape":0,"runtimeProxyInfo":{"url":runtime_url,"token":"fixture-proxy","tokenExpiresInSeconds":3600}}]}).to_string())).unwrap();
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert!(request.url().starts_with("/tun/m/unassign/ep-run?"));
    request
        .respond(tiny_http::Response::from_string(
            json!({"token":"xsrf"}).to_string(),
        ))
        .unwrap();
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(request.method().as_str(), "POST");
    request
        .respond(tiny_http::Response::empty(release_status))
        .unwrap();
}
fn install_cli_token(f: &Fixture) {
    let mut credentials = token();
    credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &credentials).unwrap();
}
fn cli_command(f: &Fixture, base: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_colab"));
    cmd.arg("--config-dir")
        .arg(f.temp.path())
        .args(["--token-storage", "file", "--control-plane-url", base])
        .env_remove("COLAB_ACCOUNT")
        .env_remove("COLAB_CLI_REQUEST_TIMEOUT")
        .env_remove("COLAB_CLI_ASSIGN_RETRIES")
        .env_remove("COLAB_CLI_RETRY_BACKOFF");
    cmd
}
fn run_payload(message: &KernelMessage) -> Value {
    let source = message.content()["code"].as_str().unwrap();
    let literal = source
        .lines()
        .find_map(|line| line.trim().strip_prefix("data = json.loads("))
        .unwrap()
        .strip_suffix(')')
        .unwrap();
    let encoded: String = serde_json::from_str(literal).unwrap();
    serde_json::from_str(&encoded).unwrap()
}
#[test]
fn cli_run_preserves_argv_and_script_exit_code_even_if_cleanup_fails() {
    let f = Fixture::new(TokenStorageMode::File);
    install_cli_token(&f);
    let listener = listener();
    let runtime_url = state(&listener).url;
    let script = f.temp.path().join("script.py");
    fs::write(
        &script,
        "#!/usr/bin/env colab run\nfrom __future__ import annotations\nimport sys\nsys.exit(7)\n",
    )
    .unwrap();
    let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", control.server_addr());
    let control_handler =
        thread::spawn(move || run_control_server(control, runtime_url, 503, "NONE"));
    let runtime_handler = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&listener);
        let request = user_request(&mut socket);
        let data = run_payload(&request);
        assert_eq!(
            data["argv"],
            json!([
                "script.py",
                "--env",
                "SCRIPT_VALUE=preserved",
                "--gpu",
                "quoted'\\中文"
            ])
        );
        assert!(!data["source"].as_str().unwrap().starts_with("#!"));
        assert!(
            data["source"]
                .as_str()
                .unwrap()
                .starts_with("from __future__")
        );
        send(
            &mut socket,
            incoming(
                "stream",
                "iopub",
                request.id(),
                json!({"name":"stdout","text":"script-output\n"}),
            ),
        );
        send(
            &mut socket,
            incoming(
                "display_data",
                "iopub",
                request.id(),
                json!({"data":{colabcli_rust::runs::RESULT_MIME:{"nonce":data["nonce"],"exit_code":7}},"metadata":{}}),
            ),
        );
        finish(&mut socket, request.id());
        drop(socket);
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/kernel-1",
            204,
            Value::Null,
        );
    });
    let output = cli_command(&f, &base)
        .args([
            "run",
            "--json",
            "--env",
            "SECRET=not-in-history",
            "-s",
            "one-shot",
        ])
        .arg(&script)
        .args(["--env", "SCRIPT_VALUE=preserved", "--gpu", "quoted'\\中文"])
        .output()
        .unwrap();
    runtime_handler.join().unwrap();
    control_handler.join().unwrap();
    assert_eq!(output.status.code(), Some(7));
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["exit_code"], 7);
    assert_eq!(outcome["cleanup"], "pending");
    assert_eq!(outcome["session"], "one-shot");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-proxy"));
    let run_id = outcome["run_id"].as_str().unwrap();
    assert_eq!(
        colabcli_rust::runs::pending(&f.storage, &id("default"))
            .unwrap()
            .len(),
        1
    );
    assert!(
        f.storage
            .profile_path(&id("default"), "control_operation.json")
            .unwrap()
            .exists()
    );
    let events = colabcli_rust::history::History::new(f.storage.clone(), id("default"))
        .read("one-shot", colabcli_rust::history::HistoryQuery::default())
        .unwrap();
    let text = serde_json::to_string(&events).unwrap();
    assert!(!text.contains("not-in-history"));
    assert!(!text.contains("__colab_cli_run__"));
    assert!(text.contains("sys.exit(7)"));
    // Cleanup-only recovery queries absence and never repeats either allocation or script.
    let recovery = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", recovery.server_addr());
    let handler = thread::spawn(move || {
        let r = recovery
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(r.url().starts_with("/tun/m/assignments?"));
        r.respond(tiny_http::Response::from_string("{\"assignments\":[]}"))
            .unwrap();
    });
    let output = cli_command(&f, &base)
        .args(["run-cleanup", run_id])
        .output()
        .unwrap();
    handler.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        colabcli_rust::runs::pending(&f.storage, &id("default"))
            .unwrap()
            .is_empty()
    );
    assert!(f.storage.sessions(&id("default")).unwrap().is_empty());
}
#[test]
fn run_keep_preserves_vm_and_missing_script_never_allocates() {
    let f = Fixture::new(TokenStorageMode::File);
    install_cli_token(&f);
    let listener = listener();
    let runtime_url = state(&listener).url;
    let script = f.temp.path().join("keep.py");
    fs::write(&script, "print('keep')").unwrap();
    let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", control.server_addr());
    let handler = thread::spawn(move || {
        let request = control
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(request.url().starts_with("/tun/m/assign?"));
        request.respond(tiny_http::Response::from_string(json!({"endpoint":"ep-keep","runtimeProxyInfo":{"url":runtime_url,"token":"fixture-proxy","tokenExpiresInSeconds":3600}}).to_string())).unwrap();
        assert!(
            control
                .recv_timeout(Duration::from_millis(350))
                .unwrap()
                .is_none()
        );
    });
    let runtime = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&listener);
        let request = user_request(&mut socket);
        let data = run_payload(&request);
        send(
            &mut socket,
            incoming(
                "display_data",
                "iopub",
                request.id(),
                json!({"data":{colabcli_rust::runs::RESULT_MIME:{"nonce":data["nonce"],"exit_code":0}},"metadata":{}}),
            ),
        );
        finish(&mut socket, request.id());
    });
    let output = cli_command(&f, &base)
        .args(["run", "--json", "--keep", "-s", "kept"])
        .arg(&script)
        .output()
        .unwrap();
    runtime.join().unwrap();
    handler.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["cleanup"], "kept");
    assert_eq!(
        f.storage.sessions(&id("default")).unwrap()["kept"]
            .kernel_id
            .as_deref(),
        Some("kernel-1")
    );
    assert!(
        colabcli_rust::runs::pending(&f.storage, &id("default"))
            .unwrap()
            .is_empty()
    );
    let output = cli_command(&f, &base)
        .args(["run", "/nonexistent/script.py"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
#[test]
fn run_cleans_up_when_websocket_startup_fails() {
    let f = Fixture::new(TokenStorageMode::File);
    install_cli_token(&f);
    let listener = listener();
    let runtime_url = state(&listener).url;
    let script = f.temp.path().join("fail.py");
    fs::write(&script, "pass").unwrap();
    let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", control.server_addr());
    let handler = thread::spawn(move || run_control_server(control, runtime_url, 204, "NONE"));
    let runtime = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut stream = accept(&listener);
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut b = [0];
            stream.read_exact(&mut b).unwrap();
            headers.push(b[0]);
        }
        stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        drop(stream);
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/kernel-1",
            204,
            Value::Null,
        );
    });
    let output = cli_command(&f, &base)
        .args(["run", "--json"])
        .arg(script)
        .output()
        .unwrap();
    runtime.join().unwrap();
    handler.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["cleanup"], "confirmed_released");
    assert!(f.storage.sessions(&id("default")).unwrap().is_empty());
}
#[cfg(unix)]
#[test]
fn run_sigterm_exits_143_and_releases_the_owned_vm() {
    let f = Fixture::new(TokenStorageMode::File);
    install_cli_token(&f);
    let listener = listener();
    let runtime_url = state(&listener).url;
    let script = f.temp.path().join("wait.py");
    fs::write(&script, "long_task()").unwrap();
    let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", control.server_addr());
    let handler = thread::spawn(move || run_control_server(control, runtime_url, 204, "NONE"));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let runtime = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&listener);
        let _request = user_request(&mut socket);
        ready_tx.send(()).unwrap();
        let _ = socket.read();
        drop(socket);
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/kernel-1",
            204,
            Value::Null,
        );
    });
    let child = cli_command(&f, &base)
        .args(["run", "--json", "--timeout", "0"])
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let output = child.wait_with_output().unwrap();
    runtime.join().unwrap();
    handler.join().unwrap();
    assert_eq!(output.status.code(), Some(143));
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["cleanup"], "confirmed_released");
}
