use chrono::{Duration as Delta, Utc};
use colabcli_rust::{
    auth::{AdcProvider, AwsEnvironment, MetadataOptions, TokenClient},
    context::{AccountContext, AccountSource},
    models::{AuthProvider, ProfileId, Secret},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, sync::Arc, thread, time::Duration};
use tiny_http::{Request, Response, Server, StatusCode};
const AUD: &str =
    "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p";
const VERIFY: &str =
    "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15";
fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn doc(s: &Server) -> Value {
    let base = format!("http://{}", s.server_addr());
    json!({"type":"external_account","audience":AUD,"subject_token_type":"urn:ietf:params:aws:token-type:aws4_request","token_url":format!("{base}/v1/token"),"credential_source":{"environment_id":"aws1","regional_cred_verification_url":VERIFY,"region_url":format!("{base}/latest/meta-data/placement/availability-zone"),"url":format!("{base}/latest/meta-data/iam/security-credentials"),"imdsv2_session_token_url":format!("{base}/latest/api/token")}})
}
fn environment(name: &str) -> AwsEnvironment {
    AwsEnvironment {
        region: Some("us-east-1".into()),
        access_key: Some(Secret::new(format!("AKID{name}"))),
        secret_key: Some(Secret::new(format!("synthetic-{name}"))),
        session_token: Some(Secret::new(format!("session-{name}"))),
    }
}
fn provider(path: &Path, doc: &Value, aws: AwsEnvironment) -> AdcProvider {
    fs::write(path, doc.to_string()).unwrap();
    AdcProvider::from_context(
        &AccountContext {
            profile: ProfileId::new("aws").unwrap(),
            source: AccountSource::Flag,
            provider: AuthProvider::Adc,
            adc_file: Some(path.into()),
            quota_project: None,
            adc_file_required: true,
            cancellation: None,
            metadata: MetadataOptions {
                disabled: true,
                ..Default::default()
            },
            allow_executables: false,
            aws,
            certificate: Default::default(),
        },
        TokenClient::new().unwrap(),
    )
}
fn recv(s: &Server) -> Request {
    let r = s
        .recv_timeout(Duration::from_secs(15))
        .unwrap()
        .expect("expected request");
    assert!(!r.headers().iter().any(|h| h.field.equiv("Authorization")));
    r
}
fn reply(r: Request, value: Value) {
    r.respond(Response::from_string(value.to_string())).unwrap();
}
fn metadata(s: &Server, path: &str, method: &str, token: bool) -> Request {
    let r = recv(s);
    assert_eq!(r.url(), path);
    assert_eq!(r.method().as_str(), method);
    let h = r
        .headers()
        .iter()
        .find(|h| h.field.equiv("X-aws-ec2-metadata-token"));
    if token {
        assert_eq!(h.unwrap().value.as_str(), "imds-fixture");
    } else {
        assert!(h.is_none());
    }
    r
}
fn imds(s: &Server) {
    let r = metadata(s, "/latest/api/token", "PUT", false);
    assert_eq!(
        r.headers()
            .iter()
            .find(|h| h.field.equiv("X-aws-ec2-metadata-token-ttl-seconds"))
            .unwrap()
            .value
            .as_str(),
        "300"
    );
    r.respond(Response::from_string("imds-fixture")).unwrap();
}
fn creds(s: &Server, name: &str) {
    metadata(s, "/latest/meta-data/iam/security-credentials", "GET", true)
        .respond(Response::from_string("role-fixture\n"))
        .unwrap();
    reply(
        metadata(
            s,
            "/latest/meta-data/iam/security-credentials/role-fixture",
            "GET",
            true,
        ),
        json!({"Code":"Success","AccessKeyId":format!("AKID{name}"),"SecretAccessKey":format!("synthetic-{name}"),"Token":format!("session-{name}"),"Expiration":(Utc::now()+Delta::hours(1)).to_rfc3339()}),
    );
}
fn sts(s: &Server, region: &str, name: &str) -> Request {
    let mut r = recv(s);
    assert_eq!(r.url(), "/v1/token");
    assert_eq!(r.method().as_str(), "POST");
    assert!(
        !r.headers()
            .iter()
            .any(|h| h.field.equiv("X-aws-ec2-metadata-token"))
    );
    let mut body = String::new();
    r.as_reader().read_to_string(&mut body).unwrap();
    let form: BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(
        form["subject_token_type"],
        "urn:ietf:params:aws:token-type:aws4_request"
    );
    assert_eq!(form["audience"], AUD);
    assert_eq!(
        form["grant_type"],
        "urn:ietf:params:oauth:grant-type:token-exchange"
    );
    assert!(form["subject_token"].starts_with("%7B"));
    let raw = percent_encoding::percent_decode_str(&form["subject_token"])
        .decode_utf8()
        .unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["method"], "POST");
    assert_eq!(
        value["url"],
        format!("https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15")
    );
    let h: BTreeMap<_, _> = value["headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["key"].as_str().unwrap(), h["value"].as_str().unwrap()))
        .collect();
    assert_eq!(h["x-goog-cloud-target-resource"], AUD);
    assert_eq!(h["x-amz-security-token"], format!("session-{name}"));
    assert!(h["Authorization"].contains(&format!("Credential=AKID{name}/")));
    assert!(h["Authorization"].contains("x-goog-cloud-target-resource"));
    assert!(!body.contains("synthetic-"));
    r
}
fn access(r: Request, name: &str) {
    reply(
        r,
        json!({"access_token":name,"expires_in":3600,"token_type":"Bearer","issued_token_type":"urn:ietf:params:oauth:token-type:access_token"}),
    );
}
fn quiet(s: &Server) {
    assert!(s.recv_timeout(Duration::from_millis(75)).unwrap().is_none());
}

