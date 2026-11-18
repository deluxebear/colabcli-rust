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
use common::interrupted_stream::InterruptedTcpStream as TcpStream;
use common::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
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
                return stream.into();
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
    websocket_for(listener, "kernel-1")
}
#[allow(clippy::result_large_err)]
fn websocket_for(listener: &TcpListener, kernel: &str) -> WebSocket<TcpStream> {
    websocket_with_session(listener, kernel).0
}
#[allow(clippy::result_large_err)]
fn websocket_with_session(listener: &TcpListener, kernel: &str) -> (WebSocket<TcpStream>, String) {
    let mut session = String::new();
    let socket = tungstenite::accept_hdr(
        accept(listener),
        |request: &tungstenite::handshake::server::Request,
         response: tungstenite::handshake::server::Response| {
            assert_eq!(
                request.uri().path(),
                format!("/prefix/api/kernels/{kernel}/channels")
            );
            let query: BTreeMap<_, _> =
                url::form_urlencoded::parse(request.uri().query().unwrap().as_bytes())
                    .into_owned()
                    .collect();
            assert_eq!(query["token"], "fixture-proxy");
            assert!(!query["session_id"].is_empty());
            session.clone_from(&query["session_id"]);
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
    .unwrap();
    (socket, session)
}
fn http(listener: &TcpListener, method: &str, path: &str, status: u16, response: Value) -> Value {
    http_inner(listener, method, path, status, response, false)
}
fn http_inner(
    listener: &TcpListener,
    method: &str,
    path: &str,
    status: u16,
    response: Value,
    fetch_content: bool,
) -> Value {
    let mut stream = accept(listener);
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
        assert!(headers.len() < 32768);
    }
    let text = String::from_utf8(headers).unwrap();
    let contents = path.contains("/api/contents/");
    let expected_target = if contents {
        format!(
            "{path}?authuser=0&colab-runtime-proxy-token=fixture-proxy{}",
            if fetch_content { "&content=1" } else { "" }
        )
    } else {
        path.to_owned()
    };
    assert!(
        text.starts_with(&format!("{method} {expected_target} HTTP/1.1\r\n")),
        "{text}"
    );
    let fields: BTreeMap<_, _> = text
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    if contents {
        assert!(!fields.contains_key("authorization"));
    } else {
        assert_eq!(fields["authorization"], "Bearer fixture-proxy");
        assert_eq!(fields["x-colab-runtime-proxy-token"], "fixture-proxy");
        assert_eq!(fields["x-colab-client-agent"], "colab-cli");
    }
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
fn drivefs_automation_uses_private_input_and_never_replays_unknown_mutations() {
    use colabcli_rust::drivefs::remote::{Action, RemoteRequest, prepare};
    for unknown in [false, true] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        let mut original = state(&listener);
        original.kernel_id = Some("normal-k".into());
        original.session_id = Some("normal-session".into());
        let endpoint = original.endpoint.clone();
        let svc = service(&f, original);
        let credentials = serde_json::from_value(json!({"flavor":"imported","user_id":"fixture-gaia",
            "client_id":"fixture-client","client_secret":"private-drive-client-secret",
            "refresh_token":"private-drive-refresh-token","token":"private-drive-access-token",
            "token_uri":"https://oauth2.googleapis.com/token","scopes":[colabcli_rust::drive_credentials::DRIVE_SCOPE]})).unwrap();
        let request = RemoteRequest::new("/content/drive 空格'$(printf x)").unwrap();
        let operation_id = request.operation_id.clone();
        let mut prepared = prepare(Action::Mount, &request, Some(&credentials), &endpoint).unwrap();
        let server = thread::spawn(move || {
            http(
                &listener,
                "POST",
                "/prefix/api/kernels",
                201,
                json!({"id":"drivefs-k"}),
            );
            let mut socket = websocket_for(&listener, "drivefs-k");
            let execute = user_request(&mut socket);
            assert_eq!(execute.content()["allow_stdin"], true);
            let source = execute.content()["code"].as_str().unwrap();
            for secret in [
                "private-drive-client-secret",
                "private-drive-refresh-token",
                "private-drive-access-token",
            ] {
                assert!(!source.contains(secret));
            }
            send(
                &mut socket,
                incoming(
                    "input_request",
                    "stdin",
                    "unrelated-parent",
                    json!({"prompt":"ignore","password":false}),
                ),
            );
            let question = incoming(
                "input_request",
                "stdin",
                execute.id(),
                json!({
                "prompt":format!("COLAB_DRIVE_CONFIG:{operation_id}"),"password":true}),
            );
            send(&mut socket, question.clone());
            let reply = read_message(&mut socket);
            assert_eq!(reply.kind(), "input_reply");
            assert_eq!(reply.envelope["parent_header"], question.envelope["header"]);
            let value: Value =
                serde_json::from_str(reply.content()["value"].as_str().unwrap()).unwrap();
            assert_eq!(value["refresh_token"], "private-drive-refresh-token");
            assert_eq!(value["client_secret"], "private-drive-client-secret");
            assert!(value.get("token").is_none());
            if unknown {
                socket.close(None).unwrap();
            } else {
                send(
                    &mut socket,
                    incoming(
                        "display_data",
                        "iopub",
                        execute.id(),
                        json!({"data":{
                    "application/vnd.colab-cli.drivefs-result+json":{"version":1,"operation_id":operation_id,"ok":true}},"metadata":{}}),
                    ),
                );
                finish(&mut socket, execute.id());
            }
        });
        let mut document = prepared.document().unwrap();
        let reports = execution::execute_document_with_input(
            &svc,
            Some("work"),
            &mut document,
            opts(),
            |_| Ok(()),
            Some(prepared.input.as_mut().unwrap()),
        )
        .unwrap();
        assert_eq!(
            reports[0].status,
            if unknown {
                ExecutionStatus::Unknown
            } else {
                ExecutionStatus::Succeeded
            }
        );
        if unknown {
            let mut retry = prepared.document().unwrap();
            assert!(matches!(
                execution::execute_document_with_input(
                    &svc,
                    Some("work"),
                    &mut retry,
                    opts(),
                    |_| Ok(()),
                    Some(prepared.input.as_mut().unwrap())
                ),
                Err(Error::AutomationPending)
            ));
        }
        server.join().unwrap();
        let sessions = f.storage.sessions(&id("default")).unwrap();
        let state = &sessions["work"];
        assert_eq!(state.kernel_id.as_deref(), Some("normal-k"));
        assert_eq!(state.session_id.as_deref(), Some("normal-session"));
        assert_eq!(state.extra.contains_key("automation_pending"), unknown);
        if unknown {
            assert_eq!(state.extra["automation_pending"]["operation"], "drivefs");
        }
        let history = colabcli_rust::history::History::new(f.storage.clone(), id("default"))
            .read("work", Default::default())
            .unwrap();
        let persisted = format!(
            "{}{}",
            serde_json::to_string(&history).unwrap(),
            serde_json::to_string(&sessions).unwrap()
        );
        for secret in [
            "private-drive-client-secret",
            "private-drive-refresh-token",
            "private-drive-access-token",
        ] {
            assert!(!persisted.contains(secret));
        }
        assert!(persisted.contains("# drivefs operation:"));
        assert!(!persisted.contains("def _colab_cli_drivefs_operation"));
    }
}

#[test]
fn drivefs_probe_can_query_during_unknown_mutation_and_recovers_only_its_kernel() {
    use colabcli_rust::drivefs::{probe, remote::RemoteRequest, result::RESULT_MIME};
    for unknown in [false, true] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        let mut original = state(&listener);
        original.kernel_id = Some("normal-k".into());
        original.session_id = Some("normal-session".into());
        original.running = Some("mutation-message".into());
        let automation = json!({"operation":"drivefs","source_hash":"a".repeat(64),"session_id":"mutation-session",
            "kernel_id":"mutation-k","phase":"unknown","msg_id":"mutation-message","private":"private-journal-field"});
        original
            .extra
            .insert("automation_pending".into(), automation.clone());
        let svc = service(&f, original);
        let request = RemoteRequest::new("/content/drive").unwrap();
        let id1 = request.operation_id.clone();
        let request2 = RemoteRequest::new("/content/drive").unwrap();
        let id2 = request2.operation_id.clone();
        let server = thread::spawn(move || {
            for (index, op) in if unknown {
                vec![(1, id1), (2, id2)]
            } else {
                vec![(1, id1)]
            } {
                if index == 2 {
                    http(
                        &listener,
                        "DELETE",
                        "/prefix/api/kernels/probe-1",
                        204,
                        Value::Null,
                    );
                }
                let kernel = format!("probe-{index}");
                http(
                    &listener,
                    "POST",
                    "/prefix/api/kernels",
                    201,
                    json!({"id":kernel}),
                );
                let mut socket = websocket_for(&listener, &kernel);
                let execute = read_message(&mut socket);
                assert_eq!(execute.content()["allow_stdin"], false);
                assert!(
                    execute.content()["code"]
                        .as_str()
                        .unwrap()
                        .contains("_colab_cli_drivefs_operation")
                );
                if unknown && index == 1 {
                    socket.close(None).unwrap();
                    continue;
                }
                send(
                    &mut socket,
                    incoming(
                        "stream",
                        "iopub",
                        execute.id(),
                        json!({"name":"stderr","text":"private-probe-diagnostic"}),
                    ),
                );
                send(
                    &mut socket,
                    incoming(
                        "display_data",
                        "iopub",
                        execute.id(),
                        json!({"metadata":{},"data":{RESULT_MIME:{
                    "version":1,"operation_id":op,"mountpoint":"/content/drive","ok":true,"stage":"unmanaged","error":null,
                    "mounted":false,"managed":false,"unknown_secret":"private-result-field"}}}),
                    ),
                );
                finish(&mut socket, execute.id());
                http(
                    &listener,
                    "DELETE",
                    &format!("/prefix/api/kernels/{kernel}"),
                    204,
                    Value::Null,
                );
            }
        });
        let first = probe::query(&svc, Some("work"), &request, Duration::from_secs(2), None);
        if unknown {
            assert!(first.is_err());
            let saved = f.storage.sessions(&id("default")).unwrap();
            assert_eq!(saved["work"].extra["drive_probe"]["phase"], "unknown");
            assert_eq!(saved["work"].extra["drive_probe"]["kernel_id"], "probe-1");
            assert!(
                probe::query(&svc, Some("work"), &request2, Duration::from_secs(2), None)
                    .unwrap()
                    .ok
            );
        } else {
            assert!(first.unwrap().ok);
        }
        server.join().unwrap();
        let saved = f.storage.sessions(&id("default")).unwrap();
        assert_eq!(saved["work"].extra["automation_pending"], automation);
        assert_eq!(saved["work"].running.as_deref(), Some("mutation-message"));
        assert_eq!(saved["work"].kernel_id.as_deref(), Some("normal-k"));
        assert_eq!(saved["work"].session_id.as_deref(), Some("normal-session"));
        assert!(!saved["work"].extra.contains_key("drive_probe"));
        let history = colabcli_rust::history::History::new(f.storage.clone(), id("default"))
            .read("work", Default::default())
            .unwrap();
        let history = serde_json::to_string(&history).unwrap();
        for private in [
            "private-probe-diagnostic",
            "private-result-field",
            "private-journal-field",
            "_colab_cli_drivefs_operation",
        ] {
            assert!(!history.contains(private));
        }
    }
}

#[test]
fn drivefs_probe_unknown_creation_requires_explicit_cleanup_and_refuses_foreign_kernels() {
    use colabcli_rust::drivefs::{probe, remote::RemoteRequest};
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let mut original = state(&listener);
    original.kernel_id = Some("normal-k".into());
    original.extra.insert(
        "automation_pending".into(),
        json!({"kernel_id":"mutation-k"}),
    );
    original.extra.insert("drive_probe".into(),json!({"version":1,"endpoint":original.endpoint,"session_id":"probe-session","kernel_id":null,"phase":"creating"}));
    let svc = service(&f, original);
    let request = RemoteRequest::new("/content/drive").unwrap();
    assert!(probe::query(&svc, Some("work"), &request, Duration::from_secs(2), None).is_err());
    for target in [None, Some("normal-k"), Some("mutation-k")] {
        assert!(probe::cleanup(&svc, Some("work"), target, Duration::from_secs(2)).is_err());
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let server = thread::spawn(move || {
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/explicit-probe",
            500,
            json!({"private":"private-error"}),
        );
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/explicit-probe",
            404,
            Value::Null,
        );
    });
    let error = probe::cleanup(
        &svc,
        Some("work"),
        Some("explicit-probe"),
        Duration::from_secs(2),
    )
    .unwrap_err()
    .to_string();
    assert!(!error.contains("private-error"));
    let saved = f.storage.sessions(&id("default")).unwrap();
    assert_eq!(saved["work"].extra["drive_probe"]["phase"], "deleting");
    assert_eq!(
        probe::cleanup(&svc, Some("work"), None, Duration::from_secs(2))
            .unwrap()
            .as_deref(),
        Some("explicit-probe")
    );
    server.join().unwrap();
    let saved = f.storage.sessions(&id("default")).unwrap();
    assert!(!saved["work"].extra.contains_key("drive_probe"));
    assert_eq!(saved["work"].kernel_id.as_deref(), Some("normal-k"));
    assert_eq!(
        saved["work"].extra["automation_pending"]["kernel_id"],
        "mutation-k"
    );
}

#[test]
fn cli_drive_status_routes_the_session_without_drive_credentials_and_keeps_pending_mutations() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let mut original = state(&listener);
    original.kernel_id = Some("normal-k".into());
    original.running = Some("mutation-message".into());
    original.extra.insert("automation_pending".into(),json!({"operation":"drivefs","source_hash":"a".repeat(64),
        "session_id":"mutation-session","kernel_id":"mutation-k","phase":"unknown","msg_id":"mutation-message"}));
    f.storage.save_session(&id("default"), original).unwrap();
    install_cli_token(&f);
    f.accounts
        .add(id("other"), colabcli_rust::models::Account::default())
        .unwrap();
    f.accounts.use_account("other").unwrap();
    let server = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"probe-cli"}),
        );
        let mut socket = websocket_for(&listener, "probe-cli");
        let execute = read_message(&mut socket);
        let source = execute.content()["code"].as_str().unwrap();
        let line = source
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("payload = json.loads(")
                    .and_then(|line| line.strip_suffix(')'))
            })
            .unwrap();
        let encoded: String = serde_json::from_str(line).unwrap();
        let payload: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(payload["action"], "status");
        assert_eq!(payload["request"]["mountpoint"], "/content/drive 空格");
        send(
            &mut socket,
            incoming(
                "display_data",
                "iopub",
                execute.id(),
                json!({"metadata":{},"data":{
            "application/vnd.colab-cli.drivefs-result+json":{"version":1,"operation_id":payload["request"]["operation_id"],
            "mountpoint":"/content/drive 空格","ok":true,"stage":"mounted","error":null,"mounted":true,"managed":true,
            "owner_operation_id":"a".repeat(32),"mode":"shim","helper_alive":true,"helper_pid":42,
            "refresh_status":"invalid_grant","private_detail":"fixture-upstream-secret"}}}),
            ),
        );
        finish(&mut socket, execute.id());
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/probe-cli",
            204,
            Value::Null,
        );
    });
    let output = cli_command(&f, "http://127.0.0.1:1")
        .args([
            "dfs",
            "status",
            "-s",
            "work",
            "--path",
            "/content/drive 空格",
            "--json",
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["mounted"], true);
    assert_eq!(result["refresh_status"], "invalid_grant");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-upstream-secret"));
    assert_eq!(
        output.stderr,
        b"[colab] Routing session 'work' -> account 'default'.\n"
    );
    assert_eq!(
        f.storage.sessions(&id("default")).unwrap()["work"].extra["automation_pending"]["kernel_id"],
        "mutation-k"
    );
    assert!(f.storage.sessions(&id("other")).unwrap().is_empty());
    assert!(
        !f.storage
            .profile_path(&id("default"), "dfs_credentials.json")
            .unwrap()
            .exists()
    );
}

#[test]
fn cli_drive_status_and_empty_probe_cleanup_validate_without_authentication() {
    let f = Fixture::new(TokenStorageMode::File);
    for args in [
        vec!["drive", "status", "--path", "/content/../drive"],
        vec!["drive", "status", "-s", "missing"],
    ] {
        let output = cli_command(&f, "http://127.0.0.1:1")
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("需要登录"));
    }
    f.storage
        .save_session(&id("default"), session("work"))
        .unwrap();
    let output = cli_command(&f, "http://127.0.0.1:1")
        .args(["drive", "probe-cleanup", "-s", "work"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["kernel_id"],
        Value::Null
    );
}

