mod common;
use colabcli_rust::{
    Error,
    colab_api::{ApiConfig, ColabApi, RuntimeOptions, notebook_identity},
    context::{self, Environment},
    credentials::TokenStorageMode,
    models::{ProfileId, Secret},
    sessions::SessionService,
};
use common::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, thread, time::Duration};
use tiny_http::{Request, Response, Server, StatusCode};
use url::Url;

fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn config(server: &Server) -> ApiConfig {
    ApiConfig {
        base_url: Url::parse(&format!("http://{}", server.server_addr())).unwrap(),
        timeout: Duration::from_secs(2),
        retries: 1,
        backoff: Duration::ZERO,
    }
}
fn api(server: &Server) -> ColabApi {
    ColabApi::new(config(server), Secret::new("oauth-fixture")).unwrap()
}
fn service(f: &Fixture, server: &Server, profile: &ProfileId) -> SessionService {
    SessionService::new(f.storage.clone(), profile.clone(), api(server))
}
fn recv(server: &Server, method: &str, path: &str) -> (Request, BTreeMap<String, String>) {
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("expected request");
    assert_eq!(request.method().as_str(), method);
    let url = Url::parse(&format!("http://localhost{}", request.url())).unwrap();
    assert_eq!(url.path(), path);
    let pairs: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs["authuser"], "0");
    let header = |name: &str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    };
    assert_eq!(header("Authorization"), Some("Bearer oauth-fixture"));
    assert_eq!(header("Accept"), Some("application/json"));
    assert_eq!(header("X-Colab-Client-Agent"), Some("colab-cli"));
    (request, pairs)
}
fn respond(request: Request, value: Value) {
    request
        .respond(Response::from_string(value.to_string()))
        .unwrap();
}
fn assignment(endpoint: &str) -> Value {
    json!({"endpoint":endpoint,"accelerator":"NONE","variant":0,"machineShape":0,
        "runtimeProxyInfo":{"token":"proxy-fixture","tokenExpiresInSeconds":3600,"url":"https://runtime.invalid"}})
}
fn options() -> RuntimeOptions {
    RuntimeOptions::new(None, None, false).unwrap()
}
fn journal(f: &Fixture, profile: &ProfileId) -> std::path::PathBuf {
    f.storage
        .profile_path(profile, "control_operation.json")
        .unwrap()
}

