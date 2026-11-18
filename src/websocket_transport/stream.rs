//! Optional TLS to the proxy, independent of tungstenite's endpoint TLS.
use super::left;
use crate::{Error, Result};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, StreamOwned,
    pki_types::{CertificateDer, ServerName},
};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::Arc,
    time::{Duration, Instant},
};
use url::Url;

/// Read/write abstraction allowing endpoint TLS inside an HTTPS proxy's TLS.
/// The underlying TCP socket remains available for timeouts and SSH polling.
pub struct Stream {
    inner: Inner,
}
enum Inner {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, DeadlineSocket>>),
}
struct DeadlineSocket {
    socket: TcpStream,
    deadline: Option<Instant>,
}
impl DeadlineSocket {
    fn remaining(&self) -> io::Result<Option<Duration>> {
        self.deadline
            .map(|deadline| left(deadline).map_err(|_| io::Error::from(io::ErrorKind::TimedOut)))
            .transpose()
    }
}
impl Read for DeadlineSocket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if let Some(left) = self.remaining()? {
            self.socket.set_read_timeout(Some(left))?;
        }
        self.socket.read(bytes)
    }
}
impl Write for DeadlineSocket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(left) = self.remaining()? {
            self.socket.set_write_timeout(Some(left))?;
        }
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.remaining()?;
        self.socket.flush()
    }
}
impl Stream {
    pub(super) fn plain(socket: TcpStream) -> Self {
        Self {
            inner: Inner::Plain(socket),
        }
    }
    pub(super) fn tls(
        socket: TcpStream,
        proxy: &Url,
        deadline: Instant,
        extra_roots: &[CertificateDer<'static>],
    ) -> Result<Self> {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for certificate in extra_roots {
            roots
                .add(certificate.clone())
                .map_err(|_| Error::WebSocketProxy("代理 CA 证书无效"))?;
        }
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let host = proxy
            .host_str()
            .ok_or(Error::WebSocketProxy("代理缺少主机名"))?;
        let host = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host);
        let name = ServerName::try_from(host.to_owned()).map_err(|_| Error::WebSocketProxyTls)?;
        let mut connection =
            ClientConnection::new(Arc::new(config), name).map_err(|_| Error::WebSocketProxyTls)?;
        let mut socket = DeadlineSocket {
            socket,
            deadline: Some(deadline),
        };
        connection
            .complete_io(&mut socket)
            .map_err(|_| Error::WebSocketProxyTls)?;
        if connection.is_handshaking() {
            return Err(Error::WebSocketProxyTls);
        }
        Ok(Self {
            inner: Inner::Tls(Box::new(StreamOwned::new(connection, socket))),
        })
    }
    pub(super) fn finish_setup(&mut self) {
        if let Inner::Tls(stream) = &mut self.inner {
            stream.sock.deadline = None;
        }
    }
    pub fn tcp(&self) -> &TcpStream {
        match &self.inner {
            Inner::Plain(socket) => socket,
            Inner::Tls(stream) => &stream.sock.socket,
        }
    }
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.tcp().set_read_timeout(timeout)
    }
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.tcp().set_write_timeout(timeout)
    }
}
// Preserve a single read budget across EINTR. A repeated signal must neither
// abort a WebSocket handshake nor keep the caller away from cancellation and
// execution-deadline checks indefinitely. No writes or messages are replayed.
fn retry_interrupted_read(
    timeout: Option<Duration>,
    mut read: impl FnMut(Option<Duration>) -> io::Result<usize>,
) -> io::Result<usize> {
    let deadline = timeout
        .map(|timeout| {
            Instant::now()
                .checked_add(timeout)
                .ok_or(io::ErrorKind::InvalidInput)
        })
        .transpose()?;
    let mut remaining = None;
    loop {
        match read(remaining) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted && deadline.is_some() => {
                let left = deadline.unwrap().saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                remaining = Some(left);
            }
            result => return result,
        }
    }
}
impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let timeout = self.tcp().read_timeout()?;
        let result = retry_interrupted_read(timeout, |remaining| {
            if let Some(remaining) = remaining {
                self.tcp().set_read_timeout(Some(remaining))?;
            }
            match &mut self.inner {
                Inner::Plain(socket) => socket.read(bytes),
                Inner::Tls(stream) => stream.read(bytes),
            }
        });
        self.tcp().set_read_timeout(timeout)?;
        result
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Plain(socket) => socket.write(bytes),
            Inner::Tls(stream) => stream.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Inner::Plain(socket) => socket.flush(),
            Inner::Tls(stream) => stream.flush(),
        }
    }
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebSocketStream")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_read_preserves_bytes_and_uses_remaining_budget() {
        let mut calls = 0;
        let count = retry_interrupted_read(Some(Duration::from_secs(1)), |remaining| {
            calls += 1;
            if calls == 1 {
                assert!(remaining.is_none());
                Err(io::ErrorKind::Interrupted.into())
            } else {
                assert!(remaining.is_some_and(|left| left <= Duration::from_secs(1)));
                Ok(7)
            }
        })
        .unwrap();
        assert_eq!((count, calls), (7, 2));
    }

    #[test]
    fn read_retry_never_hides_real_errors_or_spins_without_a_budget() {
        for (timeout, kind) in [
            (None, io::ErrorKind::Interrupted),
            (Some(Duration::from_secs(1)), io::ErrorKind::ConnectionReset),
            (Some(Duration::from_secs(1)), io::ErrorKind::WouldBlock),
        ] {
            let mut calls = 0;
            let error = retry_interrupted_read(timeout, |_| {
                calls += 1;
                Err(kind.into())
            })
            .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn interrupted_read_stops_at_expired_budget_without_repeating_io() {
        let mut calls = 0;
        let error = retry_interrupted_read(Some(Duration::ZERO), |_| {
            calls += 1;
            Err(io::ErrorKind::Interrupted.into())
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls, 1);
    }
}