mod drive_lifecycle_tests {
    use super::*;
    use colabcli_rust::{drive_credentials::DriveCredentials, drivefs::lifecycle};
    enum Step {
        Probe,
        Mutation(&'static str, bool),
        Contents,
        DeleteUnknown,
    }
    fn payload(source: &str) -> Value {
        let line = source
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("payload = json.loads(")
                    .and_then(|line| line.strip_suffix(')'))
            })
            .unwrap();
        let encoded: String = serde_json::from_str(line).unwrap();
        serde_json::from_str(&encoded).unwrap()
    }
    fn setup() -> (Fixture, TcpListener, DriveCredentials) {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        let mut original = state(&listener);
        original.kernel_id = Some("normal-k".into());
        original.session_id = Some("normal-session".into());
        f.storage.save_session(&id("default"), original).unwrap();
        install_cli_token(&f);
        f.add("storage");
        f.accounts
            .set_drive(Some(&id("default")), Some(&id("storage")))
            .unwrap();
        let credentials:DriveCredentials=serde_json::from_value(json!({"flavor":"imported","client_id":"fixture-drive-client","client_secret":"private-lifecycle-client-secret",
            "refresh_token":"private-lifecycle-original","token":"private-lifecycle-access","token_uri":"https://oauth2.googleapis.com/token",
            "user_id":"fixture-gaia","email":"fixture@example.invalid","scopes":[colabcli_rust::drive_credentials::DRIVE_SCOPE]})).unwrap();
        f.storage
            .write(
                &f.storage
                    .profile_path(&id("storage"), "dfs_credentials.json")
                    .unwrap(),
                &credentials,
            )
            .unwrap();
        (f, listener, credentials)
    }
    fn serve(
        listener: TcpListener,
        steps: Vec<Step>,
        credentials: DriveCredentials,
        storage: colabcli_rust::storage::Storage,
    ) -> thread::JoinHandle<Vec<String>> {
        serve_for(listener, steps, credentials, storage, id("storage"))
    }
    fn serve_for(
        listener: TcpListener,
        steps: Vec<Step>,
        credentials: DriveCredentials,
        storage: colabcli_rust::storage::Storage,
        drive_account: colabcli_rust::models::ProfileId,
    ) -> thread::JoinHandle<Vec<String>> {
        thread::spawn(move || {
            let identity = lifecycle::identity(&credentials).unwrap();
            let mut owner = None::<String>;
            let mut remote_stage = "unmanaged";
            let mut path = "/content/drive".to_owned();
            let mut mode = json!("shim");
            let mut readonly = false;
            let mut unknown = None::<String>;
            let mut actions = Vec::new();
            let mut remote_config: Value = serde_json::from_str(
                colabcli_rust::drivefs::metadata_config(&credentials)
                    .unwrap()
                    .expose(),
            )
            .unwrap();
            remote_config["refresh_token"] = json!("private-lifecycle-rotated");
            remote_config["scope_confirmed"] = json!(true);
            let raw = serde_json::to_vec_pretty(&remote_config).unwrap();
            for (index, step) in steps.into_iter().enumerate() {
                match step {
                    Step::Contents => {
                        let target = format!(
                            "/prefix/api/contents/content/.colab-cli-drivefs/operations/{}/credentials.json",
                            owner.as_ref().unwrap()
                        );
                        http_inner(
                            &listener,
                            "GET",
                            &target,
                            200,
                            json!({"type":"file","format":"base64","content":STANDARD.encode(&raw),"name":"credentials.json","path":target}),
                            true,
                        );
                        continue;
                    }
                    Step::DeleteUnknown => {
                        http(
                            &listener,
                            "DELETE",
                            &format!("/prefix/api/kernels/{}", unknown.take().unwrap()),
                            204,
                            Value::Null,
                        );
                        continue;
                    }
                    _ => {}
                }
                let kernel = format!("lifecycle-{index}");
                http(
                    &listener,
                    "POST",
                    "/prefix/api/kernels",
                    201,
                    json!({"id":kernel}),
                );
                let mut socket = websocket_for(&listener, &kernel);
                let execute = match &step {
                    Step::Probe => read_message(&mut socket),
                    _ => user_request(&mut socket),
                };
                let submitted = payload(execute.content()["code"].as_str().unwrap());
                let request = &submitted["request"];
                let is_probe = matches!(step, Step::Probe);
                if let Step::Mutation(action, disconnect) = step {
                    assert_eq!(submitted["action"], action);
                    actions.push(action.into());
                    assert!(
                        !execute.content()["code"]
                            .as_str()
                            .unwrap()
                            .contains("private-lifecycle")
                    );
                    if action == "mount" {
                        assert_eq!(execute.content()["allow_stdin"], true);
                        let question = incoming(
                            "input_request",
                            "stdin",
                            execute.id(),
                            json!({"prompt":submitted["prompt"],"password":true}),
                        );
                        send(&mut socket, question.clone());
                        let reply = read_message(&mut socket);
                        assert_eq!(reply.envelope["parent_header"], question.envelope["header"]);
                        let input: Value =
                            serde_json::from_str(reply.content()["value"].as_str().unwrap())
                                .unwrap();
                        let local = storage
                            .read::<DriveCredentials>(
                                &storage
                                    .profile_path(&drive_account, "dfs_credentials.json")
                                    .unwrap(),
                            )
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            input["refresh_token"],
                            local.oauth.refresh_token.as_ref().unwrap().expose()
                        );
                        assert!(input.get("token").is_none());
                        owner = Some(request["operation_id"].as_str().unwrap().into());
                        path = request["mountpoint"].as_str().unwrap().into();
                        mode = request["mode"].clone();
                        readonly = request["readonly"].as_bool().unwrap();
                        if let Some(flags) = request.get("extra_flags") {
                            assert!(
                                flags
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .all(|v| v.as_str().unwrap().starts_with("--"))
                            );
                        }
                        remote_stage = "mounted";
                    } else if action == "umount" {
                        assert_eq!(execute.content()["allow_stdin"], false);
                        assert_eq!(
                            request["owner_operation_id"],
                            owner.as_ref().unwrap().as_str()
                        );
                        remote_stage = "credentials_pending";
                    } else if action == "finalize" {
                        use sha2::{Digest, Sha256};
                        assert_eq!(
                            request["credentials_receipt"],
                            format!("{:x}", Sha256::digest(&raw))
                        );
                        let saved = storage
                            .read::<DriveCredentials>(
                                &storage
                                    .profile_path(&drive_account, "dfs_credentials.json")
                                    .unwrap(),
                            )
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            saved.oauth.refresh_token.as_ref().unwrap().expose(),
                            "private-lifecycle-rotated"
                        );
                        remote_stage = "finished";
                    }
                    if disconnect {
                        unknown = Some(kernel);
                        socket.close(None).unwrap();
                        continue;
                    }
                } else {
                    assert_eq!(submitted["action"], "status");
                    assert_eq!(execute.content()["allow_stdin"], false);
                }
                let mut result = json!({"version":1,"operation_id":request["operation_id"],"mountpoint":request["mountpoint"],"ok":true,
                    "stage":remote_stage,"error":null,"mounted":remote_stage=="mounted","managed":owner.is_some(),"unknown_private":"private-lifecycle-unexpected-result"});
                if let Some(owner) = &owner {
                    result["owner_operation_id"] = json!(owner);
                    result["mode"] = mode.clone();
                    result["readonly"] = json!(readonly);
                    result["identity"] = json!(identity);
                    result["helper_alive"] = json!(remote_stage == "mounted");
                    result["drive_alive"] = json!(remote_stage == "mounted");
                    result["helper_pid"] = json!(1234);
                    result["drive_pid"] = json!(1235);
                }
                assert_eq!(path, request["mountpoint"]);
                send(
                    &mut socket,
                    incoming(
                        "stream",
                        "iopub",
                        execute.id(),
                        json!({"name":"stderr","text":"private-lifecycle-unexpected-stream"}),
                    ),
                );
                send(
                    &mut socket,
                    incoming(
                        "display_data",
                        "iopub",
                        execute.id(),
                        json!({"metadata":{},"data":{"application/vnd.colab-cli.drivefs-result+json":result}}),
                    ),
                );
                finish(&mut socket, execute.id());
                if is_probe {
                    http(
                        &listener,
                        "DELETE",
                        &format!("/prefix/api/kernels/{kernel}"),
                        204,
                        Value::Null,
                    );
                }
            }
            actions
        })
    }
    fn command(f: &Fixture, args: &[&str]) -> std::process::Output {
        cli_command(f, "http://127.0.0.1:1")
            .args(args)
            .output()
            .unwrap()
    }
    fn success(output: &std::process::Output) -> Value {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn assert_history_private(f: &Fixture) {
        let history = colabcli_rust::history::History::new(f.storage.clone(), id("default"))
            .read("work", Default::default())
            .unwrap();
        let all = format!(
            "{}{}",
            serde_json::to_string(&history).unwrap(),
            serde_json::to_string(&f.storage.sessions(&id("default")).unwrap()).unwrap()
        );
        assert!(!all.contains("private-lifecycle"));
        let state = f.storage.sessions(&id("default")).unwrap();
        assert_eq!(state["work"].kernel_id.as_deref(), Some("normal-k"));
        assert_eq!(state["work"].session_id.as_deref(), Some("normal-session"));
    }
    #[test]
    fn explicit_unmount_recovers_retained_cross_account_lease_when_session_journals_are_missing() {
        let (f, listener, credentials) = setup();
        let server = serve(
            listener,
            vec![
                Step::Probe,
                Step::Mutation("mount", false),
                Step::Probe,
                Step::Probe,
                Step::Mutation("umount", false),
                Step::Contents,
                Step::Mutation("finalize", false),
            ],
            credentials,
            f.storage.clone(),
        );
        assert_eq!(
            success(&command(&f, &["drive", "mount", "-s", "work", "--json"]))["stage"],
            "mounted"
        );
        let profile = id("default");
        let mut sessions = f.storage.sessions(&profile).unwrap();
        sessions
            .get_mut("work")
            .unwrap()
            .extra
            .remove("drive_mounts");
        sessions.get_mut("work").unwrap().extra.remove("drive_plan");
        f.storage
            .write(
                &f.storage.profile_path(&profile, "sessions.json").unwrap(),
                &sessions,
            )
            .unwrap();
        let result = success(&command(
            &f,
            &["drive", "umount", "-s", "work", "--purge-state", "--json"],
        ));
        assert_eq!(result["stage"], "finished");
        assert_eq!(server.join().unwrap(), ["mount", "umount", "finalize"]);
        let service = colabcli_rust::drive_credentials::DriveCredentialService::new(
            f.storage.clone(),
            colabcli_rust::auth::TokenClient::new().unwrap(),
        );
        assert!(service.leases(&id("storage")).unwrap().is_empty());
    }

    #[test]
    fn cli_mount_unmount_recovers_rotation_before_cleanup_and_isolates_both_accounts() {
        let (f, listener, credentials) = setup();
        let server = serve(
            listener,
            vec![
                Step::Probe,
                Step::Mutation("mount", false),
                Step::Probe,
                Step::Probe,
                Step::Mutation("umount", false),
                Step::Contents,
                Step::Mutation("finalize", false),
            ],
            credentials,
            f.storage.clone(),
        );
        let preview = success(&command(&f, &["drive", "mount", "-s", "work", "--dry-run"]));
        assert_eq!(preview["timeout"], 30);
        assert_eq!(preview["drive_account"], "storage");
        assert!(
            !serde_json::to_string(&preview)
                .unwrap()
                .contains("private-lifecycle")
        );
        let mounted = success(&command(&f, &["drive", "mount", "-s", "work", "--json"]));
        assert_eq!(mounted["stage"], "mounted");
        let leases = success(&command(&f, &["drive", "leases"]));
        assert_eq!(leases.as_array().unwrap().len(), 1);
        assert!(!leases.to_string().contains("private-lifecycle"));
        assert!(
            !command(&f, &["drive", "token", "--drive-account", "storage"])
                .status
                .success()
        );
        assert!(matches!(
            f.accounts.rename(&id("storage"), &id("renamed")),
            Err(Error::AccountInUse)
        ));
        assert!(matches!(
            f.accounts.remove(&id("default"), true),
            Err(Error::AccountInUse)
        ));
        let result = success(&command(
            &f,
            &["drive", "umount", "-s", "work", "--purge-state", "--json"],
        ));
        assert_eq!(result["stage"], "finished");
        assert_eq!(server.join().unwrap(), ["mount", "umount", "finalize"]);
        assert!(
            success(&command(&f, &["drive", "leases"]))
                .as_array()
                .unwrap()
                .is_empty()
        );
        let saved = f.storage.sessions(&id("default")).unwrap();
        assert!(!saved["work"].extra.contains_key("drive_plan"));
        assert!(!saved["work"].extra.contains_key("drive_mounts"));
        assert_history_private(&f);
    }
    #[test]
    fn cli_force_finishes_old_token_recovery_before_replacement_and_reuses_healthy_mount() {
        let (f, listener, credentials) = setup();
        let server = serve(
            listener,
            vec![
                Step::Probe,
                Step::Mutation("mount", false),
                Step::Probe,
                Step::Probe,
                Step::Probe,
                Step::Mutation("umount", false),
                Step::Contents,
                Step::Mutation("finalize", false),
                Step::Mutation("mount", false),
            ],
            credentials,
            f.storage.clone(),
        );
        let original = success(&command(&f, &["drive", "mount", "-s", "work"]));
        let reused = success(&command(&f, &["drive", "mount", "-s", "work"]));
        assert_eq!(original["owner_operation_id"], reused["owner_operation_id"]);
        let replacement = success(&command(
            &f,
            &["drive", "mount", "-s", "work", "--force", "--readonly"],
        ));
        assert_ne!(
            original["owner_operation_id"],
            replacement["owner_operation_id"]
        );
        assert_eq!(replacement["readonly"], true);
        assert_eq!(
            server.join().unwrap(),
            ["mount", "umount", "finalize", "mount"]
        );
        assert_eq!(
            success(&command(&f, &["drive", "leases"]))
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_history_private(&f);
    }
    #[test]
    fn cli_environment_mount_preserves_rotated_token_on_retry_and_passes_extra_flags() {
        let (f, listener, mut credentials) = setup();
        credentials.oauth.refresh_token = Some(colabcli_rust::models::Secret::new(
            "private-lifecycle-env-source",
        ));
        let server = serve(
            listener,
            vec![
                Step::Probe,
                Step::Mutation("mount", false),
                Step::Probe,
                Step::Probe,
                Step::Mutation("umount", false),
                Step::Contents,
                Step::Mutation("finalize", false),
            ],
            credentials,
            f.storage.clone(),
        );
        let source_path = f
            .storage
            .profile_path(&id("storage"), "dfs_credentials.json")
            .unwrap();
        let before = fs::read(&source_path).unwrap();
        let mount = || {
            let mut c = cli_command(&f, "http://127.0.0.1:1");
            c.args([
                "drive",
                "mount",
                "-s",
                "work",
                "--from-env",
                "--via-cli",
                "--drive-extra=--curl_logging",
                "--drive-extra=--example_value=quote' 中文;$(ignored)",
            ])
            .env("COLAB_DRIVE_REFRESH_TOKEN", "private-lifecycle-env-source")
            .env("COLAB_DRIVE_CLIENT_ID", "fixture-drive-client")
            .env(
                "COLAB_DRIVE_CLIENT_SECRET",
                "private-lifecycle-client-secret",
            )
            .env("COLAB_DRIVE_USER_ID", "fixture-gaia")
            .env("COLAB_DRIVE_EMAIL", "fixture@example.invalid")
            .env(
                "COLAB_DRIVE_SCOPES",
                colabcli_rust::drive_credentials::DRIVE_SCOPE,
            );
            c
        };
        let preview = success(&mount().arg("--dry-run").output().unwrap());
        assert_eq!(preview["transport"], "native");
        assert_eq!(preview["extra_flags"].as_array().unwrap().len(), 2);
        assert_eq!(fs::read(&source_path).unwrap(), before);
        success(&mount().output().unwrap());
        success(&command(&f, &["drive", "umount", "-s", "work"]));
        // Reusing the same env after rotation selects the adopted store without rewriting it.
        success(&mount().arg("--dry-run").output().unwrap());
        let saved: DriveCredentials = f.storage.read(&source_path).unwrap().unwrap();
        assert_eq!(
            saved.oauth.refresh_token.unwrap().expose(),
            "private-lifecycle-rotated"
        );
        assert_eq!(server.join().unwrap(), ["mount", "umount", "finalize"]);
        assert_history_private(&f);
    }
    #[test]
    fn cli_compute_token_mount_uses_compute_profile_and_never_rewrites_compute_oauth() {
        let (f, listener, credentials) = setup();
        f.tokens.save(&id("default"), &credentials.oauth).unwrap();
        // The compute token is only a source copy. A long-lived access token keeps this test offline.
        let mut oauth = credentials.oauth.clone();
        oauth.token = colabcli_rust::models::Secret::new("private-compute-access");
        oauth.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
        f.tokens.save(&id("default"), &oauth).unwrap();
        let compute_path = f
            .storage
            .profile_path(&id("default"), "oauth_token.json")
            .unwrap();
        let drive_path = f
            .storage
            .profile_path(&id("storage"), "dfs_credentials.json")
            .unwrap();
        let compute_before = fs::read(&compute_path).unwrap();
        let drive_before = fs::read(&drive_path).unwrap();
        let server = serve_for(
            listener,
            vec![
                Step::Probe,
                Step::Mutation("mount", false),
                Step::Probe,
                Step::Probe,
                Step::Mutation("umount", false),
                Step::Contents,
                Step::Mutation("finalize", false),
            ],
            credentials,
            f.storage.clone(),
            id("default"),
        );
        let result = success(&command(
            &f,
            &[
                "drive",
                "mount",
                "-s",
                "work",
                "--cli-token",
                "--mode",
                "rt",
                "--user-id",
                "fixture-gaia",
                "--email",
                "fixture@example.invalid",
            ],
        ));
        assert_eq!(result["mode"], "shim");
        success(&command(&f, &["drive", "umount", "-s", "work"]));
        assert_eq!(server.join().unwrap(), ["mount", "umount", "finalize"]);
        assert_eq!(fs::read(&compute_path).unwrap(), compute_before);
        assert_eq!(fs::read(&drive_path).unwrap(), drive_before);
        let copied: DriveCredentials = f
            .storage
            .read(
                &f.storage
                    .profile_path(&id("default"), "dfs_credentials.json")
                    .unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            copied.oauth.refresh_token.unwrap().expose(),
            "private-lifecycle-rotated"
        );
        assert_history_private(&f);
    }
    #[test]
    fn drivemount_defaults_to_bound_drivefs_and_uses_the_same_rotation_cleanup() {
        let (f, listener, credentials) = setup();
        let server = serve(
            listener,
            vec![
                Step::Probe,
                Step::Mutation("mount", false),
                Step::Probe,
                Step::Probe,
                Step::Mutation("umount", false),
                Step::Contents,
                Step::Mutation("finalize", false),
            ],
            credentials,
            f.storage.clone(),
        );
        let result = success(&command(
            &f,
            &[
                "drivemount",
                "-s",
                "work",
                "--readonly",
                "--no-fallback",
                "--json",
            ],
        ));
        assert_eq!(result["readonly"], true);
        assert_eq!(result["stage"], "mounted");
        success(&command(&f, &["drive", "umount", "-s", "work"]));
        assert_eq!(server.join().unwrap(), ["mount", "umount", "finalize"]);
        assert_history_private(&f);
    }
    #[test]
    fn drivemount_unknown_result_keeps_drivefs_plan_and_never_switches_to_upstream() {
        let (f, listener, credentials) = setup();
        let server = serve(
            listener,
            vec![Step::Probe, Step::Mutation("mount", true)],
            credentials,
            f.storage.clone(),
        );
        let output = command(&f, &["drivemount", "-s", "work"]);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("交互挂载流程"));
        assert_eq!(server.join().unwrap(), ["mount"]);
        let saved = f.storage.sessions(&id("default")).unwrap();
        assert_eq!(
            saved["work"].extra["automation_pending"]["operation"],
            "drivefs"
        );
        assert!(saved["work"].extra.contains_key("drive_plan"));
        assert_history_private(&f);
    }
    #[test]
    fn official_drivemount_rechecks_native_ownership_under_the_shared_operation_lock() {
        let (f, listener, _) = setup();
        f.accounts.set_drive(Some(&id("default")), None).unwrap();
        let auth = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut token = f.tokens.load(&id("default")).unwrap().unwrap();
        token.expiry = None;
        token.token_uri = format!("http://{}/token", auth.server_addr());
        f.tokens.save(&id("default"), &token).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let request = auth.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(request.method().as_str(), "POST");
            ready_tx.send(()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            request
                .respond(tiny_http::Response::from_string(
                    json!({"access_token":"fresh-compute","expires_in":3600,"token_type":"Bearer"})
                        .to_string(),
                ))
                .unwrap();
        });
        let mut cmd = cli_command(&f, "http://127.0.0.1:1");
        cmd.args(["drivemount", "-s", "work", "--upstream", "--json"]);
        let child = thread::spawn(move || cmd.output().unwrap());
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut state = f
            .storage
            .sessions(&id("default"))
            .unwrap()
            .remove("work")
            .unwrap();
        let guard = lifecycle::operation_lock(&f.storage, &id("default"), &state.endpoint).unwrap();
        finish_tx.send(()).unwrap();
        state.extra.insert(
            "drive_plan".into(),
            json!({"request":{"mountpoint":"/content/drive"}}),
        );
        // This write takes the compute lifecycle lock after refresh releases it;
        // the operation lock keeps the official strategy from starting a kernel.
        f.storage.save_session(&id("default"), state).unwrap();
        drop(guard);
        let result = child.join().unwrap();
        server.join().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("不能切换官方挂载策略"));
        assert!(
            matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
        );
        let saved = f.storage.sessions(&id("default")).unwrap();
        assert!(!saved["work"].extra.contains_key("automation_pending"));
        assert!(saved["work"].extra.contains_key("drive_plan"));
    }
    #[test]
    fn cli_mount_after_lost_reply_observes_and_never_resubmits_mount_or_credentials() {
        let (f, listener, credentials) = setup();
        let server = serve(
            listener,
            vec![
                Step::Probe,
                Step::Mutation("mount", true),
                Step::Probe,
                Step::DeleteUnknown,
                Step::Probe,
            ],
            credentials,
            f.storage.clone(),
        );
        assert!(
            !command(&f, &["drive", "mount", "-s", "work"])
                .status
                .success()
        );
        let saved = f.storage.sessions(&id("default")).unwrap();
        assert_eq!(saved["work"].extra["drive_plan"]["phase"], "dispatching");
        assert!(saved["work"].extra["automation_pending"]["drivefs_operation_id"].is_string());
        let result = success(&command(&f, &["drive", "mount", "-s", "work"]));
        assert_eq!(result["stage"], "mounted");
        assert_eq!(server.join().unwrap(), ["mount"]);
        assert_history_private(&f);
    }
    #[test]
    fn cli_unmount_after_lost_reply_adopts_frozen_token_without_repeating_unmount() {
        let (f, listener, credentials) = setup();
        let server = serve(
            listener,
            vec![
                Step::Probe,
                Step::Mutation("mount", false),
                Step::Probe,
                Step::Probe,
                Step::Mutation("umount", true),
                Step::Probe,
                Step::DeleteUnknown,
                Step::Probe,
                Step::Contents,
                Step::Mutation("finalize", false),
            ],
            credentials,
            f.storage.clone(),
        );
        success(&command(&f, &["drive", "mount", "-s", "work"]));
        assert!(
            !command(&f, &["drive", "umount", "-s", "work"])
                .status
                .success()
        );
        assert_eq!(
            success(&command(&f, &["drive", "umount", "-s", "work"]))["stage"],
            "finished"
        );
        assert_eq!(server.join().unwrap(), ["mount", "umount", "finalize"]);
        assert_history_private(&f);
    }
    #[test]
    fn cli_finalize_after_lost_reply_keeps_adopted_rotation_and_only_confirms_completion() {
        let (f, listener, credentials) = setup();
        let server = serve(
            listener,
            vec![
                Step::Probe,
                Step::Mutation("mount", false),
                Step::Probe,
                Step::Probe,
                Step::Mutation("umount", false),
                Step::Contents,
                Step::Mutation("finalize", true),
                Step::Probe,
                Step::DeleteUnknown,
                Step::Probe,
            ],
            credentials,
            f.storage.clone(),
        );
        success(&command(&f, &["drive", "mount", "-s", "work"]));
        assert!(
            !command(&f, &["drive", "umount", "-s", "work"])
                .status
                .success()
        );
        let saved = f.storage.sessions(&id("default")).unwrap();
        assert!(saved["work"].extra["drive_plan"]["receipt"].is_string());
        assert_eq!(
            success(&command(&f, &["drive", "umount", "-s", "work"]))["stage"],
            "finished"
        );
        assert_eq!(server.join().unwrap(), ["mount", "umount", "finalize"]);
        assert_history_private(&f);
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
    const IDLE_TIMEOUT: Duration = Duration::from_secs(1);
    const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let handler = thread::spawn(move || {
        let mut socket = websocket(&listener);
        let request = read_message(&mut socket);
        for _ in 0..6 {
            thread::sleep(PROGRESS_INTERVAL);
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
        .execute("work()", ExecutionTimeout::Idle(Some(IDLE_TIMEOUT)), |_| {
            Ok(())
        })
        .unwrap();
    assert!(report.succeeded(), "{report:?}");
    assert!(started.elapsed() > IDLE_TIMEOUT);
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
    assert!(!report.timed_out);
    assert!(!report.msg_id.is_empty());
    handler.join().unwrap();
}
#[test]
fn parentless_kernel_restart_aborts_waiting_and_error_reply_without_output_is_visible() {
    for broadcast in [Some("restarting"), Some("dead"), None] {
        let listener = listener();
        let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
        let handler = thread::spawn(move || {
            let mut socket = websocket(&listener);
            let request = read_message(&mut socket);
            if let Some(status) = broadcast {
                let mut message =
                    incoming("status", "iopub", "", json!({"execution_state":status}));
                message.envelope["parent_header"] = json!({});
                send(&mut socket, message);
            } else {
                // Only iopub status broadcasts can terminate an active execution.
                send(
                    &mut socket,
                    incoming(
                        "status",
                        "shell",
                        request.id(),
                        json!({"execution_state":"dead"}),
                    ),
                );
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
        if broadcast.is_some() {
            assert_eq!(report.status, ExecutionStatus::Unknown);
            assert!(report.error.as_deref().unwrap().contains("尚未确认"));
        } else {
            assert_eq!(report.status, ExecutionStatus::Failed);
            assert_eq!(report.outputs[0].data["ename"], "Oops");
        }
        handler.join().unwrap();
    }
}
#[test]
fn kernel_death_broadcast_retains_partial_output_and_durable_replay_guard() {
    for status in ["dead", "restarting"] {
        for compatible in [false, true] {
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
                let mut socket = websocket(&listener);
                let request = user_request(&mut socket);
                send(
                    &mut socket,
                    incoming(
                        "stream",
                        "iopub",
                        request.id(),
                        json!({"name":"stdout","text":"side-effect-started\n"}),
                    ),
                );
                let mut broadcast =
                    incoming("status", "iopub", "", json!({"execution_state":status}));
                broadcast.envelope["parent_header"] = json!({});
                send(&mut socket, broadcast);
                request.id().to_owned()
            });
            let mut document = Document::from_stdin("mutate_then_die()".as_bytes(), &[]).unwrap();
            if compatible {
                document.use_official_output();
            }
            let results =
                execution::execute_document(&svc, Some("work"), &mut document, opts(), |_| Ok(()))
                    .unwrap();
            let message = handler.join().unwrap();
            assert_eq!(results[0].status, ExecutionStatus::Unknown);
            assert_eq!(results[0].outputs[0].data["text"], "side-effect-started\n");
            let saved = f.storage.sessions(&id("default")).unwrap();
            if compatible {
                assert_eq!(saved["work"].extra["execution_pending"], message);
            } else {
                assert_eq!(saved["work"].running.as_deref(), Some(message.as_str()));
            }
            assert!(matches!(
                execution::execute_document(&svc, Some("work"), &mut document, opts(), |_| Ok(())),
                Err(Error::ExecutionPending(id)) if id == message
            ));
        }
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
        let (mut first, first_wire_session) = websocket_with_session(&listener, "kernel-1");
        let saved = storage.sessions(&id("default")).unwrap();
        assert_eq!(saved["work"].kernel_id.as_deref(), Some("kernel-1"));
        assert_eq!(
            saved["work"].session_id.as_deref(),
            Some(first_wire_session.as_str())
        );
        let request = user_request(&mut first);
        assert!(request.content()["code"].as_str().unwrap().contains("x=7"));
        let first_session = request.envelope["header"]["session"].clone();
        assert_eq!(first_session, first_wire_session);
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
        let (mut second, second_wire_session) = websocket_with_session(&listener, "kernel-1");
        assert_eq!(second_wire_session, first_wire_session);
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
    let original = serde_json::to_vec(&json!({"nbformat":4,"nbformat_minor":4,"metadata":{"custom":7},"extra":"keep","cells":[
        {"cell_type":"markdown","id":"kept_ID","source":["# Title"],"metadata":{}},
        {"cell_type":"code","id":"kept_ID","source":["print('partial')"],"metadata":{},"outputs":[],"execution_count":null},
        {"cell_type":"code","id":"bad id","source":"must_not_run()","metadata":{},"outputs":[],"execution_count":null},
        {"cell_type":"raw","source":"notes","metadata":{}}]})).unwrap();
    fs::write(&file, &original).unwrap();
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
    assert_eq!(saved["nbformat_minor"], 5);
    assert_eq!(fs::read(&file).unwrap(), original);
    assert_eq!(saved["metadata"]["custom"], 7);
    assert_eq!(saved["cells"][0]["source"], json!(["# Title"]));
    assert_eq!(saved["cells"][1]["outputs"][0]["text"], "partial");
    assert_eq!(
        saved["cells"][1]["metadata"]["colab_cli_execution"]["status"],
        "unknown"
    );
    let ids: std::collections::HashSet<_> = saved["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cell| cell["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4);
    assert_eq!(saved["cells"][0]["id"], "kept_ID");
    assert!(!ids.contains("bad id"));
    assert!(ids.iter().all(|id| {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }));
    let events = colabcli_rust::history::History::new(f.storage.clone(), id("default"))
        .read(
            "work",
            colabcli_rust::history::HistoryQuery {
                event_type: Some("execution"),
                lines: None,
            },
        )
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["cell_index"], 1);
    assert_eq!(events[0]["cell_id"], saved["cells"][1]["id"]);
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
        json!({"nbformat":4,"nbformat_minor":-1,"cells":[]}),
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
fn cli_exec_errors_follow_text_and_json_exit_contracts() {
    for as_json in [false, true] {
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
            .args(if as_json { vec!["--json"] } else { vec![] })
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
        assert_eq!(output.status.success(), !as_json);
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        if as_json {
            let results: Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(results[0]["status"], "failed");
        } else {
            assert_eq!(stdout, "hello\n");
            assert!(stderr.contains("warning"));
            assert!(stderr.contains("ValueError: fixture-failure"));
        }
        assert!(!stderr.contains("fixture-proxy"));
        assert!(!stderr.contains("fixture-access"));
    }
}

#[test]
fn cli_exec_silent_timeout_preserves_pending_id_and_zero_allows_completion() {
    for (timeout_arg, timed_out, json_output) in [
        ("0.75", true, true),
        ("0.75", true, false),
        ("0", false, true),
    ] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        let base = format!("http://{}", listener.local_addr().unwrap());
        f.storage
            .save_session(&id("default"), state(&listener))
            .unwrap();
        install_cli_token(&f);
        let source = f.temp.path().join("silent.py");
        fs::write(&source, "silent_work()\n").unwrap();
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
            assert_eq!(request.content()["code"], "silent_work()\n");
            if timed_out {
                // Let the client close the silent connection after its own deadline.
                let _ = socket.read();
            } else {
                thread::sleep(Duration::from_secs(1));
                finish(&mut socket, request.id());
            }
        });
        let output = cli_command(&f, &base)
            .args(["exec", "-s", "work", "--timeout", timeout_arg])
            .args(json_output.then_some("--json"))
            .arg("-f")
            .arg(&source)
            .output()
            .unwrap();
        handler.join().unwrap();

        assert_eq!(output.status.success(), !timed_out);
        let sessions = f.storage.sessions(&id("default")).unwrap();
        let session = &sessions["work"];
        let stderr = String::from_utf8(output.stderr).unwrap();
        if !json_output {
            assert!(output.stdout.is_empty());
            assert!(
                stderr.contains("等待本次执行消息超时（空闲期限 0.75 秒）。可用 --timeout 调整。")
            );
            assert!(stderr.contains("执行结果未知"));
            let pending = session.extra["execution_pending"].as_str().unwrap();
            assert!(stderr.contains(pending));
            continue;
        }
        let results: Value = serde_json::from_slice(&output.stdout).unwrap();
        let pending = session.running.as_deref();
        assert!(results[0].get("timed_out").is_none());
        if timed_out {
            assert_eq!(results[0]["status"], "unknown");
            assert!(results[0]["error"].as_str().unwrap().contains("超时"));
            assert_eq!(pending, results[0]["msg_id"].as_str());
            assert!(!stderr.contains("可用 --timeout 调整"));
        } else {
            assert_eq!(results[0]["status"], "succeeded");
            assert!(pending.is_none());
        }
    }
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
        // Keep relevant output flowing beyond the deadline. Send the first
        // frame immediately; scheduling delay must not consume an 80ms window.
        for _ in 0..100 {
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
            thread::sleep(Duration::from_millis(20));
        }
    });
    let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "s").unwrap();
    let result = runtime
        .execute(
            "pass",
            ExecutionTimeout::Deadline(Some(Duration::from_millis(400))),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Unknown);
    assert!(result.timed_out);
    assert!(!result.outputs.is_empty());
    // EOF also produces Unknown; it must not satisfy this deadline test.
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("超时"))
    );
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
            json!({"data":{"image/png":format!("{}\n", STANDARD.encode(b"fake-png")),"text/plain":"plot"},"metadata":{},"transient":{"display_id":"plot-1"}}),
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
    let images = execution::save_images(&results[0].outputs, f.temp.path()).unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(fs::read(&images[0]).unwrap(), b"fake-png");
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
    let runtime_url = state.url.clone();
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
        http(
            &runtime_listener,
            "DELETE",
            "/prefix/api/kernels/kernel-1",
            204,
            Value::Null,
        );
        stop_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        socket.close(None).unwrap();
    });
    let control_handler =
        thread::spawn(move || {
            for (method, path, body) in [
                (
                    "GET",
                    "/tun/m/assignments",
                    release_inventory("ep-work", &runtime_url),
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
                assert!(r.headers().iter().any(|h| h.field.equiv("Authorization")
                    && h.value.as_str() == "Bearer control-oauth"));
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
fn stale_execution_checkpoint_preserves_independently_added_recovery_records() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let svc = service(&f, state(&listener));
    svc.with_execution_session(None, |initial, checkpoint| {
        // Interleave an account-locked writer after the execution snapshot.
        svc.with_session_checkpoints(None, |current, save| {
            let mut changed = current.clone();
            changed
                .extra
                .insert("drive_plan".into(), json!({"phase":"dispatching"}));
            changed
                .extra
                .insert("drive_mounts".into(), json!({"/content/owned":"lease"}));
            changed
                .extra
                .insert("drive_probe".into(), json!({"phase":"unknown"}));
            save(changed)
        })?;
        let mut updated = initial.clone();
        updated.running = Some("owned-execution".into());
        checkpoint(updated.clone())?;
        updated.running = None;
        checkpoint(updated)?;
        let current = f.storage.sessions(&id("default"))?.remove("work").unwrap();
        assert_eq!(current.extra["drive_plan"], json!({"phase":"dispatching"}));
        assert_eq!(
            current.extra["drive_mounts"],
            json!({"/content/owned":"lease"})
        );
        assert_eq!(current.extra["drive_probe"], json!({"phase":"unknown"}));
        assert!(current.running.is_none());
        Ok(())
    })
    .unwrap();
}

#[test]
fn execution_checkpoint_detects_recovery_conflicts_and_advances_only_after_commit() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let mut original = state(&listener);
    original
        .extra
        .insert("drive_plan".into(), json!({"phase":"prepared"}));
    let svc = service(&f, original);
    svc.with_execution_session(None, |initial, checkpoint| {
        let mut updated = initial.clone();
        updated
            .extra
            .insert("drive_plan".into(), json!({"phase":"dispatching"}));
        checkpoint(updated.clone())?;
        svc.with_session_checkpoints(None, |current, save| {
            let mut changed = current.clone();
            changed
                .extra
                .insert("drive_plan".into(), json!({"phase":"observed"}));
            save(changed)
        })?;
        // An unchanged field belongs to the other writer and must survive.
        updated.running = Some("message".into());
        checkpoint(updated.clone())?;
        let before = f.storage.sessions(&id("default"))?;
        updated.extra.remove("drive_plan");
        updated.running = None;
        assert!(matches!(checkpoint(updated), Err(Error::KernelProtocol(_))));
        assert_eq!(
            serde_json::to_value(f.storage.sessions(&id("default"))?).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        Ok(())
    })
    .unwrap();
    // A fresh owner may explicitly remove its acknowledged recovery record.
    svc.with_execution_session(None, |initial, checkpoint| {
        let mut updated = initial.clone();
        updated.extra.remove("drive_plan");
        checkpoint(updated.clone())?;
        checkpoint(updated)
    })
    .unwrap();
    assert!(
        !f.storage.sessions(&id("default")).unwrap()["work"]
            .extra
            .contains_key("drive_plan")
    );
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

fn release_inventory(endpoint: &str, runtime_url: &str) -> Value {
    json!({"assignments": [{"endpoint": endpoint, "accelerator": "NONE", "variant": 0, "machineShape": 0,
        "runtimeProxyInfo": {"url": runtime_url, "token": "fixture-proxy", "tokenExpiresInSeconds": 3600}}]})
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
    assert_eq!(request.method().as_str(), "GET");
    assert!(request.url().starts_with("/tun/m/assignments?"));
    request
        .respond(tiny_http::Response::from_string(
            release_inventory("ep-run", &runtime_url).to_string(),
        ))
        .unwrap();
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
#[test]
fn explicit_runtime_uses_valid_proxy_without_opening_control_credentials_but_refresh_requires_auth()
{
    for fresh in [true, false] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        let mut saved = state(&listener);
        saved.kernel_id = Some("kernel-1".into());
        saved.session_id = Some("fixture-session".into());
        if !fresh {
            saved.token_expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        }
        f.storage.save_session(&id("default"), saved).unwrap();
        let handler = fresh.then(|| {
            thread::spawn(move || {
                http(
                    &listener,
                    "GET",
                    "/prefix/api/kernels/kernel-1",
                    200,
                    json!({"id":"kernel-1"}),
                );
                let mut socket = websocket(&listener);
                let execute = user_request(&mut socket);
                finish(&mut socket, execute.id());
            })
        });
        let script = f.temp.path().join("no-control-auth.py");
        fs::write(&script, "pass\n").unwrap();
        let result = cli_command(&f, "http://127.0.0.1:1")
            .args(["exec", "-s", "work", "--json", "-f"])
            .arg(&script)
            .output()
            .unwrap();
        if fresh {
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let rows: Value = serde_json::from_slice(&result.stdout).unwrap();
            assert_eq!(rows[0]["status"], "succeeded");
            handler.unwrap().join().unwrap();
        } else {
            assert!(!result.status.success());
            assert!(String::from_utf8_lossy(&result.stderr).contains("授权"));
        }
    }
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
    for name in [
        "COLAB_DRIVE_DEFAULT",
        "COLAB_DFS_DEFAULT",
        "COLAB_DRIVE_REFRESH_TOKEN",
        "COLAB_DRIVE_CLIENT_ID",
        "COLAB_DRIVE_CLIENT_SECRET",
        "COLAB_DRIVE_TOKEN_URI",
        "COLAB_DRIVE_SCOPES",
        "COLAB_DRIVE_USER_ID",
        "COLAB_DRIVE_EMAIL",
        "COLAB_DRIVE_ACCOUNT",
        "COLAB_DFS_ACCOUNT",
        "DFS_UID",
        "DFS_EMAIL",
    ] {
        cmd.env_remove(name);
    }
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
        "\u{feff}#!/usr/bin/env colab run\r\nfrom __future__ import annotations\nimport sys\nsys.exit(7)\n",
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
        assert_eq!(
            data["source"].as_str().unwrap(),
            "\r\nfrom __future__ import annotations\nimport sys\nsys.exit(7)\n"
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

#[test]
fn compatible_unknown_execution_keeps_replay_guard_and_restart_clears_it() {
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
        let mut socket = websocket(&listener);
        let _request = user_request(&mut socket);
        socket.close(None).unwrap();
        http(
            &listener,
            "POST",
            "/prefix/api/kernels/kernel-1/restart",
            200,
            json!({"id":"kernel-1"}),
        );
    });
    let mut document = Document::from_stdin(b"maybe_runs()".as_slice(), &[]).unwrap();
    document.use_official_output();
    let reports =
        execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())).unwrap();
    assert_eq!(reports[0].status, ExecutionStatus::Unknown);
    let state = f.storage.sessions(&id("default")).unwrap();
    assert!(state["work"].running.is_none());
    assert_eq!(state["work"].extra["execution_pending"], reports[0].msg_id);
    let mut again = Document::from_stdin(b"must_not_replay()".as_slice(), &[]).unwrap();
    assert!(matches!(
        execution::execute_document(&svc, None, &mut again, opts(), |_| Ok(())),
        Err(Error::ExecutionPending(_))
    ));
    execution::restart_kernel(&svc, Some("work"), Duration::from_secs(2)).unwrap();
    assert!(
        f.storage.sessions(&id("default")).unwrap()["work"]
            .extra
            .get("execution_pending")
            .is_none()
    );
    handler.join().unwrap();
}

#[test]
fn interactive_input_routes_replies_and_services_ping_without_recording_values() {
    use colabcli_rust::jupyter::InputRequest;
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let handler = thread::spawn(move || {
        let mut socket = websocket(&listener);
        let execute = read_message(&mut socket);
        assert_eq!(execute.content()["allow_stdin"], true);
        send(
            &mut socket,
            incoming(
                "input_request",
                "stdin",
                "other-execution",
                json!({"prompt":"ignore", "password":false}),
            ),
        );
        for password in [false, true] {
            let request = incoming(
                "input_request",
                "stdin",
                execute.id(),
                json!({"prompt":"请输入：", "password":password}),
            );
            send(&mut socket, request.clone());
            socket
                .send(Message::Ping(b"heartbeat".to_vec().into()))
                .unwrap();
            // The polling handler intentionally waits for a round trip before returning input.
            match socket.read().unwrap() {
                Message::Pong(bytes) => assert_eq!(&bytes[..], b"heartbeat"),
                other => panic!("expected PONG while waiting for input: {other:?}"),
            }
            let reply = read_message(&mut socket);
            assert_eq!(reply.kind(), "input_reply");
            assert_eq!(reply.channel(), "stdin");
            assert_eq!(reply.envelope["parent_header"], request.envelope["header"]);
            assert_eq!(reply.envelope["header"]["session"], "interactive-session");
            assert_eq!(
                reply.content()["value"],
                if password {
                    "private-test-password"
                } else {
                    "你好"
                }
            );
        }
        finish(&mut socket, execute.id());
    });
    let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "interactive-session").unwrap();
    let mut active = String::new();
    let mut began = Instant::now();
    let mut requests = 0;
    let mut input = |request: &InputRequest| {
        if request.id != active {
            active = request.id.clone();
            began = Instant::now();
            requests += 1;
            assert_eq!(request.prompt, "请输入：");
        }
        Ok((began.elapsed() >= Duration::from_millis(150)).then(|| {
            if request.password {
                "private-test-password".into()
            } else {
                "你好".into()
            }
        }))
    };
    let result = runtime
        .execute_with_input(
            "input()",
            ExecutionTimeout::Deadline(Some(Duration::from_secs(3))),
            |_| Ok(()),
            |_| Ok(()),
            Some(&mut input),
        )
        .unwrap();
    handler.join().unwrap();
    assert!(result.succeeded());
    assert_eq!(requests, 2);
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("private-test-password")
    );
}