#[test]
fn aws_environment_profiles_sign_without_metadata_and_share_only_own_token_cache() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let document = doc(&server);
    let a = Arc::new(provider(
        &temp.path().join("a"),
        &document,
        environment("A"),
    ));
    let b = provider(&temp.path().join("b"), &document, environment("B"));
    let handler = thread::spawn(move || {
        access(sts(&server, "us-east-1", "A"), "token-A");
        access(sts(&server, "us-east-1", "B"), "token-B");
        quiet(&server);
    });
    let jobs: Vec<_> = (0..8)
        .map(|_| {
            let p = a.clone();
            thread::spawn(move || {
                assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "token-A")
            })
        })
        .collect();
    for j in jobs {
        j.join().unwrap();
    }
    assert_eq!(b.access_token(Utc::now()).unwrap().expose(), "token-B");
    handler.join().unwrap();
}
#[test]
fn aws_imdsv2_region_role_and_rotating_credentials_are_reread_only_on_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let document = doc(&server);
    let path = temp.path().join("adc");
    let p = provider(&path, &document, Default::default());
    let original = fs::read(&path).unwrap();
    let handler = thread::spawn(move || {
        for name in ["A", "B"] {
            imds(&server);
            metadata(
                &server,
                "/latest/meta-data/placement/availability-zone",
                "GET",
                true,
            )
            .respond(Response::from_string("eu-west-2b"))
            .unwrap();
            creds(&server, name);
            access(sts(&server, "eu-west-2", name), name);
        }
        quiet(&server);
    });
    let now = Utc::now();
    assert_eq!(p.access_token(now).unwrap().expose(), "A");
    assert_eq!(p.access_token(now).unwrap().expose(), "A");
    assert_eq!(p.refresh_access_token(now).unwrap().expose(), "B");
    assert_eq!(fs::read(path).unwrap(), original);
    handler.join().unwrap();
}
#[test]
fn aws_partial_environment_credentials_fall_back_as_whole_and_environment_region_skips_zone() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let mut env = environment("PARTIAL");
    env.secret_key = None;
    let p = provider(&temp.path().join("adc"), &doc(&server), env);
    let handler = thread::spawn(move || {
        imds(&server);
        creds(&server, "M");
        access(sts(&server, "us-east-1", "M"), "access");
        quiet(&server);
    });
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "access");
    handler.join().unwrap();
}
#[test]
fn aws_imdsv1_is_used_only_when_v2_endpoint_is_absent() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let mut document = doc(&server);
    document["credential_source"]
        .as_object_mut()
        .unwrap()
        .remove("imdsv2_session_token_url");
    let p = provider(
        &temp.path().join("adc"),
        &document,
        AwsEnvironment {
            region: Some("us-east-1".into()),
            ..Default::default()
        },
    );
    let handler = thread::spawn(move || {
        metadata(
            &server,
            "/latest/meta-data/iam/security-credentials",
            "GET",
            false,
        )
        .respond(Response::from_string("role"))
        .unwrap();
        reply(
            metadata(
                &server,
                "/latest/meta-data/iam/security-credentials/role",
                "GET",
                false,
            ),
            json!({"AccessKeyId":"AKIDV1","SecretAccessKey":"synthetic-V1","Token":"session-V1"}),
        );
        access(sts(&server, "us-east-1", "V1"), "v1");
        quiet(&server);
    });
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "v1");
    handler.join().unwrap();
}
#[test]
fn aws_invalid_configuration_and_environment_are_rejected_without_network_or_secret_output() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let valid = doc(&server);
    let mut cases = Vec::new();
    for (key, value) in [
        ("environment_id", json!("aws2")),
        (
            "regional_cred_verification_url",
            json!("https://private-marker.invalid"),
        ),
        (
            "region_url",
            json!("http://private-marker.invalid/latest/meta-data/placement/availability-zone"),
        ),
        ("url", json!("http://169.254.169.254/other")),
        (
            "imdsv2_session_token_url",
            json!("http://169.254.169.254/latest/api/token?private-marker"),
        ),
        ("file", json!("private-marker")),
        ("format", json!({"type":"text"})),
    ] {
        let mut d = valid.clone();
        d["credential_source"][key] = value;
        cases.push(d);
    }
    let mut d = valid.clone();
    d["audience"] = json!("//iam.googleapis.com/locations/global/workforcePools/p/providers/p");
    cases.push(d);
    let mut d = valid.clone();
    d["subject_token_type"] = json!("urn:ietf:params:oauth:token-type:jwt");
    cases.push(d);
    for d in cases {
        let p = provider(&temp.path().join("adc"), &d, environment("A"));
        assert!(
            !p.access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("private-marker")
        );
    }
    for env in [
        AwsEnvironment {
            region: Some("private-marker/?".into()),
            ..environment("A")
        },
        AwsEnvironment {
            access_key: Some(Secret::new("bad/key")),
            ..environment("A")
        },
        AwsEnvironment {
            session_token: Some(Secret::new("private marker\r\n")),
            ..environment("A")
        },
    ] {
        let p = provider(&temp.path().join("adc"), &valid, env);
        assert!(
            !p.access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("private-marker")
        );
    }
    quiet(&server);
}
#[test]
fn aws_v2_failure_does_not_downgrade_and_retry_preserves_valid_access_cache() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let p = provider(
        &temp.path().join("adc"),
        &doc(&server),
        AwsEnvironment {
            region: Some("us-east-1".into()),
            ..Default::default()
        },
    );
    let handler = thread::spawn(move || {
        imds(&server);
        creds(&server, "A");
        access(sts(&server, "us-east-1", "A"), "old");
        metadata(&server, "/latest/api/token", "PUT", false)
            .respond(Response::from_string("private-marker").with_status_code(StatusCode(403)))
            .unwrap();
        imds(&server);
        creds(&server, "B");
        access(sts(&server, "us-east-1", "B"), "new");
        quiet(&server);
    });
    let now = Utc::now();
    assert_eq!(p.access_token(now).unwrap().expose(), "old");
    assert!(
        !p.refresh_access_token(now)
            .unwrap_err()
            .to_string()
            .contains("private-marker")
    );
    assert_eq!(p.access_token(now).unwrap().expose(), "old");
    assert_eq!(p.refresh_access_token(now).unwrap().expose(), "new");
    handler.join().unwrap();
}
#[test]
fn aws_role_path_injection_bad_json_and_expired_credentials_never_reach_sts() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let mut d = doc(&server);
    d["credential_source"]
        .as_object_mut()
        .unwrap()
        .remove("imdsv2_session_token_url");
    let p = provider(
        &temp.path().join("adc"),
        &d,
        AwsEnvironment {
            region: Some("us-east-1".into()),
            ..Default::default()
        },
    );
    let handler = thread::spawn(move || {
        for role in ["../private-marker", ".", ".."] {
            metadata(
                &server,
                "/latest/meta-data/iam/security-credentials",
                "GET",
                false,
            )
            .respond(Response::from_string(role))
            .unwrap();
        }
        for body in ["private-marker-invalid-json".into(),json!({"AccessKeyId":"AKID","SecretAccessKey":"secret","Expiration":"2000-01-01T00:00:00Z"}).to_string(),json!({"AccessKeyId":"AKID","SecretAccessKey":"secret","Code":"Denied"}).to_string()]{metadata(&server,"/latest/meta-data/iam/security-credentials","GET",false).respond(Response::from_string("role")).unwrap();metadata(&server,"/latest/meta-data/iam/security-credentials/role","GET",false).respond(Response::from_string(body)).unwrap();}
        quiet(&server);
    });
    for _ in 0..6 {
        assert!(
            !p.access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("private-marker")
        );
    }
    handler.join().unwrap();
}
#[test]
fn aws_metadata_redirect_and_oversize_do_not_leak_credentials_or_reach_sts() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let p = provider(&temp.path().join("adc"), &doc(&server), Default::default());
    let handler = thread::spawn(move || {
        let r = metadata(&server, "/latest/api/token", "PUT", false);
        r.respond(
            Response::from_string("private-marker")
                .with_status_code(StatusCode(302))
                .with_header(
                    tiny_http::Header::from_bytes("Location", "http://private-marker.invalid")
                        .unwrap(),
                ),
        )
        .unwrap();
        let r = metadata(&server, "/latest/api/token", "PUT", false);
        r.respond(Response::from_string("x".repeat(65537))).unwrap();
        quiet(&server);
    });
    for _ in 0..2 {
        assert!(
            !p.access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("private-marker")
        );
    }
    handler.join().unwrap();
}

