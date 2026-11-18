use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{Duration as Delta, Utc};
use colabcli_rust::auth::{AdcProvider, PUBLIC_SCOPES, TokenClient};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, sync::Arc, thread, time::Duration};
use tiny_http::{Request, Response, Server, StatusCode};

const AUDIENCE: &str = "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/pool/providers/provider";
const ACCESS: &str = "urn:ietf:params:oauth:token-type:access_token";
fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn base(s: &Server) -> String {
    format!("http://{}", s.server_addr())
}
fn document(base: &str, source: Value) -> Value {
    json!({"type":"external_account", "audience":AUDIENCE,
        "subject_token_type":"urn:ietf:params:oauth:token-type:jwt",
        "token_url":format!("{base}/v1/token"),"credential_source":source})
}
fn provider(path: &Path, value: &Value) -> AdcProvider {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    AdcProvider::new(path.into(), TokenClient::new().unwrap())
}
fn recv(s: &Server) -> Request {
    s.recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("expected request")
}
fn form(r: &mut Request) -> BTreeMap<String, String> {
    assert_eq!(r.method().as_str(), "POST");
    assert_eq!(r.url(), "/v1/token");
    assert!(!r.headers().iter().any(|h| h.field.equiv("Authorization")));
    let mut body = String::new();
    r.as_reader().read_to_string(&mut body).unwrap();
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}
fn success(token: &str) -> Value {
    json!({"access_token":token,"token_type":"Bearer","issued_token_type":ACCESS,"expires_in":3600})
}
fn reply(r: Request, value: Value) {
    r.respond(Response::from_string(value.to_string())).unwrap();
}
fn quiet(s: &Server) {
    assert!(s.recv_timeout(Duration::from_millis(75)).unwrap().is_none());
}

#[test]
fn client_basic_auth_is_sts_only_and_suppresses_workforce_user_project() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let mut doc = document(
        &uri,
        json!({"url":format!("{uri}/subject"),
        "headers":{"Authorization":"Bearer subject-only"}}),
    );
    doc["audience"] =
        json!("//iam.googleapis.com/locations/global/workforcePools/pool/providers/provider");
    doc["workforce_pool_user_project"] = json!("12345");
    doc["client_id"] = json!("client-é");
    doc["client_secret"] = json!("secret:+& 字");
    doc["service_account_impersonation_url"] = json!(format!(
        "{uri}/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken"
    ));
    let path = temp.path().join("adc");
    let p = provider(&path, &doc);
    let original = fs::read(&path).unwrap();
    let now = Utc::now();
    let handler = thread::spawn(move || {
        let authorization = |r: &Request| {
            r.headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .unwrap()
                .value
                .as_str()
                .to_owned()
        };
        let r = recv(&server);
        assert_eq!(r.url(), "/subject");
        assert_eq!(authorization(&r), "Bearer subject-only");
        reply(r, json!("jwt"));
        let mut r = recv(&server);
        assert_eq!(r.url(), "/v1/token");
        assert_eq!(
            authorization(&r),
            format!("Basic {}", STANDARD.encode("client-é:secret:+& 字"))
        );
        let mut body = String::new();
        r.as_reader().read_to_string(&mut body).unwrap();
        let fields: BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert!(!fields.contains_key("options"));
        assert!(!fields.contains_key("client_id"));
        assert!(!fields.contains_key("client_secret"));
        assert_eq!(fields["subject_token"], "\"jwt\"");
        reply(r, success("sts-only"));
        for attempt in 0..2 {
            let r = recv(&server);
            assert!(r.url().ends_with(":generateAccessToken"));
            assert_eq!(authorization(&r), "Bearer sts-only");
            reply(
                r,
                json!({"accessToken":format!("target-{attempt}"),
                "expireTime":(now+Delta::hours(1)).to_rfc3339()}),
            );
        }
        quiet(&server);
    });
    assert_eq!(p.access_token(now).unwrap().expose(), "target-0");
    assert_eq!(p.refresh_access_token(now).unwrap().expose(), "target-1");
    assert_eq!(fs::read(path).unwrap(), original);
    handler.join().unwrap();
}