#[test]
fn waiting_for_input_observes_deadlines_cancellation_and_redacts_handler_errors() {
    use colabcli_rust::jupyter::InputRequest;
    for mode in [
        "timeout",
        "cancel",
        "error",
        "noninteractive",
        "malformed",
        "overlap",
        "premature",
    ] {
        let listener = listener();
        let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let flag = cancel.clone();
        let server = thread::spawn(move || {
            let mut socket = websocket(&listener);
            let execute = read_message(&mut socket);
            send(
                &mut socket,
                incoming(
                    "input_request",
                    "stdin",
                    execute.id(),
                    json!({"prompt":if mode == "malformed" {Value::Null} else {json!("Password: ")},"password":true}),
                ),
            );
            if mode == "cancel" {
                flag.store(2, std::sync::atomic::Ordering::SeqCst);
            }
            if mode == "overlap" {
                send(
                    &mut socket,
                    incoming(
                        "input_request",
                        "stdin",
                        execute.id(),
                        json!({"prompt":"second", "password":false}),
                    ),
                );
            }
            if mode == "premature" {
                finish(&mut socket, execute.id());
            }
            // Every mode must end without a submitted input value.
            let _ = socket.read();
        });
        let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "interactive-session").unwrap();
        runtime.set_cancellation(Some(cancel));
        let mut input = |_: &InputRequest| -> colabcli_rust::Result<Option<String>> {
            if mode == "error" {
                return Err(Error::ExecutionPending("secret-input-must-not-leak".into()));
            }
            Ok(None)
        };
        let result = runtime
            .execute_with_input(
                "input()",
                ExecutionTimeout::Deadline(Some(Duration::from_millis(250))),
                |_| Ok(()),
                |_| Ok(()),
                if mode == "noninteractive" {
                    None
                } else {
                    Some(&mut input)
                },
            )
            .unwrap();
        assert_eq!(result.status, ExecutionStatus::Unknown);
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("secret-input-must-not-leak")
        );
        assert!(
            runtime
                .execute(
                    "must_not_replay()",
                    ExecutionTimeout::Idle(None),
                    |_| Ok(())
                )
                .is_err()
        );
        runtime.close();
        server.join().unwrap();
    }
}

