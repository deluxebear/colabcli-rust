//! Default public upstream client, fetched without committing client secrets.
use super::OAuthClient;
use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, time::Duration};

const SOURCE: &str = "https://raw.githubusercontent.com/googlecolab/google-colab-cli/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/oauth_config.json";
const SHA256: &str = "d3127103bea2884c9f8269d17bcfa74b21356837672bb5b46067cecfe219fc5d";
const MAX_BYTES: u64 = 16 * 1024;

impl OAuthClient {
    /// Explicit files fail closed. Only an absent implicit default uses upstream.
    /// The pending authorization stores the selected client for code exchange;
    /// finishing or refreshing never needs to download it again.
    pub fn resolve(explicit: Option<&Path>, home: Option<&Path>) -> Result<Self> {
        resolve_with(explicit, home, || download(SOURCE, SHA256))
    }
}

fn resolve_with(
    explicit: Option<&Path>,
    home: Option<&Path>,
    fallback: impl FnOnce() -> Result<OAuthClient>,
) -> Result<OAuthClient> {
    if let Some(path) = explicit {
        return OAuthClient::from_file(path);
    }
    if let Some(home) = home {
        let path = home.join(".colab-cli-oauth-config.json");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => return OAuthClient::from_file(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(crate::error::io(&path, error)),
        }
    }
    fallback()
}

fn download(source: &str, expected: &str) -> Result<OAuthClient> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| Error::OAuthTransport)?;
    let response = client.get(source).send().map_err(|_| {
        Error::OAuth("无法获取默认 OAuth 配置；请检查 GitHub 网络连接，或用 -c 指定已有客户端 JSON")
    })?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(Error::OAuth(
            "默认 OAuth 配置下载失败；可用 -c 指定客户端 JSON",
        ));
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::OAuth("默认 OAuth 配置读取失败"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Error::OAuth("默认 OAuth 配置超过大小限制"));
    }
    if format!("{:x}", Sha256::digest(&bytes)) != expected {
        return Err(Error::OAuth("默认 OAuth 配置 SHA-256 不匹配"));
    }
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| Error::OAuth("默认 OAuth 配置格式无效"))?;
    let config: OAuthClient = serde_json::from_value(raw["installed"].clone())
        .map_err(|_| Error::OAuth("默认 OAuth 配置格式无效"))?;
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Vec<u8> {
        serde_json::to_vec(&json!({"installed": {
            "client_id": "fixture-client", "client_secret": "fixture-secret",
            "auth_uri": "https://accounts.google.com/o/oauth2/auth",
            "token_uri": "https://oauth2.googleapis.com/token"
        }}))
        .unwrap()
    }

    #[test]
    fn selection_only_falls_back_for_absent_implicit_file() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join(".colab-cli-oauth-config.json");
        let missing = dir.path().join("explicit.json");
        let fallback = || Err(Error::OAuth("fallback invoked"));
        assert_eq!(
            resolve_with(None, Some(dir.path()), fallback)
                .unwrap_err()
                .to_string(),
            Error::OAuth("fallback invoked").to_string()
        );
        assert!(matches!(
            resolve_with(Some(&missing), Some(dir.path()), fallback),
            Err(Error::Io { .. })
        ));
        std::fs::write(&default, b"broken").unwrap();
        assert!(matches!(
            resolve_with(None, Some(dir.path()), fallback),
            Err(Error::InvalidData(_))
        ));
        std::fs::write(&default, fixture()).unwrap();
        assert_eq!(
            resolve_with(None, Some(dir.path()), fallback)
                .unwrap()
                .client_id,
            "fixture-client"
        );
        std::fs::write(&missing, fixture()).unwrap();
        std::fs::write(&default, b"broken").unwrap();
        assert!(resolve_with(Some(&missing), Some(dir.path()), fallback).is_ok());
        assert_eq!(
            resolve_with(None, None, fallback).unwrap_err().to_string(),
            Error::OAuth("fallback invoked").to_string()
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_default_symlink_does_not_silently_change_client() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("missing.json"),
            dir.path().join(".colab-cli-oauth-config.json"),
        )
        .unwrap();
        assert!(matches!(
            resolve_with(None, Some(dir.path()), || panic!("unexpected fallback")),
            Err(Error::Io { .. })
        ));
    }

    #[test]
    fn download_checks_hash_status_size_and_json_without_exposing_body() {
        let good = fixture();
        let hash = format!("{:x}", Sha256::digest(&good));
        for (status, bytes, expected, succeeds) in [
            (200, good.clone(), hash.clone(), true),
            (200, good.clone(), "0".repeat(64), false),
            (302, good.clone(), hash.clone(), false),
            (500, good, hash, false),
            (
                200,
                b"fixture-secret".to_vec(),
                format!("{:x}", Sha256::digest(b"fixture-secret")),
                false,
            ),
            (
                200,
                vec![b'x'; MAX_BYTES as usize + 1],
                "0".repeat(64),
                false,
            ),
        ] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}/config", server.server_addr());
            let worker = std::thread::spawn(move || {
                let request = server.recv().unwrap();
                assert_eq!(request.method(), &tiny_http::Method::Get);
                assert!(
                    request
                        .headers()
                        .iter()
                        .all(|h| !h.field.equiv("Authorization"))
                );
                request
                    .respond(tiny_http::Response::from_data(bytes).with_status_code(status))
                    .unwrap();
            });
            let result = download(&url, &expected);
            worker.join().unwrap();
            assert_eq!(result.is_ok(), succeeds);
            if let Err(error) = result {
                assert!(!error.to_string().contains("fixture-secret"));
            }
        }
    }
}
