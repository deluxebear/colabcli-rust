mod common;
use chrono::{Duration as Delta, Utc};
use colabcli_rust::{
    auth::{AdcProvider, AwsEnvironment, MetadataOptions, TokenClient},
    colab_api::{ApiConfig, ColabApi},
    context::{AccountContext, AccountSource},
    credentials::TokenStorageMode,
    models::{Account, AuthProvider},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap, fs, path::Path, process::Command, sync::Arc, thread, time::Duration,
};
use tiny_http::{Request, Response, Server, StatusCode};
fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn base(server: &Server) -> String {
    format!("http://{}", server.server_addr())
}
fn recv(server: &Server) -> Request {
    server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("expected loopback request")
}
fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}
fn reply(request: Request, value: Value) {
    request
        .respond(Response::from_string(value.to_string()))
        .unwrap();
}
fn user(base: &str, name: &str) -> Value {
    json!({"type":"authorized_user","token":format!("token-{name}"),"expiry":(Utc::now()+Delta::hours(1)).to_rfc3339(),"client_id":"fixture-client","client_secret":"fixture-secret","refresh_token":"fixture-refresh","token_uri":format!("{base}/token"),"quota_project_id":format!("quota-{name}")})
}
fn context(path: Option<&Path>, quota: Option<&str>) -> AccountContext {
    AccountContext {
        profile: common::id("default"),
        source: AccountSource::Flag,
        provider: AuthProvider::Adc,
        adc_file: path.map(Path::to_path_buf),
        adc_file_required: path.is_some(),
        quota_project: quota.map(str::to_owned),
        cancellation: None,
        metadata: MetadataOptions {
            disabled: true,
            ..Default::default()
        },
        allow_executables: false,
        aws: AwsEnvironment::default(),
        certificate: Default::default(),
    }
}
fn provider(path: &Path, quota: Option<&str>) -> AdcProvider {
    AdcProvider::from_context(&context(Some(path), quota), TokenClient::new().unwrap())
}
fn api(server_base: &str, credentials: colabcli_rust::auth::RequestCredentials) -> ColabApi {
    ColabApi::new(
        ApiConfig {
            base_url: server_base.parse().unwrap(),
            timeout: Duration::from_secs(3),
            retries: 0,
            backoff: Duration::ZERO,
        },
        credentials.token,
    )
    .unwrap()
    .with_quota_project(credentials.quota_project_id.as_deref())
    .unwrap()
}
fn iam(base: &str) -> String {
    format!("{base}/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken")
}
fn iam_success() -> Value {
    json!({"accessToken":"target-token","expireTime":(Utc::now()+Delta::hours(1)).to_rfc3339()})
}
#[test]
fn quota_precedence_empty_values_and_provider_snapshot_preserve_the_source_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    let oracle: Value =
        serde_json::from_str(include_str!("../research/adc-quota-probe.json")).unwrap();
    let mut cases: Vec<Value> = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["kind"] == "user")
        .cloned()
        .collect();
    cases.push(json!({"file":false,"environment":"env-project","effective":"env-project"}));
    for case in cases {
        let file_quota = case["file"].clone();
        let override_quota = case["environment"].as_str();
        let expected = case["effective"].as_str();
        let mut value = user("http://127.0.0.1:1", "source");
        value["quota_project_id"] = file_quota;
        let original = value.to_string();
        fs::write(&path, &original).unwrap();
        let mut selected = context(Some(&path), override_quota);
        let provider = AdcProvider::from_context(&selected, TokenClient::new().unwrap());
        selected.quota_project = Some("changed-context".into());
        let credentials = provider.request_credentials(Utc::now()).unwrap();
        assert_eq!(credentials.quota_project_id.as_deref(), expected);
        assert_eq!(credentials.token.expose(), "token-source");
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        value["quota_project_id"] = json!("changed-file");
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            provider
                .request_credentials(Utc::now())
                .unwrap()
                .quota_project_id
                .as_deref(),
            expected
        );
    }
}
#[test]
fn concurrent_control_clients_keep_bearer_and_quota_paired_on_get_and_post() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let base = base(&server);
    let mut providers = vec![];
    for name in ["a", "b"] {
        let path = temp.path().join(format!("{name}.json"));
        fs::write(&path, user(&base, name).to_string()).unwrap();
        providers.push(Arc::new(provider(&path, None)));
    }
    let handler = thread::spawn(move || {
        let mut seen = BTreeMap::new();
        for _ in 0..6 {
            let request = recv(&server);
            let name = header(&request, "Authorization")
                .unwrap()
                .strip_prefix("Bearer token-")
                .unwrap()
                .to_owned();
            assert!(matches!(name.as_str(), "a" | "b"));
            assert_eq!(
                header(&request, "x-goog-user-project"),
                Some(format!("quota-{name}").as_str())
            );
            *seen.entry(name).or_insert(0) += 1;
            if request.url().starts_with("/tun/m/assignments") {
                reply(request, json!({"assignments":[]}));
            } else if request.method().as_str() == "GET" {
                reply(request, json!({"token":"release-xsrf"}));
            } else {
                assert_eq!(request.method().as_str(), "POST");
                request.respond(Response::empty(200)).unwrap();
            }
        }
        assert_eq!(seen.values().copied().collect::<Vec<_>>(), vec![3, 3]);
    });
    thread::scope(|scope| {
        for provider in providers {
            let base = &base;
            scope.spawn(move || {
                let client = api(base, provider.request_credentials(Utc::now()).unwrap());
                assert!(client.assignments().unwrap().is_empty());
                client.unassign("fixture-endpoint").unwrap();
            });
        }
    });
    handler.join().unwrap();
}
#[test]
fn oauth_refresh_never_receives_quota_and_failed_refresh_keeps_source_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    let server = server();
    let base = base(&server);
    let mut value = user(&base, "source");
    value["expiry"] = json!((Utc::now() - Delta::hours(1)).to_rfc3339());
    let original = value.to_string();
    fs::write(&path, &original).unwrap();
    let provider = provider(&path, None);
    let handler = thread::spawn(move || {
        for reject in [false, true] {
            let mut request = recv(&server);
            assert_eq!(request.url(), "/token");
            assert!(header(&request, "x-goog-user-project").is_none());
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            assert!(!body.contains("quota_project_id") && !body.contains("quota-source"));
            if reject {
                request
                    .respond(
                        Response::from_string("private-quota-marker")
                            .with_status_code(StatusCode(400)),
                    )
                    .unwrap();
            } else {
                reply(
                    request,
                    json!({"access_token":"refreshed-token","expires_in":3600,"token_type":"Bearer","quota_project_id":"untrusted-response-project"}),
                );
            }
        }
    });
    let credentials = provider.request_credentials(Utc::now()).unwrap();
    assert_eq!(
        credentials.quota_project_id.as_deref(),
        Some("quota-source")
    );
    assert_eq!(credentials.token.expose(), "refreshed-token");
    assert!(
        !provider
            .refresh_access_token(Utc::now())
            .err()
            .unwrap()
            .to_string()
            .contains("private-quota-marker")
    );
    let cached = provider.request_credentials(Utc::now()).unwrap();
    assert_eq!(cached.token.expose(), "refreshed-token");
    assert_eq!(cached.quota_project_id.as_deref(), Some("quota-source"));
    handler.join().unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}
