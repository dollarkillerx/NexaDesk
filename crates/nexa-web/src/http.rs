//! The host's side of serving the page: plain HTTP or TLS on one port, the page, and
//! the WebSocket on `/ws` handed to the session. Over HTTPS when the gateway has a
//! certificate; plain HTTP then only redirects there.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use nexa_web_page::{
    assets,
    http::{
        Io, MAX_HEAD, Rewind, parse_head, redirect, respond, respond_len, same_origin, valid_host,
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    net::TcpStream,
    time::{Instant, timeout},
};

use crate::{Shared, session};

const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn handle(stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let Ok(local) = stream.local_addr() else {
        return;
    };
    let Some(tls) = shared.tls.clone() else {
        return serve(stream, peer, local, shared, false).await;
    };
    // One port for both: a TLS handshake starts with a handshake record (0x16).
    let mut first = [0u8; 1];
    match timeout(HEAD_TIMEOUT, stream.peek(&mut first)).await {
        Ok(Ok(1)) => {}
        _ => return,
    }
    if first[0] != 0x16 {
        return serve(stream, peer, local, shared, false).await;
    }
    let Ok(Ok(stream)) = timeout(HEAD_TIMEOUT, tls.acceptor.accept(stream)).await else {
        return;
    };
    serve(stream, peer, local, shared, true).await
}

async fn serve<S>(
    mut stream: S,
    peer: SocketAddr,
    local: SocketAddr,
    shared: Arc<Shared>,
    secure: bool,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let deadline = Instant::now() + HEAD_TIMEOUT;
    let mut buf = vec![0u8; MAX_HEAD];
    let mut n = 0;
    let head = loop {
        match parse_head(&buf[..n]) {
            Some(Ok(head)) => break head,
            Some(Err(())) => return respond(&mut stream, 400, "text/plain", b"bad request").await,
            None if n == MAX_HEAD => return respond(&mut stream, 431, "text/plain", b"").await,
            None => {}
        }
        match timeout(deadline - Instant::now(), stream.read(&mut buf[n..])).await {
            Ok(Ok(read)) if read > 0 => n += read,
            _ => return,
        }
    };

    // With HTTPS on, plain HTTP only points the way there.
    if shared.tls.is_some() && !secure {
        let Some(host) = head.host.as_deref().filter(|h| valid_host(h)) else {
            return respond(&mut stream, 400, "text/plain", b"bad request").await;
        };
        return redirect(&mut stream, &format!("https://{host}{}", head.path)).await;
    }

    if head.path == "/ws" && head.upgrade_ws {
        if !same_origin(&head) {
            return respond(&mut stream, 403, "text/plain", b"forbidden").await;
        }
        // The WebSocket handshake reads the request again, from what we already took.
        let replay = Rewind::new(buf[..n].to_vec(), Box::new(stream) as Box<dyn Io>);
        let Ok(ws) = tokio_tungstenite::accept_async(replay).await else {
            return;
        };
        let pipe = session::Pipe::Ws(Box::new(ws));
        return session::run(pipe, peer, crate::rtc::Path::Direct(local.ip()), shared).await;
    }

    if head.method != "GET" && head.method != "HEAD" {
        return respond(&mut stream, 405, "text/plain", b"").await;
    }
    if head.path == "/mode" {
        return respond(
            &mut stream,
            200,
            "application/json",
            nexa_web_page::MODE_DIRECT,
        )
        .await;
    }
    match assets::find(&head.path) {
        Some(a) => {
            let body = if head.method == "HEAD" {
                &[][..]
            } else {
                a.body
            };
            respond_len(&mut stream, 200, a.content_type, body, a.body.len(), secure).await
        }
        None if head.path == "/favicon.ico" => respond(&mut stream, 204, "text/plain", b"").await,
        None => respond(&mut stream, 404, "text/plain", b"not found").await,
    }
}
