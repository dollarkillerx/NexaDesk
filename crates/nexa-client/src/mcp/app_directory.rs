//! The devices AI agents may operate, as the person keeps them in NexaDesk
//! (docs/MCP.md §16): the saved connections in `recent.json` that have "Allow AI to
//! operate" on, signed in to with their saved fixed password (from the Keychain, in
//! this process only) or, for a device ID, by asking the person there through the
//! signaling server set in `settings.json` (its access key also from the Keychain).

use std::path::PathBuf;

use serde_json::Value;

use super::viewer::{Device, Directory, SignIn};
use crate::proto::ConnectAuth;

/// The saved connections of the app whose data directory this is.
pub struct AppDirectory {
    data_dir: PathBuf,
}

/// One saved connection, as the app writes it (camelCase JSON).
struct Saved {
    address: String,
    name: String,
    password_account: String,
    has_password: bool,
}

impl AppDirectory {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    fn read(&self, file: &str) -> Value {
        std::fs::read_to_string(self.data_dir.join(file))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null)
    }

    /// The saved connections the person allowed AI agents to operate.
    fn allowed(&self) -> Vec<Saved> {
        let text = |d: &Value, k: &str| {
            d.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned()
        };
        let flag = |d: &Value, k: &str| d.get(k).and_then(Value::as_bool).unwrap_or(false);
        self.read("recent.json")
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| flag(d, "aiAllowed"))
            .filter_map(|d| {
                let address = text(d, "address");
                if address.is_empty() {
                    return None;
                }
                let label = text(d, "label");
                let host_key = text(d, "hostKey");
                Some(Saved {
                    name: if label.is_empty() {
                        text(d, "name")
                    } else {
                        label
                    },
                    password_account: if host_key.is_empty() {
                        address.clone()
                    } else {
                        host_key
                    },
                    has_password: flag(d, "hasPassword"),
                    address,
                })
            })
            .collect()
    }

    /// The signaling server the app uses, and its access key.
    fn server(&self) -> Option<(String, String)> {
        let settings = self.read("settings.json");
        let server = settings
            .get("signalServer")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if server.is_empty() {
            return None;
        }
        let secret =
            crate::abi::load_saved_password(&format!("signal-access:{server}")).unwrap_or_default();
        Some((server, secret))
    }
}

fn is_device_id(address: &str) -> bool {
    address.len() == 9 && address.bytes().all(|b| b.is_ascii_digit())
}

impl Directory for AppDirectory {
    fn devices(&self) -> Vec<Device> {
        let has_server = self.server().is_some();
        self.allowed()
            .into_iter()
            .map(|s| {
                let by_id = is_device_id(&s.address);
                let sign_in = if s.has_password {
                    "the fixed password saved in NexaDesk"
                } else if by_id && has_server {
                    "a request the person there allows"
                } else {
                    ""
                };
                Device {
                    name: s.name,
                    address: s.address,
                    sign_in: sign_in.to_owned(),
                }
            })
            .collect()
    }

    fn sign_in(&self, device: &Device) -> Result<SignIn, String> {
        let saved = self
            .allowed()
            .into_iter()
            .find(|s| s.address == device.address)
            .ok_or_else(|| format!("{} may no longer be operated by AI agents", device.name))?;
        let by_id = is_device_id(&saved.address);
        let (signal_server, signal_secret) = match self.server() {
            Some(s) if by_id => s,
            _ => (String::new(), String::new()),
        };
        let password = saved
            .has_password
            .then(|| crate::abi::load_saved_password(&saved.password_account))
            .flatten();
        let auth = match password {
            Some(p) => ConnectAuth::Password(p),
            None if by_id && !signal_server.is_empty() => ConnectAuth::Request(true),
            None => {
                return Err(format!(
                    "NexaDesk has no way to sign in to {}: save its fixed password in NexaDesk, \
                     or reach it by device ID",
                    device.name
                ));
            }
        };
        Ok(SignIn {
            auth,
            signal_server,
            signal_secret,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_devices_allowed_for_ai_are_listed_by_their_label() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("recent.json"),
            r#"[
              {"address":"220041958","name":"MacBook","hostKey":"ab","label":"Work","hasPassword":false,"aiAllowed":true},
              {"address":"10.0.0.2:47800","name":"Mini","hostKey":"","label":"","hasPassword":false,"aiAllowed":false},
              {"address":"10.0.0.3:47800","name":"PC","hostKey":"","label":"","hasPassword":false,"aiAllowed":true}
            ]"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"signalServer":"wss://s.example:8443"}"#,
        )
        .unwrap();
        let d = AppDirectory::new(dir.path().to_path_buf());
        let devices = d.devices();
        assert_eq!(
            devices,
            vec![
                Device {
                    name: "Work".into(),
                    address: "220041958".into(),
                    sign_in: "a request the person there allows".into(),
                },
                Device {
                    name: "PC".into(),
                    address: "10.0.0.3:47800".into(),
                    sign_in: String::new(),
                },
            ]
        );
        let by_id = d.sign_in(&devices[0]).unwrap();
        assert_eq!(by_id.auth, ConnectAuth::Request(true));
        assert_eq!(by_id.signal_server, "wss://s.example:8443");
        let e = d.sign_in(&devices[1]).err().unwrap();
        assert!(e.contains("no way to sign in"), "{e}");
    }
}
