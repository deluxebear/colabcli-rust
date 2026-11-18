mod common;
use chrono::{Duration as Delta, Utc};
use colabcli_rust::{
    Error,
    auth::{AdcProvider, ControlCredentials, OAuthService, TokenClient},
    colab_api::{ApiConfig, ColabApi, RuntimeOptions},
    credentials::TokenStorageMode,
    models::Secret,
    sessions::SessionService,
};
use common::{Fixture, id};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
use tiny_http::{Request, Response, Server, StatusCode};
fn recv(server: &Server) -> Request {
    server
        .recv_timeout(Duration::from_secs(4))
        .unwrap()
        .expect("loopback request")
}
fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}
fn reply(request: Request, value: Value) {
    request
        .respond(Response::from_string(value.to_string()))
        .unwrap();
}
fn status(request: Request, code: u16) {
    request
        .respond(Response::from_string("private-server-marker").with_status_code(StatusCode(code)))
        .unwrap();
}
fn source(f: &Fixture, base: &str, adc: bool) -> ControlCredentials {
    let mut token = common::token();
    token.expiry = Some(Utc::now() + Delta::hours(1));
    token.token_uri = format!("{base}/token");
    if adc {
        let path = f.temp.path().join("adc.json");
        let mut value = serde_json::to_value(token).unwrap();
        value["type"] = json!("authorized_user");
        value["quota_project_id"] = json!("quota-project");
        fs::write(&path, value.to_string()).unwrap();
        ControlCredentials::adc(AdcProvider::new(path, TokenClient::new().unwrap()))
    } else {
        f.tokens.save(&id("default"), &token).unwrap();
        ControlCredentials::oauth(
            OAuthService::new(
                f.storage.clone(),
                f.tokens.clone(),
                TokenClient::new().unwrap(),
            ),
            id("default"),
        )
    }
}
fn api(base: &str, source: ControlCredentials) -> ColabApi {
    ColabApi::with_credentials_source(
        ApiConfig {
            base_url: base.parse().unwrap(),
            timeout: Duration::from_secs(2),
            retries: 0,
            backoff: Duration::ZERO,
        },
        source,
    )
    .unwrap()
}
fn refreshed(request: Request) {
    assert_eq!(request.method().as_str(), "POST");
    assert_eq!(request.url(), "/token");
    assert!(header(&request, "x-goog-user-project").is_none());
    assert!(header(&request, "Authorization").is_none());
    let mut request = request;
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).unwrap();
    let fields: std::collections::BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(fields["refresh_token"], "fixture-refresh");
    reply(
        request,
        json!({"access_token":"fresh-access","refresh_token":"rotated-refresh","expires_in":3600,"token_type":"Bearer"}),
    );
}
#[test]
fn oauth_and_adc_refresh_once_on_inventory_401_and_keep_request_identity() {
    for adc in [false, true] {
        let f = Fixture::new(TokenStorageMode::File);
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let api = api(&base, source(&f, &base, adc));
        let worker = thread::spawn(move || {
            let request = recv(&server);
            assert_eq!(
                header(&request, "Authorization"),
                Some("Bearer fixture-access")
            );
            status(request, 401);
            refreshed(recv(&server));
            for _ in 0..2 {
                let request = recv(&server);
                assert_eq!(
                    header(&request, "Authorization"),
                    Some("Bearer fresh-access")
                );
                assert_eq!(
                    header(&request, "x-goog-user-project"),
                    adc.then_some("quota-project")
                );
                reply(request, json!({"assignments":[]}));
            }
            assert!(
                server
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .is_none()
            );
        });
        assert!(api.assignments().unwrap().is_empty());
        assert!(api.clone().assignments().unwrap().is_empty());
        worker.join().unwrap();
        if !adc {
            let saved = f.tokens.load(&id("default")).unwrap().unwrap();
            assert_eq!(saved.token.expose(), "fresh-access");
            assert_eq!(saved.refresh_token.unwrap().expose(), "rotated-refresh");
        } else {
            let value: Value =
                serde_json::from_slice(&fs::read(f.temp.path().join("adc.json")).unwrap()).unwrap();
            assert_eq!(value["token"], "fixture-access");
            assert_eq!(value["refresh_token"], "fixture-refresh");
        }
    }
}
#[test]
fn read_retry_is_bounded_and_403_never_refreshes_or_exposes_body() {
    for adc in [false, true] {
        for code in [401, 403] {
            let f = Fixture::new(TokenStorageMode::File);
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let api = api(&base, source(&f, &base, adc));
            let worker = thread::spawn(move || {
                status(recv(&server), code);
                if code == 401 {
                    refreshed(recv(&server));
                    status(recv(&server), 401);
                }
                assert!(
                    server
                        .recv_timeout(Duration::from_millis(100))
                        .unwrap()
                        .is_none()
                );
            });
            let error = api.assignments().unwrap_err();
            assert!(matches!(error,Error::ControlStatus(c) if c==code));
            assert!(!error.to_string().contains("private-server-marker"));
            worker.join().unwrap();
        }
    }
}
#[test]
fn allocation_release_and_propagation_401_are_never_replayed() {
    for operation in ["assign-get", "assign-post", "release", "propagate"] {
        let f = Fixture::new(TokenStorageMode::File);
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let api = api(&base, source(&f, &base, false));
        let worker = thread::spawn(move || {
            let mut request = recv(&server);
            if operation == "assign-post" || operation == "release" {
                assert_eq!(request.method().as_str(), "GET");
                reply(
                    request,
                    if operation == "assign-post" {
                        json!({"token":"xsrf","variant":"DEFAULT","acc":"NONE","nbh":"server"})
                    } else {
                        json!({"token":"xsrf"})
                    },
                );
                request = recv(&server);
                assert_eq!(request.method().as_str(), "POST");
                assert_eq!(header(&request, "X-Goog-Colab-Token"), Some("xsrf"));
            }
            status(request, 401);
            assert!(
                server
                    .recv_timeout(Duration::from_millis(150))
                    .unwrap()
                    .is_none()
            );
        });
        let result = match operation {
            "assign-get" | "assign-post" => api
                .assign("notebook", &RuntimeOptions::new(None, None, false).unwrap())
                .map(|_| ()),
            "release" => api.unassign("ep-work"),
            _ => api
                .propagate_credentials("ep-work", &Secret::new("xsrf"), false)
                .map(|_| ()),
        };
        assert!(matches!(result, Err(Error::ControlStatus(401))));
        worker.join().unwrap();
    }
}
#[test]
fn failed_refresh_does_not_send_retry_or_overwrite_valid_oauth_credentials() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let api = api(&base, source(&f, &base, false));
    let before = serde_json::to_value(f.tokens.load(&id("default")).unwrap()).unwrap();
    let worker = thread::spawn(move || {
        status(recv(&server), 401);
        let request = recv(&server);
        assert_eq!(request.url(), "/token");
        status(request, 400);
        assert!(
            server
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
    });
    assert!(api.assignments().is_err());
    worker.join().unwrap();
    assert_eq!(
        before,
        serde_json::to_value(f.tokens.load(&id("default")).unwrap()).unwrap()
    );
    assert!(
        !f.storage
            .profile_path(&id("default"), "oauth_refresh_pending.json")
            .unwrap()
            .exists()
    );
}
#[test]
fn shared_clients_deduplicate_simultaneous_rejected_bearers() {
    for adc in [false, true] {
        let f = Fixture::new(TokenStorageMode::File);
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let api = api(&base, source(&f, &base, adc));
        let handler = thread::spawn(move || {
            let first = recv(&server);
            let second = recv(&server);
            for request in [first, second] {
                assert_eq!(
                    header(&request, "Authorization"),
                    Some("Bearer fixture-access")
                );
                status(request, 401);
            }
            refreshed(recv(&server));
            for _ in 0..2 {
                let request = recv(&server);
                assert_eq!(
                    header(&request, "Authorization"),
                    Some("Bearer fresh-access")
                );
                reply(request, json!({"assignments":[]}));
            }
            assert!(
                server
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .is_none()
            );
        });
        let first = api.clone();
        let second = api.clone();
        let a = thread::spawn(move || first.assignments());
        let b = thread::spawn(move || second.assignments());
        assert!(a.join().unwrap().unwrap().is_empty());
        assert!(b.join().unwrap().unwrap().is_empty());
        handler.join().unwrap();
    }
}
#[test]
fn cached_oauth_requests_do_not_reopen_the_credential_backend() {
    let f = Fixture::new(TokenStorageMode::Keyring);
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let api = api(&base, source(&f, &base, false));
    f.backend.fail_get.store(true, Ordering::SeqCst);
    let handler = thread::spawn(move || {
        for _ in 0..2 {
            reply(recv(&server), json!({"assignments":[]}));
        }
    });
    for _ in 0..2 {
        assert!(api.assignments().unwrap().is_empty());
    }
    handler.join().unwrap();
}
#[test]
fn session_guard_is_reused_and_a_different_accounts_guard_is_rejected() {
    let f = Fixture::new(TokenStorageMode::File);
    let other = f.add("other");
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let api = api(&base, source(&f, &base, false));
    let handler = thread::spawn(move || {
        status(recv(&server), 401);
        refreshed(recv(&server));
        reply(recv(&server), json!({"assignments":[]}));
        assert!(
            server
                .recv_timeout(Duration::from_millis(150))
                .unwrap()
                .is_none()
        );
    });
    let service = SessionService::new(f.storage.clone(), id("default"), api.clone());
    assert!(service.status(None).unwrap().is_empty());
    let wrong = SessionService::new(f.storage.clone(), other, api);
    let error = wrong.status(None).unwrap_err();
    assert!(matches!(error, Error::OAuth("凭据刷新账户锁不匹配")));
    handler.join().unwrap();
    let _guard = f.storage.profile_lock(&id("default")).unwrap();
}
#[test]
fn cancellation_stops_cached_requests_before_network() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let cancelled = Arc::new(AtomicUsize::new(0));
    let api = api(
        &base,
        source(&f, &base, false).with_cancellation(Some(cancelled.clone())),
    );
    cancelled.store(2, Ordering::SeqCst);
    assert!(matches!(api.assignments(), Err(Error::CliExit(130))));
    assert!(
        server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
}

