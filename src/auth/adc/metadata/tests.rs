use super::super::test_tls::*;
use super::*;
use crate::{
    auth::{AdcProvider, TokenClient},
    context::{AccountContext, AccountSource},
    models::{AuthProvider, ProfileId},
};
use rustls::ServerConnection;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    sync::Arc,
    thread,
};

fn options(f: &Fixture, name: &str, mode: &str) -> (MetadataOptions, Vec<u8>) {
    let root = f.path("mds-root.pem");
    fs::write(&root, encode_pem("CERTIFICATE", f.ca.der())).unwrap();
    let bundle = f.path(name);
    let (pem, key, der) = f.leaf();
    fs::write(&bundle, pem + &key).unwrap();
    (
        MetadataOptions {
            host: f.listener.local_addr().unwrap().to_string(),
            probe_host: f.listener.local_addr().unwrap().to_string(),
            disabled: false,
            timeout_seconds: 2,
            mtls: Box::new(MetadataMtlsOptions {
                mode: mode.into(),
                root_cert: root,
                client_bundle: bundle,
            }),
        },
        der,
    )
}
fn provider(f: &Fixture, name: &str, options: MetadataOptions) -> AdcProvider {
    let context = AccountContext {
        profile: ProfileId::new(name).unwrap(),
        source: AccountSource::Flag,
        provider: AuthProvider::Adc,
        adc_file: Some(f.path(&format!("missing-{name}"))),
        adc_file_required: false,
        quota_project: Some("must-not-leak".into()),
        cancellation: None,
        metadata: options,
        allow_executables: false,
        aws: Default::default(),
        certificate: Default::default(),
    };
    AdcProvider::from_context(&context, TokenClient::new().unwrap())
}
fn response(mut socket: impl Write, status: u16, value: &str, flavor: bool) {
    write!(
        socket,
        "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{value}",
        value.len(),
        if flavor {
            "Metadata-Flavor: Google\r\n"
        } else {
            ""
        }
    )
    .unwrap();
    socket.flush().unwrap();
}
fn plain_request(socket: &mut std::net::TcpStream) -> (String, BTreeMap<String, String>) {
    let mut reader = BufReader::new(socket);
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.to_ascii_lowercase(), value.trim().into());
    }
    (first, headers)
}
fn check_headers(headers: &BTreeMap<String, String>) {
    assert_eq!(headers["metadata-flavor"], "Google");
    assert!(!headers.contains_key("authorization"));
    assert!(!headers.contains_key("x-goog-user-project"));
}
fn access(token: &str) -> String {
    json!({"access_token":token,"token_type":"Bearer","expires_in":3600}).to_string()
}
fn transport(options: &MetadataOptions, mode: MtlsMode) -> Metadata {
    Metadata {
        client: builder().build().unwrap(),
        token_uri: origin(&options.host).unwrap(),
        tls_token_uri: https_uri(&origin(&options.host).unwrap(), &options.host).unwrap(),
        cached: None,
        mtls: options.mtls.clone(),
        mode,
    }
}