#[test]
fn identity_and_hardware_queries_match_contract() {
    let a = notebook_identity();
    assert_eq!(a.len(), 44);
    assert_eq!(&a[36..], "........");
    assert_eq!(a.as_bytes()[14], b'4');
    assert_ne!(a, notebook_identity());
    assert!(RuntimeOptions::new(Some("t4"), Some("v5e1"), false).is_err());
    assert!(RuntimeOptions::new(Some("wrong"), None, false).is_err());
    assert_eq!(
        RuntimeOptions::new(Some("a100"), None, true)
            .unwrap()
            .shape(),
        Some("hm")
    );
    for (gpu, tpu) in [
        (Some("l4"), None),
        (None, Some("v5e1")),
        (None, Some("v6e1")),
    ] {
        assert_eq!(RuntimeOptions::new(gpu, tpu, true).unwrap().shape(), None);
    }
}
#[test]
fn allocation_posts_same_query_and_xsrf_without_body_then_persists_private_state() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = server();
    let svc = service(&f, &server, &id("default"));
    let handle = thread::spawn(move || {
        let (request, get) = recv(&server, "GET", "/tun/m/assign");
        assert_eq!(get["accelerator"], "A100");
        assert_eq!(get["variant"], "GPU");
        assert_eq!(get["shape"], "hm");
        respond(
            request,
            json!({"acc":"A100","nbh":get["nbh"],"token":"xsrf-fixture","variant":"GPU"}),
        );
        let (mut request, post) = recv(&server, "POST", "/tun/m/assign");
        assert_eq!(get, post);
        assert!(
            request
                .headers()
                .iter()
                .any(|h| h.field.equiv("X-Goog-Colab-Token") && h.value.as_str() == "xsrf-fixture")
        );
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        assert!(body.is_empty());
        let mut a = assignment("ep-new");
        a["accelerator"] = json!("A100");
        a["variant"] = json!(1);
        respond(request, a);
    });
    let view = svc
        .create(
            Some("work"),
            RuntimeOptions::new(Some("a100"), None, true).unwrap(),
        )
        .unwrap();
    handle.join().unwrap();
    assert_eq!(view.machine_shape, "HIGH_RAM");
    assert_eq!(view.shape_source, "request");
    let output = serde_json::to_string(&view).unwrap();
    assert!(!output.contains("fixture"));
    assert_eq!(
        f.storage.sessions(&id("default")).unwrap()["work"]
            .token
            .expose(),
        "proxy-fixture"
    );
    assert!(!journal(&f, &id("default")).exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(
                f.storage
                    .profile_path(&id("default"), "sessions.json")
                    .unwrap()
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o777,
            0o600
        );
    }
}
#[test]
fn post_gateway_failure_reconciles_same_identity_without_second_post() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = server();
    let svc = service(&f, &server, &id("default"));
    let handle = thread::spawn(move || {
        let (r, first) = recv(&server, "GET", "/tun/m/assign");
        respond(
            r,
            json!({"acc":"NONE","nbh":first["nbh"],"token":"xsrf","variant":"DEFAULT"}),
        );
        let (r, _) = recv(&server, "POST", "/tun/m/assign");
        r.respond(Response::empty(StatusCode(503))).unwrap();
        let (r, retry) = recv(&server, "GET", "/tun/m/assign");
        assert_eq!(first, retry);
        // GET union permits only endpoint and proxy info.
        let mut a = assignment("ep-reconciled");
        a.as_object_mut().unwrap().remove("accelerator");
        a.as_object_mut().unwrap().remove("variant");
        respond(r, a);
    });
    assert_eq!(
        svc.create(Some("work"), options()).unwrap().endpoint,
        "ep-reconciled"
    );
    handle.join().unwrap();
}
#[test]
fn rejected_allocation_keeps_identity_across_invocations_and_blocks_account_removal() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = f.add("work");
    let server = server();
    let svc = service(&f, &server, &profile);
    let handle = thread::spawn(move || {
        let (r, query) = recv(&server, "GET", "/tun/m/assign");
        r.respond(
            Response::from_string("sensitive-server-error").with_status_code(StatusCode(412)),
        )
        .unwrap();
        query["nbh"].clone()
    });
    let error = svc.create(Some("work"), options()).unwrap_err();
    assert!(matches!(error, Error::ControlStatus(412)));
    assert!(!error.to_string().contains("sensitive"));
    let nbh = handle.join().unwrap();
    assert!(matches!(
        f.accounts.remove(&profile, true),
        Err(Error::AccountInUse)
    ));
    assert!(matches!(
        f.accounts.rename(&profile, &id("renamed")),
        Err(Error::AccountInUse)
    ));
    assert!(matches!(
        svc.create(
            Some("other"),
            RuntimeOptions::new(Some("T4"), None, false).unwrap()
        ),
        Err(Error::ControlPending)
    ));
    let server = self::server();
    let svc = service(&f, &server, &profile);
    let handle = thread::spawn(move || {
        let (r, query) = recv(&server, "GET", "/tun/m/assign");
        assert_eq!(query["nbh"], nbh);
        respond(r, assignment("ep-resumed"));
    });
    svc.create(Some("work"), options()).unwrap();
    handle.join().unwrap();
}
#[test]
fn local_commit_failure_recovers_staged_assignment_without_network() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let path = f.storage.profile_path(&profile, "sessions.json").unwrap();
    // A directory causes the local session commit to fail after the assignment has been staged.
    std::fs::create_dir_all(&path).unwrap();
    let server = server();
    let svc = service(&f, &server, &profile);
    let handle = thread::spawn(move || {
        let (r, _) = recv(&server, "GET", "/tun/m/assign");
        respond(r, assignment("ep-staged"));
    });
    // Initial duplicate-name check also reads sessions, so stage an allocation intent first.
    f.storage.write(&journal(&f, &profile), &json!({"operation":"allocate","name":"work","nbh":notebook_identity(),"options":options(),"session":null})).unwrap();
    assert!(svc.create(Some("work"), options()).is_err());
    handle.join().unwrap();
    let saved: Value = f.storage.read(&journal(&f, &profile)).unwrap().unwrap();
    assert_eq!(saved["session"]["endpoint"], "ep-staged");
    std::fs::remove_dir(&path).unwrap();
    assert_eq!(
        svc.create(Some("work"), options()).unwrap().endpoint,
        "ep-staged"
    );
}
#[test]
fn status_refreshes_prunes_and_displays_orphans_with_unknown_hardware() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    f.storage.save_session(&profile, session("live")).unwrap();
    f.storage.save_session(&profile, session("gone")).unwrap();
    let other = f.add("other");
    f.storage.save_session(&other, session("gone")).unwrap();
    let server = server();
    let svc = service(&f, &server, &profile);
    let handle = thread::spawn(move || {
        let (r, _) = recv(&server, "GET", "/tun/m/assignments");
        let mut a = assignment("ep-live");
        a["variant"] = json!(99);
        a["machineShape"] = json!(8);
        a["accelerator"] = json!("FUTURE");
        r.respond(Response::from_string(format!(
            ")]}}'\n{}",
            json!({"assignments":[a,assignment("orphan")]})
        )))
        .unwrap();
    });
    let views = svc.status(None).unwrap();
    handle.join().unwrap();
    assert_eq!(views.len(), 2);
    assert_eq!(views[0].variant, "Unknown(99)");
    assert_eq!(views[0].machine_shape, "Unknown(8)");
    assert_eq!(views[1].name, None);
    assert!(!f.storage.sessions(&profile).unwrap().contains_key("gone"));
    assert!(f.storage.sessions(&other).unwrap().contains_key("gone"));
    assert!(
        !serde_json::to_string(&views)
            .unwrap()
            .contains("proxy-fixture")
    );
}
#[test]
fn malformed_and_http_failures_never_prune_local_sessions() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    f.storage.save_session(&profile, session("keep")).unwrap();
    for (status, body) in [
        (200, "<html>login</html>"),
        (401, "secret"),
        (302, ""),
        (200, "{}"),
        (200, "{\"assignments\":[{\"endpoint\":\"bad\"}]}"),
    ] {
        let server = server();
        let svc = service(&f, &server, &profile);
        let handle = thread::spawn(move || {
            let (r, _) = recv(&server, "GET", "/tun/m/assignments");
            r.respond(Response::from_string(body).with_status_code(StatusCode(status)))
                .unwrap();
        });
        assert!(svc.status(None).is_err());
        handle.join().unwrap();
        assert!(f.storage.sessions(&profile).unwrap().contains_key("keep"));
    }
}
#[test]
fn release_failure_preserves_state_then_retry_confirms_absence() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    f.storage.save_session(&profile, session("work")).unwrap();
    let server = server();
    let svc = service(&f, &server, &profile);
    let handle = thread::spawn(move || {
        let (r, _) = recv(&server, "GET", "/tun/m/assignments");
        respond(r, json!({"assignments":[assignment("ep-work")]}));
        let (r, _) = recv(&server, "GET", "/tun/m/unassign/ep-work");
        respond(r, json!({"token":"release-xsrf"}));
        let (r, _) = recv(&server, "POST", "/tun/m/unassign/ep-work");
        assert!(
            r.headers()
                .iter()
                .any(|h| h.field.equiv("X-Goog-Colab-Token") && h.value.as_str() == "release-xsrf")
        );
        r.respond(Response::empty(StatusCode(504))).unwrap();
    });
    assert!(svc.stop("work").is_err());
    handle.join().unwrap();
    assert!(f.storage.sessions(&profile).unwrap().contains_key("work"));
    assert!(journal(&f, &profile).exists());
    let server = self::server();
    let svc = service(&f, &server, &profile);
    let handle = thread::spawn(move || {
        let (r, _) = recv(&server, "GET", "/tun/m/assignments");
        respond(r, json!({"assignments":[]}));
    });
    svc.stop("work").unwrap();
    handle.join().unwrap();
    assert!(f.storage.sessions(&profile).unwrap().is_empty());
    assert!(!journal(&f, &profile).exists());
}
#[test]
fn successful_release_accepts_empty_body_and_encodes_endpoint_as_one_segment() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let mut state = session("work");
    state.endpoint = "ep/space ?#%".into();
    f.storage.save_session(&profile, state).unwrap();
    let server = server();
    let svc = service(&f, &server, &profile);
    let handle = thread::spawn(move || {
        let (r, _) = recv(&server, "GET", "/tun/m/assignments");
        respond(r, json!({"assignments":[assignment("ep/space ?#%")]}));
        let (r, _) = recv(&server, "GET", "/tun/m/unassign/ep%2Fspace%20%3F%23%25");
        respond(r, json!({"token":"xsrf"}));
        let (r, _) = recv(&server, "POST", "/tun/m/unassign/ep%2Fspace%20%3F%23%25");
        r.respond(Response::empty(StatusCode(204))).unwrap();
    });
    svc.stop("work").unwrap();
    handle.join().unwrap();
    assert!(f.storage.sessions(&profile).unwrap().is_empty());
}
#[test]
fn usage_decodes_and_oauth_is_not_a_proxy_token() {
    let server = server();
    let api = api(&server);
    let handle = thread::spawn(move || {
        let (r, _) = recv(&server, "GET", "/tun/m/ccu-info");
        respond(
            r,
            json!({"currentBalance":12.5,"consumptionRateHourly":1.25,"assignmentsCount":2}),
        );
    });
    assert_eq!(api.usage().unwrap().current_balance, 12.5);
    handle.join().unwrap();
}
#[test]
fn concurrent_same_name_creation_does_not_allocate_twice() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let server = server();
    let one = service(&f, &server, &profile);
    let two = service(&f, &server, &profile);
    let handler = thread::spawn(move || {
        let (r, _) = recv(&server, "GET", "/tun/m/assign");
        respond(r, assignment("ep-once"));
    });
    let a = thread::spawn(move || one.create(Some("same"), options()));
    let b = thread::spawn(move || two.create(Some("same"), options()));
    let results = [a.join().unwrap(), b.join().unwrap()];
    handler.join().unwrap();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .any(|r| matches!(r, Err(Error::SessionExists(_))))
    );
}
#[test]
fn explicit_account_and_session_routing_do_not_switch_active_profile() {
    let f = Fixture::new(TokenStorageMode::File);
    let other = f.add("other");
    f.storage.save_session(&other, session("remote")).unwrap();
    let selected = context::resolve(
        &f.accounts,
        &Environment::default(),
        None,
        Some("remote"),
        true,
    )
    .unwrap();
    assert_eq!(selected.profile, other);
    assert_eq!(f.accounts.registry().unwrap().active, id("default"));
    let selected = context::resolve(
        &f.accounts,
        &Environment::default(),
        Some("default"),
        Some("remote"),
        true,
    )
    .unwrap();
    assert_eq!(selected.profile, id("default"));
}