#[test]
fn retained_provider_refreshes_short_lived_credentials_before_guarded_release() {
    for adc in [false, true] {
        let f = Fixture::new(TokenStorageMode::File);
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let source = source(&f, &base, adc);
        if adc {
            let path = f.temp.path().join("adc.json");
            let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value["expiry"] = json!((Utc::now() - Delta::hours(1)).to_rfc3339());
            fs::write(path, value.to_string()).unwrap();
        } else {
            let mut token = f.tokens.load(&id("default")).unwrap().unwrap();
            token.expiry = Some(Utc::now() - Delta::hours(1));
            f.tokens.save(&id("default"), &token).unwrap();
        }
        f.storage
            .save_session(&id("default"), common::session("work"))
            .unwrap();
        let handler = thread::spawn(move || {
            let request = recv(&server);
            assert_eq!(request.url(), "/token");
            reply(
                request,
                json!({"access_token":"short-lived","expires_in":60,"token_type":"Bearer"}),
            );
            // Already within the 60-second safety margin: the retained source
            // must refresh again before the release GET, without any HTTP 401.
            refreshed(recv(&server));
            for method in ["GET", "POST"] {
                let request = recv(&server);
                assert_eq!(request.method().as_str(), method);
                assert!(request.url().starts_with("/tun/m/unassign/ep-work?"));
                assert_eq!(
                    header(&request, "Authorization"),
                    Some("Bearer fresh-access")
                );
                assert_eq!(
                    header(&request, "x-goog-user-project"),
                    adc.then_some("quota-project")
                );
                if method == "GET" {
                    reply(request, json!({"token":"xsrf"}));
                } else {
                    request.respond(Response::empty(204)).unwrap();
                }
            }
        });
        let api = api(&base, source);
        let service = SessionService::new(f.storage.clone(), id("default"), api);
        service.stop("work").unwrap();
        handler.join().unwrap();
        assert!(f.storage.sessions(&id("default")).unwrap().is_empty());
        assert!(
            !f.storage
                .profile_path(&id("default"), "control_operation.json")
                .unwrap()
                .exists()
        );
    }
}

