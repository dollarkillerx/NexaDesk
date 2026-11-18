//! The server as the binary starts it: from a configuration file, through `bind`.
//! The other tests build `Settings` by hand, which would not notice a setting that
//! the file reads and `bind` forgets.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use nexa_auth::DeviceIdentity;
use nexa_session::rendezvous::{
    seal::SignalKeys,
    signal::{SignalClient, SignalConfig, SignalError},
};
use nexa_signal::{Config, bind, start};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const KEYS: &str = r#"access_secrets = ["first secret", "second secret"]"#;

async fn serve_with(signal: &str, web: Option<&str>) -> (nexa_signal::Server, SignalConfig) {
    let web = web.map_or(String::new(), |w| format!("[web]\n{w}\n"));
    let toml = format!(
        r#"
[signal]
listen = "127.0.0.1:0"
{signal}
[stun]
listen = ["127.0.0.1:0"]
{web}"#
    );
    let config = Config::from_toml(&toml).unwrap();
    let (settings, sockets) = bind(&config).await.unwrap();
    let server = start(settings, sockets).await.unwrap();
    let url = SignalConfig::new(format!("ws://127.0.0.1:{}", server.ws_addr.port()));
    (server, url)
}

fn keys() -> SignalKeys {
    SignalKeys::new(Arc::new(DeviceIdentity::generate()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_started_from_a_file_asks_for_one_of_its_keys() {
    let (server, url) = serve_with(KEYS, None).await;
    match SignalClient::connect(&url, &keys()).await {
        Err(SignalError::AccessDenied(why)) => assert!(why.contains("needs"), "{why}"),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("registered without a key"),
    }
    for secret in ["first secret", "second secret", " second secret\n"] {
        let client = SignalClient::connect(&url.clone().with_access_secret(secret), &keys())
            .await
            .unwrap();
        std::mem::forget(client);
    }
    assert_eq!(server.registered(), 3);
}

/// There is no open mode: a file without a key does not start a server.
#[test]
fn a_file_without_a_key_does_not_start_a_server() {
    let toml = "[signal]\nlisten = \"127.0.0.1:0\"\n[stun]\nlisten = [\"127.0.0.1:0\"]\n";
    assert!(Config::from_toml(toml).is_err());
}

/// What `GET path` on the server answers, as text.
async fn http_get(port: u16, path: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    s.write_all(request.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

/// One `hello` on `/web` and the server's answer.
async fn hello(port: u16, id: &str, key: Option<&str>) -> String {
    let mut request = format!("ws://127.0.0.1:{port}/web")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Origin",
        format!("http://127.0.0.1:{port}").parse().unwrap(),
    );
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(request, tcp).await.unwrap();
    let mut m = serde_json::json!({"type": "hello", "id": id});
    if let Some(key) = key {
        m["key"] = key.into();
    }
    ws.send(Message::text(m.to_string())).await.unwrap();
    match ws.next().await {
        Some(Ok(Message::Text(t))) => {
            serde_json::from_str::<serde_json::Value>(&t).unwrap()["code"]
                .as_str()
                .unwrap_or("routed")
                .to_owned()
        }
        other => panic!("{other:?}"),
    }
}

/// Browsers have a password of their own, which is not the signaling key: the page
/// is told to ask for it, and the server puts a browser through only with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_web_password_is_asked_for_apart_from_the_signaling_key() {
    let (server, _) = serve_with(KEYS, Some(r#"password = "let me in""#)).await;
    let port = server.ws_addr.port();
    let mode = http_get(port, "/mode").await;
    assert!(
        mode.ends_with(r#"{"mode":"server","password":true}"#),
        "{mode}"
    );
    assert_eq!(hello(port, "123456789", None).await, "key");
    assert_eq!(hello(port, "123456789", Some("first secret")).await, "key");
    assert_eq!(hello(port, "123456789", Some("let me in")).await, "offline");
}

/// A server whose browsers have no password asks them for nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browsers_are_asked_for_nothing_when_the_web_has_no_password() {
    let (server, _) = serve_with(KEYS, Some("")).await;
    let port = server.ws_addr.port();
    let mode = http_get(port, "/mode").await;
    assert!(mode.ends_with(r#"{"mode":"server"}"#), "{mode}");
    assert_eq!(hello(port, "123456789", None).await, "offline");
    // ...and none of the signaling key either.
    assert_eq!(hello(port, "123456789", Some("anything")).await, "offline");
}
