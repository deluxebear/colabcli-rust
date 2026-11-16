mod common;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Duration as TimeDelta;
use colabcli_rust::{
    Error,
    auth::{
        AdcProvider, OAuthClient, OAuthService, PendingAuthorization, TokenClient,
        validate_endpoint,
    },
    credentials::TokenStorageMode,
    models::Secret,
};
use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, atomic::Ordering},
    thread,
    time::Duration,
};
use tiny_http::{Response, Server, StatusCode};
use url::Url;

fn client(server: &Server) -> OAuthClient {
    let base = format!("http://{}", server.server_addr());
    OAuthClient {
        client_id: "fixture-client".into(),
        client_secret: Some(Secret::new("fixture-secret")),
        auth_uri: Url::parse(&format!("{base}/authorize")).unwrap(),
        token_uri: Url::parse(&format!("{base}/token")).unwrap(),
    }
}
fn service(f: &Fixture) -> OAuthService {
    OAuthService::new(
        f.storage.clone(),
        f.tokens.clone(),
        TokenClient::new().unwrap(),
    )
}
fn form(request: &mut tiny_http::Request) -> BTreeMap<String, String> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).unwrap();
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}
fn success() -> Value {
    json!({"access_token":"fresh-access","refresh_token":"fresh-refresh","scope":"openid","expires_in":3600,"token_type":"Bearer"})
}

