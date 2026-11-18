mod common;
use colabcli_rust::{
    Error,
    auth::{OAuthClient, TokenClient},
    context::{self, Environment},
    credentials::TokenStorageMode,
    drive_credentials::{
        self as drive, ConsentRequest, DRIVE_SCOPE, DriveCredentialService, DriveCredentials,
    },
    models::Secret,
};
use common::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpStream,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
use tiny_http::{Response, Server};
use url::Url;
fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn base(s: &Server) -> String {
    format!("http://{}", s.server_addr())
}
fn service(f: &Fixture, s: &Server) -> DriveCredentialService {
    DriveCredentialService::new(f.storage.clone(), TokenClient::new().unwrap())
        .with_userinfo_endpoint(Url::parse(&format!("{}/userinfo", base(s))).unwrap())
        .unwrap()
}
fn credentials(s: &Server, uid: Option<&str>) -> DriveCredentials {
    drive::imported(
        drive::ImportRequest {
            refresh_token: Secret::new("drive-refresh-original"),
            client_id: "drive-client".into(),
            client_secret: Some(Secret::new("drive-client-secret")),
            token_uri: format!("{}/token", base(s)),
            scopes: vec!["openid".into(), DRIVE_SCOPE.into()],
            user_id: uid.map(str::to_owned),
            email: None,
        },
        now(),
    )
    .unwrap()
}
#[test]
fn frozen_remote_rotation_is_durable_idempotent_and_preserves_scope_provenance() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = service(&f, &s);
    let mut old = credentials(&s, Some("gaia"));
    old.oauth
        .extra
        .insert("unknown_private".into(), json!("preserve-private-metadata"));
    let path = f
        .storage
        .profile_path(&id("default"), "dfs_credentials.json")
        .unwrap();
    f.storage.write(&path, &old).unwrap();
    let compute = f
        .storage
        .profile_path(&id("default"), "oauth_token.json")
        .unwrap();
    f.storage
        .write(&compute, &json!({"token":"synthetic-compute-token"}))
        .unwrap();
    let before_compute = std::fs::read(&compute).unwrap();
    let expected = drive::remote_fingerprint(&old);
    let mut remote: Value = serde_json::from_str(
        colabcli_rust::drivefs::metadata_config(&old)
            .unwrap()
            .expose(),
    )
    .unwrap();
    remote["refresh_token"] = json!("rotated-from-remote");
    remote["scopes"] = json!([]);
    remote["scope_confirmed"] = json!(true);
    let raw = serde_json::to_vec_pretty(&remote).unwrap();
    let result = svc.adopt_remote(&id("default"), &expected, &raw).unwrap();
    assert!(result.updated);
    assert_eq!(result.receipt, format!("{:x}", Sha256::digest(&raw)));
    let saved = svc.load(&id("default")).unwrap().unwrap();
    assert_eq!(
        saved.oauth.refresh_token.as_ref().unwrap().expose(),
        "rotated-from-remote"
    );
    assert!(saved.oauth.scopes.is_empty());
    assert!(saved.oauth.token.is_empty());
    assert!(saved.oauth.expiry.is_none());
    assert_eq!(saved.oauth.extra["drive_scope_source"], "token_response");
    assert_eq!(
        saved.oauth.extra["unknown_private"],
        "preserve-private-metadata"
    );
    let replay = svc.adopt_remote(&id("default"), &expected, &raw).unwrap();
    assert!(!replay.updated);
    assert_eq!(result.receipt, replay.receipt);
    assert_eq!(std::fs::read(&compute).unwrap(), before_compute);
    assert!(s.recv_timeout(Duration::from_millis(30)).unwrap().is_none());
    assert!(!format!("{result:?}").contains("rotated-from-remote"));
}

