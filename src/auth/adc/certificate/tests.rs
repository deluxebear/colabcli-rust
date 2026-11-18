use super::super::test_tls::*;
use super::*;
use crate::auth::{AdcProvider, TokenClient};
use chrono::{Duration as Delta, Utc};
use rcgen::KeyPair;
use rustls::ServerConnection;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, thread};

#[test]
fn x509_mtls_rotation_concurrency_cache_and_failed_rotation_preserve_identity_boundaries() {
    let f = Fixture::new();
    let mut providers = Vec::new();
    let mut expected = BTreeMap::new();
    let mut paths = Vec::new();
    for id in ["a", "b"] {
        let (pem, key, der) = f.leaf();
        let bundle = f.path(id);
        fs::write(&bundle, pem + &key).unwrap();
        let (p, path) = f.adc(id, &bundle, false);
        providers.push(Arc::new(p));
        paths.push((path.clone(), fs::read(path).unwrap()));
        expected.insert(der, id.to_owned());
    }
    let (pem, key, rotated) = f.leaf();
    expected.insert(rotated, "rotated".into());
    let listener = f.listener.try_clone().unwrap();
    let config = f.server.clone();
    let handler = thread::spawn(move || {
        for _ in 0..3 {
            let (stream, first, headers, body) = request(&listener, &config);
            assert!(first.starts_with("POST /v1/token "));
            assert!(!headers.contains_key("authorization"));
            let peer = stream.conn.peer_certificates().unwrap()[0].to_vec();
            let id = &expected[&peer];
            let form: BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect();
            assert_eq!(
                form["subject_token_type"],
                "urn:ietf:params:oauth:token-type:mtls"
            );
            assert_eq!(
                serde_json::from_str::<Vec<String>>(&form["subject_token"]).unwrap(),
                vec![STANDARD.encode(&peer)]
            );
            reply(stream, sts(id));
        }
    });
    let workers: Vec<_> = (0..12)
        .map(|i| {
            let p = providers[i % 2].clone();
            thread::spawn(move || {
                assert_eq!(
                    p.access_token(Utc::now()).unwrap().expose(),
                    if i % 2 == 0 { "a" } else { "b" }
                )
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let temporary = f.path("rotation");
    fs::write(&temporary, pem + &key).unwrap();
    fs::rename(temporary, f.path("a")).unwrap();
    assert_eq!(
        providers[0]
            .refresh_access_token(Utc::now())
            .unwrap()
            .expose(),
        "rotated"
    );
    fs::write(f.path("a"), "PRIVATE_INVALID_KEY").unwrap();
    assert!(
        !providers[0]
            .refresh_access_token(Utc::now())
            .unwrap_err()
            .to_string()
            .contains("PRIVATE_INVALID_KEY")
    );
    assert_eq!(
        providers[0].access_token(Utc::now()).unwrap().expose(),
        "rotated"
    );
    assert_eq!(providers[1].access_token(Utc::now()).unwrap().expose(), "b");
    for (path, original) in paths {
        assert_eq!(fs::read(path).unwrap(), original);
    }
    handler.join().unwrap();
}

#[test]
fn x509_iam_uses_mtls_and_only_sts_bearer_and_reuses_valid_sts() {
    let f = Fixture::new();
    let (pem, key, der) = f.leaf();
    let bundle = f.path("bundle");
    fs::write(&bundle, pem + &key).unwrap();
    let (p, _) = f.adc("target", &bundle, true);
    let listener = f.listener.try_clone().unwrap();
    let config = f.server.clone();
    let now = Utc::now();
    let handler = thread::spawn(move || {
        for i in 0..3 {
            let (stream, first, headers, body) = request(&listener, &config);
            assert_eq!(stream.conn.peer_certificates().unwrap()[0].as_ref(), der);
            if i == 0 {
                assert!(first.contains("/v1/token "));
                assert!(!headers.contains_key("authorization"));
                reply(stream, sts("sts-only"));
            } else {
                assert!(first.contains(":generateAccessToken "));
                assert_eq!(headers["authorization"], "Bearer sts-only");
                assert_eq!(
                    serde_json::from_str::<Value>(&body).unwrap()["scope"],
                    json!(crate::auth::PUBLIC_SCOPES)
                );
                reply(
                    stream,
                    json!({"accessToken":format!("target-{i}"),"expireTime":(now+Delta::hours(1)).to_rfc3339()}),
                );
            }
        }
    });
    assert_eq!(p.access_token(now).unwrap().expose(), "target-1");
    assert_eq!(p.refresh_access_token(now).unwrap().expose(), "target-2");
    handler.join().unwrap();
}

#[test]
fn certificate_config_chain_validation_and_default_snapshot_are_bounded_and_redacted() {
    let f = Fixture::new();
    let (pem, key, der) = f.leaf();
    let bundle = f.path("bundle");
    fs::write(&bundle, pem.clone() + &key).unwrap();
    let config = f.path("certificate_config.json");
    fs::write(
        &config,
        json!({"cert_configs":{"workload":{"cert_path":bundle,"key_path":bundle}}}).to_string(),
    )
    .unwrap();
    let options = CertificateOptions {
        default_config: Some(config.clone()),
        fallback_config: None,
        home: Some(f._directory.path().into()),
    };
    let source = json!({"use_default_certificate_config":true});
    let c = Certificate::load(source.clone(), &options).unwrap();
    assert_eq!(c.config, config);
    let explicit = Certificate::load(
        json!({"certificate_config_location":"~/certificate_config.json"}),
        &options,
    )
    .unwrap();
    assert_eq!(explicit.config, c.config);
    let trust = f.path("trust");
    for contents in [
        encode_pem("CERTIFICATE", f.ca.der()),
        pem.clone() + &encode_pem("CERTIFICATE", f.ca.der()),
    ] {
        fs::write(&trust, contents).unwrap();
        let c = Certificate::load(
            json!({"certificate_config_location":config,"trust_chain_path":trust}),
            &options,
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<String>>(c.material().unwrap().subject.expose()).unwrap(),
            vec![STANDARD.encode(&der), STANDARD.encode(f.ca.der())]
        );
    }
    fs::write(&trust, encode_pem("CERTIFICATE", f.ca.der()) + &pem).unwrap();
    assert!(
        Certificate::load(
            json!({"certificate_config_location":config,"trust_chain_path":trust}),
            &options
        )
        .unwrap()
        .material()
        .is_err()
    );
    for value in [
        json!({}),
        json!({"use_default_certificate_config":true,"certificate_config_location":config}),
        json!({"use_default_certificate_config":"true"}),
    ] {
        assert!(Certificate::load(value, &options).is_err());
    }
    let different = KeyPair::generate().unwrap();
    fs::write(
        &bundle,
        pem + &encode_pem("PRIVATE KEY", &different.serialize_der()),
    )
    .unwrap();
    assert!(c.material().is_err());
    fs::write(&config, "PRIVATE_CONFIG".repeat(100000)).unwrap();
    assert!(
        !c.material()
            .err()
            .unwrap()
            .to_string()
            .contains("PRIVATE_CONFIG")
    );
}

#[test]
fn certificate_mtls_never_accepts_an_untrusted_server_or_downgrades_to_http() {
    let f = Fixture::new();
    let (pem, key, _) = f.leaf();
    let bundle = f.path("bundle");
    fs::write(&bundle, pem + &key).unwrap();
    let (_, path) = f.adc("untrusted", &bundle, false);
    let p = AdcProvider::new(path, TokenClient::new().unwrap());
    let listener = f.listener.try_clone().unwrap();
    let config = f.server.clone();
    let handler = thread::spawn(move || {
        let mut connection = ServerConnection::new(config).unwrap();
        let mut socket = accept(&listener);
        assert!(connection.complete_io(&mut socket).is_err());
    });
    assert!(p.access_token(Utc::now()).is_err());
    handler.join().unwrap();
    assert!(matches!(f.listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
}

#[test]
fn certificate_source_rejects_http_mixed_sources_and_wrong_type_before_network() {
    let f = Fixture::new();
    let (pem, key, _) = f.leaf();
    let bundle = f.path("bundle");
    fs::write(&bundle, pem + &key).unwrap();
    let (_, path) = f.adc("validation", &bundle, false);
    let valid: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut cases = Vec::new();
    let mut http = valid.clone();
    http["token_url"] = json!(format!("{}/v1/token", f.uri().replace("https:", "http:")));
    cases.push(http);
    let mut jwt = valid.clone();
    jwt["subject_token_type"] = json!("urn:ietf:params:oauth:token-type:jwt");
    cases.push(jwt);
    let mut no_cert = valid.clone();
    no_cert["credential_source"] = json!({"file":bundle});
    cases.push(no_cert);
    let mut workforce = valid.clone();
    workforce["audience"] =
        json!("//iam.googleapis.com/locations/global/workforcePools/pool/providers/provider");
    cases.push(workforce);
    for key in ["file", "url", "executable", "environment_id"] {
        let mut mixed = valid.clone();
        mixed["credential_source"][key] = json!("PRIVATE_NOT_READ");
        cases.push(mixed);
    }
    for value in cases {
        fs::write(&path, value.to_string()).unwrap();
        let p = AdcProvider::new(path.clone(), f.client());
        assert!(
            !p.access_token(Utc::now())
                .unwrap_err()
                .to_string()
                .contains("PRIVATE_NOT_READ")
        );
    }
    assert!(matches!(f.listener.accept(), Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
}

#[test]
fn default_certificate_selection_is_snapshotted_and_reaches_account_provider() {
    use crate::{
        context::{AccountContext, AccountSource},
        models::{AuthProvider, ProfileId},
    };
    let f = Fixture::new();
    let (pem, key, _) = f.leaf();
    let bundle = f.path("bundle");
    fs::write(&bundle, pem + &key).unwrap();
    let (_, path) = f.adc("default", &bundle, false);
    let mut doc: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let selected: PathBuf = serde_json::from_value(
        doc["credential_source"]["certificate"]["certificate_config_location"].clone(),
    )
    .unwrap();
    doc["credential_source"]["certificate"] = json!({"use_default_certificate_config":true});
    fs::write(&path, doc.to_string()).unwrap();
    let mut values = BTreeMap::from([
        ("HOME", f.path("home")),
        ("CLOUDSDK_CONFIG", f.path("gcloud")),
        ("GOOGLE_API_CERTIFICATE_CONFIG", selected.clone()),
        (
            "CLOUDSDK_CONTEXT_AWARE_CERTIFICATE_CONFIG_FILE_PATH",
            f.path("context"),
        ),
    ]);
    let options = CertificateOptions::from_values(|key| values.get(key).cloned());
    assert_eq!(options.default_config, Some(selected));
    assert_eq!(
        options.fallback_config,
        Some(f.path("gcloud/certificate_config.json"))
    );
    values.remove("GOOGLE_API_CERTIFICATE_CONFIG");
    assert_eq!(
        CertificateOptions::from_values(|key| values.get(key).cloned()).default_config,
        Some(f.path("context"))
    );
    values.remove("CLOUDSDK_CONTEXT_AWARE_CERTIFICATE_CONFIG_FILE_PATH");
    assert_eq!(
        CertificateOptions::from_values(|key| values.get(key).cloned()).default_config,
        Some(f.path("gcloud/certificate_config.json"))
    );
    let context = AccountContext {
        profile: ProfileId::new("certificate").unwrap(),
        source: AccountSource::Flag,
        provider: AuthProvider::Adc,
        adc_file: Some(path),
        quota_project: None,
        adc_file_required: true,
        cancellation: None,
        metadata: Default::default(),
        allow_executables: false,
        aws: Default::default(),
        certificate: options,
    };
    let p = AdcProvider::from_context(&context, f.client());
    let listener = f.listener.try_clone().unwrap();
    let config = f.server.clone();
    let handler = thread::spawn(move || {
        let (stream, _, _, _) = request(&listener, &config);
        reply(stream, sts("default-selected"));
    });
    assert_eq!(
        p.access_token(Utc::now()).unwrap().expose(),
        "default-selected"
    );
    handler.join().unwrap();
}

#[test]
fn default_context_config_fallback_and_projected_bundle_aliases_preserve_one_key() {
    let f = Fixture::new();
    let (pem, key, _) = f.leaf();
    let bundle = f.path("bundle");
    let alias = f.path("alias");
    fs::write(&bundle, pem + &key).unwrap();
    fs::hard_link(&bundle, &alias).unwrap();
    let config = f.path("workload.json");
    fs::write(
        &config,
        json!({"cert_configs":{"workload":{"cert_path":bundle,"key_path":alias}}}).to_string(),
    )
    .unwrap();
    let context = f.path("context.json");
    fs::write(&context, json!({"cert_configs":{"other":{}}}).to_string()).unwrap();
    let options = CertificateOptions {
        default_config: Some(context.clone()),
        fallback_config: Some(config.clone()),
        home: None,
    };
    assert!(
        Certificate::load(json!({"use_default_certificate_config":true}), &options)
            .unwrap()
            .material()
            .is_ok()
    );
    assert!(
        Certificate::load(json!({"certificate_config_location":context}), &options)
            .unwrap()
            .material()
            .is_err()
    );
    let c = Certificate::load(json!({"certificate_config_location":config}), &options).unwrap();
    fs::remove_file(&bundle).unwrap();
    fs::create_dir(&bundle).unwrap();
    assert!(c.material().is_err());
    #[cfg(unix)]
    {
        fs::remove_dir(&bundle).unwrap();
        let name = std::ffi::CString::new(bundle.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(c.material().is_err());
    }
}