#[test]
fn optional_client_secret_and_empty_id_match_google_auth_and_failures_keep_cache() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    fs::write(temp.path().join("subject"), "jwt").unwrap();
    let handler = thread::spawn(move || {
        for expected in [Some("client:"), Some("client:"), None, None] {
            let mut r = recv(&server);
            let actual = r
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .map(|h| h.value.as_str().to_owned());
            assert_eq!(
                actual,
                expected.map(|s| format!("Basic {}", STANDARD.encode(s)))
            );
            let mut body = String::new();
            r.as_reader().read_to_string(&mut body).unwrap();
            assert!(!body.contains("unused-secret"));
            reply(r, success("access"));
        }
        let r = recv(&server);
        assert_eq!(
            r.headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .unwrap()
                .value
                .as_str(),
            format!("Basic {}", STANDARD.encode("client:private-secret"))
        );
        reply(r, success("cached"));
        let r = recv(&server);
        r.respond(
            Response::from_string("private-secret response").with_status_code(StatusCode(401)),
        )
        .unwrap();
        quiet(&server);
    });
    for (id, secret) in [
        (Some("client"), None),
        (Some("client"), Some("")),
        (Some(""), Some("unused-secret")),
        (None, Some("unused-secret")),
    ] {
        let mut doc = document(&uri, json!({"file":"subject"}));
        if let Some(id) = id {
            doc["client_id"] = json!(id);
        }
        if let Some(secret) = secret {
            doc["client_secret"] = json!(secret);
        }
        assert_eq!(
            provider(&temp.path().join("adc"), &doc)
                .access_token(Utc::now())
                .unwrap()
                .expose(),
            "access"
        );
    }
    let mut doc = document(&uri, json!({"file":"subject"}));
    doc["client_id"] = json!("client");
    doc["client_secret"] = json!("private-secret");
    let p = provider(&temp.path().join("adc"), &doc);
    let now = Utc::now();
    assert_eq!(p.access_token(now).unwrap().expose(), "cached");
    let error = p.refresh_access_token(now).unwrap_err().to_string();
    assert!(!error.contains("private-secret"));
    assert_eq!(p.access_token(now).unwrap().expose(), "cached");
    handler.join().unwrap();
}