#[test]
fn interactive_exec_rejects_missing_file_or_nonterminal_before_authentication() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("input.py");
    fs::write(&script, "input('prompt: ')").unwrap();
    for file in [None, Some(&script)] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_colab"));
        cmd.arg("--config-dir")
            .arg(temp.path().join("config"))
            .args(["--token-storage", "file", "exec", "--allow-stdin"])
            .stdin(Stdio::null());
        if let Some(file) = file {
            cmd.arg("-f").arg(file);
        }
        let output = cmd.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(if file.is_some() { 1 } else { 2 })
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("需要登录"));
        assert!(
            !temp
                .path()
                .join("config/accounts/default/sessions.json")
                .exists()
        );
    }
}

#[cfg(unix)]
#[test]
fn cli_interactive_password_uses_tty_keeps_json_clean_and_does_not_log_reply() {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    };
    for command in ["exec", "auth", "drivemount"] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        f.storage
            .save_session(&id("default"), state(&listener))
            .unwrap();
        install_cli_token(&f);
        let script = f.temp.path().join("password.py");
        fs::write(
            &script,
            "from getpass import getpass\ngetpass('Password: ')",
        )
        .unwrap();
        let (mut master_fd, mut slave_fd) = (-1, -1);
        // SAFETY: output pointers are valid, optional terminal settings are null.
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
        // SAFETY: successful openpty returned owned distinct descriptors.
        let mut master = unsafe { fs::File::from_raw_fd(master_fd) };
        let slave = unsafe { fs::File::from_raw_fd(slave_fd) };
        let terminal_fd = slave.as_raw_fd();
        let server = thread::spawn(move || {
            http(
                &listener,
                "POST",
                "/prefix/api/kernels",
                201,
                json!({"id":"kernel-1"}),
            );
            let mut socket = websocket(&listener);
            let execute = user_request(&mut socket);
            assert_eq!(execute.content()["allow_stdin"], true);
            let request = incoming(
                "input_request",
                "stdin",
                execute.id(),
                json!({"prompt":"Password: ", "password":true}),
            );
            send(&mut socket, request.clone());
            socket
                .send(Message::Ping(b"echo-off".to_vec().into()))
                .unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Pong(_)));
            let mut attrs = std::mem::MaybeUninit::uninit();
            // SAFETY: the PTY master is owned and open; output storage is valid.
            assert_eq!(
                unsafe { libc::tcgetattr(master.as_raw_fd(), attrs.as_mut_ptr()) },
                0
            );
            assert_eq!(unsafe { attrs.assume_init().c_lflag } & libc::ECHO, 0);
            master.write_all(b"private-pty-value\n").unwrap();
            let reply = read_message(&mut socket);
            assert_eq!(reply.content()["value"], "private-pty-value");
            assert_eq!(reply.envelope["parent_header"], request.envelope["header"]);
            finish(&mut socket, execute.id());
            master
        });
        let mut cmd = cli_command(&f, "http://127.0.0.1:1");
        cmd.args([command, "-s", "work", "--json"]);
        if command == "exec" {
            cmd.args(["--allow-stdin", "-f"]).arg(&script);
        }
        if command == "drivemount" {
            cmd.arg("/content/quote'\ntext");
        }
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: the child only calls async-signal-safe OS functions before exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 || libc::ioctl(terminal_fd, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = cmd.spawn().unwrap().wait_with_output().unwrap();
        let master = server.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let results: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(results[0]["status"], "succeeded");
        let expected = if command == "drivemount" {
            "没有已保存的 DriveFS 凭据；将使用当前计算账户的官方交互挂载流程。\nPassword: \n"
        } else {
            "Password: \n"
        };
        assert_eq!(String::from_utf8_lossy(&output.stderr), expected);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("private-pty-value"));
        assert!(
            f.storage.sessions(&id("default")).unwrap()["work"]
                .running
                .is_none()
        );
        let histories = colabcli_rust::history::History::new(f.storage.clone(), id("default"))
            .read("work", colabcli_rust::history::HistoryQuery::default())
            .unwrap();
        assert!(
            !serde_json::to_string(&histories)
                .unwrap()
                .contains("private-pty-value")
        );
        let mut attrs = std::mem::MaybeUninit::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(master.as_raw_fd(), attrs.as_mut_ptr()) },
            0
        );
        assert_ne!(unsafe { attrs.assume_init().c_lflag } & libc::ECHO, 0);
    }
}