#[test]
fn aws_environment_keys_need_only_region_metadata_and_invalid_zone_cannot_reach_sts() {
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let mut env = environment("A");
    env.region = None;
    let p = provider(&temp.path().join("adc"), &doc(&server), env);
    let handler = thread::spawn(move || {
        for zone in ["private-marker/zone", "us-east-1a"] {
            imds(&server);
            metadata(
                &server,
                "/latest/meta-data/placement/availability-zone",
                "GET",
                true,
            )
            .respond(Response::from_string(zone))
            .unwrap();
        }
        access(sts(&server, "us-east-1", "A"), "access");
        quiet(&server);
    });
    assert!(
        !p.access_token(Utc::now())
            .unwrap_err()
            .to_string()
            .contains("private-marker")
    );
    assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "access");
    handler.join().unwrap();
}
#[test]
fn aws_snapshot_debug_redacts_keys_and_cancellation_stops_before_metadata() {
    use std::sync::atomic::AtomicUsize;
    let temp = tempfile::tempdir().unwrap();
    let server = server();
    let path = temp.path().join("adc");
    fs::write(&path, doc(&server).to_string()).unwrap();
    let aws = environment("PRIVATE");
    assert!(!format!("{aws:?}").contains("synthetic-PRIVATE"));
    assert!(!format!("{aws:?}").contains("AKIDPRIVATE"));
    let c = AccountContext {
        profile: ProfileId::new("aws").unwrap(),
        source: AccountSource::Flag,
        provider: AuthProvider::Adc,
        adc_file: Some(path),
        quota_project: None,
        adc_file_required: true,
        cancellation: Some(Arc::new(AtomicUsize::new(15))),
        metadata: Default::default(),
        allow_executables: false,
        aws,
        certificate: Default::default(),
    };
    assert!(matches!(
        AdcProvider::from_context(&c, TokenClient::new().unwrap()).access_token(Utc::now()),
        Err(colabcli_rust::Error::CliExit(143))
    ));
    quiet(&server);
}