#[test]
fn pkce_url_uses_s256_and_random_state_and_verifier() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = Server::http("127.0.0.1:0").unwrap();
    let url = service(&f)
        .begin(
            &id("default"),
            client(&server),
            Url::parse("http://localhost:9999/callback").unwrap(),
            Some("hint@example.invalid"),
            now(),
        )
        .unwrap();
    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let pending: PendingAuthorization = f
        .storage
        .read(
            &f.storage
                .profile_path(&id("default"), "oauth_pending.json")
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["state"], pending.state.expose());
    assert_eq!(
        query["code_challenge"],
        URL_SAFE_NO_PAD.encode(Sha256::digest(pending.code_verifier.expose().as_bytes()))
    );
    assert_eq!(pending.code_verifier.expose().len(), 43);
    assert_ne!(pending.state, pending.code_verifier);
    assert_eq!(query["login_hint"], "hint@example.invalid");
    assert!(!format!("{pending:?}").contains(pending.code_verifier.expose()));
}
#[test]
fn callback_requires_exact_redirect_and_unique_matching_state() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = Server::http("127.0.0.1:0").unwrap();
    service(&f)
        .begin(
            &id("default"),
            client(&server),
            Url::parse("http://localhost:9999/callback").unwrap(),
            None,
            now(),
        )
        .unwrap();
    let pending: PendingAuthorization = f
        .storage
        .read(
            &f.storage
                .profile_path(&id("default"), "oauth_pending.json")
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    for suffix in [
        "?code=x",
        "?code=x&state=wrong",
        "?code=x&state=wrong&state=wrong",
        "?error=denied&state=wrong",
    ] {
        assert!(
            pending
                .callback_code(
                    &Url::parse(&format!("http://localhost:9999/callback{suffix}")).unwrap()
                )
                .is_err()
        );
    }
    let good = format!(
        "http://localhost:9999/callback?state={}&code=fixture-code",
        pending.state.expose()
    );
    assert_eq!(
        pending
            .callback_code(&Url::parse(&good).unwrap())
            .unwrap()
            .expose(),
        "fixture-code"
    );
    assert!(
        pending
            .callback_code(&Url::parse(&good.replace("/callback?", "/other?")).unwrap())
            .is_err()
    );
    assert!(
        pending
            .callback_code(&Url::parse(&format!("{good}&code=second")).unwrap())
            .is_err()
    );
}
#[test]
fn headless_flow_survives_restart_and_saves_only_granted_scopes() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = Server::http("127.0.0.1:0").unwrap();
    service(&f)
        .begin(
            &id("default"),
            client(&server),
            Url::parse("https://sdk.cloud.google.com/applicationdefaultauthcode.html").unwrap(),
            None,
            now(),
        )
        .unwrap();
    let handler = thread::spawn(move || {
        let mut request = server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert_eq!(request.url(), "/token");
        let fields = form(&mut request);
        assert_eq!(fields["grant_type"], "authorization_code");
        assert_eq!(fields["code"], "fixture-code");
        assert_eq!(fields["code_verifier"].len(), 43);
        request
            .respond(Response::from_string(success().to_string()))
            .unwrap();
    });
    service(&f)
        .finish(
            &id("default"),
            Some(Secret::new("fixture-code")),
            None,
            now(),
        )
        .unwrap();
    handler.join().unwrap();
    let saved = f.tokens.load(&id("default")).unwrap().unwrap();
    assert_eq!(saved.scopes, vec!["openid"]);
    assert!(
        !f.storage
            .profile_path(&id("default"), "oauth_pending.json")
            .unwrap()
            .exists()
    );
    assert!(matches!(
        service(&f).finish(
            &id("default"),
            Some(Secret::new("fixture-code")),
            None,
            now()
        ),
        Err(Error::NeedsAuth)
    ));
}
#[test]
fn expired_or_wrong_account_pending_is_not_exchanged() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = Server::http("127.0.0.1:0").unwrap();
    service(&f)
        .begin(
            &id("default"),
            client(&server),
            Url::parse("http://localhost:9999/callback").unwrap(),
            None,
            now(),
        )
        .unwrap();
    assert!(
        service(&f)
            .finish(
                &id("default"),
                Some(Secret::new("x")),
                None,
                now() + TimeDelta::minutes(16)
            )
            .is_err()
    );
    let path = f
        .storage
        .profile_path(&id("default"), "oauth_pending.json")
        .unwrap();
    let mut pending: PendingAuthorization = f.storage.read(&path).unwrap().unwrap();
    pending.account = id("other");
    f.storage.write(&path, &pending).unwrap();
    assert!(
        service(&f)
            .finish(&id("default"), Some(Secret::new("x")), None, now())
            .is_err()
    );
    assert!(
        server
            .recv_timeout(Duration::from_millis(20))
            .unwrap()
            .is_none()
    );
}
#[test]
fn successful_exchange_is_staged_when_keyring_fails_and_can_resume() {
    let f = Fixture::new(TokenStorageMode::Keyring);
    let server = Server::http("127.0.0.1:0").unwrap();
    service(&f)
        .begin(
            &id("default"),
            client(&server),
            Url::parse("http://localhost:9999/callback").unwrap(),
            None,
            now(),
        )
        .unwrap();
    let handler = thread::spawn(move || {
        server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .respond(Response::from_string(success().to_string()))
            .unwrap()
    });
    f.backend.fail_set.store(true, Ordering::SeqCst);
    assert!(
        service(&f)
            .finish(&id("default"), Some(Secret::new("x")), None, now())
            .is_err()
    );
    handler.join().unwrap();
    let pending: PendingAuthorization = f
        .storage
        .read(
            &f.storage
                .profile_path(&id("default"), "oauth_pending.json")
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert!(pending.result.is_some());
    f.backend.fail_set.store(false, Ordering::SeqCst);
    service(&f)
        .finish(&id("default"), None, None, now())
        .unwrap();
    assert_eq!(
        f.tokens
            .load(&id("default"))
            .unwrap()
            .unwrap()
            .token
            .expose(),
        "fresh-access"
    );
}
#[test]
fn rejected_exchange_does_not_overwrite_existing_credentials_or_replay_code() {
    let f = Fixture::new(TokenStorageMode::File);
    f.tokens.save(&id("default"), &token()).unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    service(&f)
        .begin(
            &id("default"),
            client(&server),
            Url::parse("http://localhost:9999/callback").unwrap(),
            None,
            now(),
        )
        .unwrap();
    let handler = thread::spawn(move || {
        server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .respond(
                Response::from_string("fixture-secret-reflection")
                    .with_status_code(StatusCode(400)),
            )
            .unwrap()
    });
    let err = service(&f)
        .finish(&id("default"), Some(Secret::new("x")), None, now())
        .unwrap_err();
    handler.join().unwrap();
    assert!(!format!("{err:?} {err}").contains("reflection"));
    assert_eq!(
        f.tokens
            .load(&id("default"))
            .unwrap()
            .unwrap()
            .token
            .expose(),
        "fixture-access"
    );
    assert!(
        service(&f)
            .finish(&id("default"), Some(Secret::new("x")), None, now())
            .is_err()
    );
}
#[test]
fn refresh_rotates_credentials_without_expanding_requested_scopes() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let mut old = token();
    old.token_uri = client(&server).token_uri.to_string();
    let handler = thread::spawn(move || {
        let mut request = server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        let fields = form(&mut request);
        assert_eq!(fields["grant_type"], "refresh_token");
        assert_eq!(fields["refresh_token"], "fixture-refresh");
        assert!(!fields.contains_key("scope"));
        request
            .respond(Response::from_string(success().to_string()))
            .unwrap();
    });
    let new = TokenClient::new().unwrap().refresh(&old, now()).unwrap();
    handler.join().unwrap();
    assert_eq!(
        new.refresh_token.as_ref().unwrap().expose(),
        "fresh-refresh"
    );
    assert_eq!(new.scopes, vec!["openid"]);
    assert!(new.valid_at(now()));
}
#[test]
fn concurrent_oauth_refresh_requests_are_coalesced_per_profile() {
    let f = Fixture::new(TokenStorageMode::File);
    let server = Server::http("127.0.0.1:0").unwrap();
    let mut old = token();
    old.token_uri = client(&server).token_uri.to_string();
    f.tokens.save(&id("default"), &old).unwrap();
    let handler = thread::spawn(move || {
        server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .respond(Response::from_string(success().to_string()))
            .unwrap();
        assert!(
            server
                .recv_timeout(Duration::from_millis(200))
                .unwrap()
                .is_none()
        );
    });
    let service = Arc::new(service(&f));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let service = service.clone();
            thread::spawn(move || service.access_token(&id("default"), now()).unwrap())
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().unwrap().expose(), "fresh-access");
    }
    handler.join().unwrap();
}
#[test]
fn adc_accounts_concurrently_use_their_own_token_sources() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let uri = client(&server).token_uri.to_string();
    let mut providers = Vec::new();
    for name in ["a", "b"] {
        let path = temp.path().join(format!("{name}.json"));
        fs::write(&path,json!({"type":"authorized_user","client_id":name,"client_secret":"fixture","refresh_token":format!("refresh-{name}"),"token_uri":uri}).to_string()).unwrap();
        providers.push(AdcProvider::new(path, TokenClient::new().unwrap()));
    }
    let before = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS");
    let handler = thread::spawn(move || {
        for _ in 0..2 {
            let mut request = server
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            let fields = form(&mut request);
            let name = &fields["client_id"];
            assert_eq!(fields["refresh_token"], format!("refresh-{name}"));
            request
                .respond(Response::from_string(
                    json!({"access_token":format!("token-{name}"),"expires_in":3600}).to_string(),
                ))
                .unwrap();
        }
    });
    let workers: Vec<_> = providers
        .into_iter()
        .map(|p| thread::spawn(move || p.access_token(now()).unwrap()))
        .collect();
    let values: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    handler.join().unwrap();
    assert_eq!(values[0].expose(), "token-a");
    assert_eq!(values[1].expose(), "token-b");
    assert_eq!(std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS"), before);
}
#[test]
fn unsupported_adc_types_fail_explicitly_without_network_access() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adc.json");
    for kind in [
        "service_account",
        "external_account",
        "impersonated_service_account",
    ] {
        fs::write(&path, json!({"type":kind}).to_string()).unwrap();
        assert!(matches!(
            AdcProvider::new(path.clone(), TokenClient::new().unwrap()).access_token(now()),
            Err(Error::UnsupportedAdc(_))
        ));
    }
}
#[test]
fn legacy_naive_utc_expiry_is_supported_and_invalid_time_is_rejected() {
    let mut raw = serde_json::to_value(token()).unwrap();
    raw["expiry"] = json!("2026-11-16T01:00:00.000000");
    let parsed: colabcli_rust::auth::OAuthCredentials =
        serde_json::from_value(raw.clone()).unwrap();
    assert!(parsed.valid_at(now()));
    raw["expiry"] = json!("broken");
    assert!(serde_json::from_value::<colabcli_rust::auth::OAuthCredentials>(raw).is_err());
}
#[test]
fn insecure_remote_endpoints_and_url_credentials_are_rejected() {
    for uri in [
        "http://example.invalid/token",
        "https://user:secret@example.invalid/token",
        "https://example.invalid/token#fragment",
    ] {
        assert!(validate_endpoint(&Url::parse(uri).unwrap()).is_err());
    }
    assert!(validate_endpoint(&Url::parse("http://127.0.0.1:1234/token").unwrap()).is_ok());
}
#[test]
fn oauth_redirect_is_not_followed_with_credential_form() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let mut old = token();
    old.token_uri = client(&server).token_uri.to_string();
    let handler = thread::spawn(move || {
        let request = server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        request
            .respond(Response::empty(StatusCode(302)).with_header(
                tiny_http::Header::from_bytes("Location", "https://example.invalid/leak").unwrap(),
            ))
            .unwrap();
    });
    assert!(matches!(
        TokenClient::new().unwrap().refresh(&old, now()),
        Err(Error::OAuthStatus(302))
    ));
    handler.join().unwrap();
}

