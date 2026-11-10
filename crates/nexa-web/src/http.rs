//! Just enough HTTP/1.1: GET for the page, and the WebSocket upgrade on `/ws`.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, sleep, timeout},
};

use crate::{Shared, assets, session};

const MAX_HEAD: usize = 8 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Script only from us (plus WebAssembly compilation), connections only back to us.
const CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self'; img-src 'self'; connect-src 'self'; media-src 'self' blob:; \
    base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub(crate) struct Head {
    pub method: String,
    pub path: String,
    pub host: Option<String>,
    pub origin: Option<String>,
    pub upgrade_ws: bool,
    pub len: usize,
}

/// Parses a request head, `None` until it is complete.
pub(crate) fn parse_head(buf: &[u8]) -> Option<Result<Head, ()>> {
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
        len: end,
    }))
}

/// A WebSocket must come from a page we served: same host, either scheme. This stops
/// another site open in the same browser from talking to the host.
pub(crate) fn same_origin(head: &Head) -> bool {
    match (&head.origin, &head.host) {
        (Some(origin), Some(host)) => {
            origin == &format!("http://{host}") || origin == &format!("https://{host}")
        }
        _ => false,
    }
}

pub(crate) async fn handle(mut stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let Ok(local) = stream.local_addr() else {
        return;
    };
    // Peek, so that a WebSocket upgrade can be handed over with its request unread.
    let deadline = Instant::now() + HEAD_TIMEOUT;
    let mut buf = vec![0u8; MAX_HEAD];
    let head = loop {
        let n = match timeout(deadline - Instant::now(), stream.peek(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return,
        };
        match parse_head(&buf[..n]) {
            Some(Ok(head)) => break head,
            Some(Err(())) => return respond(&mut stream, 400, "text/plain", b"bad request").await,
            None if n == MAX_HEAD => return respond(&mut stream, 431, "text/plain", b"").await,
            None if Instant::now() >= deadline => return,
            None => sleep(Duration::from_millis(5)).await,
        }
    };

    if head.path == "/ws" && head.upgrade_ws {
        if !same_origin(&head) {
            return respond(&mut stream, 403, "text/plain", b"forbidden").await;
        }
        let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        return session::run(ws, peer, local, shared).await;
    }

    let mut discard = vec![0u8; head.len];
    if stream.read_exact(&mut discard).await.is_err() {
        return;
    }
    if head.method != "GET" && head.method != "HEAD" {
        return respond(&mut stream, 405, "text/plain", b"").await;
    }
    match assets::find(&head.path) {
        Some(a) => {
            let body = if head.method == "HEAD" {
                &[][..]
            } else {
                a.body
            };
            respond_len(&mut stream, 200, a.content_type, body, a.body.len()).await
        }
        None if head.path == "/favicon.ico" => respond(&mut stream, 204, "text/plain", b"").await,
        None => respond(&mut stream, 404, "text/plain", b"not found").await,
    }
}

async fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    respond_len(stream, status, content_type, body, body.len()).await
}

async fn respond_len(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    len: usize,
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
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Cross-Origin-Opener-Policy: same-origin\r\n\
         Content-Security-Policy: {CSP}\r\n\
         Connection: close\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).await.is_ok() {
        let _ = stream.write_all(body).await;
    }
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

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