#[test]
fn colab_credentials_flow_preserves_request_identity_account_and_pings_during_http() {
    use colabcli_rust::{automation::AutomationInput, jupyter::InputRequest};
    for colab_id in [json!(0), json!("colab-request-string")] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        let state = state(&listener);
        f.storage
            .save_session(&id("default"), state.clone())
            .unwrap();
        let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let svc = SessionService::new(
            f.storage.clone(),
            id("default"),
            ColabApi::new(
                ApiConfig {
                    base_url: format!("http://{}", control.server_addr()).parse().unwrap(),
                    ..ApiConfig::default()
                },
                Secret::new("owned-oauth"),
            )
            .unwrap(),
        );
        let endpoint = state.endpoint.clone();
        let api_server = thread::spawn(move || {
            for (method, dryrun) in [("GET", "true"), ("POST", "true"), ("POST", "false")] {
                let request = control
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.method().as_str(), method);
                let url = url::Url::parse(&format!("http://localhost{}", request.url())).unwrap();
                assert_eq!(
                    url.path(),
                    format!("/tun/m/credentials-propagation/{endpoint}")
                );
                assert!(url.query_pairs().any(|(k, v)| k == "dryrun" && v == dryrun));
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|h| h.field.equiv("Authorization")
                            && h.value.as_str() == "Bearer owned-oauth")
                );
                assert!(
                    !request
                        .headers()
                        .iter()
                        .any(|h| h.value.as_str().contains("fixture-proxy"))
                );
                // The first HTTP wait overlaps a WS ping. A synchronous handler would miss it.
                if method == "GET" {
                    thread::sleep(Duration::from_millis(250));
                }
                request
                    .respond(tiny_http::Response::from_string(if method == "GET" {
                        "{\"token\":\"xsrf\"}"
                    } else {
                        "{\"success\":true}"
                    }))
                    .unwrap();
            }
        });
        let expected_id = colab_id.clone();
        let ws_server = thread::spawn(move || {
            http(
                &listener,
                "POST",
                "/prefix/api/kernels",
                201,
                json!({"id":"kernel-1"}),
            );
            let mut socket = websocket(&listener);
            let execute = user_request(&mut socket);
            assert_eq!(execute.content()["allow_stdin"], true);
            let mut request = incoming(
                "colab_request",
                "iopub",
                execute.id(),
                json!({"request":{"authType":"dfs_ephemeral"}}),
            );
            request.envelope["metadata"]["colab_msg_id"] = colab_id;
            send(&mut socket, request.clone());
            socket
                .send(Message::Ping(b"credentials-http".to_vec().into()))
                .unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Pong(_)));
            let reply = read_message(&mut socket);
            assert_eq!(reply.kind(), "input_reply");
            assert_eq!(reply.channel(), "stdin");
            assert_eq!(
                reply.content(),
                &json!({"value":{"type":"colab_reply","colab_msg_id":expected_id}})
            );
            assert_eq!(reply.envelope["parent_header"], request.envelope["header"]);
            finish(&mut socket, execute.id());
        });
        let mut terminal = |_: &InputRequest| -> colabcli_rust::Result<Option<String>> {
            panic!("already-authorized flow must not ask for user input")
        };
        let mut input = AutomationInput::new(&svc, &mut terminal).with_history("work".into());
        let mut document =
            Document::automation(colabcli_rust::automation::auth_code(), "auth").unwrap();
        let results = execution::execute_document_with_input(
            &svc,
            Some("work"),
            &mut document,
            opts(),
            |_| Ok(()),
            Some(&mut input),
        )
        .unwrap();
        api_server.join().unwrap();
        ws_server.join().unwrap();
        assert!(results[0].succeeded());
        let state = f.storage.sessions(&id("default")).unwrap();
        assert!(state["work"].running.is_none());
        assert_eq!(
            state["work"].last_execution.as_ref().unwrap().0,
            "automation:auth"
        );
        let history = colabcli_rust::history::History::new(f.storage.clone(), id("default"))
            .read("work", Default::default())
            .unwrap();
        assert!(
            history
                .iter()
                .any(|event| event["event_type"] == "colab_request")
        );
        assert!(
            history
                .iter()
                .any(|event| event["event_type"] == "drive_auth_success")
        );
        assert!(
            !serde_json::to_string(&history)
                .unwrap()
                .contains("owned-oauth")
        );
    }
}

#[test]
fn unsupported_malformed_or_rejected_colab_requests_never_reply_or_replay() {
    use colabcli_rust::{automation::AutomationInput, jupyter::InputRequest};
    for mode in ["unsupported", "malformed", "rejected"] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        let state = state(&listener);
        f.storage.save_session(&id("default"), state).unwrap();
        let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let svc = SessionService::new(
            f.storage.clone(),
            id("default"),
            ColabApi::new(
                ApiConfig {
                    base_url: format!("http://{}", control.server_addr()).parse().unwrap(),
                    ..ApiConfig::default()
                },
                Secret::new("owned-oauth"),
            )
            .unwrap(),
        );
        let api_server = thread::spawn(move || {
            if mode != "rejected" {
                assert!(
                    control
                        .recv_timeout(Duration::from_millis(250))
                        .unwrap()
                        .is_none()
                );
                return;
            }
            for body in [
                "{\"token\":\"xsrf\"}",
                "{\"success\":true}",
                "{\"success\":false}",
            ] {
                let request = control
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                request
                    .respond(tiny_http::Response::from_string(body))
                    .unwrap();
            }
        });
        let ws_server = thread::spawn(move || {
            http(
                &listener,
                "POST",
                "/prefix/api/kernels",
                201,
                json!({"id":"kernel-1"}),
            );
            let mut socket = websocket(&listener);
            let execute = user_request(&mut socket);
            let mut request = incoming(
                "colab_request",
                "iopub",
                execute.id(),
                json!({"request":{"authType":if mode=="unsupported" {"other"} else {"dfs_ephemeral"}}}),
            );
            request.envelope["metadata"]["colab_msg_id"] = if mode == "malformed" {
                Value::Null
            } else {
                json!(1)
            };
            send(&mut socket, request);
            match socket.read() {
                Ok(Message::Close(_)) | Err(_) => {}
                other => panic!("authorization failure must not send input_reply: {other:?}"),
            }
        });
        let mut terminal = |_: &InputRequest| -> colabcli_rust::Result<Option<String>> {
            panic!("must not prompt")
        };
        let mut input = AutomationInput::new(&svc, &mut terminal);
        let mut document =
            Document::automation(colabcli_rust::automation::auth_code(), "auth").unwrap();
        let results = execution::execute_document_with_input(
            &svc,
            Some("work"),
            &mut document,
            opts(),
            |_| Ok(()),
            Some(&mut input),
        )
        .unwrap();
        assert_eq!(results[0].status, ExecutionStatus::Unknown);
        let mut next = Document::from_stdin(b"must_not_repeat()".as_slice(), &[]).unwrap();
        assert!(matches!(
            execution::execute_document(&svc, Some("work"), &mut next, opts(), |_| Ok(())),
            Err(Error::AutomationPending)
        ));
        api_server.join().unwrap();
        ws_server.join().unwrap();
    }
    for path in ["", "relative/path", "/content/bad\0path"] {
        assert!(colabcli_rust::automation::mount_code(path).is_err());
    }
    let code = colabcli_rust::automation::mount_code("/content/quote'\"\n中文").unwrap();
    assert_eq!(code.lines().count(), 2);
}

#[test]
fn cli_auth_without_tty_propagates_only_the_explicit_accounts_credentials() {
    for needs_consent in [false, true] {
        let f = Fixture::new(TokenStorageMode::File);
        let other = f.add("other");
        let listener = listener();
        let mut own = state(&listener);
        own.endpoint = "ep-other-owned".into();
        f.storage.save_session(&other, own).unwrap();
        let mut unrelated = session("work");
        unrelated.url = "http://127.0.0.1:1".into();
        f.storage
            .save_session(&id("default"), unrelated.clone())
            .unwrap();
        let mut credentials = token();
        credentials.token = Secret::new("other-oauth-only");
        credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
        f.tokens.save(&other, &credentials).unwrap();
        let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", control.server_addr());
        let api_server = thread::spawn(move || {
            let responses = if needs_consent {
                vec![
                    ("GET", "{\"token\":\"other-xsrf\"}"),
                    (
                        "POST",
                        "{\"success\":false,\"unauthorized_redirect_uri\":\"https://accounts.google.com/authorize?state=private-test-state\"}",
                    ),
                ]
            } else {
                vec![
                    ("GET", "{\"token\":\"other-xsrf\"}"),
                    ("POST", "{\"success\":true}"),
                    ("POST", "{\"success\":true}"),
                ]
            };
            for (method, body) in responses {
                let request = control
                    .recv_timeout(Duration::from_secs(4))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.method().as_str(), method);
                assert!(
                    request
                        .url()
                        .starts_with("/tun/m/credentials-propagation/ep-other-owned?")
                );
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|h| h.field.equiv("Authorization")
                            && h.value.as_str() == "Bearer other-oauth-only")
                );
                request
                    .respond(tiny_http::Response::from_string(body))
                    .unwrap();
            }
        });
        let ws_server = thread::spawn(move || {
            http(
                &listener,
                "POST",
                "/prefix/api/kernels",
                201,
                json!({"id":"kernel-1"}),
            );
            let mut socket = websocket(&listener);
            let execute = user_request(&mut socket);
            assert!(
                execute.content()["code"]
                    .as_str()
                    .unwrap()
                    .contains("auth.authenticate_user()")
            );
            let mut request = incoming(
                "colab_request",
                "iopub",
                execute.id(),
                json!({"request":{"authType":"dfs_ephemeral"}}),
            );
            request.envelope["metadata"]["colab_msg_id"] = json!(42);
            send(&mut socket, request);
            if needs_consent {
                match socket.read() {
                    Ok(Message::Close(_)) | Err(_) => {}
                    other => panic!("consent without TTY must not send success: {other:?}"),
                }
            } else {
                let reply = read_message(&mut socket);
                assert_eq!(reply.content()["value"]["colab_msg_id"], 42);
                finish(&mut socket, execute.id());
            }
        });
        let output = cli_command(&f, &base)
            .args(["-a", "other", "auth", "-s", "work", "--json"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        api_server.join().unwrap();
        ws_server.join().unwrap();
        let results: Value = serde_json::from_slice(&output.stdout).unwrap();
        if needs_consent {
            assert_eq!(output.status.code(), Some(1));
            assert_eq!(results[0]["status"], "unknown");
            assert!(results[0]["error"].as_str().unwrap().contains("交互终端"));
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains(results[0]["msg_id"].as_str().unwrap())
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("private-test-state"));
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(results[0]["status"], "succeeded");
        }
        assert!(!String::from_utf8_lossy(&output.stdout).contains("other-oauth-only"));
        assert!(
            f.storage.sessions(&id("default")).unwrap()["work"]
                .kernel_id
                .is_none()
        );
        assert!(
            f.storage.sessions(&other).unwrap()["work"]
                .running
                .is_none()
                == !needs_consent
        );
    }
}

#[test]
fn automation_creates_a_fresh_kernel_each_time_and_preserves_the_exec_kernel() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let mut original = state(&listener);
    original.kernel_id = Some("normal-k".into());
    original.session_id = Some("normal-session".into());
    let svc = service(&f, original);
    let server = thread::spawn(move || {
        for kernel in ["automation-1", "automation-2"] {
            http(
                &listener,
                "POST",
                "/prefix/api/kernels",
                201,
                json!({"id":kernel}),
            );
            let mut socket = websocket_for(&listener, kernel);
            let execute = user_request(&mut socket);
            finish(&mut socket, execute.id());
        }
        http(
            &listener,
            "GET",
            "/prefix/api/kernels/normal-k",
            200,
            json!({"id":"normal-k"}),
        );
        let mut socket = websocket_for(&listener, "normal-k");
        let execute = user_request(&mut socket);
        assert_eq!(execute.content()["code"], "print(existing_variable)");
        send(
            &mut socket,
            incoming(
                "stream",
                "iopub",
                execute.id(),
                json!({"name":"stdout","text":"42\n"}),
            ),
        );
        finish(&mut socket, execute.id());
    });
    for expected in ["automation-1", "automation-2"] {
        let mut document = Document::automation("print('authorize')", "auth").unwrap();
        assert!(
            execution::execute_document(&svc, Some("work"), &mut document, opts(), |_| Ok(()))
                .unwrap()[0]
                .succeeded()
        );
        let saved = f.storage.sessions(&id("default")).unwrap();
        let state = &saved["work"];
        assert_eq!(state.kernel_id.as_deref(), Some("normal-k"));
        assert_eq!(state.session_id.as_deref(), Some("normal-session"));
        assert!(!state.extra.contains_key("automation_pending"));
        assert_eq!(state.extra["automation_last"]["kernel_id"], expected);
    }
    let mut normal = Document::from_stdin(b"print(existing_variable)".as_slice(), &[]).unwrap();
    let results =
        execution::execute_document(&svc, Some("work"), &mut normal, opts(), |_| Ok(())).unwrap();
    assert!(results[0].succeeded());
    assert_eq!(results[0].outputs[0].data["text"], "42\n");
    server.join().unwrap();
}

#[test]
fn automation_handshake_failure_can_resume_only_the_same_unexecuted_request() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let svc = service(&f, state(&listener));
    let server = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"automation-1"}),
        );
        let mut stream = accept(&listener);
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
            assert!(headers.len() < 32768);
        }
        assert!(
            String::from_utf8(headers)
                .unwrap()
                .starts_with("GET /prefix/api/kernels/automation-1/channels?")
        );
        stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        drop(stream);
        http(
            &listener,
            "GET",
            "/prefix/api/kernels/automation-1",
            200,
            json!({"id":"automation-1"}),
        );
        let mut socket = websocket_for(&listener, "automation-1");
        let execute = user_request(&mut socket);
        finish(&mut socket, execute.id());
    });
    let mut first = Document::automation("authorize()", "auth").unwrap();
    assert!(matches!(
        execution::execute_document(&svc, None, &mut first, opts(), |_| Ok(())),
        Err(Error::WebSocketTransport)
    ));
    let saved = f.storage.sessions(&id("default")).unwrap();
    assert_eq!(saved["work"].extra["automation_pending"]["phase"], "ready");
    assert!(saved["work"].kernel_id.is_none());
    let mut different = Document::automation("authorize_other_path()", "auth").unwrap();
    assert!(matches!(
        execution::execute_document(&svc, None, &mut different, opts(), |_| Ok(())),
        Err(Error::AutomationPending)
    ));
    assert!(
        execution::execute_document(&svc, None, &mut first, opts(), |_| Ok(())).unwrap()[0]
            .succeeded()
    );
    server.join().unwrap();
}

#[test]
fn automation_unknown_creation_requires_explicit_owned_cleanup_and_retries_delete_safely() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let mut state = state(&listener);
    state.kernel_id = Some("normal-k".into());
    state.session_id = Some("normal-session".into());
    let svc = service(&f, state);
    let server = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            500,
            json!({"error":"uncertain-create"}),
        );
        http(
            &listener,
            "GET",
            "/prefix/api/kernels/automation-lost",
            200,
            json!({"id":"automation-lost"}),
        );
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/automation-lost",
            503,
            json!({"error":"uncertain-delete"}),
        );
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/automation-lost",
            404,
            json!({"error":"gone"}),
        );
    });
    let mut document = Document::automation("authorize()", "auth").unwrap();
    let mut cancelled = opts();
    cancelled.cancellation = Some(std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(2)));
    assert!(matches!(
        execution::execute_document(&svc, None, &mut document, cancelled, |_| Ok(())),
        Err(Error::Run(_))
    ));
    assert!(
        !f.storage.sessions(&id("default")).unwrap()["work"]
            .extra
            .contains_key("automation_pending")
    );
    assert!(matches!(
        execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())),
        Err(Error::RuntimeStatus(500))
    ));
    assert!(matches!(
        execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())),
        Err(Error::AutomationPending)
    ));
    assert!(matches!(
        execution::automation_cleanup(&svc, None, None, Duration::from_secs(2)),
        Err(Error::AutomationKernelUnknown)
    ));
    assert!(
        execution::automation_cleanup(&svc, None, Some("normal-k"), Duration::from_secs(2))
            .is_err()
    );
    assert!(matches!(
        execution::automation_cleanup(&svc, None, Some("automation-lost"), Duration::from_secs(2)),
        Err(Error::RuntimeStatus(503))
    ));
    let saved = f.storage.sessions(&id("default")).unwrap();
    assert_eq!(
        saved["work"].extra["automation_pending"]["phase"],
        "deleting"
    );
    assert!(
        execution::automation_cleanup(&svc, None, Some("unrelated"), Duration::from_secs(2))
            .is_err()
    );
    assert_eq!(
        execution::automation_cleanup(&svc, None, None, Duration::from_secs(2))
            .unwrap()
            .as_deref(),
        Some("automation-lost")
    );
    assert!(
        execution::automation_cleanup(&svc, None, None, Duration::from_secs(2))
            .unwrap()
            .is_none()
    );
    let saved = f.storage.sessions(&id("default")).unwrap();
    assert_eq!(saved["work"].kernel_id.as_deref(), Some("normal-k"));
    assert_eq!(saved["work"].session_id.as_deref(), Some("normal-session"));
    assert!(!saved["work"].extra.contains_key("automation_pending"));
    server.join().unwrap();
}

