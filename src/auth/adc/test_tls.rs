use crate::auth::{AdcProvider, TokenClient};
use base64::{Engine, engine::general_purpose::STANDARD};
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::{
    RootCertStore, ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
pub(super) struct Fixture {
    pub(super) _directory: tempfile::TempDir,
    pub(super) ca: rcgen::Certificate,
    pub(super) ca_key: KeyPair,
    pub(super) listener: TcpListener,
    pub(super) server: Arc<ServerConfig>,
}
impl Fixture {
    pub(super) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_key = KeyPair::generate().unwrap();
        let ca = params.self_signed(&ca_key).unwrap();
        let mut params = CertificateParams::new(vec!["127.0.0.1".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &ca, &ca_key).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .unwrap();
        let server = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![cert.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            _directory: directory,
            ca,
            ca_key,
            listener,
            server: Arc::new(server),
        }
    }
    pub(super) fn path(&self, name: &str) -> PathBuf {
        self._directory.path().join(name)
    }
    pub(super) fn uri(&self) -> String {
        format!("https://{}", self.listener.local_addr().unwrap())
    }
    pub(super) fn client(&self) -> TokenClient {
        let root = reqwest::Certificate::from_der(self.ca.der()).unwrap();
        TokenClient {
            client: reqwest::blocking::Client::builder()
                .add_root_certificate(root.clone())
                .build()
                .unwrap(),
            extra_roots: vec![root],
        }
    }
    pub(super) fn leaf(&self) -> (String, String, Vec<u8>) {
        let mut params = CertificateParams::default();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.ca, &self.ca_key).unwrap();
        (
            encode_pem("CERTIFICATE", cert.der()),
            encode_pem("PRIVATE KEY", &key.serialize_der()),
            cert.der().to_vec(),
        )
    }
    pub(super) fn adc(
        &self,
        name: &str,
        bundle: &Path,
        impersonate: bool,
    ) -> (AdcProvider, PathBuf) {
        let config = self.path(&format!("{name}-cert.json"));
        fs::write(
            &config,
            json!({"cert_configs":{"workload":{"cert_path":bundle,"key_path":bundle}}}).to_string(),
        )
        .unwrap();
        let path = self.path(&format!("{name}-adc.json"));
        let mut doc = json!({"type":"external_account",
            "audience":"//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/pool/providers/provider",
            "subject_token_type":"urn:ietf:params:oauth:token-type:mtls",
            "token_url":format!("{}/v1/token",self.uri()),
            "credential_source":{"certificate":{"certificate_config_location":config}}});
        if impersonate {
            doc["service_account_impersonation_url"] = json!(format!(
                "{}/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken",
                self.uri()
            ));
        }
        fs::write(&path, doc.to_string()).unwrap();
        (AdcProvider::new(path.clone(), self.client()), path)
    }
}
pub(super) fn accept(listener: &TcpListener) -> std::net::TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return socket;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "missing TLS request");
                thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("accept failed: {e}"),
        }
    }
}
pub(super) fn request(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
) -> (
    StreamOwned<ServerConnection, std::net::TcpStream>,
    String,
    BTreeMap<String, String>,
    String,
) {
    let mut stream = StreamOwned::new(
        ServerConnection::new(config.clone()).unwrap(),
        accept(listener),
    );
    let mut reader = BufReader::new(&mut stream);
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.split_once(':').unwrap();
        headers.insert(key.to_ascii_lowercase(), value.trim().into());
    }
    let mut body = vec![
        0;
        headers
            .get("content-length")
            .map(|s| s.parse::<usize>().unwrap())
            .unwrap_or(0)
    ];
    reader.read_exact(&mut body).unwrap();
    drop(reader);
    (stream, first, headers, String::from_utf8(body).unwrap())
}
pub(super) fn reply(mut stream: StreamOwned<ServerConnection, std::net::TcpStream>, value: Value) {
    let body = value.to_string();
    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    stream.flush().unwrap();
}
pub(super) fn sts(token: &str) -> Value {
    json!({"access_token":token,"token_type":"Bearer",
    "issued_token_type":"urn:ietf:params:oauth:token-type:access_token","expires_in":3600})
}

pub(super) fn encode_pem(label: &str, bytes: &[u8]) -> String {
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        STANDARD.encode(bytes)
    )
}
