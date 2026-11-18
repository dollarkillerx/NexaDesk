//! The server as the binary starts it: from a configuration file, through `bind`.
//! The other tests build `Settings` by hand, which would not notice a setting that
//! the file reads and `bind` forgets.

use std::sync::Arc;

use nexa_auth::DeviceIdentity;
use nexa_session::rendezvous::{
    seal::SignalKeys,
    signal::{SignalClient, SignalConfig, SignalError},
};
use nexa_signal::{Config, bind, start};

async fn serve(extra: &str) -> (nexa_signal::Server, SignalConfig) {
    let toml = format!(
        r#"
[signal]
listen = "127.0.0.1:0"
{extra}
[stun]
listen = ["127.0.0.1:0"]
"#
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
async fn a_server_started_from_a_file_with_secrets_asks_for_one_of_them() {
    let (server, url) = serve(r#"access_secrets = ["first secret", "second secret"]"#).await;
    match SignalClient::connect(&url, &keys()).await {
        Err(SignalError::AccessDenied(why)) => assert!(why.contains("needs"), "{why}"),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("registered without a password"),
    }
    for secret in ["first secret", "second secret", " second secret\n"] {
        let client = SignalClient::connect(&url.clone().with_access_secret(secret), &keys())
            .await
            .unwrap();
        std::mem::forget(client);
    }
    assert_eq!(server.registered(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_started_from_a_file_without_secrets_is_open() {
    let (server, url) = serve("").await;
    let plain = SignalClient::connect(&url, &keys()).await.unwrap();
    // A password that nobody asked for does no harm.
    let given = SignalClient::connect(&url.clone().with_access_secret("anything"), &keys())
        .await
        .unwrap();
    assert_eq!(server.registered(), 2);
    drop((plain, given));
}
