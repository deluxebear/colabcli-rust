//! URL and identity protocol boundaries shared by the native utility commands.
use crate::{Error, Result, auth::validate_endpoint, models::Secret};
use serde::{Deserialize, Serialize};
use std::{io::Read, time::Duration};
use url::Url;
fn quote(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}
pub fn connection_url(host: &str, endpoint: &str, email: Option<&str>) -> Result<String> {
    if host.chars().any(char::is_control) {
        return Err(Error::OAuth("浏览器 host 必须为 HTTP(S) origin"));
    }
    let host = Url::parse(host).map_err(|_| Error::OAuth("浏览器 host 无效"))?;
    if !matches!(host.scheme(), "https" | "http")
        || host.host_str().is_none()
        || !host.username().is_empty()
        || host.password().is_some()
        || host.query().is_some()
        || host.fragment().is_some()
        || !matches!(host.path(), "" | "/")
    {
        return Err(Error::OAuth(
            "浏览器 host 必须为不带认证、路径、query 或 fragment 的 HTTP(S) origin",
        ));
    }
    if endpoint.is_empty()
        || matches!(endpoint, "." | "..")
        || endpoint.len() > 1024
        || endpoint.chars().any(char::is_control)
    {
        return Err(Error::KernelProtocol("会话 endpoint 无效"));
    }
    let host = host.origin().ascii_serialization();
    let backend = format!("/tun/m/{}", quote(endpoint));
    let authuser = match email.filter(|s| !s.is_empty()) {
        Some(email) if email.len() <= 1024 && !email.chars().any(char::is_control) => {
            format!("authuser={}&", quote(email))
        }
        Some(_) => return Err(Error::OAuth("账户 email 无效")),
        None => String::new(),
    };
    Ok(format!(
        "{host}/notebooks/empty.ipynb?{authuser}dbu={}#datalabBackendUrl={host}{backend}",
        quote(&backend)
    ))
}
#[derive(Debug, Serialize)]
pub struct TokenIdentity {
    pub email: Option<String>,
    pub audience: Option<String>,
    pub expires_in: Option<u64>,
    pub scopes: Vec<String>,
}
#[derive(Deserialize)]
struct TokenInfo {
    email: Option<String>,
    audience: Option<String>,
    aud: Option<String>,
    expires_in: Option<serde_json::Value>,
    scope: Option<String>,
}
fn safe_field(value: &str, token: &Secret) -> bool {
    value.len() <= 8192 && !value.chars().any(char::is_control) && !value.contains(token.expose())
}
/// No redirects or bearer headers: tokeninfo accepts only the query parameter.
/// All transport/status/protocol errors are static and never contain its URL/body.
pub fn token_identity(endpoint: &Url, token: &Secret, timeout: Duration) -> Result<TokenIdentity> {
    validate_endpoint(endpoint)?;
    if endpoint.query().is_some() || token.is_empty() {
        return Err(Error::OAuth("tokeninfo 参数无效"));
    }
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|_| Error::OAuthTransport)?;
    let response = client
        .get(endpoint.clone())
        .query(&[("access_token", token.expose())])
        .send()
        .map_err(|_| Error::OAuthTransport)?;
    if !response.status().is_success() {
        return Err(Error::OAuthStatus(response.status().as_u16()));
    }
    let mut raw = zeroize::Zeroizing::new(Vec::new());
    response
        .take(65537)
        .read_to_end(&mut raw)
        .map_err(|_| Error::OAuthTransport)?;
    if raw.len() > 65536 {
        return Err(Error::OAuth("tokeninfo 响应过大"));
    }
    let info: TokenInfo =
        serde_json::from_slice(&raw).map_err(|_| Error::OAuth("tokeninfo 响应无效"))?;
    let audience = info.audience.filter(|s| !s.is_empty()).or(info.aud);
    if info
        .email
        .iter()
        .chain(audience.iter())
        .any(|value| !safe_field(value, token))
    {
        return Err(Error::OAuth("tokeninfo 身份字段无效"));
    }
    let scope = info.scope.unwrap_or_default();
    if !safe_field(&scope.replace(['\r', '\n', '\t'], " "), token) || scope.len() > 8192 {
        return Err(Error::OAuth("tokeninfo scope 无效"));
    }
    let mut scopes: Vec<String> = scope.split_whitespace().map(str::to_owned).collect();
    if scopes.len() > 64
        || scopes
            .iter()
            .any(|s| s.len() > 2048 || s.chars().any(char::is_control))
    {
        return Err(Error::OAuth("tokeninfo scope 无效"));
    }
    scopes.sort();
    let expires_in = info
        .expires_in
        .map(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
                .ok_or(Error::OAuth("tokeninfo 到期字段无效"))
        })
        .transpose()?;
    Ok(TokenIdentity {
        email: info.email.filter(|s| !s.is_empty()),
        audience: audience.filter(|s| !s.is_empty()),
        expires_in,
        scopes,
    })
}