#[test]
fn timed_out_post_is_reconciled_by_get_instead_of_reposted() {
    let server = server();
    let mut cfg = config(&server);
    cfg.timeout = Duration::from_millis(100);
    let api = ColabApi::new(cfg, Secret::new("oauth-fixture")).unwrap();
    let handle = thread::spawn(move || {
        let (r, first) = recv(&server, "GET", "/tun/m/assign");
        respond(
            r,
            json!({"acc":"NONE","nbh":first["nbh"],"token":"xsrf","variant":"DEFAULT"}),
        );
        let (held, _) = recv(&server, "POST", "/tun/m/assign");
        let (r, retry) = recv(&server, "GET", "/tun/m/assign");
        assert_eq!(first, retry);
        respond(r, assignment("ep-timeout"));
        drop(held);
    });
    assert_eq!(
        api.assign(&notebook_identity(), &options())
            .unwrap()
            .endpoint,
        "ep-timeout"
    );
    handle.join().unwrap();
}
#[test]
fn pending_release_remains_routable_without_local_session() {
    let f = Fixture::new(TokenStorageMode::File);
    let other = f.add("other");
    f.storage
        .write(
            &journal(&f, &other),
            &json!({"operation":"release","name":"pending","endpoint":"ep-pending"}),
        )
        .unwrap();
    let selected = context::resolve(
        &f.accounts,
        &Environment::default(),
        None,
        Some("pending"),
        true,
    )
    .unwrap();
    assert_eq!(selected.profile, other);
}
#[test]
fn cli_controls_selected_account_and_redacts_all_output() {
    use std::process::Command;
    let f = Fixture::new(TokenStorageMode::File);
    let other = f.add("other");
    let mut credential = token();
    credential.token = Secret::new("oauth-fixture");
    credential.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&other, &credential).unwrap();
    let server = server();
    let base = config(&server).base_url.to_string();
    let handle = thread::spawn(move || {
        let (r, _) = recv(&server, "GET", "/tun/m/assign");
        respond(r, assignment("ep-cli"));
        let (r, _) = recv(&server, "GET", "/tun/m/assignments");
        respond(r, json!({"assignments":[assignment("ep-cli")]}));
        let (r, _) = recv(&server, "GET", "/tun/m/ccu-info");
        respond(
            r,
            json!({"currentBalance":10,"consumptionRateHourly":0,"assignmentsCount":1}),
        );
        let (r, _) = recv(&server, "GET", "/tun/m/assignments");
        respond(r, json!({"assignments":[assignment("ep-cli")]}));
        let (r, _) = recv(&server, "GET", "/tun/m/unassign/ep-cli");
        respond(r, json!({"token":"xsrf"}));
        let (r, _) = recv(&server, "POST", "/tun/m/unassign/ep-cli");
        r.respond(Response::empty(StatusCode(204))).unwrap();
    });
    let invoke = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_colab"))
            .arg("--config-dir")
            .arg(f.temp.path())
            .args(["--token-storage", "file", "--control-plane-url", &base])
            .env_remove("COLAB_ACCOUNT")
            .env_remove("COLAB_CLI_REQUEST_TIMEOUT")
            .env_remove("COLAB_CLI_ASSIGN_RETRIES")
            .env_remove("COLAB_CLI_RETRY_BACKOFF")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture"));
        output.stdout
    };
    let created: Value =
        serde_json::from_slice(&invoke(&["-a", "other", "new", "-s", "remote", "--json"])).unwrap();
    assert_eq!(created["account"], "other");
    let listed: Value =
        serde_json::from_slice(&invoke(&["status", "-s", "remote", "--json"])).unwrap();
    assert_eq!(listed[0]["endpoint"], "ep-cli");
    let usage: Value =
        serde_json::from_slice(&invoke(&["-a", "other", "usage", "--json"])).unwrap();
    assert_eq!(usage["currentBalance"], 10.0);
    invoke(&["stop", "-s", "remote"]);
    handle.join().unwrap();
    assert!(f.storage.sessions(&other).unwrap().is_empty());
    assert_eq!(f.accounts.registry().unwrap().active, id("default"));
}

