//! The MCP server as the app runs it (docs/MCP.md §16), with one made-up saved device:
//! the demo host (`cargo run -p nexa-agent --example demo_host -- --agents`), signed in
//! to with its fixed demo password. An AI app connects through
//! `nexa-mcp --socket <socket>`; nothing of this machine's Keychain or saved
//! connections is used.
//!
//! Usage: `cargo run -p nexa-client --example mcp_demo -- <socket> [address] [password]`
//! (defaults `127.0.0.1:47900` and the demo host's password).

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {}

#[cfg(any(target_os = "macos", windows))]
fn main() {
    use std::sync::Arc;

    use nexa_client::{
        mcp::{
            Service,
            viewer::{Device, Directory, SignIn},
        },
        proto::ConnectAuth,
    };

    struct Demo {
        address: String,
        password: String,
    }

    impl Directory for Demo {
        fn devices(&self) -> Vec<Device> {
            vec![Device {
                name: "NexaDesk demo".into(),
                address: self.address.clone(),
                sign_in: "the fixed password saved in NexaDesk".into(),
            }]
        }
        fn sign_in(&self, _: &Device) -> Result<SignIn, String> {
            Ok(SignIn {
                auth: ConnectAuth::Password(self.password.clone()),
                signal_server: String::new(),
                signal_secret: String::new(),
            })
        }
    }

    let mut args = std::env::args().skip(1);
    let socket = args
        .next()
        .expect("usage: mcp_demo <socket> [address] [password]");
    let address = args.next().unwrap_or_else(|| "127.0.0.1:47900".into());
    let password = args.next().unwrap_or_else(|| "nexademohost".into());
    let ai_dir = std::env::temp_dir().join(format!("nexa-mcp-demo-{}", std::process::id()));
    let _service = Service::start_with(
        &socket,
        ai_dir,
        "MCP demo".into(),
        Arc::new(Demo { address, password }),
    )
    .expect("serve");
    println!("SERVING {socket}");
    loop {
        std::thread::park();
    }
}