#[test]
fn refreshed_credentials_survive_keyring_failure_without_second_exchange() {
    let f = Fixture::new(TokenStorageMode::Keyring);
    let server = Server::http("127.0.0.1:0").unwrap();
    let mut old = token();
    old.token_uri = client(&server).token_uri.to_string();
    f.tokens.save(&id("default"), &old).unwrap();
    let handler = thread::spawn(move || {
        server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .respond(Response::from_string(success().to_string()))
            .unwrap();
    });
    f.backend.fail_set.store(true, Ordering::SeqCst);
    assert!(service(&f).access_token(&id("default"), now()).is_err());
    handler.join().unwrap();
    let stage = f
        .storage
        .profile_path(&id("default"), "oauth_refresh_pending.json")
        .unwrap();
    assert!(stage.exists());
    f.backend.fail_set.store(false, Ordering::SeqCst);
    assert_eq!(
        service(&f)
            .access_token(&id("default"), now())
            .unwrap()
            .expose(),
        "fresh-access"
    );
    assert!(!stage.exists());
    assert_eq!(
        f.tokens
            .load(&id("default"))
            .unwrap()
            .unwrap()
            .refresh_token
            .unwrap()
            .expose(),
        "fresh-refresh"
    );
}

#[test]
fn new_login_replaces_an_older_staged_refresh() {
    let f = Fixture::new(TokenStorageMode::File);
    let stage = f
        .storage
        .profile_path(&id("default"), "oauth_refresh_pending.json")
        .unwrap();
    f.storage.write(&stage, &token()).unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    service(&f)
        .begin(
            &id("default"),
            client(&server),
            Url::parse("http://localhost:9999/callback").unwrap(),
            None,
            now(),
        )
        .unwrap();
    let handler = thread::spawn(move || {
        server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .respond(Response::from_string(success().to_string()))
            .unwrap()
    });
    service(&f)
        .finish(&id("default"), Some(Secret::new("fixture")), None, now())
        .unwrap();
    handler.join().unwrap();
    assert!(!stage.exists());
    assert_eq!(
        service(&f)
            .access_token(&id("default"), now())
            .unwrap()
            .expose(),
        "fresh-access"
    );
}

#[test]
fn renamed_account_cannot_be_recreated_by_stale_auth_context() {
    let f = Fixture::new(TokenStorageMode::File);
    let old = f.add("old");
    f.accounts.rename(&old, &id("new")).unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    assert!(matches!(
        service(&f).begin(
            &old,
            client(&server),
            Url::parse("http://localhost:9999/callback").unwrap(),
            None,
            now()
        ),
        Err(Error::AccountMissing(_))
    ));
    assert!(!f.temp.path().join("accounts/old").exists());
}

#[test]
fn default_account_lazily_adopts_legacy_keyring_without_deleting_it() {
    use colabcli_rust::credentials::CredentialBackend;
    let f = Fixture::new(TokenStorageMode::Keyring);
    let mut old = token();
    old.expiry = Some(now() + TimeDelta::hours(1));
    f.backend
        .set(
            "oauth_token",
            &Secret::new(serde_json::to_string(&old).unwrap()),
        )
        .unwrap();
    assert_eq!(
        service(&f)
            .access_token(&id("default"), now())
            .unwrap()
            .expose(),
        "fixture-access"
    );
    assert!(f.backend.get("oauth_token").unwrap().is_some());
}