#[test]
fn response_body_read_timeout_uses_same_notebook_identity() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let cfg = ApiConfig {
        base_url: Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap(),
        timeout: Duration::from_millis(150),
        retries: 1,
        backoff: Duration::ZERO,
    };
    let api = ColabApi::new(cfg, Secret::new("oauth-fixture")).unwrap();
    let nbh = notebook_identity();
    let expected = nbh.clone();
    let handle = thread::spawn(move || {
        let accept = || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        break stream;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "expected retry after body timeout"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        };
        let read_headers = |stream: &mut std::net::TcpStream| {
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let text = String::from_utf8(headers).unwrap();
            assert!(text.contains(&expected));
        };
        let mut first = accept();
        read_headers(&mut first);
        // Deliver headers but no declared body, exercising Response's Read implementation.
        first
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\n")
            .unwrap();
        first.flush().unwrap();
        let mut retry = accept();
        read_headers(&mut retry);
        let body = assignment("ep-body-timeout").to_string();
        write!(
            retry,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let result = api.assign(&nbh, &options());
    handle.join().unwrap();
    assert_eq!(result.unwrap().endpoint, "ep-body-timeout");
}

#[test]
fn lifecycle_history_failure_keeps_allocation_recovery_without_another_network_request() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let blocked = f.storage.profile_path(&profile, "history").unwrap();
    f.storage.ensure_parent(&blocked).unwrap();
    std::fs::write(&blocked, b"blocked").unwrap();
    let server = server();
    let svc = service(&f, &server, &profile);
    let handler = thread::spawn(move || {
        let (r, _) = recv(&server, "GET", "/tun/m/assign");
        respond(r, assignment("ep-history"));
    });
    assert!(svc.create(Some("job"), options()).is_err());
    handler.join().unwrap();
    assert!(journal(&f, &profile).exists());
    assert_eq!(
        f.storage.sessions(&profile).unwrap()["job"].endpoint,
        "ep-history"
    );
    std::fs::remove_file(blocked).unwrap();
    assert_eq!(
        svc.create(Some("job"), options()).unwrap().endpoint,
        "ep-history"
    );
    let events = colabcli_rust::history::History::new(f.storage.clone(), profile.clone())
        .read("job", colabcli_rust::history::HistoryQuery::default())
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event_type"], "session_created");
    assert!(!journal(&f, &profile).exists());
}