#[test]
fn invalid_quota_is_redacted_and_rejected_before_refresh_metadata_or_iam() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    let server = server();
    let base = base(&server);
    for bad in [
        json!("private-marker\r\nInjected: value"),
        json!(" "),
        json!("中文"),
        json!("x".repeat(257)),
        json!(12),
        json!(false),
    ] {
        let mut value = user(&base, "source");
        value["expiry"] = Value::Null;
        value["quota_project_id"] = bad;
        fs::write(&path, value.to_string()).unwrap();
        let error = provider(&path, None)
            .request_credentials(Utc::now())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("quota_project_id") && !error.contains("private-marker"));
    }
    fs::write(&path, user(&base, "source").to_string()).unwrap();
    assert!(
        provider(&path, Some("private-marker\n"))
            .request_credentials(Utc::now())
            .is_err()
    );
    for root in [json!([]), json!(42), json!("private-marker"), Value::Null] {
        fs::write(&path, root.to_string()).unwrap();
        assert!(
            provider(&path, None)
                .request_credentials(Utc::now())
                .is_err()
        );
    }
    let mut source = user(&base, "source");
    source["quota_project_id"] = json!("private-marker\r\n");
    let value = json!({"type":"impersonated_service_account","quota_project_id":"outer-project","source_credentials":source,"service_account_impersonation_url":iam(&base)});
    fs::write(&path, value.to_string()).unwrap();
    assert!(
        provider(&path, Some("env-project"))
            .request_credentials(Utc::now())
            .is_err()
    );
    let mut selected = context(None, Some("private-marker\r\n"));
    selected.metadata = MetadataOptions {
        host: server.server_addr().to_string(),
        probe_host: server.server_addr().to_string(),
        disabled: false,
        timeout_seconds: 1,
        mtls: Box::default(),
    };
    assert!(
        AdcProvider::from_context(&selected, TokenClient::new().unwrap())
            .request_credentials(Utc::now())
            .is_err()
    );
    assert!(
        server
            .recv_timeout(Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
}
#[test]
fn impersonation_uses_embedded_source_quota_for_iam_and_target_quota_for_colab() {
    for override_quota in [None, Some("env-project")] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("adc.json");
        let server = server();
        let base = base(&server);
        let value = json!({"type":"impersonated_service_account","quota_project_id":"target-project","source_credentials":user(&base,"source"),"service_account_impersonation_url":iam(&base)});
        fs::write(&path, value.to_string()).unwrap();
        let provider = provider(&path, override_quota);
        let handler = thread::spawn(move || {
            let request = recv(&server);
            assert!(request.url().ends_with(":generateAccessToken"));
            assert_eq!(
                header(&request, "Authorization"),
                Some("Bearer token-source")
            );
            assert_eq!(
                header(&request, "x-goog-user-project"),
                Some("quota-source")
            );
            reply(request, iam_success());
            let request = recv(&server);
            assert_eq!(
                header(&request, "Authorization"),
                Some("Bearer target-token")
            );
            assert_eq!(
                header(&request, "x-goog-user-project"),
                Some(override_quota.unwrap_or("target-project"))
            );
            reply(request, json!({"assignments":[]}));
        });
        api(&base, provider.request_credentials(Utc::now()).unwrap())
            .assignments()
            .unwrap();
        handler.join().unwrap();
    }
}
#[test]
fn federated_source_quota_reaches_iam_and_colab_but_not_sts() {
    for override_quota in [None, Some("env-project")] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("adc.json");
        let subject = temp.path().join("subject.jwt");
        fs::write(&subject, "fixture-subject").unwrap();
        let server = server();
        let base = base(&server);
        let value = json!({"type":"external_account","audience":"//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p","subject_token_type":"urn:ietf:params:oauth:token-type:jwt","token_url":format!("{base}/v1/token"),"quota_project_id":"file-project","credential_source":{"file":subject},"service_account_impersonation_url":iam(&base)});
        fs::write(&path, value.to_string()).unwrap();
        let provider = provider(&path, override_quota);
        let handler = thread::spawn(move || {
            let request = recv(&server);
            assert_eq!(request.url(), "/v1/token");
            assert!(header(&request, "x-goog-user-project").is_none());
            reply(
                request,
                json!({"access_token":"sts-token","expires_in":3600,"token_type":"Bearer","issued_token_type":"urn:ietf:params:oauth:token-type:access_token"}),
            );
            let request = recv(&server);
            assert_eq!(header(&request, "Authorization"), Some("Bearer sts-token"));
            assert_eq!(
                header(&request, "x-goog-user-project"),
                Some(override_quota.unwrap_or("file-project"))
            );
            reply(request, iam_success());
            let request = recv(&server);
            assert_eq!(
                header(&request, "x-goog-user-project"),
                Some(override_quota.unwrap_or("file-project"))
            );
            reply(request, json!({"assignments":[]}));
        });
        api(&base, provider.request_credentials(Utc::now()).unwrap())
            .assignments()
            .unwrap();
        handler.join().unwrap();
    }
}
#[test]
fn service_account_jwt_exchange_keeps_quota_out_of_the_assertion_and_token_endpoint() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    let server = server();
    let base = base(&server);
    let mut value =
        common::service_account::document(0, "service@example.invalid", &format!("{base}/token"));
    value["quota_project_id"] = json!("service-project");
    let original = value.to_string();
    fs::write(&path, &original).unwrap();
    let provider = provider(&path, None);
    let handler = thread::spawn(move || {
        let mut request = recv(&server);
        assert!(header(&request, "x-goog-user-project").is_none());
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        let fields: BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        common::service_account::verify_recent(&fields["assertion"], 0, "service@example.invalid");
        reply(
            request,
            json!({"access_token":"service-token","token_type":"Bearer","expires_in":3600}),
        );
        let request = recv(&server);
        assert_eq!(
            header(&request, "x-goog-user-project"),
            Some("service-project")
        );
        assert_eq!(
            header(&request, "Authorization"),
            Some("Bearer service-token")
        );
        reply(request, json!({"assignments":[]}));
    });
    api(&base, provider.request_credentials(Utc::now()).unwrap())
        .assignments()
        .unwrap();
    handler.join().unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}