#[test]
fn metadata_mtls_full_provider_rotates_certificates_and_isolates_concurrent_accounts() {
    let f = Fixture::new();
    let (a, ader) = options(&f, "a", "default");
    let (b, bder) = options(&f, "b", "default");
    let bundle = a.mtls.client_bundle.clone();
    let pa = Arc::new(provider(&f, "a", a));
    let pb = Arc::new(provider(&f, "b", b));
    let (pem, key, rotated) = f.leaf();
    let expected = BTreeMap::from([(ader, "a"), (bder, "b"), (rotated, "rotated")]);
    let listener = f.listener.try_clone().unwrap();
    let config = f.server.clone();
    let handler = thread::spawn(move || {
        for _ in 0..5 {
            let (stream, path, headers, body) = request(&listener, &config);
            check_headers(&headers);
            assert!(body.is_empty());
            let peer = stream.conn.peer_certificates().unwrap()[0].to_vec();
            let id = expected[&peer];
            if path.starts_with("GET / HTTP/") {
                response(stream, 200, "{}", true);
            } else {
                let raw = path.split_whitespace().nth(1).unwrap();
                let u = Url::parse(&format!("http://fixture{raw}")).unwrap();
                assert_eq!(
                    u.path(),
                    "/computeMetadata/v1/instance/service-accounts/default/token"
                );
                assert_eq!(
                    u.query_pairs().collect::<Vec<_>>(),
                    vec![("scopes".into(), PUBLIC_SCOPES.join(",").into())]
                );
                response(stream, 200, &access(id), true);
            }
        }
    });
    let workers: Vec<_> = (0..12)
        .map(|i| {
            let p = if i % 2 == 0 { pa.clone() } else { pb.clone() };
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
    let next = f.path("next");
    fs::write(&next, pem + &key).unwrap();
    fs::rename(&next, &bundle).unwrap();
    assert_eq!(
        pa.refresh_access_token(Utc::now()).unwrap().expose(),
        "rotated"
    );
    fs::write(bundle, "PRIVATE_BAD_BUNDLE").unwrap();
    assert!(
        !pa.refresh_access_token(Utc::now())
            .unwrap_err()
            .to_string()
            .contains("PRIVATE_BAD_BUNDLE")
    );
    assert_eq!(pa.access_token(Utc::now()).unwrap().expose(), "rotated");
    assert_eq!(pb.access_token(Utc::now()).unwrap().expose(), "b");
    handler.join().unwrap();
    assert!(f.listener.accept().is_err());
}

#[test]
fn strict_metadata_policy_rejects_invalid_hosts_and_missing_material_without_network() {
    let f = Fixture::new();
    let (mut opts, _) = options(&f, "bundle", "STRICT");
    assert!(opts.mtls.mode().unwrap() == MtlsMode::Strict);
    assert!(Metadata::discover(&opts).is_err());
    opts.host = "metadata.google.internal".into();
    fs::remove_file(&opts.mtls.client_bundle).unwrap();
    assert!(Metadata::discover(&opts).is_err());
    opts.mtls.mode = "invalid".into();
    assert!(Metadata::discover(&opts).is_err());
    opts.disabled = true;
    assert!(matches!(Metadata::discover(&opts), Err(Error::NeedsAuth)));
    assert!(f.listener.accept().is_err());
}

#[test]
fn strict_metadata_transport_does_not_fall_back_on_status_or_tls_failure() {
    let f = Fixture::new();
    let (opts, _) = options(&f, "bundle", "strict");
    let listener = f.listener.try_clone().unwrap();
    let config = f.server.clone();
    let handler = thread::spawn(move || {
        let (stream, _, headers, _) = request(&listener, &config);
        check_headers(&headers);
        response(stream, 503, "private-body", true);
        let mut socket = accept(&listener);
        let mut tls = ServerConnection::new(config).unwrap();
        assert!(tls.complete_io(&mut socket).is_err());
    });
    let t = transport(&opts, MtlsMode::Strict);
    let uri = origin(&opts.host).unwrap();
    assert_eq!(
        t.get(
            uri.clone(),
            https_uri(&uri, &opts.host).unwrap(),
            Duration::from_secs(2)
        )
        .unwrap()
        .status()
        .as_u16(),
        503
    );
    let wrong = Fixture::new();
    fs::write(
        &opts.mtls.root_cert,
        encode_pem("CERTIFICATE", wrong.ca.der()),
    )
    .unwrap();
    assert!(
        t.get(
            uri.clone(),
            https_uri(&uri, &opts.host).unwrap(),
            Duration::from_secs(2)
        )
        .is_err()
    );
    handler.join().unwrap();
    assert!(f.listener.accept().is_err());
}

#[test]
fn default_metadata_falls_back_to_http_after_tls_failure_with_no_identity_headers() {
    let f = Fixture::new();
    let (opts, _) = options(&f, "bundle", "default");
    let wrong = Fixture::new();
    fs::write(
        &opts.mtls.root_cert,
        encode_pem("CERTIFICATE", wrong.ca.der()),
    )
    .unwrap();
    let listener = f.listener.try_clone().unwrap();
    let config = f.server.clone();
    let handler = thread::spawn(move || {
        for i in 0..2 {
            let mut socket = accept(&listener);
            let mut tls = ServerConnection::new(config.clone()).unwrap();
            assert!(tls.complete_io(&mut socket).is_err());
            let mut socket = accept(&listener);
            let (path, headers) = plain_request(&mut socket);
            check_headers(&headers);
            if i == 0 {
                assert!(path.starts_with("GET / HTTP/"));
                response(socket, 200, "{}", true);
            } else {
                assert!(path.contains("/default/token?"));
                response(socket, 200, &access("http-fallback"), true);
            }
        }
    });
    let p = provider(&f, "fallback", opts);
    assert_eq!(
        p.access_token(Utc::now()).unwrap().expose(),
        "http-fallback"
    );
    handler.join().unwrap();
}

#[test]
fn default_metadata_retries_http_after_tls_http_error_but_not_forged_flavor() {
    let f = Fixture::new();
    let (opts, _) = options(&f, "bundle", "default");
    let listener = f.listener.try_clone().unwrap();
    let config = f.server.clone();
    let handler = thread::spawn(move || {
        let (stream, _, headers, _) = request(&listener, &config);
        check_headers(&headers);
        response(stream, 503, "tls-error", true);
        let mut socket = accept(&listener);
        let (_, headers) = plain_request(&mut socket);
        check_headers(&headers);
        response(socket, 200, "{}", true);
        let (stream, _, _, _) = request(&listener, &config);
        response(stream, 200, "{}", false);
    });
    let mut metadata = Metadata::discover(&opts).unwrap();
    assert!(metadata.access_token(Utc::now(), false).is_err());
    handler.join().unwrap();
    assert!(f.listener.accept().is_err());
}

#[test]
fn none_ignores_invalid_material_and_default_without_material_uses_http() {
    for mode in ["none", "default"] {
        let f = Fixture::new();
        let (opts, _) = options(&f, "bundle", mode);
        fs::write(&opts.mtls.client_bundle, "PRIVATE_INVALID").unwrap();
        if mode == "default" {
            fs::remove_file(&opts.mtls.root_cert).unwrap();
        } else {
            fs::write(&opts.mtls.root_cert, "PRIVATE_INVALID").unwrap();
        }
        let listener = f.listener.try_clone().unwrap();
        let handler = thread::spawn(move || {
            for i in 0..2 {
                let mut socket = accept(&listener);
                let (_, headers) = plain_request(&mut socket);
                check_headers(&headers);
                response(
                    socket,
                    200,
                    if i == 0 {
                        "{}"
                    } else {
                        r#"{"access_token":"plain","token_type":"Bearer","expires_in":3600}"#
                    },
                    true,
                );
            }
        });
        let p = provider(&f, "plain", opts);
        assert_eq!(p.access_token(Utc::now()).unwrap().expose(), "plain");
        handler.join().unwrap();
    }
}

#[test]
fn metadata_tls_preserves_explicit_default_http_port_path_and_query() {
    for (authority, port) in [
        ("metadata.google.internal:80", 80),
        ("127.0.0.1:80", 80),
        ("[::1]:80", 80),
        ("metadata.google.internal", 443),
        ("metadata.google.internal:443", 443),
    ] {
        let mut http = origin(authority).unwrap();
        http.set_path("/computeMetadata/v1/instance/service-accounts/default/token");
        http.set_query(Some("scopes=fixture"));
        let tls = https_uri(&http, authority).unwrap();
        assert_eq!(tls.scheme(), "https");
        assert_eq!(tls.port_or_known_default(), Some(port));
        assert_eq!(tls.path(), http.path());
        assert_eq!(tls.query(), http.query());
    }
}