#[test]
fn assignment_preparation_metadata_is_not_an_echo_contract() {
    let server = server();
    let api = api(&server);
    let handler = thread::spawn(move || {
        let (r, get) = recv(&server, "GET", "/tun/m/assign");
        // The upstream model permits arbitrary acc/nbh strings; only its token is consumed.
        respond(
            r,
            json!({"acc":"","nbh":"server-canonical-identity","token":"xsrf-from-server","variant":"GPU"}),
        );
        let (r, post) = recv(&server, "POST", "/tun/m/assign");
        assert_eq!(post, get);
        assert!(
            r.headers()
                .iter()
                .any(|h| h.field.equiv("X-Goog-Colab-Token")
                    && h.value.as_str() == "xsrf-from-server")
        );
        respond(r, assignment("ep-compatible"));
    });
    let result = api.assign(&notebook_identity(), &options());
    // Fail immediately with the old over-validation instead of waiting for the mock's POST timeout.
    assert!(result.is_ok(), "{result:?}");
    handler.join().unwrap();
}
#[test]
fn anonymous_pending_allocation_can_be_named_without_changing_notebook_identity() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let nbh = notebook_identity();
    f.storage.write(&journal(&f,&profile),&json!({"operation":"allocate","name":"session-generated","nbh":nbh,"options":options(),"session":null})).unwrap();
    let server = server();
    let svc = service(&f, &server, &profile);
    let handler = thread::spawn(move || {
        let (r, query) = recv(&server, "GET", "/tun/m/assign");
        assert_eq!(query["nbh"], nbh);
        respond(r, assignment("ep-recovered-test"));
    });
    let result = svc.create(Some("test"), options());
    assert!(result.is_ok(), "{result:?}");
    handler.join().unwrap();
    let sessions = f.storage.sessions(&profile).unwrap();
    assert_eq!(sessions["test"].endpoint, "ep-recovered-test");
    assert!(!sessions.contains_key("session-generated"));
    assert!(!journal(&f, &profile).exists());
}