#[test]
fn remote_adoption_refuses_changed_client_identity_and_newer_local_credentials() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = service(&f, &s);
    let original = credentials(&s, Some("gaia"));
    let path = f
        .storage
        .profile_path(&id("default"), "dfs_credentials.json")
        .unwrap();
    f.storage.write(&path, &original).unwrap();
    let expected = drive::remote_fingerprint(&original);
    let remote: Value = serde_json::from_str(
        colabcli_rust::drivefs::metadata_config(&original)
            .unwrap()
            .expose(),
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    for (key, value) in [
        ("user_id", json!("other-gaia")),
        ("client_id", json!("other-client")),
        ("client_secret", json!("fixture-private-wrong-secret")),
        ("token_uri", json!("https://example.invalid/token")),
        ("email", json!("other@example.invalid")),
        ("version", json!(2)),
        ("scope_confirmed", json!("fixture-private-wrong-type")),
    ] {
        let mut changed = remote.clone();
        changed[key] = value;
        let error = svc
            .adopt_remote(
                &id("default"),
                &expected,
                &serde_json::to_vec(&changed).unwrap(),
            )
            .unwrap_err()
            .to_string();
        assert!(!error.contains("fixture-private"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    let mut newer = original.clone();
    newer.oauth.refresh_token = Some(Secret::new("newer-local-refresh"));
    f.storage.write(&path, &newer).unwrap();
    let mut changed = remote;
    changed["refresh_token"] = json!("remote-refresh");
    assert!(
        svc.adopt_remote(
            &id("default"),
            &expected,
            &serde_json::to_vec(&changed).unwrap()
        )
        .is_err()
    );
    assert_eq!(
        svc.load(&id("default"))
            .unwrap()
            .unwrap()
            .oauth
            .refresh_token
            .as_ref()
            .unwrap()
            .expose(),
        "newer-local-refresh"
    );
    assert!(
        svc.adopt_remote(&id("missing"), &expected, b"private-invalid-json")
            .is_err()
    );
    assert!(s.recv_timeout(Duration::from_millis(30)).unwrap().is_none());
}

#[test]
fn concurrent_remote_adoptions_have_one_winner_and_resume_staged_rotation() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = Arc::new(service(&f, &s));
    let original = credentials(&s, Some("gaia"));
    let path = f
        .storage
        .profile_path(&id("default"), "dfs_credentials.json")
        .unwrap();
    f.storage.write(&path, &original).unwrap();
    let expected = drive::remote_fingerprint(&original);
    let template: Value = serde_json::from_str(
        colabcli_rust::drivefs::metadata_config(&original)
            .unwrap()
            .expose(),
    )
    .unwrap();
    let workers: Vec<_> = (0..2)
        .map(|index| {
            let svc = svc.clone();
            let expected = expected.clone();
            let mut remote = template.clone();
            remote["refresh_token"] = json!(format!("concurrent-remote-{index}"));
            thread::spawn(move || {
                svc.adopt_remote(
                    &id("default"),
                    &expected,
                    &serde_json::to_vec(&remote).unwrap(),
                )
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let winner = svc.load(&id("default")).unwrap().unwrap();
    f.storage.write(&path, &original).unwrap();
    f.storage
        .write(
            &f.storage
                .profile_path(&id("default"), "dfs_refresh_pending.json")
                .unwrap(),
            &json!({"version":1,"account":"default","credentials":winner}),
        )
        .unwrap();
    let raw = colabcli_rust::drivefs::metadata_config(&winner).unwrap();
    assert!(
        !svc.adopt_remote(&id("default"), &expected, raw.expose().as_bytes())
            .unwrap()
            .updated
    );
    assert_eq!(
        drive::remote_fingerprint(&svc.load(&id("default")).unwrap().unwrap()),
        drive::remote_fingerprint(&winner)
    );
    assert!(
        !f.storage
            .profile_path(&id("default"), "dfs_refresh_pending.json")
            .unwrap()
            .exists()
    );
}
fn receive(s: &Server, method: &str, path: &str) -> tiny_http::Request {
    let request = s
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("expected HTTP request");
    assert_eq!(request.method().as_str(), method);
    assert_eq!(request.url(), path);
    request
}
fn form(request: &mut tiny_http::Request) -> BTreeMap<String, String> {
    let mut bytes = String::new();
    request.as_reader().read_to_string(&mut bytes).unwrap();
    url::form_urlencoded::parse(bytes.as_bytes())
        .into_owned()
        .collect()
}
fn reply(request: tiny_http::Request, value: Value) {
    request
        .respond(Response::from_string(value.to_string()))
        .unwrap();
}
fn refreshed() -> Value {
    json!({"access_token":"drive-access-fresh","refresh_token":"drive-refresh-rotated","expires_in":3600,"scope":format!("openid {DRIVE_SCOPE}"),"token_type":"Bearer"})
}
fn request(s: &Server) -> ConsentRequest {
    ConsentRequest {
        flavor: "custom".into(),
        client: OAuthClient {
            client_id: "drive-client".into(),
            client_secret: Some(Secret::new("drive-client-secret")),
            auth_uri: Url::parse(&format!("{}/authorize", base(s))).unwrap(),
            token_uri: Url::parse(&format!("{}/token", base(s))).unwrap(),
        },
        redirect: Url::parse("http://localhost:12345/callback").unwrap(),
        scopes: vec!["openid".into(), DRIVE_SCOPE.into()],
        user_id: None,
        email: None,
        login_hint: Some("hint@example.invalid".into()),
        include_granted_scopes: false,
    }
}

#[test]
fn imported_drive_refresh_and_userinfo_are_isolated_from_compute_and_bound_accounts() {
    let f = Fixture::new(TokenStorageMode::File);
    let drive_account = f.add("storage");
    f.accounts
        .set_drive(Some(&id("default")), Some(&drive_account))
        .unwrap();
    f.tokens.save(&id("default"), &token()).unwrap();
    let s = server();
    let svc = service(&f, &s);
    let credentials = credentials(&s, None);
    let server =
        thread::spawn(move || {
            let mut refresh = receive(&s, "POST", "/token");
            let form = form(&mut refresh);
            assert_eq!(form["client_id"], "drive-client");
            assert_eq!(form["client_secret"], "drive-client-secret");
            assert_eq!(form["refresh_token"], "drive-refresh-original");
            assert!(!form.contains_key("scope"));
            reply(refresh, refreshed());
            let who = receive(&s, "GET", "/userinfo");
            assert!(who.headers().iter().any(|h| h.field.equiv("Authorization")
                && h.value.as_str() == "Bearer drive-access-fresh"));
            reply(
                who,
                json!({"sub":"gaia-storage","email":"drive@example.invalid"}),
            );
        });
    let saved = svc.import(&drive_account, credentials, now()).unwrap();
    server.join().unwrap();
    assert_eq!(saved.user_id.as_deref(), Some("gaia-storage"));
    assert_eq!(
        saved.oauth.refresh_token.as_ref().unwrap().expose(),
        "drive-refresh-rotated"
    );
    assert!(svc.load(&id("default")).unwrap().is_none());
    assert_eq!(
        f.tokens
            .load(&id("default"))
            .unwrap()
            .unwrap()
            .refresh_token
            .unwrap()
            .expose(),
        "fixture-refresh"
    );
    let selected = context::resolve_drive(
        &f.accounts,
        &Environment::default(),
        &id("default"),
        None,
        false,
    )
    .unwrap();
    assert_eq!(selected, drive_account);
    assert_eq!(
        context::resolve_drive(
            &f.accounts,
            &Environment::default(),
            &id("default"),
            None,
            true
        )
        .unwrap(),
        id("default")
    );
    let read = svc.load(&drive_account).unwrap().unwrap();
    assert_eq!(read.user_id, saved.user_id);
    let text = read.summary().to_string();
    for private in [
        "drive-refresh-original",
        "drive-refresh-rotated",
        "drive-client-secret",
        "drive-access-fresh",
    ] {
        assert!(!text.contains(private));
        assert!(!format!("{read:?}").contains(private));
    }
    assert_eq!(read.email.as_deref(), Some("drive@example.invalid"));
}
#[test]
fn drive_scope_membership_and_rt_flavor_are_exact_and_unknown_fields_never_displayed() {
    let s = server();
    let mut c = credentials(&s, Some("gaia"));
    c.oauth.scopes = vec!["https://www.googleapis.com/auth/drive.file".into()];
    assert!(c.validate_for_mount(false).is_err());
    c.oauth.scopes = vec![format!("{DRIVE_SCOPE}.readonly")];
    assert!(c.validate_for_mount(false).is_err());
    c.oauth.scopes = vec![DRIVE_SCOPE.into()];
    assert!(c.validate_for_mount(false).is_ok());
    assert!(c.validate_for_mount(true).is_err());
    c.flavor = "dfs".into();
    assert!(c.validate_for_mount(true).is_err());
    c.oauth.client_id = drive::DRIVEFS_CLIENT_ID.into();
    assert!(c.validate_for_mount(true).is_ok());
    c.oauth
        .extra
        .insert("arbitrary_secret".into(), json!("must-never-display"));
    let encoded = serde_json::to_value(&c).unwrap();
    let read: DriveCredentials = serde_json::from_value(encoded).unwrap();
    assert_eq!(read.oauth.extra["arbitrary_secret"], "must-never-display");
    assert!(!read.summary().to_string().contains("must-never-display"));
    c.user_id = None;
    assert!(c.validate_for_mount(false).is_err());
    c.user_id = Some("bad\nidentity".into());
    assert!(c.validate().is_err());
    c.user_id = Some(" ".into());
    assert!(c.validate().is_err());
}
#[test]
fn drive_consent_state_pkce_granted_scope_and_duplicate_callback_checks() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = service(&f, &s);
    let url = svc.begin(&id("default"), request(&s), now()).unwrap();
    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query["scope"], format!("openid {DRIVE_SCOPE}"));
    assert_eq!(query["login_hint"], "hint@example.invalid");
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(!query.contains_key("include_granted_scopes"));
    assert!(!query.contains_key("client_secret"));
    let path = f
        .storage
        .profile_path(&id("default"), "dfs_pending.json")
        .unwrap();
    let initial: Value = f.storage.read(&path).unwrap().unwrap();
    assert_eq!(initial["account"], "default");
    assert_eq!(initial["state"], query["state"]);
    assert_ne!(initial["code_verifier"], initial["state"]);
    for target in [
        "http://localhost:12345/callback?code=fixture&state=wrong".to_string(),
        format!(
            "http://localhost:12345/callback?code=fixture&state={}&state={}",
            query["state"], query["state"]
        ),
        format!(
            "http://localhost:12345/other?code=fixture&state={}",
            query["state"]
        ),
    ] {
        assert!(
            svc.finish(
                &id("default"),
                None,
                Some(Url::parse(&target).unwrap()),
                now()
            )
            .is_err()
        );
        assert_eq!(
            f.storage.read::<Value>(&path).unwrap().unwrap()["exchange_started"],
            false
        );
    }
    let verifier = initial["code_verifier"].as_str().unwrap().to_owned();
    let server = thread::spawn(move || {
        let mut token = receive(&s, "POST", "/token");
        let form = form(&mut token);
        assert_eq!(form["code_verifier"], verifier);
        assert_eq!(form["code"], "authorization-code-fixture");
        assert_eq!(form["client_id"], "drive-client");
        reply(token, refreshed());
        let user = receive(&s, "GET", "/userinfo");
        reply(
            user,
            json!({"sub":"gaia-consent","email":"consent@example.invalid"}),
        );
    });
    let callback = Url::parse(&format!(
        "http://localhost:12345/callback?state={}&code=authorization-code-fixture",
        query["state"]
    ))
    .unwrap();
    let saved = svc
        .finish(&id("default"), None, Some(callback), now())
        .unwrap();
    server.join().unwrap();
    assert_eq!(saved.user_id.as_deref(), Some("gaia-consent"));
    assert!(!path.exists());
    assert!(f.tokens.load(&id("default")).unwrap().is_none());
    assert!(
        !f.storage
            .profile_path(&id("default"), "oauth_pending.json")
            .unwrap()
            .exists()
    );
}
#[test]
fn drive_unknown_exchange_cannot_replay_but_userinfo_failure_resumes_without_reexchange() {
    for mode in ["exchange-failure", "userinfo-failure"] {
        let f = Fixture::new(TokenStorageMode::File);
        let s = server();
        let svc = service(&f, &s);
        svc.begin(&id("default"), request(&s), now()).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let server = thread::spawn(move || {
            let exchange = receive(&s, "POST", "/token");
            calls.fetch_add(1, Ordering::SeqCst);
            if mode == "exchange-failure" {
                exchange
                    .respond(
                        Response::from_string("private-error-never-print").with_status_code(503),
                    )
                    .unwrap();
            } else {
                reply(exchange, refreshed());
                let info = receive(&s, "GET", "/userinfo");
                info.respond(Response::from_string("private-user-error").with_status_code(503))
                    .unwrap();
                let info = receive(&s, "GET", "/userinfo");
                reply(info, json!({"sub":"gaia-recovered"}));
            }
        });
        assert!(
            svc.finish(
                &id("default"),
                Some(Secret::new("one-use-code")),
                None,
                now()
            )
            .is_err()
        );
        if mode == "exchange-failure" {
            assert!(
                matches!(svc.finish(&id("default"),Some(Secret::new("one-use-code")),None,now()),Err(Error::OAuth(message))if message.contains("结果未知"))
            );
        } else {
            let saved = svc.finish(&id("default"), None, None, now()).unwrap();
            assert_eq!(saved.user_id.as_deref(), Some("gaia-recovered"));
        }
        server.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn drive_rotation_survives_failed_userinfo_and_clear_never_resurrects_staged_credentials() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = service(&f, &s);
    let old = credentials(&s, Some("gaia"));
    let original = old.clone();
    f.storage
        .write(
            &f.storage
                .profile_path(&id("default"), "dfs_credentials.json")
                .unwrap(),
            &old,
        )
        .unwrap();
    let server = thread::spawn(move || {
        let mut refresh = receive(&s, "POST", "/token");
        assert_eq!(
            form(&mut refresh)["refresh_token"],
            "drive-refresh-original"
        );
        reply(refresh, refreshed());
        let info = receive(&s, "GET", "/userinfo");
        info.respond(Response::from_string("sensitive-error").with_status_code(503))
            .unwrap();
        let mut refresh = receive(&s, "POST", "/token");
        assert_eq!(form(&mut refresh)["refresh_token"], "drive-refresh-rotated");
        reply(refresh, refreshed());
    });
    assert!(svc.token(&id("default"), true, now()).is_err());
    let staged = f
        .storage
        .profile_path(&id("default"), "dfs_refresh_pending.json")
        .unwrap();
    assert!(staged.exists());
    let result = svc.token(&id("default"), false, now()).unwrap();
    assert_eq!(
        result.oauth.refresh_token.unwrap().expose(),
        "drive-refresh-rotated"
    );
    server.join().unwrap();
    assert!(!staged.exists());
    f.storage
        .write(
            &staged,
            &json!({"version":1,"account":"default","credentials":original}),
        )
        .unwrap();
    assert!(svc.clear(&id("default")).unwrap());
    assert!(!staged.exists());
    assert!(svc.load(&id("default")).unwrap().is_none());
    assert!(!svc.clear(&id("default")).unwrap());
}
#[test]
fn drive_rename_moves_pending_and_rotation_state_and_removed_accounts_cannot_be_recreated() {
    let f = Fixture::new(TokenStorageMode::File);
    let old = f.add("old");
    let new = id("new");
    let s = server();
    let svc = service(&f, &s);
    let mut req = request(&s);
    req.user_id = Some("gaia".into());
    svc.begin(&old, req, now()).unwrap();
    let c = credentials(&s, Some("gaia"));
    let staged = f
        .storage
        .profile_path(&old, "dfs_refresh_pending.json")
        .unwrap();
    f.storage
        .write(&staged, &json!({"version":1,"account":old,"credentials":c}))
        .unwrap();
    f.accounts.rename(&old, &new).unwrap();
    for name in ["dfs_pending.json", "dfs_refresh_pending.json"] {
        let data: Value = f
            .storage
            .read(&f.storage.profile_path(&new, name).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(data["account"], "new");
    }
    let server = thread::spawn(move || {
        let exchange = receive(&s, "POST", "/token");
        reply(exchange, refreshed());
    });
    let result = svc
        .finish(&new, Some(Secret::new("code")), None, now())
        .unwrap();
    server.join().unwrap();
    assert_eq!(result.user_id.as_deref(), Some("gaia"));
    f.accounts.remove(&new, true).unwrap();
    assert!(matches!(svc.clear(&new), Err(Error::AccountMissing(_))));
    assert!(
        !f.storage
            .profile_path(&new, "dfs_credentials.json")
            .unwrap()
            .exists()
    );
}
#[test]
fn drive_concurrent_refresh_serializes_and_retains_unknown_credential_fields() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = service(&f, &s);
    let mut c = credentials(&s, Some("gaia"));
    c.oauth
        .extra
        .insert("future-field".into(), json!({"private":"preserved"}));
    f.storage
        .write(
            &f.storage
                .profile_path(&id("default"), "dfs_credentials.json")
                .unwrap(),
            &c,
        )
        .unwrap();
    let server = thread::spawn(move || {
        for (old, new) in [
            ("drive-refresh-original", "rotation-1"),
            ("rotation-1", "rotation-2"),
        ] {
            let mut request = receive(&s, "POST", "/token");
            assert_eq!(form(&mut request)["refresh_token"], old);
            let mut result = refreshed();
            result["refresh_token"] = json!(new);
            reply(request, result);
        }
    });
    let first = svc.clone();
    let second = svc.clone();
    let a = thread::spawn(move || first.token(&id("default"), false, now()).unwrap());
    let b = thread::spawn(move || second.token(&id("default"), false, now()).unwrap());
    a.join().unwrap();
    b.join().unwrap();
    server.join().unwrap();
    let saved = svc.load(&id("default")).unwrap().unwrap();
    assert_eq!(saved.oauth.refresh_token.unwrap().expose(), "rotation-2");
    assert_eq!(saved.oauth.extra["future-field"]["private"], "preserved");
}
#[test]
fn drive_pending_expiry_account_and_journal_guards_prevent_network_and_writes() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = service(&f, &s);
    let mut invalid = request(&s);
    invalid.client.token_uri.set_query(Some("private=query"));
    assert!(svc.begin(&id("default"), invalid, now()).is_err());
    let mut empty = request(&s);
    empty.scopes.clear();
    assert!(svc.begin(&id("default"), empty, now()).is_err());
    svc.begin(&id("default"), request(&s), now()).unwrap();
    assert!(
        svc.finish(
            &id("default"),
            Some(Secret::new("code")),
            None,
            now() + chrono::Duration::minutes(16)
        )
        .is_err()
    );
    let path = f
        .storage
        .profile_path(&id("default"), "dfs_pending.json")
        .unwrap();
    let mut pending: Value = f.storage.read(&path).unwrap().unwrap();
    pending["account"] = json!("other");
    f.storage.write(&path, &pending).unwrap();
    assert!(
        svc.finish(&id("default"), Some(Secret::new("code")), None, now())
            .is_err()
    );
    f.storage
        .write(
            &f.storage.path("account-transaction.json").unwrap(),
            &json!({"synthetic":"pending"}),
        )
        .unwrap();
    assert!(matches!(
        svc.load(&id("default")),
        Err(Error::MigrationPending)
    ));
    assert!(matches!(
        svc.clear(&id("default")),
        Err(Error::MigrationPending)
    ));
    assert!(path.exists());
}
fn cli(f: &Fixture) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_colab"));
    c.arg("--config-dir")
        .arg(f.storage.root())
        .args(["--token-storage", "file"])
        .env_remove("COLAB_ACCOUNT")
        .env_remove("COLAB_DRIVE_ACCOUNT")
        .env_remove("COLAB_DFS_ACCOUNT");
    for name in [
        "COLAB_DRIVE_DEFAULT",
        "COLAB_DFS_DEFAULT",
        "COLAB_DRIVE_REFRESH_TOKEN",
        "COLAB_DRIVE_CLIENT_ID",
        "COLAB_DRIVE_CLIENT_SECRET",
        "COLAB_DRIVE_TOKEN_URI",
        "COLAB_DRIVE_USER_ID",
        "COLAB_DRIVE_EMAIL",
        "COLAB_DRIVE_SCOPES",
    ] {
        c.env_remove(name);
    }
    c
}

fn url_only_consent(command: &mut Command) -> &mut Command {
    command.args([
        "--url-only",
        "--no-browser",
        "--flavor",
        "custom",
        "--client-id",
        "new-drive-client",
        "--client-secret",
        "new-drive-secret",
        "--auth-uri",
        "http://127.0.0.1:9/auth",
        "--token-uri",
        "http://127.0.0.1:9/token",
    ])
}

#[test]
fn cli_drive_consent_begin_registers_only_selected_profiles_and_preserves_active_and_bindings() {
    let cases = [
        (
            vec!["-a", "fresh", "drive", "consent"],
            None,
            None,
            "fresh",
            vec!["fresh"],
        ),
        (
            vec!["drive", "-a", "fresh", "consent"],
            None,
            None,
            "fresh",
            vec!["fresh"],
        ),
        (
            vec!["dfs", "consent", "-a", "fresh"],
            None,
            None,
            "fresh",
            vec!["fresh"],
        ),
        (
            vec!["drive", "consent"],
            Some("fresh"),
            None,
            "fresh",
            vec!["fresh"],
        ),
        (
            vec!["drive", "consent"],
            None,
            Some("storage"),
            "storage",
            vec!["storage"],
        ),
        (
            vec!["-a", "fresh", "drive", "consent"],
            None,
            Some("storage"),
            "storage",
            vec!["fresh", "storage"],
        ),
        (
            vec!["-a", "work", "drive", "consent"],
            None,
            None,
            "work",
            vec![],
        ),
        (
            vec!["-a", "PERSON@example.invalid", "drive", "consent"],
            None,
            None,
            "work",
            vec![],
        ),
    ];
    for (args, compute_env, drive_env, owner, added) in cases {
        let f = Fixture::new(TokenStorageMode::File);
        f.add("work");
        f.accounts
            .update(|r| {
                r.accounts.get_mut(&id("work")).unwrap().email =
                    Some("person@example.invalid".into());
                r.accounts.get_mut(&id("work")).unwrap().drive_account = Some(id("default"));
                Ok(())
            })
            .unwrap();
        let before = f.accounts.registry().unwrap();
        let mut command = cli(&f);
        command.args(args);
        if let Some(value) = compute_env {
            command.env("COLAB_ACCOUNT", value);
        }
        if let Some(value) = drive_env {
            command.env("COLAB_DRIVE_ACCOUNT", value);
        }
        let result = url_only_consent(&mut command).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let url = Url::parse(String::from_utf8(result.stdout).unwrap().trim()).unwrap();
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key == "login_hint")
                .map(|(_, value)| value.into_owned()),
            (owner == "work").then(|| "person@example.invalid".to_owned())
        );
        let after = f.accounts.registry().unwrap();
        assert_eq!(after.active, before.active);
        assert_eq!(after.default_drive_account, before.default_drive_account);
        assert_eq!(after.accounts.len(), before.accounts.len() + added.len());
        for (profile, metadata) in before.accounts {
            assert_eq!(
                serde_json::to_value(&after.accounts[&profile]).unwrap(),
                serde_json::to_value(metadata).unwrap()
            );
        }
        let pending: Value = f
            .storage
            .read(
                &f.storage
                    .profile_path(&id(owner), "dfs_pending.json")
                    .unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(pending["account"], owner);
        assert_eq!(
            pending["state"],
            url.query_pairs()
                .find(|(key, _)| key == "state")
                .unwrap()
                .1
                .as_ref()
        );
        for profile in after.accounts.keys() {
            assert!(
                !f.storage
                    .profile_path(profile, "oauth_token.json")
                    .unwrap()
                    .exists()
            );
            assert!(
                !f.storage
                    .profile_path(profile, "dfs_credentials.json")
                    .unwrap()
                    .exists()
            );
            assert_eq!(
                f.storage
                    .profile_path(profile, "dfs_pending.json")
                    .unwrap()
                    .exists(),
                profile.as_str() == owner
            );
        }
    }
}

#[test]
fn cli_drive_consent_rejects_invalid_selection_atomically_and_never_creates_on_completion_or_other_operations()
 {
    let f = Fixture::new(TokenStorageMode::File);
    let path = f.storage.path("accounts.json").unwrap();
    let before = std::fs::read(&path).unwrap();
    for args in [
        vec![
            "-a",
            "missing",
            "drive",
            "consent",
            "--code",
            "synthetic-code",
        ],
        vec![
            "-a",
            "missing",
            "drive",
            "consent",
            "--callback-url",
            "https://example.invalid/callback?code=x&state=y",
        ],
        vec!["-a", "missing", "drive", "show"],
        vec!["-a", "missing", "drive", "clear"],
        vec!["-a", "missing", "drive", "token"],
    ] {
        assert!(!cli(&f).args(args).output().unwrap().status.success());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    for storage_selector in ["../escape", "FRESH"] {
        let mut command = cli(&f);
        command
            .args(["-a", "fresh", "drive", "consent"])
            .env("COLAB_DRIVE_ACCOUNT", storage_selector);
        assert!(
            !url_only_consent(&mut command)
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    assert!(!f.storage.root().join("accounts/fresh").exists());
}

#[test]
fn cli_failed_drive_consent_begin_keeps_registered_profile_for_retry_without_authorization() {
    let f = Fixture::new(TokenStorageMode::File);
    let failed = cli(&f)
        .args([
            "-a",
            "fresh",
            "drive",
            "consent",
            "--url-only",
            "--flavor",
            "dfs",
            "--client-id",
            "wrong-client",
        ])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let registry = f.accounts.registry().unwrap();
    assert_eq!(registry.active.as_str(), "default");
    assert!(registry.accounts.contains_key(&id("fresh")));
    for file in [
        "oauth_token.json",
        "dfs_credentials.json",
        "dfs_pending.json",
    ] {
        assert!(!f.storage.profile_path(&id("fresh"), file).unwrap().exists());
    }
    let before = std::fs::read(f.storage.path("accounts.json").unwrap()).unwrap();
    let mut retry = cli(&f);
    retry.args(["-a", "fresh", "drive", "consent"]);
    assert!(
        url_only_consent(&mut retry)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        std::fs::read(f.storage.path("accounts.json").unwrap()).unwrap(),
        before
    );
}

#[test]
fn concurrent_drive_consent_begins_register_the_same_profile_once_without_lost_accounts() {
    let f = Fixture::new(TokenStorageMode::File);
    f.add("untouched");
    let before =
        serde_json::to_value(f.accounts.registry().unwrap().accounts[&id("untouched")].clone())
            .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let mut command = cli(&f);
            command.args(["-a", "fresh", "drive", "consent"]);
            url_only_consent(&mut command);
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                command.output().unwrap()
            })
        })
        .collect();
    for worker in workers {
        let output = worker.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let registry = f.accounts.registry().unwrap();
    assert_eq!(registry.accounts.len(), 3);
    assert_eq!(registry.active.as_str(), "default");
    assert_eq!(
        serde_json::to_value(&registry.accounts[&id("untouched")]).unwrap(),
        before
    );
    let pending: Value = f
        .storage
        .read(
            &f.storage
                .profile_path(&id("fresh"), "dfs_pending.json")
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(pending["account"], "fresh");
}

#[test]
fn drive_consent_hint_uses_selected_storage_metadata_and_explicit_precedence() {
    for (options, expected) in [
        (vec![], "storage@example.invalid"),
        (
            vec!["--email", "requested@example.invalid"],
            "requested@example.invalid",
        ),
        (
            vec![
                "--email",
                "requested@example.invalid",
                "--login-hint",
                "explicit@example.invalid",
            ],
            "explicit@example.invalid",
        ),
        (vec!["--login-hint", ""], "storage@example.invalid"),
    ] {
        let f = Fixture::new(TokenStorageMode::File);
        f.add("storage");
        f.accounts
            .update(|r| {
                r.accounts.get_mut(&id("default")).unwrap().email =
                    Some("compute@example.invalid".into());
                r.accounts.get_mut(&id("storage")).unwrap().email =
                    Some("storage@example.invalid".into());
                Ok(())
            })
            .unwrap();
        let before = std::fs::read(f.storage.path("accounts.json").unwrap()).unwrap();
        let mut command = cli(&f);
        command
            .args(["drive", "consent"])
            .env("COLAB_DRIVE_ACCOUNT", "storage");
        url_only_consent(&mut command).args(options);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let url = Url::parse(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key == "login_hint")
                .unwrap()
                .1,
            expected
        );
        assert_eq!(
            std::fs::read(f.storage.path("accounts.json").unwrap()).unwrap(),
            before
        );
    }
}

#[test]
fn cli_drive_import_show_token_clear_and_alias_do_not_open_compute_credentials() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let token_uri = format!("{}/token", base(&s));
    let userinfo = format!("{}/userinfo", base(&s));
    let server = thread::spawn(move || {
        let refresh = receive(&s, "POST", "/token");
        reply(refresh, refreshed());
        let user = receive(&s, "GET", "/userinfo");
        reply(
            user,
            json!({"sub":"gaia-cli","email":"cli@example.invalid"}),
        );
        let refresh = receive(&s, "POST", "/token");
        reply(refresh, refreshed());
    });
    let imported = cli(&f)
        .args([
            "--drive-userinfo-url",
            &userinfo,
            "drive",
            "import",
            "--refresh-token",
            "drive-refresh-original",
            "--client-id",
            "drive-client",
            "--client-secret",
            "drive-client-secret",
            "--token-uri",
            &token_uri,
            "--scopes",
            DRIVE_SCOPE,
        ])
        .output()
        .unwrap();
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let show = cli(&f).args(["dfs", "show"]).output().unwrap();
    assert!(show.status.success());
    let value: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(value["user_id"], "gaia-cli");
    assert_eq!(value["account"], "default");
    assert_eq!(value["drive_account"], "default");
    assert!(value.get("credentials").is_none());
    for secret in [
        "drive-access-fresh",
        "drive-refresh-rotated",
        "drive-client-secret",
    ] {
        assert!(!String::from_utf8_lossy(&show.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&imported.stdout).contains(secret));
    }
    let token = cli(&f).args(["drive", "token", "--json"]).output().unwrap();
    assert!(
        token.status.success(),
        "{}",
        String::from_utf8_lossy(&token.stderr)
    );
    let token: Value = serde_json::from_slice(&token.stdout).unwrap();
    assert_eq!(token["access_token"], "drive-access-fresh");
    server.join().unwrap();
    let clear = cli(&f).args(["drive", "clear"]).output().unwrap();
    assert!(clear.status.success());
    let path = f
        .storage
        .profile_path(&id("default"), "dfs_credentials.json")
        .unwrap();
    assert_eq!(
        String::from_utf8(clear.stdout).unwrap(),
        format!(
            "removed [default] {} (remember to revoke the grant on https://myaccount.google.com/permissions)\n",
            path.display()
        )
    );
    let empty = cli(&f).args(["drive", "show"]).output().unwrap();
    assert!(empty.status.success());
    assert_eq!(
        String::from_utf8(empty.stdout).unwrap(),
        format!("[default] {}: empty (run `consent`)\n", path.display())
    );
    let repeated_clear = cli(&f).args(["drive", "clear"]).output().unwrap();
    assert!(repeated_clear.status.success());
    assert_eq!(
        String::from_utf8(repeated_clear.stdout).unwrap(),
        format!("[default] {}: nothing to remove\n", path.display())
    );
    assert!(f.tokens.load(&id("default")).unwrap().is_none());
    assert!(
        !f.storage
            .profile_path(&id("default"), "dfs_credentials.json")
            .unwrap()
            .exists()
    );
}
#[test]
fn cli_drive_consent_loopback_rejects_bad_state_then_completes_pkce_without_browser() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let token_uri = format!("{}/token", base(&s));
    let auth_uri = format!("{}/authorize", base(&s));
    let userinfo = format!("{}/userinfo", base(&s));
    let server = thread::spawn(move || {
        let mut exchange = receive(&s, "POST", "/token");
        let form = form(&mut exchange);
        assert_eq!(form["code"], "fixture-code");
        assert_eq!(form["code_verifier"].len(), 43);
        reply(exchange, refreshed());
        let info = receive(&s, "GET", "/userinfo");
        reply(info, json!({"sub":"gaia-loopback"}));
    });
    let mut child = cli(&f)
        .args([
            "--drive-userinfo-url",
            &userinfo,
            "drive",
            "consent",
            "--flavor",
            "custom",
            "--client-id",
            "drive-client",
            "--token-uri",
            &token_uri,
            "--auth-uri",
            &auth_uri,
            "--redirect-uri",
            "http://127.0.0.1:0/callback",
            "--no-browser",
            "--consent-timeout",
            "5",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    use std::io::{BufRead, BufReader};
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let url = Url::parse(line.trim()).unwrap();
    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let redirect = Url::parse(&query["redirect_uri"]).unwrap();
    let send = |state: &str| {
        let mut stream = TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET /callback?state={state}&code=fixture-code HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    };
    assert!(send("wrong").starts_with("HTTP/1.1 400"));
    assert!(send(&query["state"]).starts_with("HTTP/1.1 200"));
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(rest.contains("已保存"));
    let data: Value = f
        .storage
        .read(
            &f.storage
                .profile_path(&id("default"), "dfs_credentials.json")
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(data["user_id"], "gaia-loopback");
    assert!(
        !f.storage
            .profile_path(&id("default"), "dfs_pending.json")
            .unwrap()
            .exists()
    );
}

#[test]
fn drive_granted_scopes_override_requested_and_missing_scope_is_marked_unverified() {
    for grant in [Some("openid"), Some(""), None] {
        let f = Fixture::new(TokenStorageMode::File);
        let s = server();
        let svc = service(&f, &s);
        let mut req = request(&s);
        req.user_id = Some("gaia".into());
        req.redirect = Url::parse(colabcli_rust::auth::REMOTE_REDIRECT).unwrap();
        let custom = svc.begin(&id("default"), req, now()).unwrap();
        assert!(!custom.query_pairs().any(|(k, _)| k == "token_usage"));
        let mut req = request(&s);
        req.flavor = "sdk".into();
        req.user_id = Some("gaia".into());
        req.include_granted_scopes = true;
        let sdk = svc.begin(&id("default"), req, now()).unwrap();
        let pairs: BTreeMap<_, _> = sdk.query_pairs().into_owned().collect();
        assert_eq!(pairs["token_usage"], "remote");
        assert_eq!(pairs["include_granted_scopes"], "true");
        let server = thread::spawn(move || {
            let token = receive(&s, "POST", "/token");
            let mut result = refreshed();
            if let Some(scope) = grant {
                result["scope"] = json!(scope);
            } else {
                result.as_object_mut().unwrap().remove("scope");
            }
            reply(token, result);
        });
        let saved = svc
            .finish(&id("default"), Some(Secret::new("code")), None, now())
            .unwrap();
        server.join().unwrap();
        if let Some(grant) = grant {
            assert_eq!(
                saved.oauth.scopes,
                grant
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            );
            assert_eq!(saved.summary()["scope_source"], "token_response");
            assert!(saved.validate_for_mount(false).is_err());
        } else {
            assert_eq!(saved.summary()["scope_source"], "requested_unverified");
            assert!(saved.oauth.scopes.contains(&DRIVE_SCOPE.to_owned()));
        }
    }
}
#[test]
fn failed_drive_refresh_preserves_existing_credentials_and_redacts_response() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = service(&f, &s);
    let credentials = credentials(&s, Some("gaia"));
    let path = f
        .storage
        .profile_path(&id("default"), "dfs_credentials.json")
        .unwrap();
    f.storage.write(&path, &credentials).unwrap();
    let before = std::fs::read(&path).unwrap();
    let server = thread::spawn(move || {
        let request = receive(&s, "POST", "/token");
        request
            .respond(Response::from_string("private-refresh-material").with_status_code(400))
            .unwrap();
    });
    let error = svc.token(&id("default"), false, now()).unwrap_err();
    server.join().unwrap();
    assert!(matches!(error, Error::OAuthStatus(400)));
    assert!(!error.to_string().contains("private-refresh-material"));
    assert_eq!(before, std::fs::read(&path).unwrap());
    let mut credentials = credentials;
    credentials.oauth.token_uri.push_str("?private=token");
    assert!(credentials.validate().is_err());
}
#[test]
fn cli_drive_url_only_writes_private_separate_pending_and_finishes_after_rename() {
    let f = Fixture::new(TokenStorageMode::File);
    f.add("old-drive");
    let s = server();
    let token_uri = format!("{}/token", base(&s));
    let auth_uri = format!("{}/authorize", base(&s));
    let output = cli(&f)
        .args([
            "--account",
            "old-drive",
            "drive",
            "consent",
            "--url-only",
            "--client-id",
            "drive-client",
            "--flavor",
            "custom",
            "--token-uri",
            &token_uri,
            "--auth-uri",
            &auth_uri,
            "--user-id",
            "gaia-cli",
            "--minimal",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let url = Url::parse(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    assert_eq!(url.path(), "/authorize");
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "scope" && v.contains(DRIVE_SCOPE))
    );
    assert!(!url.query_pairs().any(|(k, _)| k == "token_usage"));
    let path = f
        .storage
        .profile_path(&id("old-drive"), "dfs_pending.json")
        .unwrap();
    assert!(path.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    f.accounts
        .rename(&id("old-drive"), &id("renamed-drive"))
        .unwrap();
    let server = thread::spawn(move || {
        let token = receive(&s, "POST", "/token");
        reply(token, refreshed());
    });
    let output = cli(&f)
        .args([
            "--account",
            "renamed-drive",
            "drive",
            "consent",
            "--code",
            "fixture-code",
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !f.storage
            .profile_path(&id("renamed-drive"), "dfs_pending.json")
            .unwrap()
            .exists()
    );
    let credentials: Value = f
        .storage
        .read(
            &f.storage
                .profile_path(&id("renamed-drive"), "dfs_credentials.json")
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(credentials["user_id"], "gaia-cli");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("drive-refresh-rotated"));
}
#[test]
fn drive_pending_schema_and_staged_account_mismatch_are_not_silently_overwritten() {
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = service(&f, &s);
    let c = credentials(&s, Some("gaia"));
    let staged = f
        .storage
        .profile_path(&id("default"), "dfs_refresh_pending.json")
        .unwrap();
    f.storage
        .write(
            &staged,
            &json!({"version":1,"account":"wrong-account","credentials":c}),
        )
        .unwrap();
    assert!(svc.load(&id("default")).is_err());
    assert!(staged.exists());
    assert!(
        !f.storage
            .profile_path(&id("default"), "dfs_credentials.json")
            .unwrap()
            .exists()
    );
    svc.clear(&id("default")).unwrap();
    svc.begin(&id("default"), request(&s), now()).unwrap();
    let path = f
        .storage
        .profile_path(&id("default"), "dfs_pending.json")
        .unwrap();
    let mut pending: Value = f.storage.read(&path).unwrap().unwrap();
    pending["version"] = json!(2);
    f.storage.write(&path, &pending).unwrap();
    assert!(matches!(
        svc.finish(&id("default"), Some(Secret::new("code")), None, now()),
        Err(Error::Schema(2))
    ));
    assert_eq!(
        f.storage.read::<Value>(&path).unwrap().unwrap()["exchange_started"],
        false
    );
}

#[cfg(unix)]
#[test]
fn cli_drive_loopback_denial_timeout_and_term_preserve_unconsumed_pending() {
    for mode in ["denied", "timeout", "term"] {
        let f = Fixture::new(TokenStorageMode::File);
        let mut child = cli(&f)
            .args([
                "drive",
                "consent",
                "--flavor",
                "custom",
                "--client-id",
                "synthetic-client",
                "--auth-uri",
                "http://127.0.0.1:1/auth",
                "--token-uri",
                "http://127.0.0.1:1/token",
                "--redirect-uri",
                "http://127.0.0.1:0/callback",
                "--no-browser",
                "--consent-timeout",
                "1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        use std::io::{BufRead, BufReader};
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let auth = Url::parse(line.trim()).unwrap();
        let pairs: BTreeMap<_, _> = auth.query_pairs().into_owned().collect();
        let redirect = Url::parse(&pairs["redirect_uri"]).unwrap();
        match mode {
            "denied" => {
                let mut stream =
                    TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                write!(stream,"GET /callback?error=access_denied&state={} HTTP/1.1\r\nHost: localhost\r\n\r\n",pairs["state"]).unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).unwrap();
                assert!(response.starts_with("HTTP/1.1 400"));
            }
            // SAFETY: signal only the owned live child process.
            "term" => assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0),
            _ => {}
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                panic!("Drive callback did not stop");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(if mode == "term" { 143 } else { 1 }),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pending: Value = f
            .storage
            .read(
                &f.storage
                    .profile_path(&id("default"), "dfs_pending.json")
                    .unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(pending["exchange_started"], false);
        assert!(
            !f.storage
                .profile_path(&id("default"), "dfs_credentials.json")
                .unwrap()
                .exists()
        );
    }
}

#[cfg(unix)]
#[test]
fn cli_drive_pasted_authorization_code_uses_tty_without_echo_or_secret_output() {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    };
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let token_uri = format!("{}/token", base(&s));
    let auth_uri = format!("{}/auth", base(&s));
    let server = thread::spawn(move || {
        let mut exchange = receive(&s, "POST", "/token");
        assert_eq!(form(&mut exchange)["code"], "private-pasted-code");
        reply(exchange, refreshed());
    });
    let (mut master_fd, mut slave_fd) = (-1, -1);
    // SAFETY: valid descriptor output pointers, optional settings are null.
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
    // SAFETY: successful openpty returned distinct owned descriptors.
    let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
    let terminal_fd = slave.as_raw_fd();
    let mut command = cli(&f);
    command
        .args([
            "drive",
            "consent",
            "--flavor",
            "custom",
            "--client-id",
            "drive-client",
            "--token-uri",
            &token_uri,
            "--auth-uri",
            &auth_uri,
            "--user-id",
            "gaia-tty",
            "--no-browser",
            "--consent-timeout",
            "5",
        ])
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe calls are made between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(terminal_fd, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let mut attrs = std::mem::MaybeUninit::uninit();
        // SAFETY: valid open descriptor and output storage.
        assert_eq!(
            unsafe { libc::tcgetattr(master.as_raw_fd(), attrs.as_mut_ptr()) },
            0
        );
        if unsafe { attrs.assume_init().c_lflag } & libc::ECHO == 0 {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    master.write_all(b"private-pasted-code\n").unwrap();
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains("private-pasted-code"));
        assert!(!text.contains("drive-refresh-rotated"));
    }
    let mut attrs = std::mem::MaybeUninit::uninit();
    // SAFETY: descriptor and output storage remain valid.
    assert_eq!(
        unsafe { libc::tcgetattr(master.as_raw_fd(), attrs.as_mut_ptr()) },
        0
    );
    assert_ne!(unsafe { attrs.assume_init().c_lflag } & libc::ECHO, 0);
}

#[test]
fn leases_are_exact_idempotent_and_guard_credentials_and_pruned_compute_accounts() {
    use colabcli_rust::drivefs::{lease::Lease, remote::RemoteRequest};
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = service(&f, &s);
    let drive_account = f.add("drive");
    let compute = id("default");
    let creds = credentials(&s, Some("gaia"));
    f.storage
        .write(
            &f.storage
                .profile_path(&drive_account, "dfs_credentials.json")
                .unwrap(),
            &creds,
        )
        .unwrap();
    f.storage.save_session(&compute, session("work")).unwrap();
    let lease = Lease {
        version: 1,
        compute_account: compute.clone(),
        drive_account: drive_account.clone(),
        session: "work".into(),
        endpoint: "ep-work".into(),
        request: RemoteRequest::new("/content/drive").unwrap(),
        fingerprint: drive::remote_fingerprint(&creds),
        identity: colabcli_rust::drivefs::lifecycle::identity(&creds).unwrap(),
    };
    let mut invalid = lease.clone();
    invalid.endpoint = "replaced".into();
    assert!(svc.acquire_lease(&invalid).is_err());
    invalid = lease.clone();
    invalid.fingerprint = "0".repeat(64);
    assert!(svc.acquire_lease(&invalid).is_err());
    assert!(svc.leases(&drive_account).unwrap().is_empty());
    svc.acquire_lease(&lease).unwrap();
    svc.acquire_lease(&lease).unwrap();
    let mut other = lease.clone();
    other.request = RemoteRequest::new("/content/drive-other").unwrap();
    svc.acquire_lease(&other).unwrap();
    let mut collision = lease.clone();
    collision.request.readonly = true;
    assert!(svc.acquire_lease(&collision).is_err());
    assert!(svc.release_lease(&collision).is_err());
    assert!(svc.clear(&drive_account).is_err());
    assert!(svc.token(&drive_account, false, now()).is_err());
    assert!(svc.import(&drive_account, creds.clone(), now()).is_err());
    assert!(svc.load(&drive_account).unwrap().is_some());
    // Backend sync may prune local sessions; cross-account ownership must survive it.
    f.storage
        .write(
            &f.storage.profile_path(&compute, "sessions.json").unwrap(),
            &json!({}),
        )
        .unwrap();
    for profile in [&compute, &drive_account] {
        assert!(f.accounts.rename(profile, &id("renamed")).is_err());
        assert!(f.accounts.remove(profile, true).is_err());
    }
    svc.release_lease(&lease).unwrap();
    svc.release_lease(&lease).unwrap();
    assert_eq!(svc.leases(&drive_account).unwrap(), vec![other.clone()]);
    assert!(svc.clear(&drive_account).is_err());
    svc.release_lease(&other).unwrap();
    assert!(svc.clear(&drive_account).unwrap());
    assert!(s.recv_timeout(Duration::from_millis(30)).unwrap().is_none());
}

#[test]
fn drive_identity_matches_python_json_for_unicode_quotes_and_surrogates() {
    use sha2::{Digest, Sha256};
    let s = server();
    let mut creds = credentials(&s, Some("用户😀\"\\"));
    creds.oauth.client_id = "客户\"\\".into();
    let expected = r#"["\u5ba2\u6237\"\\", "\u7528\u6237\ud83d\ude00\"\\"]"#;
    assert_eq!(
        colabcli_rust::drivefs::lifecycle::identity(&creds).unwrap(),
        format!("{:x}", Sha256::digest(expected.as_bytes()))
    );
}

#[test]
fn mount_sources_preview_validate_preserve_rotation_and_refuse_active_source_changes() {
    use colabcli_rust::drivefs::{lease::Lease, remote::RemoteRequest};
    let f = Fixture::new(TokenStorageMode::File);
    let s = server();
    let svc = service(&f, &s);
    let profile = id("default");
    f.storage.save_session(&profile, session("work")).unwrap();
    let path = f
        .storage
        .profile_path(&profile, "dfs_credentials.json")
        .unwrap();
    let candidate = credentials(&s, Some("gaia"));
    svc.configure_mount(&profile, Some(&candidate), None, None, false, false)
        .unwrap();
    assert!(!path.exists());
    let mut invalid = candidate.clone();
    invalid.oauth.scopes = vec!["https://www.googleapis.com/auth/drive.file".into()];
    assert!(
        svc.configure_mount(&profile, Some(&invalid), None, None, false, true)
            .is_err()
    );
    assert!(!path.exists());
    let saved = svc
        .configure_mount(&profile, Some(&candidate), None, None, false, true)
        .unwrap();
    let fingerprint = drive::remote_fingerprint(&saved);
    let lease = Lease {
        version: 1,
        compute_account: profile.clone(),
        drive_account: profile.clone(),
        session: "work".into(),
        endpoint: "ep-work".into(),
        request: RemoteRequest::new("/content/drive").unwrap(),
        fingerprint: fingerprint.clone(),
        identity: colabcli_rust::drivefs::lifecycle::identity(&saved).unwrap(),
    };
    svc.acquire_lease(&lease).unwrap();
    // The exact source can retry, but neither a new CI token nor identity can replace an active source.
    svc.configure_mount(&profile, Some(&candidate), None, None, false, true)
        .unwrap();
    let mut changed = candidate.clone();
    changed.oauth.refresh_token = Some(Secret::new("different-ci-source"));
    assert!(
        svc.configure_mount(&profile, Some(&changed), None, None, false, true)
            .is_err()
    );
    assert!(
        svc.configure_mount(&profile, None, Some("different-gaia"), None, false, false)
            .is_err()
    );
    let mut remote: Value = serde_json::from_str(
        colabcli_rust::drivefs::metadata_config(&saved)
            .unwrap()
            .expose(),
    )
    .unwrap();
    remote["refresh_token"] = json!("remote-ci-rotation");
    svc.adopt_remote(
        &profile,
        &fingerprint,
        &serde_json::to_vec(&remote).unwrap(),
    )
    .unwrap();
    svc.release_lease(&lease).unwrap();
    let resumed = svc
        .configure_mount(&profile, Some(&candidate), None, None, false, true)
        .unwrap();
    assert_eq!(
        resumed.oauth.refresh_token.as_ref().unwrap().expose(),
        "remote-ci-rotation"
    );
    // Repeated CI input must not resurrect the original refresh token after unmount.
    let read_back = svc.load(&profile).unwrap().unwrap();
    assert_eq!(
        read_back.oauth.refresh_token.as_ref().unwrap().expose(),
        "remote-ci-rotation"
    );
    let mut with_unknown = svc.load(&profile).unwrap().unwrap();
    with_unknown
        .oauth
        .extra
        .insert("preserved_unknown".into(), json!("keep-private-extension"));
    f.storage.write(&path, &with_unknown).unwrap();
    svc.configure_mount(&profile, Some(&changed), None, None, false, true)
        .unwrap();
    assert_eq!(
        svc.load(&profile).unwrap().unwrap().oauth.extra["preserved_unknown"],
        "keep-private-extension"
    );
    assert_eq!(
        svc.load(&profile).unwrap().unwrap().oauth.extra["drive_scope_source"],
        "configured_unverified"
    );
    assert_eq!(
        svc.load(&profile)
            .unwrap()
            .unwrap()
            .oauth
            .refresh_token
            .unwrap()
            .expose(),
        "different-ci-source"
    );
    assert!(s.recv_timeout(Duration::from_millis(30)).unwrap().is_none());
}

#[test]
fn cli_mount_injection_preflight_is_offline_and_cli_token_forces_shim_without_writing_compute() {
    let f = Fixture::new(TokenStorageMode::File);
    f.storage
        .save_session(&id("default"), session("work"))
        .unwrap();
    let drive_path = f
        .storage
        .profile_path(&id("default"), "dfs_credentials.json")
        .unwrap();
    let compute_path = f
        .storage
        .profile_path(&id("default"), "oauth_token.json")
        .unwrap();
    let mut oauth = token();
    oauth.scopes = vec![DRIVE_SCOPE.into()];
    f.tokens.save(&id("default"), &oauth).unwrap();
    let before = std::fs::read(&compute_path).unwrap();
    let preview = cli(&f)
        .args([
            "drive",
            "mount",
            "-s",
            "work",
            "--cli-token",
            "--mode",
            "rt",
            "--user-id",
            "synthetic-gaia",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let value: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(value["mode"], "shim");
    assert_eq!(value["drive_account"], "default");
    assert_eq!(value["transport"], "native");
    assert!(!drive_path.exists());
    assert_eq!(std::fs::read(&compute_path).unwrap(), before);
    for args in [
        vec!["--from-env"],
        vec!["--cli-token", "--from-env"],
        vec!["--cli-token", "--drive-account", "default"],
        vec!["--drive-extra=--features=read_only_mode:false"],
    ] {
        let output = cli(&f)
            .args(["drive", "mount", "-s", "work", "--dry-run"])
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!drive_path.exists());
    }
    oauth.scopes = vec!["https://www.googleapis.com/auth/drive.file".into()];
    f.tokens.save(&id("default"), &oauth).unwrap();
    let rejected = cli(&f)
        .args([
            "drive",
            "mount",
            "-s",
            "work",
            "--cli-token",
            "--user-id",
            "synthetic-gaia",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("完整 drive scope"));
    assert!(!drive_path.exists());
    for output in [preview, rejected] {
        for bytes in [output.stdout, output.stderr] {
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("fixture-refresh"));
            assert!(!text.contains("fixture-secret"));
        }
    }
}

#[test]
fn drivemount_preflight_honors_strategy_environment_and_refuses_noninteractive_fallback() {
    let f = Fixture::new(TokenStorageMode::File);
    f.storage
        .save_session(&id("default"), session("work"))
        .unwrap();
    // No compute OAuth is available; all these failures must happen before authentication.
    for flags in [
        vec![],
        vec!["--no-fallback"],
        vec!["--upstream", "--readonly"],
        vec!["--upstream", "--force"],
        vec!["--from-env"],
        vec!["--dry-run"],
    ] {
        let result = cli(&f)
            .args(["drivemount", "-s", "work"])
            .args(flags)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(
            !f.storage
                .profile_path(&id("default"), "oauth_token.json")
                .unwrap()
                .exists()
        );
    }
    let s = server();
    let credentials = credentials(&s, Some("gaia"));
    let path = f
        .storage
        .profile_path(&id("default"), "dfs_credentials.json")
        .unwrap();
    f.storage.write(&path, &credentials).unwrap();
    let before = std::fs::read(&path).unwrap();
    let preview = cli(&f)
        .args(["drivemount", "-s", "work", "--dry-run", "--readonly"])
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let value: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(value["timeout"], 30);
    assert_eq!(value["readonly"], true);
    assert_eq!(value["transport"], "native");
    for env_name in ["COLAB_DRIVE_DEFAULT", "COLAB_DFS_DEFAULT"] {
        let result = cli(&f)
            .args(["drivemount", "-s", "work", "--dry-run"])
            .env(env_name, "OFF")
            .output()
            .unwrap();
        assert!(!result.status.success());
    }
    for timeout in ["0", "-1", "0.5", "3601", "NaN"] {
        let result = cli(&f)
            .args([
                "drivemount",
                "-s",
                "work",
                "--timeout",
                timeout,
                "--dry-run",
            ])
            .output()
            .unwrap();
        assert!(!result.status.success());
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut owned = session("work");
    owned.extra.insert(
        "drive_plan".into(),
        json!({"request":{"mountpoint":"/content/drive"}}),
    );
    f.storage.save_session(&id("default"), owned).unwrap();
    let rejected = cli(&f)
        .args(["drivemount", "-s", "work", "--upstream"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("不能切换官方挂载策略"));
    for target in [
        "/content/drive/",
        "/content/drive/child",
        "/content/other/../drive",
        "/content",
    ] {
        let rejected = cli(&f)
            .args(["drivemount", "-s", "work", target, "--upstream"])
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("不能切换官方挂载策略"));
    }
    assert!(s.recv_timeout(Duration::from_millis(30)).unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn drivemount_no_fallback_and_cross_drive_refuse_even_with_terminal_stdin() {
    use std::os::fd::FromRawFd;
    let f = Fixture::new(TokenStorageMode::File);
    f.storage
        .save_session(&id("default"), session("work"))
        .unwrap();
    let (mut master_fd, mut slave_fd) = (-1, -1);
    // SAFETY: valid output pointers and no optional termios/window pointers.
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
    // SAFETY: successful openpty returned two distinct owned descriptors.
    let _master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
    let rejected = cli(&f)
        .args(["drivemount", "-s", "work", "--no-fallback"])
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("缺少 DriveFS 凭据"));
    assert!(!String::from_utf8_lossy(&rejected.stderr).contains("官方交互挂载流程"));
    let drive = f.add("storage");
    f.accounts
        .set_drive(Some(&id("default")), Some(&drive))
        .unwrap();
    let rejected = cli(&f)
        .args(["drivemount", "-s", "work"])
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(!String::from_utf8_lossy(&rejected.stderr).contains("官方交互挂载流程"));
    // Clearing the binding must not bypass existing native ownership records.
    f.accounts.set_drive(Some(&id("default")), None).unwrap();
    f.storage
        .write(
            &f.storage
                .profile_path(&id("default"), "managed/drivefs-owned.json")
                .unwrap(),
            &json!({"private":"ownership"}),
        )
        .unwrap();
    let rejected = cli(&f)
        .args(["drivemount", "-s", "work"])
        .stdin(Stdio::from(slave))
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("已有 DriveFS 恢复责任"));
}