#[test]
fn automation_uncertain_or_finished_operation_is_never_replayed_and_normal_restart_cannot_clear_it()
{
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let mut state = state(&listener);
    state.kernel_id = Some("normal-k".into());
    let svc = service(&f, state);
    let server = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"automation-1"}),
        );
        let mut socket = websocket_for(&listener, "automation-1");
        let _ = user_request(&mut socket);
        socket.close(None).unwrap();
        drop(socket);
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/automation-1",
            204,
            Value::Null,
        );
    });
    let mut document = Document::automation("authorize()", "auth").unwrap();
    let result =
        execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())).unwrap();
    assert_eq!(result[0].status, ExecutionStatus::Unknown);
    let snapshot = f.storage.sessions(&id("default")).unwrap()["work"].clone();
    for phase in ["unknown", "executing", "finished"] {
        // Model a crash in each durable stage, including after a confirmed reply but before retirement.
        let mut interrupted = snapshot.clone();
        interrupted.extra.get_mut("automation_pending").unwrap()["phase"] = json!(phase);
        f.storage.save_session(&id("default"), interrupted).unwrap();
        assert!(matches!(
            execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())),
            Err(Error::AutomationPending)
        ));
        let mut ordinary = Document::from_stdin(b"must_not_run()".as_slice(), &[]).unwrap();
        assert!(matches!(
            execution::execute_document(&svc, None, &mut ordinary, opts(), |_| Ok(())),
            Err(Error::AutomationPending)
        ));
        assert!(matches!(
            execution::restart_kernel(&svc, None, Duration::from_secs(2)),
            Err(Error::AutomationPending)
        ));
    }
    execution::automation_cleanup(&svc, None, None, Duration::from_secs(2)).unwrap();
    let saved = f.storage.sessions(&id("default")).unwrap();
    assert_eq!(saved["work"].kernel_id.as_deref(), Some("normal-k"));
    assert!(saved["work"].running.is_none());
    server.join().unwrap();
}

#[test]
fn concurrent_automation_cleanup_invalidates_the_old_writer_without_replacing_exec_state() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let mut state = state(&listener);
    state.kernel_id = Some("normal-k".into());
    let svc = service(&f, state);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (clean_tx, clean_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"automation-1"}),
        );
        let mut socket = websocket_for(&listener, "automation-1");
        let execute = user_request(&mut socket);
        ready_tx.send(()).unwrap();
        http(
            &listener,
            "DELETE",
            "/prefix/api/kernels/automation-1",
            204,
            Value::Null,
        );
        clean_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        finish(&mut socket, execute.id());
    });
    let executor = svc.clone();
    let execution = thread::spawn(move || {
        let mut document = Document::automation("authorize()", "auth").unwrap();
        execution::execute_document(&executor, None, &mut document, opts(), |_| Ok(()))
    });
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    execution::automation_cleanup(&svc, None, None, Duration::from_secs(2)).unwrap();
    clean_tx.send(()).unwrap();
    assert!(matches!(
        execution.join().unwrap(),
        Err(Error::KernelProtocol(_))
    ));
    server.join().unwrap();
    let saved = f.storage.sessions(&id("default")).unwrap();
    assert_eq!(saved["work"].kernel_id.as_deref(), Some("normal-k"));
    assert!(saved["work"].running.is_none());
    assert!(!saved["work"].extra.contains_key("automation_pending"));
}

#[test]
fn corrupt_automation_journal_never_creates_or_deletes_a_kernel() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let original = state(&listener);
    let svc = service(&f, original.clone());
    for invalid in [
        Value::Null,
        json!({"phase":"ready"}),
        json!({"operation":"auth","source_hash":"0".repeat(64),"kernel_id":"normal","session_id":"s","phase":"unknown","msg_id":null}),
    ] {
        let mut state = original.clone();
        state
            .extra
            .insert("automation_pending".into(), invalid.clone());
        f.storage.save_session(&id("default"), state).unwrap();
        let mut document = Document::automation("authorize()", "auth").unwrap();
        assert!(matches!(
            execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())),
            Err(Error::KernelProtocol(_))
        ));
        assert!(matches!(
            execution::automation_cleanup(&svc, None, None, Duration::from_secs(1)),
            Err(Error::KernelProtocol(_))
        ));
        assert_eq!(
            f.storage.sessions(&id("default")).unwrap()["work"].extra["automation_pending"],
            invalid
        );
    }
    // No runtime requests were made, including for corrupt ownership fields.
    assert!(matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
}

fn install_arguments(source: &str) -> Vec<String> {
    let literal = source
        .lines()
        .find_map(|line| line.trim().strip_prefix("packages = json.loads("))
        .unwrap()
        .strip_suffix(')')
        .unwrap();
    let encoded: String = serde_json::from_str(literal).unwrap();
    serde_json::from_str(&encoded).unwrap()
}

#[test]
fn cli_install_transmits_argument_data_without_logging_private_package_values() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let mut normal = state(&listener);
    normal.kernel_id = Some("normal-k".into());
    normal.session_id = Some("normal-session".into());
    f.storage.save_session(&id("default"), normal).unwrap();
    install_cli_token(&f);
    let packages = vec![
        "demo>=1".to_owned(),
        "quote'\"\n中文; __import__('os')".into(),
        "__COLAB_INSTALL_ARGUMENTS_JSON__".into(),
        "--index-url".into(),
        "https://name:synthetic-private-password@example.invalid/simple".into(),
    ];
    let expected = packages.clone();
    let server = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"install-k"}),
        );
        let mut socket = websocket_for(&listener, "install-k");
        let execute = user_request(&mut socket);
        assert_eq!(execute.content()["allow_stdin"], false);
        let source = execute.content()["code"].as_str().unwrap();
        assert_eq!(install_arguments(source), expected);
        assert_eq!(source.matches("\ninstall()\n").count(), 1);
        send(
            &mut socket,
            incoming(
                "stream",
                "iopub",
                execute.id(),
                json!({"name":"stdout","text":"Installation Complete (via uv)!\n"}),
            ),
        );
        finish(&mut socket, execute.id());
    });
    let output = cli_command(&f, "http://127.0.0.1:1")
        .args(["install", "-s", "work", "--json", "--"])
        .args(&packages)
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()[0]["status"],
        "succeeded"
    );
    let history = colabcli_rust::history::History::new(f.storage.clone(), id("default"))
        .read("work", Default::default())
        .unwrap();
    let history = serde_json::to_string(&history).unwrap();
    assert!(!history.contains("synthetic-private-password"));
    assert!(!history.contains("__COLAB_INSTALL_ARGUMENTS_JSON__"));
    assert!(history.contains("argument values omitted"));
    let saved = f.storage.sessions(&id("default")).unwrap();
    assert_eq!(saved["work"].kernel_id.as_deref(), Some("normal-k"));
    assert_eq!(saved["work"].session_id.as_deref(), Some("normal-session"));
    assert_eq!(
        saved["work"].last_execution.as_ref().unwrap().0,
        "automation:install"
    );
}

#[test]
fn cli_install_uploads_exact_requirements_before_starting_its_kernel() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    f.storage
        .save_session(&id("default"), state(&listener))
        .unwrap();
    install_cli_token(&f);
    let path = f.temp.path().join("requirements #中.txt");
    let bytes = b"# synthetic test requirement\ndemo==1.2\n";
    fs::write(&path, bytes).unwrap();
    let server = thread::spawn(move || {
        let body = http(
            &listener,
            "PUT",
            "/prefix/api/contents/content/requirements%20%23%E4%B8%AD.txt",
            200,
            json!({"name":"requirements #中.txt","path":"content/requirements #中.txt","type":"file","format":"base64","content":null}),
        );
        assert_eq!(
            STANDARD.decode(body["content"].as_str().unwrap()).unwrap(),
            bytes
        );
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"install-k"}),
        );
        let mut socket = websocket_for(&listener, "install-k");
        let execute = user_request(&mut socket);
        assert_eq!(
            install_arguments(execute.content()["code"].as_str().unwrap()),
            ["-r", "/content/requirements #中.txt", "extra>=1"]
        );
        send(
            &mut socket,
            incoming(
                "stream",
                "iopub",
                execute.id(),
                json!({"name":"stdout","text":"Installation Complete (via pip)!\n"}),
            ),
        );
        finish(&mut socket, execute.id());
    });
    let output = cli_command(&f, "http://127.0.0.1:1")
        .args(["install", "-s", "work", "-r"])
        .arg(&path)
        .arg("extra>=1")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Installation Complete (via pip)!"));
    let history = colabcli_rust::history::History::new(f.storage.clone(), id("default"))
        .read("work", Default::default())
        .unwrap();
    assert!(
        !serde_json::to_string(&history)
            .unwrap()
            .contains("synthetic test requirement")
    );
}

#[test]
fn install_upload_failure_does_not_create_a_kernel_or_mark_execution_pending() {
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let svc = service(&f, state(&listener));
    let path = f.temp.path().join("requirements.txt");
    fs::write(&path, "demo==1").unwrap();
    let server = thread::spawn(move || {
        http(
            &listener,
            "PUT",
            "/prefix/api/contents/content/requirements.txt",
            503,
            json!({"error":"unavailable"}),
        );
    });
    let mut document = colabcli_rust::installation::prepare(&[], Some(&path)).unwrap();
    assert!(matches!(
        execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())),
        Err(Error::RuntimeStatus(503))
    ));
    server.join().unwrap();
    let saved = f.storage.sessions(&id("default")).unwrap();
    assert!(saved["work"].kernel_id.is_none());
    assert!(saved["work"].running.is_none());
    assert!(!saved["work"].extra.contains_key("automation_pending"));
}

#[test]
fn install_requirements_fingerprint_blocks_changed_retry_and_unknown_execution_never_reuploads() {
    for unknown in [false, true] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        let svc = service(&f, state(&listener));
        let path = f.temp.path().join("requirements.txt");
        fs::write(&path, "demo==1\n").unwrap();
        let server = thread::spawn(move || {
            let response = json!({"name":"requirements.txt","path":"content/requirements.txt","type":"file","format":"base64","content":null});
            http(
                &listener,
                "PUT",
                "/prefix/api/contents/content/requirements.txt",
                200,
                response.clone(),
            );
            http(
                &listener,
                "POST",
                "/prefix/api/kernels",
                201,
                json!({"id":"install-k"}),
            );
            if unknown {
                let mut socket = websocket_for(&listener, "install-k");
                let _ = user_request(&mut socket);
                socket.close(None).unwrap();
                drop(socket);
                http(
                    &listener,
                    "DELETE",
                    "/prefix/api/kernels/install-k",
                    204,
                    Value::Null,
                );
            } else {
                let mut stream = accept(&listener);
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                    assert!(headers.len() < 32768);
                }
                stream
                    .write_all(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                drop(stream);
                let body = http(
                    &listener,
                    "PUT",
                    "/prefix/api/contents/content/requirements.txt",
                    200,
                    response,
                );
                assert_eq!(
                    STANDARD.decode(body["content"].as_str().unwrap()).unwrap(),
                    b"demo==1\n"
                );
                http(
                    &listener,
                    "GET",
                    "/prefix/api/kernels/install-k",
                    200,
                    json!({"id":"install-k"}),
                );
                let mut socket = websocket_for(&listener, "install-k");
                let execute = user_request(&mut socket);
                finish(&mut socket, execute.id());
            }
        });
        let mut document = colabcli_rust::installation::prepare(&[], Some(&path)).unwrap();
        let first = execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(()));
        if unknown {
            assert_eq!(first.unwrap()[0].status, ExecutionStatus::Unknown);
        } else {
            assert!(matches!(first, Err(Error::WebSocketTransport)));
        }
        fs::write(&path, "demo==2\n").unwrap();
        let mut changed = colabcli_rust::installation::prepare(&[], Some(&path)).unwrap();
        assert!(matches!(
            execution::execute_document(&svc, None, &mut changed, opts(), |_| Ok(())),
            Err(Error::AutomationPending)
        ));
        if unknown {
            assert!(matches!(
                execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())),
                Err(Error::AutomationPending)
            ));
            execution::automation_cleanup(&svc, None, None, Duration::from_secs(2)).unwrap();
        } else {
            assert!(
                execution::execute_document(&svc, None, &mut document, opts(), |_| Ok(())).unwrap()
                    [0]
                .succeeded()
            );
        }
        server.join().unwrap();
    }
}

#[test]
fn install_preflight_rejects_missing_inputs_files_and_bad_parameters_before_authentication() {
    assert!(Document::automation("noop()", "unknown-operation").is_err());
    for packages in [vec![], vec![String::new()], vec!["bad\0package".into()]] {
        assert!(colabcli_rust::installation::prepare(&packages, None).is_err());
    }
    let temp = tempfile::tempdir().unwrap();
    for args in [
        vec!["install"],
        vec!["install", "-r", "missing-requirements.txt"],
        vec!["install", "demo", "--timeout", "NaN"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_colab"))
            .arg("--config-dir")
            .arg(temp.path())
            .args(["--token-storage", "file"])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("需要登录"));
        assert!(!temp.path().join("accounts/default/sessions.json").exists());
    }
}

#[test]
fn websocket_idle_heartbeat_pong_and_disconnect_never_replay() {
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let server = thread::spawn(move || {
        let mut socket = websocket(&listener);
        assert!(
            matches!(socket.read().unwrap(), Message::Ping(bytes) if bytes == b"colab-cli"[..])
        );
        socket
            .send(Message::Ping(b"idle-probe".to_vec().into()))
            .unwrap();
        loop {
            if let Message::Pong(bytes) = socket.read().unwrap() {
                if bytes == b"idle-probe"[..] {
                    break;
                }
            }
        }
        socket.close(None).unwrap();
    });
    let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "repl-session").unwrap();
    assert!(runtime.set_heartbeat_interval(Duration::ZERO).is_err());
    runtime
        .set_heartbeat_interval(Duration::from_millis(30))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if runtime.poll_idle().is_err() {
            break;
        }
        assert!(Instant::now() < deadline);
    }
    assert!(
        runtime
            .execute("must_not_send()", ExecutionTimeout::Idle(None), |_| Ok(()))
            .is_err()
    );
    server.join().unwrap();
}