#[test]
fn cli_sessions_and_status_have_distinct_upstream_text_behavior() {
    let f = Fixture::new(TokenStorageMode::File);
    let mut credential = token();
    credential.token = Secret::new("oauth-fixture");
    credential.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &credential).unwrap();
    let server = server();
    let base = config(&server).base_url.to_string();
    let handle = thread::spawn(move || {
        for endpoints in [
            vec![],
            vec![],
            vec!["orphan"],
            vec!["orphan"],
            vec!["ep-work", "orphan"],
            vec!["ep-work", "orphan"],
            vec!["ep-work"],
        ] {
            let (r, _) = recv(&server, "GET", "/tun/m/assignments");
            let assignments: Vec<_> = endpoints
                .into_iter()
                .map(|endpoint| {
                    let mut a = assignment(endpoint);
                    if endpoint == "ep-work" {
                        a["machineShape"] = json!(1);
                    }
                    a
                })
                .collect();
            respond(r, json!({"assignments": assignments}));
        }
    });
    let invoke = |args: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_colab"))
            .arg("--config-dir")
            .arg(f.temp.path())
            .args(["--token-storage", "file", "--control-plane-url", &base])
            .env_remove("COLAB_ACCOUNT")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.contains("fixture"));
        stdout
    };
    assert_eq!(
        invoke(&["sessions"]),
        "[colab] No active sessions found on server.\n"
    );
    assert_eq!(invoke(&["status"]), "[colab] No active sessions.\n");
    assert_eq!(
        invoke(&["sessions"]),
        "[?] orphan | Hardware: CPU | Shape: Standard | Variant: DEFAULT\n"
    );
    assert_eq!(invoke(&["status"]), "[colab] No active sessions.\n");
    let mut work = session("work");
    work.running = Some("job.py".into());
    work.last_execution = Some((
        "job.py".into(),
        Some("cell-1".into()),
        "2026-11-16T00:00:00Z".into(),
    ));
    f.storage.save_session(&id("default"), work).unwrap();
    let sessions = invoke(&["sessions"]);
    assert!(
        sessions.contains("[work] ep-work | Hardware: CPU | Shape: High-RAM | Variant: DEFAULT\n")
    );
    assert!(sessions.contains("[?] orphan"));
    assert!(!sessions.contains("Status:"));
    assert_eq!(
        invoke(&["status"]),
        "[work] ep-work | Hardware: CPU | Shape: High-RAM | Variant: DEFAULT | Status: BUSY (job.py)\n  Last Execution: job.py | Cell: cell-1 at 2026-11-16T00:00:00Z\n"
    );
    assert_eq!(
        invoke(&["status", "-s", "missing"]),
        "[colab] Session 'missing' not found.\n"
    );
    handle.join().unwrap();
}

