//! Workforce authorized-user ADC uses STS refresh with HTTP Basic client authentication.
use super::{TokenClient, expiry, validate_endpoint};
use crate::{Error, Result, models::Secret};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::io::Read;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

#[derive(Deserialize)]
pub(super) struct Workforce {
    #[serde(default)]
    token: Secret,
    #[serde(default, deserialize_with = "crate::auth::deserialize_expiry")]
    expiry: Option<DateTime<Utc>>,
    refresh_token: Option<Secret>,
    client_id: Option<String>,
    client_secret: Option<Secret>,
    token_url: Option<String>,
    audience: Option<String>,
    universe_domain: Option<String>,
}
fn valid_token(token: &Secret) -> bool {
    !token.is_empty()
        && !token
            .expose()
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
}
impl Workforce {
    pub(super) fn load(value: Value) -> Result<Self> {
        let source: Self = serde_json::from_value(value)
            .map_err(|_| Error::OAuth("workforce user ADC 字段无效"))?;
        if source
            .universe_domain
            .as_deref()
            .is_some_and(|s| s != "googleapis.com")
        {
            return Err(Error::OAuth("workforce user ADC 暂不支持其他 universe"));
        }
        if let Some(audience) = &source.audience {
            if !super::external::audience(audience)? {
                return Err(Error::OAuth(
                    "workforce user audience 必须为 workforce provider",
                ));
            }
        }
        if let Some(raw) = &source.token_url {
            let uri = Url::parse(raw).map_err(|_| Error::OAuth("workforce token URL 无效"))?;
            validate_endpoint(&uri)?;
            let loopback = matches!(uri.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
            if uri.query().is_some()
                || uri.path() != "/v1/oauth/token"
                || (!loopback
                    && (uri.host_str() != Some("sts.googleapis.com")
                        || uri.port().is_some_and(|p| p != 443)))
            {
                return Err(Error::OAuth(
                    "workforce refresh 仅接受 Google STS oauth/token 或 loopback 端点",
                ));
            }
        }
        if source
            .client_id
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.contains(':') || s.chars().any(char::is_control))
            || (!source.token.is_empty() && !valid_token(&source.token))
        {
            return Err(Error::OAuth("workforce 客户端或缓存 token 无效"));
        }
        if !source.can_refresh() && !(valid_token(&source.token) && source.expiry.is_some()) {
            return Err(Error::OAuth("workforce ADC 缺少缓存 token 或完整刷新参数"));
        }
        Ok(source)
    }
    fn can_refresh(&self) -> bool {
        self.refresh_token.as_ref().is_some_and(|s| !s.is_empty())
            && self.client_id.as_ref().is_some_and(|s| !s.is_empty())
            && self.client_secret.as_ref().is_some_and(|s| !s.is_empty())
            && self.token_url.is_some()
    }
    pub(super) fn access_token(
        &mut self,
        client: &TokenClient,
        now: DateTime<Utc>,
        force: bool,
    ) -> Result<Secret> {
        if !force
            && valid_token(&self.token)
            && self
                .expiry
                .is_some_and(|t| t > now + chrono::Duration::seconds(60))
        {
            return Ok(self.token.clone());
        }
        if !self.can_refresh() {
            return Err(Error::NeedsAuth);
        }
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            (
                "refresh_token",
                self.refresh_token.as_ref().unwrap().expose().to_string(),
            ),
        ];
        let response = client
            .client
            .post(self.token_url.as_ref().unwrap())
            .basic_auth(
                self.client_id.as_ref().unwrap(),
                Some(self.client_secret.as_ref().unwrap().expose()),
            )
            .form(&form)
            .send();
        form[1].1.zeroize();
        let response = response.map_err(|_| Error::OAuthTransport)?;
        if !response.status().is_success() {
            return Err(Error::OAuthStatus(response.status().as_u16()));
        }
        let mut body = Zeroizing::new(Vec::new());
        response
            .take(65537)
            .read_to_end(&mut body)
            .map_err(|_| Error::OAuthTransport)?;
        if body.len() > 65536 {
            return Err(Error::OAuth("workforce token 响应过大"));
        }
        #[derive(Deserialize)]
        struct Response {
            access_token: Secret,
            expires_in: i64,
            refresh_token: Option<Secret>,
            token_type: Option<String>,
        }
        let response: Response =
            serde_json::from_slice(&body).map_err(|_| Error::OAuth("workforce token 响应无效"))?;
        if !valid_token(&response.access_token)
            || response.expires_in <= 0
            || response
                .refresh_token
                .as_ref()
                .is_some_and(Secret::is_empty)
            || response
                .token_type
                .as_deref()
                .is_some_and(|t| !t.eq_ignore_ascii_case("bearer"))
        {
            return Err(Error::OAuth("workforce token 响应字段无效"));
        }
        let expiry = expiry(now, Some(response.expires_in))?;
        // Commit rotation only after every field has been validated; failures preserve old state.
        self.token = response.access_token;
        self.expiry = expiry;
        if let Some(refresh) = response.refresh_token {
            self.refresh_token = Some(refresh);
        }
        Ok(self.token.clone())
    }
}
