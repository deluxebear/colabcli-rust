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
fn runtime_endpoint_guard_precedes_refresh_and_accepts_only_captured_identity() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let server = server();
    let mut state = session("work");
    state.token = Secret::new("");
    f.storage.save_session(&profile, state.clone()).unwrap();
    let path = f.storage.profile_path(&profile, "sessions.json").unwrap();
    let before = std::fs::read(&path).unwrap();
    let bound = service(&f, &server, &profile).with_expected_endpoint(Some("ep-old".into()));
    let called = std::cell::Cell::new(false);
    assert!(matches!(
        bound.with_session(Some("work"), |_| {
            called.set(true);
            Ok(())
        }),
        Err(Error::KernelProtocol(_))
    ));
    assert!(!called.get());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(
        server
            .recv_timeout(Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
    state.token = Secret::new("fresh-runtime-token");
    state.token_expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    f.storage.save_session(&profile, state).unwrap();
    let bound = service(&f, &server, &profile).with_expected_endpoint(Some("ep-work".into()));
    assert_eq!(
        bound
            .with_session(Some("work"), |s| Ok(s.endpoint.clone()))
            .unwrap(),
        "ep-work"
    );
}

#[test]
fn runtime_endpoint_guard_never_adopts_an_orphan_after_refresh_prunes_owned_vm() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    f.storage.save_session(&profile, session("work")).unwrap();
    let server = server();
    let bound = service(&f, &server, &profile).with_expected_endpoint(Some("ep-work".into()));
    let handle = thread::spawn(move || {
        let (request, _) = recv(&server, "GET", "/tun/m/assignments");
        respond(
            request,
            json!({"assignments":[assignment("ep-replacement")]}),
        );
        assert!(
            server
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
    });
    assert!(matches!(
        bound.with_session::<()>(Some("work"), |_| panic!("must not act on replacement")),
        Err(Error::SessionMissing(_))
    ));
    handle.join().unwrap();
    assert!(!f.storage.sessions(&profile).unwrap().contains_key("work"));
}

#[test]
fn identity_and_hardware_queries_match_contract() {
    let a = notebook_identity();
    assert_eq!(a.len(), 44);
    assert_eq!(&a[36..], "........");
    assert_eq!(a.as_bytes()[14], b'4');
    assert_ne!(a, notebook_identity());
    assert_eq!(
        RuntimeOptions::new(Some("t4"), Some("v5e1"), false)
            .unwrap()
            .accelerator,
        "V5E1"
    );
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
fn first_definitive_allocation_refusal_allows_a_different_runtime() {
    for (method, status) in [("GET", 400), ("POST", 412)] {
        let f = Fixture::new(TokenStorageMode::File);
        let profile = id("default");
        let server = server();
        let svc = service(&f, &server, &profile);
        let journal_path = journal(&f, &profile);
        let storage = f.storage.clone();
        let handler = thread::spawn(move || {
            let (mut request, first) = recv(&server, "GET", "/tun/m/assign");
            assert_eq!(first["accelerator"], "T4");
            if method == "POST" {
                respond(
                    request,
                    json!({"token":"xsrf","nbh":first["nbh"],"acc":"T4","variant":"GPU"}),
                );
                (request, _) = recv(&server, "POST", "/tun/m/assign");
            }
            let intent: Value = storage.read(&journal_path).unwrap().unwrap();
            assert_eq!(intent["post_attempted"], method == "POST");
            request
                .respond(
                    Response::from_string("private refusal").with_status_code(StatusCode(status)),
                )
                .unwrap();
            let (request, next) = recv(&server, "GET", "/tun/m/assign");
            assert_ne!(next["nbh"], first["nbh"]);
            assert_eq!(next["accelerator"], "NONE");
            respond(request, assignment("ep-cpu"));
        });
        let error = svc
            .create(
                Some("gpu"),
                RuntimeOptions::new(Some("T4"), None, false).unwrap(),
            )
            .unwrap_err();
        assert!(matches!(error, Error::ControlStatus(actual) if actual == status));
        assert!(!error.to_string().contains("private refusal"));
        assert!(!journal(&f, &profile).exists());
        assert_eq!(
            svc.create(Some("cpu"), options()).unwrap().endpoint,
            "ep-cpu"
        );
        handler.join().unwrap();
    }
}

#[test]
fn refusal_after_uncertain_post_keeps_identity_and_blocks_account_removal() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = f.add("work");
    let server = server();
    let svc = service(&f, &server, &profile);
    let handle = thread::spawn(move || {
        let (r, query) = recv(&server, "GET", "/tun/m/assign");
        respond(
            r,
            json!({"token":"xsrf","nbh":query["nbh"],"acc":"NONE","variant":"DEFAULT"}),
        );
        let (r, _) = recv(&server, "POST", "/tun/m/assign");
        r.respond(Response::empty(504)).unwrap();
        let (r, retry) = recv(&server, "GET", "/tun/m/assign");
        assert_eq!(retry["nbh"], query["nbh"]);
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
        r.respond(Response::empty(412)).unwrap();
        let (r, query) = recv(&server, "GET", "/tun/m/assign");
        assert_eq!(query["nbh"], nbh);
        respond(r, assignment("ep-resumed"));
    });
    assert!(matches!(
        svc.create(Some("work"), options()),
        Err(Error::ControlStatus(412))
    ));
    assert!(journal(&f, &profile).exists());
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
    let intent: Value = f.storage.read(&journal(&f, &profile)).unwrap().unwrap();
    assert_eq!(intent["attempted"], true);
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
fn release_attempts_only_the_recorded_kernel_with_current_proxy_credentials() {
    for delete_status in [204, 404, 503] {
        let f = Fixture::new(TokenStorageMode::File);
        let profile = id("default");
        let mut state = session("work");
        state.kernel_id = Some("kernel/one".into());
        f.storage.save_session(&profile, state).unwrap();
        let control = server();
        let runtime = server();
        let svc = service(&f, &control, &profile);
        let handle = thread::spawn(move || {
            let (request, _) = recv(&control, "GET", "/tun/m/assignments");
            let mut target = assignment("ep-work");
            target["runtimeProxyInfo"]["url"] =
                json!(format!("http://{}/prefix/", runtime.server_addr()));
            respond(
                request,
                json!({"assignments":[assignment("ep-other"), target]}),
            );

            let request = runtime
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert_eq!(request.method().as_str(), "DELETE");
            assert_eq!(request.url(), "/prefix/api/kernels/kernel%2Fone");
            assert!(request.headers().iter().any(|header| {
                header.field.equiv("Authorization")
                    && header.value.as_str() == "Bearer proxy-fixture"
            }));
            request.respond(Response::empty(delete_status)).unwrap();

            let (request, _) = recv(&control, "GET", "/tun/m/unassign/ep-work");
            respond(request, json!({"token":"release-xsrf"}));
            let (request, _) = recv(&control, "POST", "/tun/m/unassign/ep-work");
            request.respond(Response::empty(204)).unwrap();
        });
        svc.stop("work").unwrap();
        handle.join().unwrap();
        assert!(f.storage.sessions(&profile).unwrap().is_empty());
        assert!(!journal(&f, &profile).exists());
    }
}

#[test]
fn first_release_continues_if_the_kernel_proxy_cannot_be_refreshed() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let mut state = session("work");
    state.kernel_id = Some("kernel-one".into());
    f.storage.save_session(&profile, state).unwrap();
    let control = server();
    let svc = service(&f, &control, &profile);
    let handle = thread::spawn(move || {
        let (request, _) = recv(&control, "GET", "/tun/m/assignments");
        request.respond(Response::empty(503)).unwrap();
        let (request, _) = recv(&control, "GET", "/tun/m/unassign/ep-work");
        respond(request, json!({"token":"release-xsrf"}));
        let (request, _) = recv(&control, "POST", "/tun/m/unassign/ep-work");
        request.respond(Response::empty(204)).unwrap();
    });
    svc.stop("work").unwrap();
    handle.join().unwrap();
    assert!(f.storage.sessions(&profile).unwrap().is_empty());
}

#[test]
fn release_with_recorded_kernel_does_not_contact_a_replacement_endpoint() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let mut state = session("work");
    state.kernel_id = Some("kernel-one".into());
    let runtime = server();
    state.url = format!("http://{}", runtime.server_addr());
    f.storage.save_session(&profile, state).unwrap();
    let control = server();
    let svc = service(&f, &control, &profile);
    let handle = thread::spawn(move || {
        let (request, _) = recv(&control, "GET", "/tun/m/assignments");
        let mut replacement = assignment("ep-other");
        replacement["runtimeProxyInfo"]["url"] = json!(format!("http://{}", runtime.server_addr()));
        respond(request, json!({"assignments":[replacement]}));
        let (request, _) = recv(&control, "GET", "/tun/m/unassign/ep-work");
        respond(request, json!({"token":"release-xsrf"}));
        let (request, _) = recv(&control, "POST", "/tun/m/unassign/ep-work");
        request.respond(Response::empty(204)).unwrap();
        assert!(
            runtime
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
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
fn pending_allocation_allows_releasing_a_separate_session_and_survives_release_recovery() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = f.add("other");
    let mut credential = token();
    credential.token = Secret::new("oauth-fixture");
    credential.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&profile, &credential).unwrap();
    let nbh = notebook_identity();
    let allocate = journal(&f, &profile);
    let release = f
        .storage
        .profile_path(&profile, "control_parallel_release.json")
        .unwrap();
    f.storage.write(&allocate, &json!({"operation":"allocate","name":"new-vm","nbh":nbh,"options":options(),"session":null,"post_attempted":true})).unwrap();
    let original = std::fs::read(&allocate).unwrap();
    f.storage
        .save_session(&profile, session("existing"))
        .unwrap();
    let control = server();
    let base = config(&control).base_url.to_string();
    let svc = service(&f, &control, &profile);
    let next_nbh = nbh.clone();
    let handle = thread::spawn(move || {
        let (request, _) = recv(&control, "GET", "/tun/m/unassign/ep-existing");
        respond(request, json!({"token":"release-xsrf"}));
        let (request, _) = recv(&control, "POST", "/tun/m/unassign/ep-existing");
        request.respond(Response::empty(504)).unwrap();
        for _ in 0..2 {
            let (request, _) = recv(&control, "GET", "/tun/m/assignments");
            respond(request, json!({"assignments":[]}));
        }
        let (request, query) = recv(&control, "GET", "/tun/m/assign");
        assert_eq!(query["nbh"], next_nbh);
        respond(request, assignment("ep-recovered-allocation"));
    });
    assert!(matches!(
        svc.stop("existing"),
        Err(Error::ControlStatus(504))
    ));
    let intent: Value = f.storage.read(&release).unwrap().unwrap();
    assert_eq!(intent["endpoint"], "ep-existing");
    assert_eq!(std::fs::read(&allocate).unwrap(), original);
    assert!(matches!(
        svc.create(Some("new-vm"), options()),
        Err(Error::ControlPending)
    ));
    assert!(svc.status(None).unwrap().is_empty());
    assert!(matches!(
        f.accounts.remove(&profile, true),
        Err(Error::AccountInUse)
    ));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_colab"))
        .arg("--config-dir")
        .arg(f.temp.path())
        .args([
            "--token-storage",
            "file",
            "--control-plane-url",
            &base,
            "stop",
            "-s",
            "existing",
            "--colab-expected-endpoint",
            "ep-existing",
        ])
        .env_remove("COLAB_ACCOUNT")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Session terminated."));
    assert!(!release.exists());
    assert_eq!(std::fs::read(&allocate).unwrap(), original);
    assert_eq!(
        svc.create(Some("new-vm"), options()).unwrap().endpoint,
        "ep-recovered-allocation"
    );
    handle.join().unwrap();
    assert!(!allocate.exists());
}

#[test]
fn pending_staged_allocation_protects_its_endpoint_from_release_under_an_alias() {
    let f = Fixture::new(TokenStorageMode::File);
    let profile = id("default");
    let owned = session("owned");
    let mut alias = owned.clone();
    alias.name = "alias".into();
    f.storage.save_session(&profile, alias).unwrap();
    f.storage.write(&journal(&f, &profile), &json!({"operation":"allocate","name":"owned","nbh":notebook_identity(),"options":options(),"session":owned})).unwrap();
    let control = server();
    let svc = service(&f, &control, &profile);
    assert!(matches!(svc.stop("alias"), Err(Error::ControlPending)));
    assert!(matches!(
        svc.cleanup_owned("another-run", "alias", Some("ep-owned")),
        Err(Error::ControlPending)
    ));
    assert!(control.try_recv().unwrap().is_none());
    assert!(
        !f.storage
            .profile_path(&profile, "control_parallel_release.json")
            .unwrap()
            .exists()
    );
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
        if matches!(args.first(), Some(&"status" | &"stop")) {
            assert_eq!(
                output.stderr,
                b"[colab] Routing session 'remote' -> account 'other'.\n"
            );
        } else {
            assert!(output.stderr.is_empty());
        }
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
    assert_eq!(usage["usage"]["balance"], 10.0);
    invoke(&["stop", "-s", "remote"]);
    handle.join().unwrap();
    assert!(f.storage.sessions(&other).unwrap().is_empty());
    assert_eq!(f.accounts.registry().unwrap().active, id("default"));
}

#[test]
fn cli_explains_accelerator_refusal_in_text_mode_and_can_retry_with_cpu() {
    let f = Fixture::new(TokenStorageMode::File);
    let mut credential = token();
    credential.token = Secret::new("oauth-fixture");
    credential.expiry = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    f.tokens.save(&id("default"), &credential).unwrap();
    let server = server();
    let base = config(&server).base_url.to_string();
    let handler = thread::spawn(move || {
        for accelerator in ["A100", "A100", "NONE"] {
            let (request, query) = recv(&server, "GET", "/tun/m/assign");
            assert_eq!(query["accelerator"], accelerator);
            if accelerator == "NONE" {
                respond(request, assignment("ep-cpu"));
            } else {
                request
                    .respond(
                        Response::from_string("private refusal").with_status_code(StatusCode(400)),
                    )
                    .unwrap();
            }
        }
    });
    let invoke = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_colab"))
            .arg("--config-dir")
            .arg(f.temp.path())
            .args([
                "--token-storage",
                "file",
                "--control-plane-url",
                &base,
                "-a",
                "default",
                "new",
                "-s",
                "work",
            ])
            .args(args)
            .output()
            .unwrap()
    };
    let rejected = invoke(&["--gpu", "A100"]);
    assert_eq!(rejected.status.code(), Some(1));
    let message = String::from_utf8(rejected.stderr).unwrap();
    assert!(message.contains("Backend rejected accelerator 'A100'"));
    assert!(message.contains("omit --gpu/--tpu for a CPU runtime"));
    assert!(!message.contains("private refusal"));
    let rejected = invoke(&["--gpu", "A100", "--json"]);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(
        String::from_utf8(rejected.stderr)
            .unwrap()
            .contains("HTTP 400")
    );
    let created = invoke(&["--json"]);
    assert!(created.status.success());
    let view: Value = serde_json::from_slice(&created.stdout).unwrap();
    assert_eq!(view["endpoint"], "ep-cpu");
    handler.join().unwrap();
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

#[test]
fn credentials_propagation_uses_oauth_encoded_endpoint_xsrf_and_real_multipart_body() {
    let server = server();
    let api = api(&server);
    let handler = thread::spawn(move || {
        for (method, dryrun) in [("GET", "true"), ("POST", "true"), ("POST", "false")] {
            let (mut request, query) = recv(
                &server,
                method,
                "/tun/m/credentials-propagation/ep%2Fwith%25chars",
            );
            assert_eq!(query.len(), 6);
            for (key, value) in [
                ("authtype", "dfs_ephemeral"),
                ("version", "2"),
                ("dryrun", dryrun),
                ("propagate", "true"),
                ("record", "false"),
            ] {
                assert_eq!(query[key], value);
            }
            if method == "POST" {
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|h| h.field.equiv("X-Goog-Colab-Token")
                            && h.value.as_str() == "propagation-xsrf")
                );
                let content_type = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Content-Type"))
                    .unwrap()
                    .value
                    .as_str()
                    .to_owned();
                let boundary = content_type
                    .strip_prefix("multipart/form-data; boundary=")
                    .unwrap();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                assert_eq!(
                    body,
                    format!(
                        "--{boundary}\r\nContent-Disposition: form-data; name=\"file_id\"\r\n\r\nempty.ipynb\r\n--{boundary}--\r\n"
                    )
                );
                request
                    .respond(Response::from_string(")]}'\n{\"success\":true}"))
                    .unwrap();
            } else {
                request
                    .respond(Response::from_string(
                        ")]}'\n{\"token\":\"propagation-xsrf\"}",
                    ))
                    .unwrap();
            }
        }
    });
    let token = api.prepare_credentials("ep/with%chars").unwrap();
    assert!(
        api.propagate_credentials("ep/with%chars", &token, true)
            .unwrap()
            .success
    );
    assert!(
        api.propagate_credentials("ep/with%chars", &token, false)
            .unwrap()
            .success
    );
    handler.join().unwrap();
}