#[test]
fn websocket_active_heartbeats_do_not_extend_execution_idle_deadline() {
    let listener = listener();
    let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
    let server = thread::spawn(move || {
        let mut socket = websocket(&listener);
        read_message(&mut socket);
        let mut count = 0;
        while let Ok(message) = socket.read() {
            match message {
                Message::Ping(_) => {
                    count += 1;
                    socket.flush().unwrap();
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        assert!(count >= 2);
    });
    let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "repl-session").unwrap();
    runtime
        .set_heartbeat_interval(Duration::from_millis(30))
        .unwrap();
    let start = Instant::now();
    let result = runtime
        .execute(
            "long_running()",
            ExecutionTimeout::Idle(Some(Duration::from_millis(350))),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Unknown);
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(runtime);
    server.join().unwrap();
}

#[test]
fn websocket_sigint_requires_rest_and_reply_idle_confirmation() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for mode in ["confirmed", "rest-error", "missing-confirmation"] {
        let listener = listener();
        let rest = KernelRest::new(&state(&listener), Duration::from_secs(2)).unwrap();
        let flag = Arc::new(AtomicUsize::new(0));
        let server_flag = flag.clone();
        let server = thread::spawn(move || {
            let mut socket = websocket(&listener);
            let request = read_message(&mut socket);
            server_flag.store(signal_hook::consts::SIGINT as usize, Ordering::SeqCst);
            http(
                &listener,
                "POST",
                "/prefix/api/kernels/kernel-1/interrupt",
                if mode == "rest-error" { 503 } else { 200 },
                Value::Null,
            );
            if mode == "confirmed" {
                send(
                    &mut socket,
                    incoming(
                        "execute_reply",
                        "shell",
                        request.id(),
                        json!({"status":"error","ename":"KeyboardInterrupt","evalue":"","traceback":[]}),
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
                let next = read_message(&mut socket);
                assert_eq!(next.content()["code"], "after_interrupt()");
                finish(&mut socket, next.id());
            } else {
                while let Ok(message) = socket.read() {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                }
            }
        });
        let mut runtime = KernelRuntime::connect(&rest, "kernel-1", "repl-session").unwrap();
        runtime.set_cancellation(Some(flag.clone()));
        runtime
            .interrupt_on_sigint(rest, "kernel-1".into(), Duration::from_millis(150))
            .unwrap();
        let result = runtime
            .execute("long_running()", ExecutionTimeout::Idle(None), |_| Ok(()))
            .unwrap();
        if mode == "confirmed" {
            assert_eq!(result.status, ExecutionStatus::Failed);
            assert!(result.error.is_none());
            assert_eq!(flag.load(Ordering::SeqCst), 0);
            assert!(
                runtime
                    .execute(
                        "after_interrupt()",
                        ExecutionTimeout::Idle(None),
                        |_| Ok(())
                    )
                    .unwrap()
                    .succeeded()
            );
        } else {
            assert_eq!(result.status, ExecutionStatus::Unknown);
            assert!(
                runtime
                    .execute("must_not_send()", ExecutionTimeout::Idle(None), |_| Ok(()))
                    .is_err()
            );
        }
        drop(runtime);
        server.join().unwrap();
    }
}

#[test]
fn repl_reuses_one_kernel_and_socket_and_continues_after_python_error() {
    use colabcli_rust::execution::repl::{self, Command as ReplCommand, Event};
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let service = service(&f, state(&listener));
    let server = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&listener);
        let setup = read_message(&mut socket);
        assert!(
            setup.content()["code"]
                .as_str()
                .unwrap()
                .contains("makedirs('/content'")
        );
        finish(&mut socket, setup.id());
        let first = read_message(&mut socket);
        assert_eq!(first.content()["code"], "变量 = 42");
        finish(&mut socket, first.id());
        let error = read_message(&mut socket);
        send(
            &mut socket,
            incoming(
                "execute_reply",
                "shell",
                error.id(),
                json!({"status":"error","ename":"ValueError","evalue":"fixture error","traceback":[]}),
            ),
        );
        send(
            &mut socket,
            incoming(
                "status",
                "iopub",
                error.id(),
                json!({"execution_state":"idle"}),
            ),
        );
        let next = read_message(&mut socket);
        assert_eq!(next.content()["code"], "print(变量)");
        assert_eq!(
            first.envelope["header"]["session"],
            next.envelope["header"]["session"]
        );
        send(
            &mut socket,
            incoming(
                "stream",
                "iopub",
                next.id(),
                json!({"name":"stdout","text":"42\n"}),
            ),
        );
        finish(&mut socket, next.id());
        assert!(matches!(socket.read().unwrap(), Message::Close(_)));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let mut statuses = Vec::new();
    let mut codes = ["变量 = 42", "raise ValueError()", "print(变量)"].into_iter();
    repl::serve(&service, Some("work"), rx, opts(), |event| {
        match event {
            Event::Ready => tx
                .send(ReplCommand::Execute(codes.next().unwrap().into()))
                .unwrap(),
            Event::Finished(result) => {
                statuses.push(result.status);
                tx.send(
                    codes
                        .next()
                        .map(|s| ReplCommand::Execute(s.into()))
                        .unwrap_or(ReplCommand::Stop),
                )
                .unwrap();
            }
            Event::Output(_) => {}
        }
        Ok(())
    })
    .unwrap();
    server.join().unwrap();
    assert_eq!(
        statuses,
        [
            ExecutionStatus::Succeeded,
            ExecutionStatus::Failed,
            ExecutionStatus::Succeeded
        ]
    );
    let saved = &f.storage.sessions(&id("default")).unwrap()["work"];
    assert_eq!(saved.kernel_id.as_deref(), Some("kernel-1"));
    assert_eq!(saved.last_execution.as_ref().unwrap().0, "REPL");
    assert!(saved.running.is_none());
}

#[test]
fn repl_unknown_result_blocks_later_exec_without_creating_another_kernel() {
    use colabcli_rust::execution::repl::{self, Command as ReplCommand, Event};
    let f = Fixture::new(TokenStorageMode::File);
    let listener = listener();
    let service = service(&f, state(&listener));
    let server = thread::spawn(move || {
        http(
            &listener,
            "POST",
            "/prefix/api/kernels",
            201,
            json!({"id":"kernel-1"}),
        );
        let mut socket = websocket(&listener);
        let request = user_request(&mut socket);
        assert_eq!(request.content()["code"], "side_effect()");
        socket.close(None).unwrap();
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let result = repl::serve(&service, Some("work"), rx, opts(), |event| {
        if matches!(event, Event::Ready) {
            tx.send(ReplCommand::Execute("side_effect()".into()))
                .unwrap();
        }
        Ok(())
    });
    assert!(matches!(result, Err(Error::ExecutionUnknown(_))));
    server.join().unwrap();
    let mut doc = Document::from_stdin("must_not_send()".as_bytes(), &[]).unwrap();
    assert!(matches!(
        execution::execute_document(&service, Some("work"), &mut doc, opts(), |_| Ok(())),
        Err(Error::ExecutionPending(_))
    ));
}

#[test]
fn cli_repl_piped_input_empty_and_python_error_match_exit_contract() {
    for code in ["print('你好')", "raise ValueError()", " \n"] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        f.storage
            .save_session(&id("default"), state(&listener))
            .unwrap();
        install_cli_token(&f);
        let server = thread::spawn(move || {
            http(
                &listener,
                "POST",
                "/prefix/api/kernels",
                201,
                json!({"id":"kernel-1"}),
            );
            let mut socket = websocket(&listener);
            let setup = read_message(&mut socket);
            finish(&mut socket, setup.id());
            if !code.trim().is_empty() {
                let execute = read_message(&mut socket);
                assert_eq!(execute.content()["code"], code);
                if code.starts_with("raise") {
                    send(
                        &mut socket,
                        incoming(
                            "execute_reply",
                            "shell",
                            execute.id(),
                            json!({"status":"error","ename":"ValueError","evalue":"test","traceback":[]}),
                        ),
                    );
                    send(
                        &mut socket,
                        incoming(
                            "status",
                            "iopub",
                            execute.id(),
                            json!({"execution_state":"idle"}),
                        ),
                    );
                } else {
                    send(
                        &mut socket,
                        incoming(
                            "stream",
                            "iopub",
                            execute.id(),
                            json!({"name":"stdout","text":"你好\n"}),
                        ),
                    );
                    finish(&mut socket, execute.id());
                }
            }
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
        });
        let mut child = cli_command(&f, "http://127.0.0.1:1")
            .args(["--colab-runtime-endpoint", "ep-work", "repl", "-s", "work"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(code.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if code.starts_with("print") {
            assert_eq!(String::from_utf8_lossy(&output.stdout), "你好\n");
        }
        if code.starts_with("raise") {
            assert!(String::from_utf8_lossy(&output.stderr).contains("ValueError"));
        }
        assert!(!String::from_utf8_lossy(&output.stdout).contains(">>>"));
        server.join().unwrap();
        assert!(
            f.storage.sessions(&id("default")).unwrap()["work"]
                .running
                .is_none()
        );
    }
}

#[cfg(unix)]
#[test]
fn cli_repl_pty_multiline_unicode_history_input_cancel_and_eof() {
    for ending in ["eof", "quit", "disconnect", "term"] {
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::process::CommandExt,
        };
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        f.storage
            .save_session(&id("default"), state(&listener))
            .unwrap();
        install_cli_token(&f);
        let (mut master_fd, mut slave_fd) = (-1, -1);
        // SAFETY: valid output storage, optional settings are null.
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
        let mut master = unsafe { fs::File::from_raw_fd(master_fd) };
        let slave = unsafe { fs::File::from_raw_fd(slave_fd) };
        let terminal_fd = slave.as_raw_fd();
        // SAFETY: the descriptor is open, flag operations do not transfer ownership.
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        let (close_tx, close_rx) = std::sync::mpsc::channel();
        let (executing_tx, executing_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            http(
                &listener,
                "POST",
                "/prefix/api/kernels",
                201,
                json!({"id":"kernel-1"}),
            );
            let mut socket = websocket(&listener);
            let setup = read_message(&mut socket);
            finish(&mut socket, setup.id());
            let expected = [
                "def f():\n    x = 1\n    return '你好'",
                "raise ValueError()",
                "f()",
                "f()",
            ];
            for (index, code) in expected.into_iter().enumerate() {
                let request = read_message(&mut socket);
                assert_eq!(request.content()["code"], code);
                if index == 2 {
                    executing_tx.send(()).unwrap();
                    http(
                        &listener,
                        "POST",
                        "/prefix/api/kernels/kernel-1/interrupt",
                        200,
                        Value::Null,
                    );
                    send(
                        &mut socket,
                        incoming(
                            "execute_reply",
                            "shell",
                            request.id(),
                            json!({"status":"error","ename":"KeyboardInterrupt","evalue":"","traceback":[]}),
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
                } else if index == 1 {
                    send(
                        &mut socket,
                        incoming(
                            "execute_reply",
                            "shell",
                            request.id(),
                            json!({"status":"error","ename":"ValueError","evalue":"fixture","traceback":[]}),
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
                } else {
                    if index >= 2 {
                        send(
                            &mut socket,
                            incoming(
                                "stream",
                                "iopub",
                                request.id(),
                                json!({"name":"stdout","text":"你好\n"}),
                            ),
                        );
                    }
                    finish(&mut socket, request.id());
                }
            }
            if ending == "disconnect" {
                close_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                socket.close(None).unwrap();
            }
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
        });
        let mut cmd = cli_command(&f, "http://127.0.0.1:1");
        cmd.args(["--colab-runtime-endpoint", "ep-work", "repl", "-s", "work"])
            .env("TERM", "xterm-256color")
            .env("NO_COLOR", "1")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // SAFETY: only async-signal-safe operations are used between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 || libc::ioctl(terminal_fd, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let mut transcript = String::new();
        let mut prompt = |master: &mut fs::File| {
            let start = Instant::now();
            let mut fresh = Vec::new();
            loop {
                let mut buffer = [0; 8192];
                match master.read(&mut buffer) {
                    Ok(size) if size > 0 => {
                        fresh.extend_from_slice(&buffer[..size]);
                    }
                    Ok(_) => panic!("REPL terminal closed: {}", String::from_utf8_lossy(&fresh)),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("{error}: {}", String::from_utf8_lossy(&fresh)),
                }
                if fresh.windows(4).any(|part| part == b">>> ")
                    && fresh.windows(8).any(|part| part == b"\x1b[?2004h")
                {
                    transcript.push_str(&String::from_utf8_lossy(&fresh));
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "{}",
                    String::from_utf8_lossy(&fresh)
                );
            }
        };
        prompt(&mut master);
        master.write_all("不应执行".as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(50));
        master.write_all(b"\x03").unwrap();
        prompt(&mut master);
        master
            .write_all("def f():\n    x = 1\x1b\r    return '你好'\r".as_bytes())
            .unwrap();
        prompt(&mut master);
        master.write_all(b"raise ValueError()\r").unwrap();
        prompt(&mut master);
        master.write_all(b"f()\r").unwrap();
        let start = Instant::now();
        loop {
            if executing_rx.try_recv().is_ok() {
                break;
            }
            let mut buf = [0; 8192];
            let _ = master.read(&mut buf);
            assert!(start.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(5));
        }
        master.write_all(b"\x03").unwrap();
        prompt(&mut master);
        master.write_all(b"\x1b[A\r").unwrap();
        prompt(&mut master);
        match ending {
            "eof" => master.write_all(b"\x04").unwrap(),
            "quit" => master.write_all(b"/QuIt\r").unwrap(),
            "disconnect" => close_tx.send(()).unwrap(),
            "term" => {
                /* SAFETY: this is the owned child process. */
                assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
            }
            _ => unreachable!(),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            let mut buffer = [0; 8192];
            if let Ok(size) = master.read(&mut buffer) {
                transcript.push_str(&String::from_utf8_lossy(&buffer[..size]));
            }
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("REPL did not exit on {ending}: {transcript}");
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            status.code(),
            Some(match ending {
                "disconnect" => 1,
                "term" => 143,
                _ => 0,
            }),
            "{ending}: {transcript}"
        );
        assert!(transcript.contains("ValueError"));
        assert!(transcript.contains("KeyboardInterrupt"));
        assert!(transcript.contains("你好"));
        server.join().unwrap();
        let mut attrs = std::mem::MaybeUninit::uninit();
        // SAFETY: valid descriptor and output storage.
        assert_eq!(
            unsafe { libc::tcgetattr(master.as_raw_fd(), attrs.as_mut_ptr()) },
            0
        );
        assert_ne!(unsafe { attrs.assume_init().c_lflag } & libc::ECHO, 0);
    }
}

#[test]
fn cli_rich_mime_streaming_and_updates_render_text_but_json_and_notebooks_keep_original_bundles() {
    let svg = "<svg>\r\n</svg>";
    let markdown = json!({"text/markdown":"Hello **Markdown**","text/html":"<p>wrong html</p>",
        "text/plain":"wrong fallback","image/svg+xml":svg,"application/javascript":"let x=1;\r\n"});
    let html = json!({"text/html":"<p>中文 &amp; <b>HTML</b></p>","text/plain":"wrong fallback"});
    let updated = json!({"text/markdown":"updated **display**"});
    for as_json in [false, true] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        f.storage
            .save_session(&id("default"), state(&listener))
            .unwrap();
        let mut credentials = token();
        credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
        f.tokens.save(&id("default"), &credentials).unwrap();
        let notebook = f.temp.path().join("rich.ipynb");
        let (code, identifier) = if as_json {
            ("# @titleSuffix ignored\ndisplay_fixture()", "test-cell")
        } else {
            ("#\t@title 中文标题\ndisplay_fixture()", "中文标题")
        };
        fs::write(&notebook,json!({"nbformat":4,"nbformat_minor":5,"metadata":{"preserve":"original"},"cells":[
            {"cell_type":"code","id":"test-cell","metadata":{},"source":code,"outputs":[],"execution_count":null},
            {"cell_type":"markdown","metadata":{},"source":"![chart](attachment:chart.svg)",
             "attachments":{"chart.svg":{"image/svg+xml":svg}}}
        ]}).to_string()).unwrap();
        let (md, html_bundle, replacement) = (markdown.clone(), html.clone(), updated.clone());
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
                    "display_data",
                    "iopub",
                    request.id(),
                    json!({"data":md,"metadata":{"source":"keep"}}),
                ),
            );
            send(
                &mut socket,
                incoming(
                    "execute_result",
                    "iopub",
                    request.id(),
                    json!({"data":html_bundle,"metadata":{},"execution_count":1,"transient":{"display_id":"updatable"}}),
                ),
            );
            send(
                &mut socket,
                incoming(
                    "update_display_data",
                    "iopub",
                    request.id(),
                    json!({"data":replacement,"metadata":{"updated":true},"transient":{"display_id":"updatable"}}),
                ),
            );
            send(
                &mut socket,
                incoming(
                    "stream",
                    "iopub",
                    request.id(),
                    json!({"name":"stderr","text":"stream-warning\n"}),
                ),
            );
            finish(&mut socket, request.id());
        });
        let output = Command::new(env!("CARGO_BIN_EXE_colab"))
            .env_remove("COLAB_ACCOUNT")
            .arg("--config-dir")
            .arg(f.temp.path())
            .args(["--token-storage", "file", "exec", "-s", "work", "-f"])
            .arg(&notebook)
            .args(if as_json { vec!["--json"] } else { vec![] })
            .output()
            .unwrap();
        handler.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if as_json {
            let results: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(results[0]["outputs"][0]["data"], markdown);
            assert_eq!(results[0]["outputs"][1]["data"], updated);
            assert!(output.stderr.is_empty());
        } else {
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(stdout.contains("[colab] Executing cell 1/1 - 中文标题..."));
            // Notebook text mode also emits the existing parse/cell/save progress.
            assert!(
                stdout.contains("Hello Markdown\n中文 & HTML\nupdated display\n"),
                "{stdout}"
            );
            assert!(!stdout.contains("wrong"));
            assert!(!stdout.contains("<p>"));
            assert!(!stdout.contains("stream-warning"));
            assert_eq!(
                String::from_utf8(output.stderr).unwrap(),
                "stream-warning\n"
            );
        }
        let saved: Value =
            serde_json::from_slice(&fs::read(f.temp.path().join("rich_output.ipynb")).unwrap())
                .unwrap();
        let sessions = f.storage.sessions(&id("default")).unwrap();
        assert_eq!(
            sessions["work"]
                .last_execution
                .as_ref()
                .unwrap()
                .1
                .as_deref(),
            Some(identifier)
        );
        assert_eq!(saved["metadata"]["preserve"], "original");
        assert_eq!(
            saved["cells"][1]["attachments"]["chart.svg"]["image/svg+xml"],
            if as_json {
                json!(svg)
            } else {
                json!(["<svg>\r\n", "</svg>"])
            }
        );
        for (index, bundle) in [(0, &markdown), (1, &updated)] {
            for (mime, original) in bundle.as_object().unwrap() {
                let value = &saved["cells"][0]["outputs"][index]["data"][mime];
                let joined: String = if as_json {
                    value.as_str().unwrap().to_owned()
                } else {
                    value
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|line| line.as_str().unwrap())
                        .collect()
                };
                assert_eq!(joined, original.as_str().unwrap());
            }
        }
        assert_eq!(
            saved["cells"][0]["outputs"][0]["metadata"],
            json!({"source":"keep"})
        );
        assert_eq!(
            saved["cells"][0]["outputs"][1]["metadata"],
            json!({"updated":true})
        );
    }
}

#[cfg(unix)]
#[test]
fn cli_images_use_tty_only_packets_and_private_fallback_files_without_polluting_json() {
    use std::{fs::File, os::fd::FromRawFd};
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRz8AAAAASUVORK5CYII=";
    #[derive(Default)]
    struct Case {
        tty: bool,
        as_json: bool,
        explicit: bool,
        jpeg: bool,
        invalid: bool,
        update: bool,
        broken_temp: bool,
        broken_jpeg: bool,
        notebook: bool,
    }
    for Case {
        tty,
        as_json,
        explicit,
        jpeg,
        invalid,
        update,
        broken_temp,
        broken_jpeg,
        notebook,
    } in [
        Case::default(),
        Case {
            explicit: true,
            ..Case::default()
        },
        Case {
            tty: true,
            ..Case::default()
        },
        Case {
            tty: true,
            explicit: true,
            ..Case::default()
        },
        Case {
            tty: true,
            as_json: true,
            ..Case::default()
        },
        Case {
            jpeg: true,
            ..Case::default()
        },
        Case {
            tty: true,
            jpeg: true,
            ..Case::default()
        },
        Case {
            tty: true,
            jpeg: true,
            explicit: true,
            ..Case::default()
        },
        Case {
            tty: true,
            jpeg: true,
            as_json: true,
            ..Case::default()
        },
        Case {
            tty: true,
            jpeg: true,
            update: true,
            ..Case::default()
        },
        Case {
            tty: true,
            jpeg: true,
            broken_jpeg: true,
            ..Case::default()
        },
        Case {
            jpeg: true,
            broken_jpeg: true,
            ..Case::default()
        },
        Case {
            tty: true,
            jpeg: true,
            notebook: true,
            ..Case::default()
        },
        Case {
            invalid: true,
            ..Case::default()
        },
        Case {
            update: true,
            ..Case::default()
        },
        Case {
            broken_temp: true,
            ..Case::default()
        },
    ] {
        let f = Fixture::new(TokenStorageMode::File);
        let listener = listener();
        f.storage
            .save_session(&id("default"), state(&listener))
            .unwrap();
        let mut credentials = token();
        credentials.expiry = Some(Utc::now() + chrono::Duration::hours(1));
        f.tokens.save(&id("default"), &credentials).unwrap();
        let image_dir = f.temp.path().join("image-temp");
        if broken_temp {
            fs::write(&image_dir, b"owned-not-a-directory").unwrap();
        } else {
            fs::create_dir(&image_dir).unwrap();
        }
        let expected = if broken_jpeg {
            b"\xff\xd8\xffprivate-image-body".to_vec()
        } else if jpeg {
            let mut bytes = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90)
                .encode(
                    &[240, 20, 10, 20, 240, 10, 20, 10, 240, 220, 220, 220],
                    2,
                    2,
                    image::ExtendedColorType::Rgb8,
                )
                .unwrap();
            bytes
        } else {
            STANDARD.decode(PNG).unwrap()
        };
        let encoded = if invalid {
            "private-invalid-base64-!".into()
        } else {
            format!("{}\n", STANDARD.encode(&expected))
        };
        let mime = if jpeg { "image/jpeg" } else { "image/png" };
        let bundle = json!({mime:encoded});
        let served = bundle.clone();
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
                    "display_data",
                    "iopub",
                    request.id(),
                    json!({"data":served,"metadata":{},"transient":{"display_id":"image-1"}}),
                ),
            );
            if update {
                send(
                    &mut socket,
                    incoming(
                        "update_display_data",
                        "iopub",
                        request.id(),
                        json!({"data":served,"metadata":{},"transient":{"display_id":"image-1"}}),
                    ),
                );
            }
            finish(&mut socket, request.id());
        });
        let target = f.temp.path().join(if jpeg {
            "explicit.jpeg"
        } else {
            "explicit.png"
        });
        let notebook_path = f.temp.path().join("image.ipynb");
        if notebook {
            fs::write(&notebook_path,json!({"cells":[{"cell_type":"code","source":["display_image()"],"metadata":{},"outputs":[],"execution_count":null}],"metadata":{"preserve":"original"},"nbformat":4,"nbformat_minor":5}).to_string()).unwrap();
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_colab"));
        command
            .env_remove("COLAB_ACCOUNT")
            .env("TMPDIR", &image_dir)
            .arg("--config-dir")
            .arg(f.temp.path())
            .args(["--token-storage", "file", "exec", "-s", "work"])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped());
        if notebook {
            command.arg("-f").arg(&notebook_path);
        }
        if as_json {
            command.arg("--json");
        }
        if explicit {
            command.arg("--output-image").arg(&target);
        }
        let mut master = None;
        if tty {
            let (mut primary, mut secondary) = (-1, -1);
            // SAFETY: pointers are valid; optional name/termios/winsize are null.
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut primary,
                        &mut secondary,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            // SAFETY: successful openpty returned distinct owned descriptors.
            master = Some(unsafe { File::from_raw_fd(primary) });
            command.stdout(unsafe { File::from_raw_fd(secondary) });
        } else {
            command.stdout(Stdio::piped());
        }
        let mut child = command.spawn().unwrap();
        drop(command); // Close the parent's PTY slave before reading to EOF.
        let mut stdin = child.stdin.take().unwrap();
        if !notebook {
            stdin.write_all(b"display_image()").unwrap();
        }
        drop(stdin);
        let mut stdout = Vec::new();
        if let Some(mut master) = master {
            loop {
                let mut bytes = [0; 4096];
                match master.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => stdout.extend_from_slice(&bytes[..count]),
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(error) => panic!("pty read: {error}"),
                }
            }
        }
        let output = child.wait_with_output().unwrap();
        handler.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if !tty {
            stdout = output.stdout;
        }
        let text = String::from_utf8(stdout).unwrap();
        if notebook {
            let saved: Value = serde_json::from_slice(
                &fs::read(f.temp.path().join("image_output.ipynb")).unwrap(),
            )
            .unwrap();
            assert_eq!(saved["metadata"]["preserve"], "original");
            assert_eq!(saved["cells"][0]["outputs"][0]["data"], bundle);
        }
        let files: Vec<_> = if broken_temp {
            Vec::new()
        } else {
            fs::read_dir(&image_dir)
                .unwrap()
                .map(|row| row.unwrap().path())
                .collect()
        };
        if as_json {
            let results: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(results[0]["outputs"][0]["data"], bundle);
            assert!(files.is_empty());
            assert!(output.stderr.is_empty());
        } else if broken_temp {
            assert!(String::from_utf8_lossy(&output.stderr).contains("图片保存失败"));
            assert!(text.is_empty());
            assert_eq!(fs::read(&image_dir).unwrap(), b"owned-not-a-directory");
        } else if invalid {
            assert!(files.is_empty());
            let warning = String::from_utf8(output.stderr).unwrap();
            assert!(warning.contains("图片 base64 无效"));
            assert!(!warning.contains("private-invalid"));
            assert!(text.is_empty());
        } else {
            if tty && broken_jpeg {
                let warning = String::from_utf8_lossy(&output.stderr);
                assert!(warning.contains("图片显示失败") && warning.contains("JPEG"));
                assert!(!warning.contains("private-image-body"));
            } else {
                assert!(
                    output.stderr.is_empty(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            if explicit {
                assert!(files.is_empty());
                assert_eq!(fs::read(&target).unwrap(), expected);
                assert!(text.contains(target.to_str().unwrap()));
            } else {
                assert_eq!(files.len(), if update { 2 } else { 1 });
                for path in &files {
                    assert!(text.contains(path.to_str().unwrap()));
                    assert_eq!(fs::read(path).unwrap(), expected);
                    assert_eq!(path.extension().unwrap(), if jpeg { "jpeg" } else { "png" });
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(
                        fs::metadata(path).unwrap().permissions().mode() & 0o777,
                        0o600
                    );
                }
            }
        }
        let packets: Vec<_> = text.split("\x1b_G").skip(1).collect();
        if tty && !as_json && !invalid && !broken_jpeg {
            assert_eq!(packets.len(), if update { 2 } else { 1 });
            for packet in packets {
                let (control, payload) = packet.split_once(';').unwrap();
                assert_eq!(control, "a=T,f=100,q=2,m=0");
                let bytes = STANDARD
                    .decode(payload.split("\x1b\\").next().unwrap())
                    .unwrap();
                if jpeg {
                    let actual =
                        image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                            .unwrap();
                    let original =
                        image::load_from_memory_with_format(&expected, image::ImageFormat::Jpeg)
                            .unwrap();
                    assert_eq!(actual.to_rgba8(), original.to_rgba8());
                } else {
                    assert_eq!(bytes, expected);
                }
            }
        } else {
            assert!(packets.is_empty(), "{text:?}");
        }
        let sessions = f.storage.sessions(&id("default")).unwrap();
        assert!(sessions["work"].running.is_none());
        assert!(!sessions["work"].extra.contains_key("execution_pending"));
    }
}

#[test]
fn run_refreshes_its_bound_oauth_or_adc_before_cleanup_and_retains_failed_cleanup() {
    for adc in [false, true] {
        for refresh_ok in [true, false] {
            let f = Fixture::new(TokenStorageMode::File);
            let listener = listener();
            let runtime_url = state(&listener).url;
            let control = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", control.server_addr());
            let mut credentials = token();
            credentials.token_uri = format!("{base}/token");
            credentials.expiry = Some(Utc::now() - chrono::Duration::hours(1));
            let adc_path = f.temp.path().join("adc.json");
            if adc {
                let mut value = serde_json::to_value(&credentials).unwrap();
                value["type"] = json!("authorized_user");
                value["quota_project_id"] = json!("run-project");
                fs::write(&adc_path, value.to_string()).unwrap();
                f.accounts
                    .update(|registry| {
                        let account = registry.accounts.get_mut(&id("default")).unwrap();
                        account.auth_provider = colabcli_rust::models::AuthProvider::Adc;
                        account.adc_credentials_file =
                            Some(adc_path.to_string_lossy().into_owned());
                        Ok(())
                    })
                    .unwrap();
            } else {
                f.tokens.save(&id("default"), &credentials).unwrap();
            }
            let script = f.temp.path().join("script.py");
            fs::write(&script, "print('finished')\n").unwrap();
            let control_handler = thread::spawn(move || {
                let recv = || {
                    control
                        .recv_timeout(Duration::from_secs(12))
                        .unwrap()
                        .unwrap()
                };
                let header = |request: &tiny_http::Request, name: &'static str| {
                    request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv(name))
                        .map(|h| h.value.as_str().to_owned())
                };
                let check_refresh = |mut request: tiny_http::Request, expected: &str| {
                    assert_eq!(request.method().as_str(), "POST");
                    assert_eq!(request.url(), "/token");
                    assert!(header(&request, "Authorization").is_none());
                    assert!(header(&request, "x-goog-user-project").is_none());
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    let fields: BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
                        .into_owned()
                        .collect();
                    assert_eq!(fields["refresh_token"], expected);
                    request
                };
                let request = check_refresh(recv(), "fixture-refresh");
                // Fresh at allocation, then crosses the provider's 60s refresh
                // margin while user code runs. No wall-clock mutation required.
                request.respond(tiny_http::Response::from_string(json!({"access_token":"run-initial","refresh_token":"run-rotated","expires_in":65,"token_type":"Bearer"}).to_string())).unwrap();
                let request = recv();
                assert!(request.url().starts_with("/tun/m/assign?"));
                assert_eq!(
                    header(&request, "Authorization").as_deref(),
                    Some("Bearer run-initial")
                );
                assert_eq!(
                    header(&request, "x-goog-user-project").as_deref(),
                    adc.then_some("run-project")
                );
                request.respond(tiny_http::Response::from_string(json!({"endpoint":"ep-run","runtimeProxyInfo":{"url":runtime_url,"token":"fixture-proxy","tokenExpiresInSeconds":3600}}).to_string())).unwrap();
                // The execution phase reuses the original provider and its rotation.
                // Cleanup must refresh it once the in-memory bearer approaches expiry.
                let request = check_refresh(recv(), "run-rotated");
                if refresh_ok {
                    request.respond(tiny_http::Response::from_string(json!({"access_token":"run-final","refresh_token":"run-final-refresh","expires_in":3600,"token_type":"Bearer"}).to_string())).unwrap();
                    let request = recv();
                    assert_eq!(request.method().as_str(), "GET");
                    assert!(request.url().starts_with("/tun/m/assignments?"));
                    assert_eq!(
                        header(&request, "Authorization").as_deref(),
                        Some("Bearer run-final")
                    );
                    assert_eq!(
                        header(&request, "x-goog-user-project").as_deref(),
                        adc.then_some("run-project")
                    );
                    request
                        .respond(tiny_http::Response::from_string(
                            release_inventory("ep-run", &runtime_url).to_string(),
                        ))
                        .unwrap();
                    for method in ["GET", "POST"] {
                        let request = recv();
                        assert_eq!(request.method().as_str(), method);
                        assert!(request.url().starts_with("/tun/m/unassign/ep-run?"));
                        assert_eq!(
                            header(&request, "Authorization").as_deref(),
                            Some("Bearer run-final")
                        );
                        assert_eq!(
                            header(&request, "x-goog-user-project").as_deref(),
                            adc.then_some("run-project")
                        );
                        if method == "GET" {
                            request
                                .respond(tiny_http::Response::from_string(
                                    json!({"token":"xsrf"}).to_string(),
                                ))
                                .unwrap();
                        } else {
                            assert_eq!(
                                header(&request, "X-Goog-Colab-Token").as_deref(),
                                Some("xsrf")
                            );
                            request.respond(tiny_http::Response::empty(204)).unwrap();
                        }
                    }
                } else {
                    request
                        .respond(
                            tiny_http::Response::from_string("private-refresh-error")
                                .with_status_code(400),
                        )
                        .unwrap();
                }
                assert!(
                    control
                        .recv_timeout(Duration::from_millis(100))
                        .unwrap()
                        .is_none()
                );
            });
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
                let payload = run_payload(&request);
                thread::sleep(Duration::from_secs(6));
                send(
                    &mut socket,
                    incoming(
                        "display_data",
                        "iopub",
                        request.id(),
                        json!({"data":{colabcli_rust::runs::RESULT_MIME:{"nonce":payload["nonce"],"exit_code":0}},"metadata":{}}),
                    ),
                );
                finish(&mut socket, request.id());
                drop(socket);
                if refresh_ok {
                    http(
                        &listener,
                        "DELETE",
                        "/prefix/api/kernels/kernel-1",
                        204,
                        Value::Null,
                    );
                }
            });
            let output = cli_command(&f, &base)
                .env_clear()
                .env("HOME", f.temp.path())
                .env("NO_PROXY", "*")
                .args(["run", "--json", "--timeout", "10", "-s", "one-shot"])
                .arg(script)
                .output()
                .unwrap();
            runtime_handler.join().unwrap();
            control_handler.join().unwrap();
            assert_eq!(output.status.code(), Some(if refresh_ok { 0 } else { 1 }));
            let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(outcome["exit_code"], 0);
            assert_eq!(
                outcome["cleanup"],
                if refresh_ok {
                    "confirmed_released"
                } else {
                    "pending"
                }
            );
            assert_eq!(
                f.storage.sessions(&id("default")).unwrap().is_empty(),
                refresh_ok
            );
            let journal: Option<Value> = f
                .storage
                .read(
                    &f.storage
                        .profile_path(&id("default"), "control_operation.json")
                        .unwrap(),
                )
                .unwrap();
            if refresh_ok {
                assert!(journal.is_none());
            } else {
                let journal = journal.unwrap();
                assert_eq!(journal["operation"], "release");
                assert_eq!(journal["endpoint"], "ep-run");
                assert_eq!(outcome["cleanup_error"], "OAuth 服务请求失败 (HTTP 400)");
            }
            assert_eq!(
                colabcli_rust::runs::pending(&f.storage, &id("default"))
                    .unwrap()
                    .is_empty(),
                refresh_ok
            );
            for marker in [
                "run-initial",
                "run-final",
                "run-rotated",
                "private-refresh-error",
            ] {
                assert!(!String::from_utf8_lossy(&output.stdout).contains(marker));
                assert!(!String::from_utf8_lossy(&output.stderr).contains(marker));
            }
            if adc {
                let value: Value = serde_json::from_slice(&fs::read(&adc_path).unwrap()).unwrap();
                assert_eq!(value["refresh_token"], "fixture-refresh");
            } else {
                let saved = f.tokens.load(&id("default")).unwrap().unwrap();
                assert_eq!(
                    saved.refresh_token.unwrap().expose(),
                    if refresh_ok {
                        "run-final-refresh"
                    } else {
                        "run-rotated"
                    }
                );
                assert!(
                    !f.storage
                        .profile_path(&id("default"), "oauth_refresh_pending.json")
                        .unwrap()
                        .exists()
                );
            }
        }
    }
}