#[test]
fn client_credentials_remain_isolated_across_concurrent_profiles() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    fs::write(temp.path().join("subject"), "jwt").unwrap();
    let providers: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|id| {
            let mut doc = document(&uri, json!({"file":"subject"}));
            doc["client_id"] = json!(id);
            doc["client_secret"] = json!(format!("secret-{id}"));
            Arc::new(provider(&temp.path().join(format!("{id}.json")), &doc))
        })
        .collect();
    let handler = thread::spawn(move || {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..2 {
            let r = recv(&server);
            let header = r
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .unwrap()
                .value
                .as_str();
            let credentials = STANDARD
                .decode(header.strip_prefix("Basic ").unwrap())
                .unwrap();
            let credentials = String::from_utf8(credentials).unwrap();
            let (id, secret) = credentials.split_once(':').unwrap();
            assert!(matches!(id, "a" | "b"));
            assert_eq!(secret, format!("secret-{id}"));
            assert!(seen.insert(id.to_owned()));
            let token = format!("access-{id}");
            reply(r, success(&token));
        }
        quiet(&server);
    });
    let workers: Vec<_> = (0..12)
        .map(|i| {
            let p = providers[i % 2].clone();
            thread::spawn(move || {
                assert_eq!(
                    p.access_token(Utc::now()).unwrap().expose(),
                    if i % 2 == 0 { "access-a" } else { "access-b" }
                )
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    handler.join().unwrap();
}

#[test]
fn relative_file_precedes_url_and_concurrent_profiles_keep_separate_caches() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let mut providers = Vec::new();
    for name in ["a", "b"] {
        fs::write(
            temp.path().join(format!("{name}.token")),
            format!(" subject-{name}\n"),
        )
        .unwrap();
        let doc = document(
            &uri,
            json!({"file":format!("{name}.token"),"url":"invalid ignored by file precedence"}),
        );
        providers.push(Arc::new(provider(
            &temp.path().join(format!("{name}.json")),
            &doc,
        )));
    }
    let handler = thread::spawn(move || {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..2 {
            let mut r = recv(&server);
            let f = form(&mut r);
            assert_eq!(f.len(), 6);
            assert_eq!(f["audience"], AUDIENCE);
            assert_eq!(
                f["grant_type"],
                "urn:ietf:params:oauth:grant-type:token-exchange"
            );
            assert_eq!(f["requested_token_type"], ACCESS);
            assert_eq!(
                f["subject_token_type"],
                "urn:ietf:params:oauth:token-type:jwt"
            );
            assert_eq!(f["scope"], PUBLIC_SCOPES.join(" "));
            assert!(seen.insert(f["subject_token"].clone()));
            reply(r, success(&format!("access-{}", f["subject_token"])));
        }
        quiet(&server);
    });
    let workers: Vec<_> = (0..12)
        .map(|i| {
            let p = providers[i % 2].clone();
            thread::spawn(move || {
                assert_eq!(
                    p.access_token(Utc::now()).unwrap().expose(),
                    if i % 2 == 0 {
                        "access-subject-a"
                    } else {
                        "access-subject-b"
                    }
                );
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    handler.join().unwrap();
}

#[test]
fn forced_refresh_rereads_projected_json_token_and_failed_refresh_keeps_valid_cache() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let subject = temp.path().join("subject");
    fs::write(&subject, r#"{"jwt":"first"}"#).unwrap();
    #[cfg(unix)]
    let file = {
        let path = temp.path().join("projected");
        std::os::unix::fs::symlink(&subject, &path).unwrap();
        path
    };
    #[cfg(not(unix))]
    let file = subject.clone();
    let p = provider(
        &temp.path().join("adc"),
        &document(
            &uri,
            json!({"file":file,"format":{"type":"json","subject_token_field_name":"jwt"}}),
        ),
    );
    let handler = thread::spawn(move || {
        for name in ["first", "second"] {
            let mut r = recv(&server);
            assert_eq!(form(&mut r)["subject_token"], name);
            reply(r, success(name));
        }
        let mut r = recv(&server);
        assert_eq!(form(&mut r)["subject_token"], "second");
        r.respond(Response::from_string("private-error").with_status_code(StatusCode(503)))
            .unwrap();
        quiet(&server);
    });
    let now = Utc::now();
    assert_eq!(p.access_token(now).unwrap().expose(), "first");
    fs::write(&subject, r#"{"jwt":"second"}"#).unwrap();
    assert_eq!(p.refresh_access_token(now).unwrap().expose(), "second");
    let error = p.refresh_access_token(now).unwrap_err().to_string();
    assert!(!error.contains("private-error"));
    assert_eq!(p.access_token(now).unwrap().expose(), "second");
    handler.join().unwrap();
}

#[test]
fn url_headers_are_confined_to_subject_and_workforce_options_reach_sts() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let mut doc = document(
        &uri,
        json!({"url":format!("{uri}/subject?audience=fixture"),"headers":{"Authorization":"Bearer subject-only","Metadata":"True"},"format":{"type":"json","subject_token_field_name":"id_token"}}),
    );
    doc["audience"] =
        json!("//iam.googleapis.com/locations/global/workforcePools/pool/providers/provider");
    doc["workforce_pool_user_project"] = json!("12345");
    let p = provider(&temp.path().join("adc"), &doc);
    let handler = thread::spawn(move || {
        let r = recv(&server);
        assert_eq!(r.method().as_str(), "GET");
        assert_eq!(r.url(), "/subject?audience=fixture");
        assert_eq!(
            r.headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .unwrap()
                .value
                .as_str(),
            "Bearer subject-only"
        );
        reply(r, json!({"id_token":"url-subject"}));
        let mut r = recv(&server);
        let f = form(&mut r);
        assert_eq!(f["subject_token"], "url-subject");
        assert!(f["options"].starts_with("%7B%22userProject%22%3A"));
        assert_eq!(
            serde_json::from_str::<Value>(
                &percent_encoding::percent_decode_str(&f["options"])
                    .decode_utf8()
                    .unwrap()
            )
            .unwrap(),
            json!({"userProject":"12345"})
        );
        reply(r, success("workforce"));
        quiet(&server);
    });
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "workforce");
    handler.join().unwrap();
}

#[test]
fn iam_failure_and_forced_target_refresh_reuse_valid_sts_without_identity_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    fs::write(temp.path().join("subject"), "jwt").unwrap();
    let mut doc = document(&uri, json!({"file":"subject"}));
    doc["service_account_impersonation_url"] = json!(format!(
        "{uri}/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken"
    ));
    doc["service_account_impersonation"] = json!({"token_lifetime_seconds":7200});
    let p = provider(&temp.path().join("adc"), &doc);
    let now = Utc::now();
    let handler = thread::spawn(move || {
        let mut r = recv(&server);
        assert_eq!(
            form(&mut r)["scope"],
            "https://www.googleapis.com/auth/cloud-platform"
        );
        reply(r, success("sts-only"));
        for attempt in 0..3 {
            let mut r = recv(&server);
            assert!(r.url().ends_with(":generateAccessToken"));
            assert_eq!(
                r.headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .unwrap()
                    .value
                    .as_str(),
                "Bearer sts-only"
            );
            let mut body = String::new();
            r.as_reader().read_to_string(&mut body).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap(),
                json!({"scope":PUBLIC_SCOPES,"lifetime":"7200s"})
            );
            if attempt == 0 {
                r.respond(Response::from_string("denied").with_status_code(StatusCode(403)))
                    .unwrap();
            } else {
                reply(
                    r,
                    json!({"accessToken":format!("target-{attempt}"),"expireTime":(now+Delta::hours(1)).to_rfc3339()}),
                );
            }
        }
        quiet(&server);
    });
    assert!(p.access_token(now).is_err());
    assert_eq!(p.access_token(now).unwrap().expose(), "target-1");
    assert_eq!(p.refresh_access_token(now).unwrap().expose(), "target-2");
    handler.join().unwrap();
}

#[test]
fn invalid_configuration_fails_before_any_subject_or_sts_request() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let valid = document(&uri, json!({"url":format!("{uri}/subject")}));
    let mut cases = Vec::new();
    for (key, value) in [
        ("audience", json!("//evil.invalid/pool")),
        (
            "audience",
            json!(format!(
                "//iam.googleapis.com/projects/{}/locations/global/workloadIdentityPools/p/providers/p",
                "1".repeat(2100)
            )),
        ),
        ("token_url", json!(format!("{uri}/other"))),
        ("token_url", json!("http://sts.googleapis.com/v1/token")),
        (
            "token_url",
            json!(format!("{uri}/v1/token?secret=private-marker")),
        ),
        ("universe_domain", json!("other.invalid")),
        ("subject_token_type", json!("aws4_request")),
        ("client_secret", json!(42)),
        ("client_id", json!({"invalid":"private-marker"})),
        ("workforce_pool_user_project", json!("123")),
    ] {
        let mut doc = valid.clone();
        doc[key] = value;
        cases.push(doc);
    }
    for source in [
        json!({"executable":{"command":"do-not-run"}}),
        json!({"environment_id":"aws1"}),
        json!({"certificate":{}}),
        json!({"url":format!("{uri}/subject"),"headers":{"Host":"private-marker"}}),
        json!({"url":format!("{uri}/subject"),"headers":{"Metadata":"True","metadata":"False"}}),
        json!({"url":format!("{uri}/subject"),"format":{"type":"json"}}),
        json!({"url":"http://169.254.169.254/other"}),
    ] {
        let mut doc = valid.clone();
        doc["credential_source"] = source;
        cases.push(doc);
    }
    for doc in cases {
        let p = provider(&temp.path().join("adc"), &doc);
        let error = p.access_token(Utc::now()).unwrap_err().to_string();
        assert!(!error.contains("private-marker"));
    }
    quiet(&server);
}

#[test]
fn invalid_subject_files_never_reach_sts() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let subject = temp.path().join("subject");
    for bytes in [vec![], vec![b' '; 8], vec![0xff], vec![b'x'; 65537]] {
        fs::write(&subject, bytes).unwrap();
        let p = provider(
            &temp.path().join("adc"),
            &document(&uri, json!({"file":subject})),
        );
        assert!(p.access_token(Utc::now()).is_err());
    }
    for value in [json!({}), json!({"token":12}), json!({"token":" "})] {
        fs::write(&subject, value.to_string()).unwrap();
        let p = provider(
            &temp.path().join("adc"),
            &document(
                &uri,
                json!({"file":subject,"format":{"type":"json","subject_token_field_name":"token"}}),
            ),
        );
        assert!(p.access_token(Utc::now()).is_err());
    }
    for path in [temp.path().join("missing"), temp.path().to_path_buf()] {
        let p = provider(
            &temp.path().join("adc"),
            &document(&uri, json!({"file":path})),
        );
        assert!(p.access_token(Utc::now()).is_err());
    }
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let path = temp.path().join("fifo");
        let raw = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o600) }, 0);
        let p = provider(
            &temp.path().join("adc"),
            &document(&uri, json!({"file":path})),
        );
        assert!(p.access_token(Utc::now()).is_err());
    }
    quiet(&server);
}