#[test]
fn concurrent_account_refresh_failure_cannot_replace_another_accounts_credentials() {
    let f = Fixture::new(TokenStorageMode::File);
    let a = f.add("a");
    let b = f.add("b");
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr());
    let create = |profile: &colabcli_rust::models::ProfileId| {
        let mut credentials = common::token();
        credentials.token = Secret::new(format!("old-{profile}"));
        credentials.refresh_token = Some(Secret::new(format!("refresh-{profile}")));
        credentials.expiry = Some(Utc::now() + Delta::hours(1));
        credentials.token_uri = format!("{base}/token");
        f.tokens.save(profile, &credentials).unwrap();
        api(
            &base,
            ControlCredentials::oauth(
                OAuthService::new(
                    f.storage.clone(),
                    f.tokens.clone(),
                    TokenClient::new().unwrap(),
                ),
                profile.clone(),
            ),
        )
    };
    let api_a = create(&a);
    let api_b = create(&b);
    let handler = thread::spawn(move || {
        let first = recv(&server);
        let second = recv(&server);
        let mut bearers = [
            header(&first, "Authorization").unwrap().to_owned(),
            header(&second, "Authorization").unwrap().to_owned(),
        ];
        bearers.sort();
        assert_eq!(bearers, ["Bearer old-a", "Bearer old-b"]);
        status(first, 401);
        status(second, 401);
        let mut exchanges = std::collections::BTreeSet::new();
        for _ in 0..3 {
            let mut request = recv(&server);
            assert!(header(&request, "x-goog-user-project").is_none());
            if request.url() == "/token" {
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let fields: std::collections::BTreeMap<_, _> =
                    url::form_urlencoded::parse(body.as_bytes())
                        .into_owned()
                        .collect();
                let refresh = fields["refresh_token"].clone();
                assert!(exchanges.insert(refresh.clone()));
                match refresh.as_str() {
                    "refresh-a" => status(request, 400),
                    "refresh-b" => reply(
                        request,
                        json!({"access_token":"new-b","refresh_token":"rotated-b","expires_in":3600}),
                    ),
                    _ => panic!("unexpected synthetic identity"),
                }
            } else {
                assert_eq!(header(&request, "Authorization"), Some("Bearer new-b"));
                reply(request, json!({"assignments":[]}));
            }
        }
        assert_eq!(exchanges.len(), 2);
    });
    let first = thread::spawn(move || api_a.assignments());
    let second = thread::spawn(move || api_b.assignments());
    assert!(first.join().unwrap().is_err());
    assert!(second.join().unwrap().unwrap().is_empty());
    handler.join().unwrap();
    let saved_a = f.tokens.load(&a).unwrap().unwrap();
    let saved_b = f.tokens.load(&b).unwrap().unwrap();
    assert_eq!(saved_a.token.expose(), "old-a");
    assert_eq!(saved_a.refresh_token.unwrap().expose(), "refresh-a");
    assert_eq!(saved_b.token.expose(), "new-b");
    assert_eq!(saved_b.refresh_token.unwrap().expose(), "rotated-b");
}
