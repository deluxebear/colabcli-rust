//! SOCKS5 CONNECT with an absolute negotiation deadline and bounded packets.
use super::left;
use crate::{Error, Result};
use std::{
    io::{Read, Write},
    net::{IpAddr, ToSocketAddrs},
    time::Instant,
};
use url::Url;
fn write(stream: &mut super::Stream, mut data: &[u8], deadline: Instant) -> Result<()> {
    while !data.is_empty() {
        stream
            .set_write_timeout(Some(left(deadline)?))
            .map_err(|_| Error::WebSocketTransport)?;
        let count = stream.write(data).map_err(|_| Error::WebSocketTransport)?;
        if count == 0 {
            return Err(Error::WebSocketTransport);
        }
        data = &data[count..];
    }
    Ok(())
}
fn read(stream: &mut super::Stream, data: &mut [u8], deadline: Instant) -> Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        stream
            .set_read_timeout(Some(left(deadline)?))
            .map_err(|_| Error::WebSocketTransport)?;
        let count = stream
            .read(&mut data[offset..])
            .map_err(|_| Error::WebSocketTransport)?;
        if count == 0 {
            return Err(Error::WebSocketTransport);
        }
        offset += count;
    }
    Ok(())
}
fn credentials(proxy: &Url) -> Result<Option<(String, String)>> {
    if proxy.username().is_empty() && proxy.password().is_none() {
        return Ok(None);
    }
    let decode = |value: &str| {
        percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .map(|value| value.into_owned())
            .map_err(|_| Error::WebSocketProxy("SOCKS5 认证编码无效"))
    };
    let user = decode(proxy.username())?;
    let password = decode(proxy.password().unwrap_or(""))?;
    if !(1..=255).contains(&user.len()) || !(1..=255).contains(&password.len()) {
        return Err(Error::WebSocketProxy("SOCKS5 用户名和密码各需 1–255 字节"));
    }
    Ok(Some((user, password)))
}
fn address(destination: &Url, remote_dns: bool, deadline: Instant) -> Result<Vec<u8>> {
    let host = destination
        .host_str()
        .ok_or(Error::WebSocketProxy("SOCKS5 目标缺少主机名"))?;
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let port = destination
        .port_or_known_default()
        .ok_or(Error::WebSocketProxy("SOCKS5 目标缺少端口"))?;
    let ip = host.parse::<IpAddr>().ok();
    let ip = if ip.is_none() && !remote_dns {
        left(deadline)?;
        let address = (host, port)
            .to_socket_addrs()
            .map_err(|_| Error::WebSocketTransport)?
            .next()
            .ok_or(Error::WebSocketTransport)?;
        left(deadline)?;
        Some(address.ip())
    } else {
        ip
    };
    let mut bytes = match ip {
        Some(IpAddr::V4(ip)) => {
            let mut bytes = vec![1];
            bytes.extend_from_slice(&ip.octets());
            bytes
        }
        Some(IpAddr::V6(ip)) => {
            let mut bytes = vec![4];
            bytes.extend_from_slice(&ip.octets());
            bytes
        }
        None => {
            if host.is_empty() || host.len() > 255 {
                return Err(Error::WebSocketProxy("SOCKS5 域名需 1–255 字节"));
            }
            let mut bytes = vec![3, host.len() as u8];
            bytes.extend_from_slice(host.as_bytes());
            bytes
        }
    };
    bytes.extend_from_slice(&port.to_be_bytes());
    Ok(bytes)
}
pub(super) fn tunnel(
    stream: &mut super::Stream,
    destination: &Url,
    proxy: &Url,
    deadline: Instant,
) -> Result<()> {
    let credentials = credentials(proxy)?;
    let method = if credentials.is_some() { 2 } else { 0 };
    write(stream, &[5, 1, method], deadline)?;
    let mut selected = [0; 2];
    read(stream, &mut selected, deadline)?;
    if selected != [5, method] {
        return Err(Error::WebSocketProxy("SOCKS5 认证方式协商失败"));
    }
    if let Some((user, password)) = credentials {
        let mut auth = vec![1, user.len() as u8];
        auth.extend_from_slice(user.as_bytes());
        auth.push(password.len() as u8);
        auth.extend_from_slice(password.as_bytes());
        write(stream, &auth, deadline)?;
        let mut reply = [0; 2];
        read(stream, &mut reply, deadline)?;
        if reply != [1, 0] {
            return Err(Error::WebSocketProxy("SOCKS5 用户名/密码认证失败"));
        }
    }
    let mut request = vec![5, 1, 0];
    request.extend(address(destination, proxy.scheme() == "socks5h", deadline)?);
    write(stream, &request, deadline)?;
    let mut header = [0; 4];
    read(stream, &mut header, deadline)?;
    if header[0] != 5 || header[2] != 0 {
        return Err(Error::WebSocketProxy("SOCKS5 CONNECT 响应无效"));
    }
    if header[1] != 0 {
        return Err(Error::WebSocketSocksStatus(header[1]));
    }
    let length = match header[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0];
            read(stream, &mut length, deadline)?;
            if length[0] == 0 {
                return Err(Error::WebSocketProxy("SOCKS5 响应域名为空"));
            }
            length[0] as usize
        }
        _ => return Err(Error::WebSocketProxy("SOCKS5 响应地址类型无效")),
    };
    let mut bound = vec![0; length + 2];
    read(stream, &mut bound, deadline)?;
    Ok(())
}