#[test]
fn pending_rename_preserves_staged_and_owned_allocations() {
    for (staged, owner) in [(Some(session("old")), None), (None, Some("run-owner"))] {
        let f = Fixture::new(TokenStorageMode::File);
        let profile = id("default");
        let pending = json!({"operation":"allocate","name":"old","nbh":notebook_identity(),"options":options(),"session":staged,"owner_id":owner});
        f.storage.write(&journal(&f, &profile), &pending).unwrap();
        let server = server();
        assert!(matches!(
            service(&f, &server, &profile).create(Some("test"), options()),
            Err(Error::ControlPending)
        ));
        assert_eq!(
            f.storage
                .read::<Value>(&journal(&f, &profile))
                .unwrap()
                .unwrap(),
            pending
        );
        assert!(server.try_recv().unwrap().is_none());
    }
}

#[test]
fn pending_rename_never_overwrites_an_existing_session() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    f.storage.save_session(&profile, session("test")).unwrap();
    let pending = json!({"operation":"allocate","name":"old","nbh":notebook_identity(),"options":options(),"session":null});
    f.storage.write(&journal(&f, &profile), &pending).unwrap();
    let server = server();
    assert!(matches!(
        service(&f, &server, &profile).create(Some("test"), options()),
        Err(Error::SessionExists(_))
    ));
    assert_eq!(
        f.storage
            .read::<Value>(&journal(&f, &profile))
            .unwrap()
            .unwrap(),
        pending
    );
    assert!(server.try_recv().unwrap().is_none());
    assert_eq!(
        f.storage.sessions(&profile).unwrap()["test"].endpoint,
        "ep-test"
    );
}

