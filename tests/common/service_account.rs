//! Ephemeral test-only keys. No private key or signed assertion is checked in.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use colabcli_rust::auth::PUBLIC_SCOPES;
use rsa::{
    Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey,
    pkcs8::{EncodePrivateKey, LineEnding},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::OnceLock};

fn keys() -> &'static [RsaPrivateKey; 2] {
    static KEYS: OnceLock<[RsaPrivateKey; 2]> = OnceLock::new();
    KEYS.get_or_init(|| {
        std::array::from_fn(|_| RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap())
    })
}
pub fn document(index: usize, email: &str, uri: &str) -> Value {
    let pem = keys()[index].to_pkcs8_pem(LineEnding::LF).unwrap();
    json!({"type":"service_account","client_email":email,"private_key":pem.as_str(),
        "private_key_id":format!("fixture-key-{index}"),"token_uri":uri,"universe_domain":"googleapis.com"})
}
pub fn write(path: &Path, index: usize, email: &str, uri: &str) -> Vec<u8> {
    let bytes = serde_json::to_vec(&document(index, email, uri)).unwrap();
    std::fs::write(path, &bytes).unwrap();
    bytes
}
pub fn verify(assertion: &str, index: usize, email: &str, now: DateTime<Utc>) {
    let parts: Vec<_> = assertion.split('.').collect();
    assert_eq!(parts.len(), 3);
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(
        header,
        json!({"alg":"RS256","typ":"JWT","kid":format!("fixture-key-{index}")})
    );
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(
        claims,
        json!({"iss":email,"scope":PUBLIC_SCOPES.join(" "),"aud":"https://oauth2.googleapis.com/token","iat":now.timestamp(),"exp":now.timestamp()+3600})
    );
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    let digest = Sha256::digest(format!("{}.{}", parts[0], parts[1]).as_bytes());
    RsaPublicKey::from(&keys()[index])
        .verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &signature)
        .unwrap();
    assert!(
        RsaPublicKey::from(&keys()[1 - index])
            .verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &signature)
            .is_err()
    );
}
pub fn verify_recent(assertion: &str, index: usize, email: &str) {
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(assertion.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    let issued = claims["iat"].as_i64().unwrap();
    assert!((Utc::now().timestamp() - issued).abs() <= 10);
    verify(
        assertion,
        index,
        email,
        DateTime::from_timestamp(issued, 0).unwrap(),
    );
}