#[test]
fn actual_cli_snapshots_quota_for_each_adc_account_and_ignores_it_for_oauth() {
    for override_quota in [None, Some(""), Some("env-project")] {
        let fixture = common::Fixture::new(TokenStorageMode::File);
        let server = self::server();
        let base = self::base(&server);
        for name in ["default", "b"] {
            let path = fixture.temp.path().join(format!("{name}.json"));
            fs::write(&path, user(&base, name).to_string()).unwrap();
            let account = Account {
                auth_provider: AuthProvider::Adc,
                adc_credentials_file: Some(path.to_string_lossy().into_owned()),
                ..Default::default()
            };
            if name == "default" {
                fixture
                    .accounts
                    .update(|registry| {
                        registry.accounts.insert(common::id(name), account);
                        Ok(())
                    })
                    .unwrap();
            } else {
                fixture.accounts.add(common::id(name), account).unwrap();
            }
        }
        let handler = thread::spawn(move || {
            let mut seen = vec![];
            for _ in 0..2 {
                let request = recv(&server);
                let name = header(&request, "Authorization")
                    .unwrap()
                    .strip_prefix("Bearer token-")
                    .unwrap()
                    .to_owned();
                assert_eq!(
                    header(&request, "x-goog-user-project"),
                    Some(
                        override_quota
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("quota-{name}"))
                            .as_str()
                    )
                );
                seen.push(name);
                reply(request, json!({"assignments":[]}));
            }
            seen.sort();
            assert_eq!(seen, ["b", "default"]);
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_colab"));
        command
            .env_clear()
            .env("HOME", fixture.temp.path())
            .arg("--config-dir")
            .arg(fixture.temp.path())
            .args([
                "--token-storage",
                "file",
                "--control-plane-url",
                &base,
                "sessions",
                "--all",
                "--json",
            ]);
        if let Some(value) = override_quota {
            command.env("GOOGLE_CLOUD_QUOTA_PROJECT", value);
        }
        let output = command.output().unwrap();
        handler.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert!(
            rows.as_array()
                .unwrap()
                .iter()
                .all(|row| row["error"].is_null())
        );
    }
    let fixture = common::Fixture::new(TokenStorageMode::File);
    let server = self::server();
    let base = self::base(&server);
    let mut token = common::token();
    token.expiry = Some(Utc::now() + Delta::hours(1));
    fixture.tokens.save(&common::id("default"), &token).unwrap();
    let handler = thread::spawn(move || {
        let request = recv(&server);
        assert!(header(&request, "x-goog-user-project").is_none());
        reply(request, json!({"assignments":[]}));
    });
    let output = Command::new(env!("CARGO_BIN_EXE_colab"))
        .env_clear()
        .env("HOME", fixture.temp.path())
        .env("GOOGLE_CLOUD_QUOTA_PROJECT", "private-marker\n")
        .arg("--config-dir")
        .arg(fixture.temp.path())
        .args([
            "--token-storage",
            "file",
            "--control-plane-url",
            &base,
            "sessions",
            "--json",
        ])
        .output()
        .unwrap();
    handler.join().unwrap();
    assert!(output.status.success());
    let row: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(row["error"].is_null(), "{row}");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let fixture = common::Fixture::new(TokenStorageMode::File);
        let server = self::server();
        let base = self::base(&server);
        let path = fixture.temp.path().join("adc.json");
        fs::write(&path, user(&base, "default").to_string()).unwrap();
        fixture
            .accounts
            .update(|registry| {
                let account = registry.accounts.get_mut(&common::id("default")).unwrap();
                account.auth_provider = AuthProvider::Adc;
                account.adc_credentials_file = Some(path.to_string_lossy().into_owned());
                Ok(())
            })
            .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_colab"))
            .env_clear()
            .env("HOME", fixture.temp.path())
            .env(
                "GOOGLE_CLOUD_QUOTA_PROJECT",
                std::ffi::OsString::from_vec(vec![255]),
            )
            .arg("--config-dir")
            .arg(fixture.temp.path())
            .args([
                "--token-storage",
                "file",
                "--control-plane-url",
                &base,
                "--request-timeout",
                "1",
                "sessions",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let row: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(row["error"].as_str().unwrap().contains("quota_project_id"));
        assert!(
            server
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn actual_cli_keeps_adc_quota_off_runtime_rest_and_websocket() {
    use colabcli_rust::jupyter::codec::{self, Frame, KernelMessage};
    use std::{io::Write, process::Stdio};
    use tungstenite::Message;
    fn send<S: std::io::Read + std::io::Write>(
        socket: &mut tungstenite::WebSocket<S>,
        kind: &str,
        parent: &str,
        content: Value,
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
    let fixture = common::Fixture::new(TokenStorageMode::File);
    let server = server();
    let base = base(&server);
    let path = fixture.temp.path().join("adc.json");
    fs::write(&path, user(&base, "default").to_string()).unwrap();
    fixture
        .accounts
        .update(|registry| {
            let account = registry.accounts.get_mut(&common::id("default")).unwrap();
            account.auth_provider = AuthProvider::Adc;
            account.adc_credentials_file = Some(path.to_string_lossy().into_owned());
            Ok(())
        })
        .unwrap();
    let mut state = common::session("work");
    state.url = format!("{base}/prefix/");
    state.token_expires_at = Some(Utc::now() + Delta::hours(1));
    fixture
        .storage
        .save_session(&common::id("default"), state)
        .unwrap();
    let handler = thread::spawn(move || {
        let mut request = recv(&server);
        assert_eq!(request.method().as_str(), "POST");
        assert_eq!(request.url(), "/prefix/api/kernels");
        assert_eq!(
            header(&request, "Authorization"),
            Some("Bearer fixture-proxy")
        );
        assert!(header(&request, "x-goog-user-project").is_none());
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        reply(request, json!({"id":"kernel-1"}));
        let request = recv(&server);
        assert!(
            request
                .url()
                .starts_with("/prefix/api/kernels/kernel-1/channels?")
        );
        assert_eq!(
            header(&request, "Authorization"),
            Some("Bearer fixture-proxy")
        );
        assert!(header(&request, "x-goog-user-project").is_none());
        let accept = tungstenite::handshake::derive_accept_key(
            header(&request, "Sec-WebSocket-Key").unwrap().as_bytes(),
        );
        let response = Response::empty(101)
            .with_header(tiny_http::Header::from_bytes("Sec-WebSocket-Accept", accept).unwrap());
        let stream = request.upgrade("websocket", response);
        let mut socket = tungstenite::WebSocket::from_raw_socket(
            stream,
            tungstenite::protocol::Role::Server,
            None,
        );
        for index in 0..3 {
            let message = match socket.read().unwrap() {
                Message::Text(text) => codec::decode_text(&text).unwrap(),
                other => panic!("{other:?}"),
            };
            let user = message.content()["code"] == "print('runtime')";
            if user {
                send(
                    &mut socket,
                    "stream",
                    message.id(),
                    json!({"name":"stdout","text":"runtime\n"}),
                );
            }
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
            if user {
                return;
            }
            assert!(index < 2, "user code must follow setup");
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_colab"))
        .env_clear()
        .env("HOME", fixture.temp.path())
        .env("GOOGLE_CLOUD_QUOTA_PROJECT", "env-project")
        .arg("--config-dir")
        .arg(fixture.temp.path())
        .args([
            "--token-storage",
            "file",
            "--request-timeout",
            "3",
            "exec",
            "-s",
            "work",
            "--timeout",
            "1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"print('runtime')")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    handler.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"runtime\n");
    let saved = fixture.storage.sessions(&common::id("default")).unwrap();
    assert!(saved["work"].running.is_none());
    assert!(!saved["work"].extra.contains_key("execution_pending"));
}
