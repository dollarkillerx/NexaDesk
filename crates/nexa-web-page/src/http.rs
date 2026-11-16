//! Just enough HTTP/1.1 to serve the page and hand WebSocket upgrades on: request
//! heads, the page's responses with their security headers, and a stream that replays
//! the bytes read while looking at the head.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

pub const MAX_HEAD: usize = 8 * 1024;

/// Script only from us (plus WebAssembly compilation), connections only back to us.
/// Inline styles are allowed because the terminal (xterm.js) writes its own `<style>`
/// elements; scripts stay ours only.
pub const CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; img-src 'self'; connect-src 'self'; media-src 'self' blob:; \
    base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub struct Head {
    pub method: String,
    pub path: String,
    pub host: Option<String>,
    pub origin: Option<String>,
    pub upgrade_ws: bool,
}

/// Parses a request head, `None` until it is complete.
pub fn parse_head(buf: &[u8]) -> Option<Result<Head, ()>> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let Ok(text) = std::str::from_utf8(&buf[..end]) else {
        return Some(Err(()));
    };
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version)) = (first.next(), first.next(), first.next())
    else {
        return Some(Err(()));
    };
    if !version.starts_with("HTTP/1.") {
        return Some(Err(()));
    }
    let path = target.split('?').next().unwrap_or("/").to_string();
    let (mut host, mut origin, mut upgrade) = (None, None, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "host" => host = Some(value.to_string()),
            "origin" => origin = Some(value.to_string()),
            "upgrade" => upgrade = value.eq_ignore_ascii_case("websocket"),
            _ => {}
        }
    }
    Some(Ok(Head {
        method: method.to_string(),
        path,
        host,
        origin,
        upgrade_ws: upgrade,
    }))
}

/// A WebSocket must come from a page we served: same host, either scheme. This stops
/// another site open in the same browser from talking to the host.
pub fn same_origin(head: &Head) -> bool {
    match (&head.origin, &head.host) {
        (Some(origin), Some(host)) => {
            origin == &format!("http://{host}") || origin == &format!("https://{host}")
        }
        _ => false,
    }
}

/// A Host header worth echoing into a Location: a name or address and a port.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 255
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b))
}

pub async fn redirect<S: AsyncWrite + Unpin>(stream: &mut S, location: &str) {
    let head = format!(
        "HTTP/1.1 308 Permanent Redirect\r\n\
         Location: {location}\r\n\
         Content-Length: 0\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n"
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.shutdown().await;
}

pub async fn respond<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    content_type: &str,
    body: &[u8],
) {
    respond_len(stream, status, content_type, body, body.len(), false).await
}

pub async fn respond_len<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    content_type: &str,
    body: &[u8],
    len: usize,
    secure: bool,
) {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Request Header Fields Too Large",
    };
    // Over HTTPS, the browser may remember to come back that way.
    let hsts = if secure {
        "Strict-Transport-Security: max-age=31536000\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Cross-Origin-Opener-Policy: same-origin\r\n\
         Content-Security-Policy: {CSP}\r\n\
         {hsts}\
         Connection: close\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).await.is_ok() {
        let _ = stream.write_all(body).await;
    }
    let _ = stream.shutdown().await;
}

/// A connection, plain or TLS.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// What a WebSocket runs over.
pub type Conn = Rewind<Box<dyn Io>>;

/// A stream whose first bytes were already read: they are read again first.
pub struct Rewind<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> Rewind<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Rewind<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(out.remaining());
            let start = self.pos;
            out.put_slice(&self.prefix[start..start + n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, out)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Rewind<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        b: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets;

    #[test]
    fn heads_parse() {
        assert!(parse_head(b"GET / HTTP/1.1\r\nHost: x").is_none());
        let h = parse_head(
            b"GET /ws?x=1 HTTP/1.1\r\nHost: 10.0.0.2:47801\r\nUpgrade: WebSocket\r\nOrigin: http://10.0.0.2:47801\r\n\r\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(h.path, "/ws");
        assert!(h.upgrade_ws);
        assert!(same_origin(&h));
        assert!(parse_head(b"GET / SPDY/3\r\n\r\n").unwrap().is_err());
    }

    #[test]
    fn other_origins_are_refused() {
        let h = parse_head(
            b"GET /ws HTTP/1.1\r\nHost: 10.0.0.2:47801\r\nUpgrade: websocket\r\nOrigin: https://evil.example\r\n\r\n",
        )
        .unwrap()
        .unwrap();
        assert!(!same_origin(&h));
        let h = parse_head(b"GET /ws HTTP/1.1\r\nHost: a\r\nUpgrade: websocket\r\n\r\n")
            .unwrap()
            .unwrap();
        assert!(!same_origin(&h));
    }

    #[test]
    fn every_asset_is_found() {
        for a in assets::ASSETS {
            assert!(!a.body.is_empty(), "{} is empty", a.path);
            assert!(assets::find(a.path).is_some());
        }
        assert!(assets::find("/index.html").is_some());
        assert!(assets::find("/../Cargo.toml").is_none());
    }
}