#[test]
fn failed_renamed_recovery_keeps_original_identity_for_next_invocation() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let nbh = notebook_identity();
    f.storage.write(&journal(&f, &profile), &json!({"operation":"allocate","name":"old","nbh":nbh,"options":options(),"session":null})).unwrap();
    let server = server();
    let svc = service(&f, &server, &profile);
    let expected = nbh.clone();
    let handle = thread::spawn(move || {
        let (r, query) = recv(&server, "GET", "/tun/m/assign");
        assert_eq!(query["nbh"], expected);
        r.respond(Response::empty(StatusCode(412))).unwrap();
        let (r, query) = recv(&server, "GET", "/tun/m/assign");
        assert_eq!(query["nbh"], expected);
        respond(r, assignment("ep-recovered"));
    });
    assert!(svc.create(Some("test"), options()).is_err());
    let pending = f
        .storage
        .read::<Value>(&journal(&f, &profile))
        .unwrap()
        .unwrap();
    assert_eq!(pending["name"], "test");
    assert_eq!(pending["nbh"], nbh);
    assert_eq!(
        svc.create(Some("test"), options()).unwrap().endpoint,
        "ep-recovered"
    );
    handle.join().unwrap();
    assert!(!journal(&f, &profile).exists());
}

#[test]
fn empty_control_posts_declare_zero_content_length_for_strict_frontends() {
    for release in [false, true] {
        let server = server();
        let api = api(&server);
        let handle = thread::spawn(move || {
            let path = if release {
                "/tun/m/unassign/ep-strict"
            } else {
                "/tun/m/assign"
            };
            let (r, get_query) = recv(&server, "GET", path);
            respond(
                r,
                if release {
                    json!({"token":"xsrf-strict"})
                } else {
                    json!({"acc":"NONE","nbh":get_query["nbh"],"variant":"DEFAULT","token":"xsrf-strict"})
                },
            );
            let (mut r, post_query) = recv(&server, "POST", path);
            assert_eq!(post_query, get_query);
            let has_length = r
                .headers()
                .iter()
                .any(|h| h.field.equiv("Content-Length") && h.value.as_str() == "0");
            if !has_length {
                r.respond(Response::empty(StatusCode(411))).unwrap();
                return;
            }
            assert!(
                !r.headers()
                    .iter()
                    .any(|h| h.field.equiv("Transfer-Encoding"))
            );
            let mut body = Vec::new();
            r.as_reader().read_to_end(&mut body).unwrap();
            assert!(body.is_empty());
            if release {
                r.respond(Response::empty(StatusCode(204))).unwrap();
            } else {
                respond(r, assignment("ep-strict"));
            }
        });
        let result = if release {
            api.unassign("ep-strict")
        } else {
            api.assign(&notebook_identity(), &options()).map(|_| ())
        };
        handle.join().unwrap();
        assert!(result.is_ok(), "release={release}: {result:?}");
    }
}
