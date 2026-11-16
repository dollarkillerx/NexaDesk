//! A browser's WebSocket (`/web`), for browsers that cannot reach the host themselves
//! (docs/WEB_CLIENT.md §12).
//!
//! ```text
//! browser → server  {type:"hello", id:"123456789"}
//! server → browser  {type:"routed", ice:{urls, username, credential}}   or {type:"error", code}
//! then              the direct-access protocol (nexa-web's session.rs), frame by frame,
//!                   between the browser and the host, which gets them as BrowserText
//! ```
//!
//! The server only forwards. Pairing binds the one-time code to both ends' DTLS
//! fingerprints, so it cannot stand in for either; it does see when, and how much.

use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use crate::{
    hub::{Hub, ToBrowser},
    limit::{Bucket, Rate},
};

/// The path browsers open.
pub const WEB_PATH: &str = "/web";
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// Silence from both sides for this long ends it; a session in progress pings.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Signaling text only: SDP, pairing, a few control messages.
pub const MAX_BROWSER_MESSAGE: usize = 64 * 1024;
const QUEUE: usize = 64;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

async fn send<S>(sink: &mut S, v: Value) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
{
    sink.send(Message::text(v.to_string())).await.is_ok()
}

pub async fn serve<S>(ws: WebSocketStream<S>, ip: IpAddr, hub: Arc<Mutex<Hub>>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();
    let hello = tokio::time::timeout(HELLO_TIMEOUT, async {
        loop {
            match stream.next().await? {
                Ok(Message::Text(t)) if t.len() <= MAX_BROWSER_MESSAGE => {
                    return serde_json::from_str::<Value>(&t).ok();
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                _ => return None,
            }
        }
    })
    .await;
    let id = match hello {
        Ok(Some(v)) if v["type"] == "hello" => v["id"].as_str().unwrap_or("").to_string(),
        _ => {
            let _ = send(&mut sink, json!({"type": "error", "code": "protocol"})).await;
            return;
        }
    };

    let (tx, mut rx) = mpsc::channel::<ToBrowser>(QUEUE);
    let opened = hub.lock().unwrap_or_else(|e| e.into_inner()).open_browser(
        ip,
        &id,
        tx,
        Instant::now(),
        unix_now(),
    );
    let route = match opened {
        Ok(r) => r,
        Err(refusal) => {
            let _ = send(&mut sink, json!({"type": "error", "code": refusal.code()})).await;
            let _ = sink.close().await;
            return;
        }
    };
    let routed = json!({
        "type": "routed",
        "ice": {
            "urls": [format!("turn:{}?transport=udp", route.turn_server)],
            "username": route.turn.username,
            "credential": route.turn.password,
        },
    });
    if !send(&mut sink, routed).await {
        hub.lock()
            .unwrap_or_else(|e| e.into_inner())
            .close_browser(route.conn);
        return;
    }

    let rate = Rate::new(20.0, 60.0);
    let mut bucket = Bucket::full(rate, Instant::now());
    loop {
        tokio::select! {
            m = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => {
                let text = match m {
                    Ok(Some(Ok(Message::Text(t)))) if t.len() <= MAX_BROWSER_MESSAGE => t,
                    Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
                    _ => break,
                };
                if !bucket.take(rate, 1.0, Instant::now()) {
                    break;
                }
                let delivered = hub
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .browser_text(route.conn, text.to_string());
                if !delivered {
                    break;
                }
            }
            out = rx.recv() => match out {
                Some(ToBrowser::Text(t)) => {
                    if sink.send(Message::text(t)).await.is_err() {
                        break;
                    }
                }
                Some(ToBrowser::Close) | None => break,
            },
        }
    }
    hub.lock()
        .unwrap_or_else(|e| e.into_inner())
        .close_browser(route.conn);
    let _ = sink.close().await;
}
