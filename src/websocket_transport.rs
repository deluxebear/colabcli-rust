//! Shared TCP, HTTP/HTTPS CONNECT and SOCKS5 setup. Endpoint TLS and WS authentication remain with
//! tungstenite; proxy credentials are never inserted into the endpoint request.
mod socks;
mod stream;
use crate::{Error, Result, models::Secret};
use base64::{Engine, engine::general_purpose::STANDARD};
use hyper_util::client::proxy::matcher::Matcher;
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};
pub use stream::Stream;
use url::Url;

#[derive(Clone, Default)]
pub struct ProxyEnvironment {
    http: Secret,
    https: Secret,
    all: Secret,
    no: Secret,
    cgi: bool,
    proxy_roots: Vec<rustls::pki_types::CertificateDer<'static>>,
}
impl ProxyEnvironment {
    pub fn capture() -> Self {
        let mut snapshot = Self::from_values(|name| std::env::var(name).ok());
        snapshot.cgi = std::env::var_os("REQUEST_METHOD").is_some();
        snapshot
    }
    /// Explicit immutable snapshot, also usable without mutating process environment.
    pub fn from_values(get: impl Fn(&str) -> Option<String>) -> Self {
        let value =
            |upper, lower| Secret::new(get(upper).or_else(|| get(lower)).unwrap_or_default());
        Self {
            http: value("HTTP_PROXY", "http_proxy"),
            https: value("HTTPS_PROXY", "https_proxy"),
            all: value("ALL_PROXY", "all_proxy"),
            no: value("NO_PROXY", "no_proxy"),
            cgi: get("REQUEST_METHOD").is_some(),
            proxy_roots: Vec::new(),
        }
    }
    /// Add one explicitly trusted DER CA for HTTPS proxies. Endpoint trust is
    /// independent and unchanged. No certificate or hostname check is disabled.
    pub fn with_proxy_root_certificate(mut self, der: Vec<u8>) -> Result<Self> {
        let certificate = rustls::pki_types::CertificateDer::from(der);
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(certificate.clone())
            .map_err(|_| Error::WebSocketProxy("代理 CA 证书无效"))?;
        self.proxy_roots.push(certificate);
        Ok(self)
    }
    fn selected(&self, destination: &Url) -> Result<Option<Url>> {
        if self.cgi {
            return Ok(None);
        }
        let scheme = match destination.scheme() {
            "ws" | "http" => "http",
            "wss" | "https" => "https",
            _ => return Err(Error::WebSocketProxy("目标协议无效")),
        };
        let host = destination
            .host_str()
            .ok_or(Error::WebSocketProxy("目标缺少主机名"))?;
        let port = destination
            .port_or_known_default()
            .ok_or(Error::WebSocketProxy("目标缺少端口"))?;
        // Reuse the same matching engine as the locked reqwest dependency. The
        // sentinel contains no credentials or runtime path/query.
        let uri = format!("{scheme}://{host}:{port}/")
            .parse::<tungstenite::http::Uri>()
            .map_err(|_| Error::WebSocketProxy("目标 authority 无效"))?;
        let matcher = Matcher::builder()
            .all("http://proxy.invalid:1")
            .no(self.no.expose())
            .build();
        if matcher.intercept(&uri).is_none() {
            return Ok(None);
        }
        let specific = if scheme == "https" {
            &self.https
        } else {
            &self.http
        };
        let raw = if specific.expose().trim().is_empty() {
            self.all.expose().trim()
        } else {
            specific.expose().trim()
        };
        if raw.is_empty() {
            return Ok(None);
        }
        if raw.len() > 8192 {
            return Err(Error::WebSocketProxy("代理地址过长"));
        }
        let raw = if raw.contains("://") {
            raw.to_owned()
        } else {
            format!("http://{raw}")
        };
        let proxy = Url::parse(&raw).map_err(|_| Error::WebSocketProxy("代理地址无效"))?;
        if !matches!(proxy.scheme(), "http" | "https" | "socks5" | "socks5h") {
            return Err(Error::WebSocketProxy(
                "仅支持 http://、https://、socks5:// 或 socks5h:// 代理",
            ));
        }
        if proxy.host_str().is_none()
            || proxy.query().is_some()
            || proxy.fragment().is_some()
            || !matches!(proxy.path(), "" | "/")
        {
            return Err(Error::WebSocketProxy(
                "代理地址不能包含路径、query 或 fragment",
            ));
        }
        Ok(Some(proxy))
    }
    pub fn uses_proxy(&self, destination: &Url) -> Result<bool> {
        Ok(self.selected(destination)?.is_some())
    }
}
fn left(deadline: Instant) -> Result<Duration> {
    let duration = deadline.saturating_duration_since(Instant::now());
    if duration.is_zero() {
        Err(Error::WebSocketTransport)
    } else {
        Ok(duration)
    }
}
fn tcp(host: &str, port: u16, deadline: Instant) -> Result<TcpStream> {
    let host = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|_| Error::WebSocketTransport)?;
    for address in addresses {
        let timeout = left(deadline)?.min(Duration::from_secs(5));
        if let Ok(stream) = TcpStream::connect_timeout(&address, timeout) {
            return Ok(stream);
        }
    }
    Err(Error::WebSocketTransport)
}
fn tunnel(stream: &mut Stream, destination: &Url, proxy: &Url, deadline: Instant) -> Result<()> {
    let authority = format!(
        "{}:{}",
        destination
            .host_str()
            .ok_or(Error::WebSocketProxy("目标缺少主机名"))?,
        destination
            .port_or_known_default()
            .ok_or(Error::WebSocketProxy("目标缺少端口"))?
    );
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if !proxy.username().is_empty() || proxy.password().is_some() {
        let user = percent_encoding::percent_decode_str(proxy.username())
            .decode_utf8()
            .map_err(|_| Error::WebSocketProxy("代理用户名编码无效"))?;
        let password = percent_encoding::percent_decode_str(proxy.password().unwrap_or(""))
            .decode_utf8()
            .map_err(|_| Error::WebSocketProxy("代理密码编码无效"))?;
        if user.contains(':') {
            return Err(Error::WebSocketProxy("Basic 用户名不能包含冒号"));
        }
        request.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            STANDARD.encode(format!("{user}:{password}"))
        ));
    }
    request.push_str("\r\n");
    let mut offset = 0;
    while offset < request.len() {
        stream
            .set_write_timeout(Some(left(deadline)?))
            .map_err(|_| Error::WebSocketTransport)?;
        let count = stream
            .write(&request.as_bytes()[offset..])
            .map_err(|_| Error::WebSocketTransport)?;
        if count == 0 {
            return Err(Error::WebSocketTransport);
        }
        offset += count;
    }
    stream.flush().map_err(|_| Error::WebSocketTransport)?;
    let mut total = 0;
    loop {
        let mut header = Vec::new();
        loop {
            if total >= 16384 {
                return Err(Error::WebSocketProxy("CONNECT 响应头超过 16 KiB"));
            }
            stream
                .set_read_timeout(Some(left(deadline)?))
                .map_err(|_| Error::WebSocketTransport)?;
            let mut byte = [0];
            if stream
                .read(&mut byte)
                .map_err(|_| Error::WebSocketTransport)?
                == 0
            {
                return Err(Error::WebSocketTransport);
            }
            total += 1;
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let line = header
            .split(|b| *b == b'\n')
            .next()
            .ok_or(Error::WebSocketProxy("CONNECT 状态行缺失"))?;
        let line =
            std::str::from_utf8(line).map_err(|_| Error::WebSocketProxy("CONNECT 状态行无效"))?;
        let mut words = line.split_whitespace();
        if !matches!(words.next(), Some("HTTP/1.1" | "HTTP/1.0")) {
            return Err(Error::WebSocketProxy("CONNECT HTTP 版本无效"));
        }
        let status = words
            .next()
            .filter(|s| s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or(Error::WebSocketProxy("CONNECT 状态码无效"))?;
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(Error::WebSocketProxyStatus(status));
        }
        return Ok(());
    }
}
/// No redirect, authentication retry or direct fallback after a proxy failure.
/// DNS resolution still uses the system's blocking resolver.
pub fn connect(
    destination: &Url,
    deadline: Instant,
    environment: &ProxyEnvironment,
) -> Result<Stream> {
    let proxy = environment.selected(destination)?;
    let peer = proxy.as_ref().unwrap_or(destination);
    let socket = tcp(
        peer.host_str().ok_or(Error::WebSocketProxy("缺少主机名"))?,
        peer.port_or_known_default()
            .or_else(|| {
                proxy
                    .as_ref()
                    .filter(|proxy| matches!(proxy.scheme(), "socks5" | "socks5h"))
                    .map(|_| 1080)
            })
            .ok_or(Error::WebSocketProxy("缺少端口"))?,
        deadline,
    )?;
    let mut stream = match &proxy {
        Some(proxy) if proxy.scheme() == "https" => {
            Stream::tls(socket, proxy, deadline, &environment.proxy_roots)?
        }
        _ => Stream::plain(socket),
    };
    if let Some(proxy) = &proxy {
        if matches!(proxy.scheme(), "http" | "https") {
            tunnel(&mut stream, destination, proxy, deadline)?;
        } else {
            socks::tunnel(&mut stream, destination, proxy, deadline)?;
        }
    }
    stream.finish_setup();
    let timeout = left(deadline)?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|_| stream.set_write_timeout(Some(timeout)))
        .map_err(|_| Error::WebSocketTransport)?;
    Ok(stream)
}