#[test]
fn malformed_sts_responses_are_rejected_and_can_be_retried() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    fs::write(temp.path().join("subject"), "jwt").unwrap();
    let p = provider(
        &temp.path().join("adc"),
        &document(&uri, json!({"file":"subject"})),
    );
    let mut cases = Vec::new();
    for (key, value) in [
        ("access_token", json!("")),
        ("access_token", json!("private marker")),
        ("token_type", json!("MAC")),
        ("issued_token_type", json!("invalid")),
        ("expires_in", json!(0)),
        ("expires_in", json!(-1)),
        ("expires_in", json!(i64::MAX)),
    ] {
        let mut body = success("fixture");
        body[key] = value;
        cases.push(body);
    }
    let mut missing = success("fixture");
    missing.as_object_mut().unwrap().remove("expires_in");
    cases.push(missing);
    let count = cases.len();
    let handler = thread::spawn(move || {
        for body in cases {
            let mut r = recv(&server);
            form(&mut r);
            reply(r, body);
        }
        let mut r = recv(&server);
        form(&mut r);
        reply(r, success("recovered"));
    });
    for _ in 0..count {
        assert!(p.access_token(Utc::now()).is_err());
    }
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "recovered");
    handler.join().unwrap();
}

#[test]
fn subject_redirect_and_http_failure_do_not_forward_secrets_or_call_sts() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    let p = provider(
        &temp.path().join("adc"),
        &document(
            &uri,
            json!({"url":format!("{uri}/subject?secret=private-marker"),"headers":{"Authorization":"private-marker"}}),
        ),
    );
    let handler = thread::spawn(move || {
        for status in [302, 500] {
            let r = recv(&server);
            r.respond(
                Response::from_string("private-marker")
                    .with_status_code(StatusCode(status))
                    .with_header(
                        tiny_http::Header::from_bytes("Location", format!("{uri}/must-not-follow"))
                            .unwrap(),
                    ),
            )
            .unwrap();
        }
        quiet(&server);
    });
    for _ in 0..2 {
        let error = p.access_token(Utc::now()).unwrap_err().to_string();
        assert!(!error.contains("private-marker"));
    }
    handler.join().unwrap();
}