#[test]
fn credential_propagation_rejects_empty_tokens_redirects_and_unconfirmed_final_success() {
    for (status, body) in [
        (302, "secret-redirect"),
        (403, "secret-response"),
        (200, "{\"success\":false}"),
        (200, "{\"success\":\"true\"}"),
        (200, "malformed-secret-json"),
    ] {
        let server = server();
        let api = api(&server);
        assert!(api.prepare_credentials("").is_err());
        assert!(
            api.propagate_credentials("ep", &Secret::new(""), false)
                .is_err()
        );
        let handler = thread::spawn(move || {
            let (request, _) = recv(&server, "POST", "/tun/m/credentials-propagation/ep");
            request
                .respond(
                    Response::from_string(body)
                        .with_status_code(StatusCode(status))
                        .with_header(
                            tiny_http::Header::from_bytes(
                                "Location",
                                "https://accounts.google.com/secret-redirect",
                            )
                            .unwrap(),
                        ),
                )
                .unwrap();
        });
        let error = api
            .propagate_credentials("ep", &Secret::new("xsrf"), false)
            .err()
            .unwrap();
        assert!(!format!("{error:?} {error}").contains("secret-"));
        handler.join().unwrap();
    }
}

#[test]
fn colab_consent_waits_for_user_before_final_propagation_and_rejects_unknown_auth_types() {
    use colabcli_rust::{
        automation::AutomationInput,
        jupyter::{ColabRequest, InputHandler, InputRequest},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let f = Fixture::new(TokenStorageMode::File);
    let server = server();
    let svc = service(&f, &server, &id("default"));
    let granted = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(AtomicBool::new(false));
    let granted_server = granted.clone();
    let observed_server = observed.clone();
    let handler = thread::spawn(move || {
        let (request, _) = recv(&server, "GET", "/tun/m/credentials-propagation/ep-owned");
        request
            .respond(Response::from_string("{\"token\":\"xsrf\"}"))
            .unwrap();
        let (request, query) = recv(&server, "POST", "/tun/m/credentials-propagation/ep-owned");
        assert_eq!(query["dryrun"], "true");
        request.respond(Response::from_string("{\"success\":false,\"unauthorized_redirect_uri\":\"https://accounts.google.com/authorize?state=private-consent\"}")).unwrap();
        assert!(
            server
                .recv_timeout(Duration::from_millis(150))
                .unwrap()
                .is_none()
        );
        observed_server.store(true, Ordering::SeqCst);
        granted_server.store(true, Ordering::SeqCst);
        let (request, query) = recv(&server, "POST", "/tun/m/credentials-propagation/ep-owned");
        assert_eq!(query["dryrun"], "false");
        request
            .respond(Response::from_string("{\"success\":true}"))
            .unwrap();
    });
    let mut polls = 0;
    let mut terminal = |request: &InputRequest| {
        polls += 1;
        assert!(!request.password);
        assert!(
            request
                .prompt
                .contains("https://accounts.google.com/authorize?state=private-consent")
        );
        Ok(granted.load(Ordering::SeqCst).then(String::new))
    };
    let mut input = AutomationInput::new(&svc, &mut terminal);
    input.bind_endpoint("ep-owned").unwrap();
    let mut request = ColabRequest {
        id: "request-1".into(),
        colab_msg_id: json!(7),
        auth_type: "unknown".into(),
    };
    assert!(input.poll_colab(&request).is_err());
    request.auth_type = "dfs_ephemeral".into();
    let start = std::time::Instant::now();
    while !input.poll_colab(&request).unwrap() {
        assert!(start.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(5));
    }
    drop(input);
    handler.join().unwrap();
    assert!(polls > 0);
    assert!(observed.load(Ordering::SeqCst));
}

#[test]
fn confirmed_absence_cleans_only_matching_compute_endpoint_leases_and_release_failure_keeps_them() {
    use colabcli_rust::{
        auth::TokenClient,
        drive_credentials::DriveCredentialService,
        drivefs::{lease::Lease, remote::RemoteRequest},
    };
    let f = Fixture::new(TokenStorageMode::File);
    let compute = id("default");
    let drive = f.add("drive");
    let other_compute = f.add("other");
    f.storage.save_session(&compute, session("work")).unwrap();
    f.storage.save_session(&compute, session("keep")).unwrap();
    let credentials = DriveCredentialService::new(f.storage.clone(), TokenClient::new().unwrap());
    // Secret-free synthetic leases are enough to exercise cleanup ownership.
    let lease = Lease {
        version: 1,
        compute_account: compute.clone(),
        drive_account: drive.clone(),
        session: "work".into(),
        endpoint: "ep-work".into(),
        request: RemoteRequest::new("/content/drive").unwrap(),
        fingerprint: "a".repeat(64),
        identity: "b".repeat(64),
    };
    let mut other_endpoint = lease.clone();
    other_endpoint.endpoint = "ep-keep".into();
    other_endpoint.request = RemoteRequest::new("/content/keep").unwrap();
    let mut other_account = lease.clone();
    other_account.compute_account = other_compute;
    other_account.request = RemoteRequest::new("/content/other").unwrap();
    for item in [&lease, &other_endpoint, &other_account] {
        f.storage
            .write(
                &f.storage
                    .profile_path(
                        &drive,
                        &format!("managed/drivefs-{}.json", item.request.operation_id),
                    )
                    .unwrap(),
                item,
            )
            .unwrap();
    }
    let s = server();
    let svc = service(&f, &s, &compute);
    let handle = thread::spawn(move || {
        let (r, _) = recv(&s, "GET", "/tun/m/assignments");
        respond(r, json!({"assignments":[assignment("ep-work")]}));
        let (r, _) = recv(&s, "GET", "/tun/m/unassign/ep-work");
        respond(r, json!({"token":"xsrf"}));
        let (r, _) = recv(&s, "POST", "/tun/m/unassign/ep-work");
        r.respond(Response::empty(StatusCode(504))).unwrap();
        let (r, _) = recv(&s, "GET", "/tun/m/assignments");
        respond(r, json!({"assignments":[]}));
    });
    assert!(svc.cleanup_drive_leases("ep-work").is_err());
    assert_eq!(credentials.leases(&drive).unwrap().len(), 3);
    assert!(svc.stop("work").is_err());
    assert_eq!(credentials.leases(&drive).unwrap().len(), 3);
    assert!(journal(&f, &compute).exists());
    svc.stop("work").unwrap();
    handle.join().unwrap();
    let remaining = credentials.leases(&drive).unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(remaining.contains(&other_endpoint));
    assert!(remaining.contains(&other_account));
    assert!(!journal(&f, &compute).exists());
    assert!(f.storage.sessions(&compute).unwrap().contains_key("keep"));
    // No local session is needed to confirm absence and clear the remaining endpoint.
    let s = server();
    let svc = service(&f, &s, &compute);
    let handle = thread::spawn(move || {
        let (r, _) = recv(&s, "GET", "/tun/m/assignments");
        respond(r, json!({"assignments":[]}));
    });
    svc.cleanup_drive_leases("ep-keep").unwrap();
    handle.join().unwrap();
    assert_eq!(credentials.leases(&drive).unwrap(), vec![other_account]);
}