#[test]
fn sts_redirect_oversize_and_status_failures_do_not_poison_retry() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    fs::write(temp.path().join("subject"), "jwt").unwrap();
    let p = provider(
        &temp.path().join("adc"),
        &document(&uri, json!({"file":"subject"})),
    );
    let handler = thread::spawn(move || {
        let mut r = recv(&server);
        form(&mut r);
        r.respond(
            Response::from_string("private-marker")
                .with_status_code(StatusCode(302))
                .with_header(
                    tiny_http::Header::from_bytes("Location", format!("{uri}/never-follow"))
                        .unwrap(),
                ),
        )
        .unwrap();
        let mut r = recv(&server);
        form(&mut r);
        r.respond(Response::from_string("x".repeat(65537))).unwrap();
        let mut r = recv(&server);
        form(&mut r);
        r.respond(Response::from_string("private-marker").with_status_code(StatusCode(500)))
            .unwrap();
        let mut r = recv(&server);
        form(&mut r);
        reply(r, success("after-errors"));
        quiet(&server);
    });
    for _ in 0..3 {
        assert!(
            !p.access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("private-marker")
        );
    }
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "after-errors");
    handler.join().unwrap();
}

#[test]
fn expiry_margin_reloads_subject_without_rewriting_adc() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    fs::write(temp.path().join("subject"), "jwt").unwrap();
    let path = temp.path().join("adc");
    let p = provider(&path, &document(&uri, json!({"file":"subject"})));
    let original = fs::read(&path).unwrap();
    let handler = thread::spawn(move || {
        for i in 0..2 {
            let mut r = recv(&server);
            form(&mut r);
            reply(r, success(&format!("access-{i}")));
        }
        quiet(&server);
    });
    let now = Utc::now();
    assert_eq!(p.access_token(now).unwrap().expose(), "access-0");
    assert_eq!(
        p.access_token(now + Delta::seconds(3500)).unwrap().expose(),
        "access-0"
    );
    assert_eq!(
        p.access_token(now + Delta::seconds(3550)).unwrap().expose(),
        "access-1"
    );
    assert_eq!(fs::read(path).unwrap(), original);
    handler.join().unwrap();
}

#[test]
fn both_documented_id_token_spellings_are_forwarded_without_normalization() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let uri = base(&server);
    fs::write(temp.path().join("subject"), "jwt").unwrap();
    let handler = thread::spawn(move || {
        for spelling in ["idToken", "id_token"] {
            let mut r = recv(&server);
            assert_eq!(
                form(&mut r)["subject_token_type"],
                format!("urn:ietf:params:oauth:token-type:{spelling}")
            );
            reply(r, success("access"));
        }
        quiet(&server);
    });
    for spelling in ["idToken", "id_token"] {
        let mut doc = document(&uri, json!({"file":"subject"}));
        doc["subject_token_type"] = json!(format!("urn:ietf:params:oauth:token-type:{spelling}"));
        let p = provider(&temp.path().join("adc"), &doc);
        assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "access");
    }
    handler.join().unwrap();
}
