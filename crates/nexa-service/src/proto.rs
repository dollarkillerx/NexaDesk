//! What the service and its clients say on the control pipe (docs/WINDOWS_SERVICE.md §3):
//! one JSON object per line, a request from the client, then events from the service.

use serde::{Deserialize, Serialize};

/// The control pipe. Local clients only; see `windows::server`.
pub const PIPE: &str = r"\\.\pipe\nexadesk-service";

/// The longest request line read.
pub const MAX_REQUEST: usize = 64 * 1024;

/// What a client asks; one per connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Whether the service is there and speaks this protocol.
    Hello,
    /// Start the host as SYSTEM in the client's session, with these arguments (checked
    /// against [`check_args`]). It runs while the connection stays open: closing it ends
    /// the host. Its output comes back as [`Event::Line`].
    Start {
        args: Vec<String>,
        /// The signaling server's access key, for `NEXA_SIGNAL_SECRET`: never on a
        /// command line.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal_secret: Option<String>,
    },
    /// Send Ctrl+Alt+Del (the secure attention sequence). Only a host this service
    /// started may ask: it received the keys from its viewer.
    Sas,
}

/// What the service answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Hello {
        protocol: u32,
    },
    Started {
        pid: u32,
    },
    /// A line the host wrote to its standard output or error.
    Line(String),
    Exited {
        code: i64,
    },
    Done,
    Error(String),
}

pub const PROTOCOL: u32 = 1;

/// Arguments taking a value. Nothing that names a file or folder: the host works those
/// out as the session's user, and a path chosen by the client would be read or written
/// by SYSTEM.
const WITH_VALUE: &[&str] = &["--listen", "--name", "--signal", "--web", "--consent"];
const FLAGS: &[&str] = &[
    "--no-lan-discovery",
    "--allow-agents",
    "--paired-only",
    "--relay-only",
];
const MOST_ARGS: usize = 32;
const LONGEST_ARG: usize = 1024;

/// Checks a client's host arguments against what the service passes on.
pub fn check_args(args: &[String]) -> Result<(), String> {
    if args.len() > MOST_ARGS {
        return Err("too many arguments".into());
    }
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a.len() > LONGEST_ARG || a.contains('\0') {
            return Err("an argument is too long".into());
        }
        if FLAGS.contains(&a.as_str()) {
            continue;
        }
        if !WITH_VALUE.contains(&a.as_str()) {
            return Err(format!("{a} is not passed on by the service"));
        }
        let value = it.next().ok_or_else(|| format!("{a} needs a value"))?;
        if value.len() > LONGEST_ARG || value.contains('\0') {
            return Err("an argument is too long".into());
        }
        if a == "--consent" && value != "ui" && value != "terminal" {
            return Err(format!("unknown consent mode {value}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_apps_arguments_pass() {
        let args = s(&[
            "--listen",
            "[::]:47800",
            "--name",
            "My PC",
            "--consent",
            "ui",
            "--signal",
            "signal.example:443",
            "--no-lan-discovery",
            "--web",
            "[::]:47801",
        ]);
        assert_eq!(check_args(&args), Ok(()));
    }

    #[test]
    fn paths_and_unknown_arguments_do_not() {
        for bad in [
            s(&["--data-dir", "C:\\Windows"]),
            s(&["--downloads", "C:\\Windows"]),
            s(&["--signal-ca", "C:\\x.pem"]),
            s(&["run-in-session"]),
            s(&["--name"]),
            s(&["--name", "--data-dir", "C:\\Windows"]),
            s(&["--consent", "everyone"]),
        ] {
            assert!(check_args(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn requests_and_events_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&Request::Hello).unwrap(),
            r#""hello""#
        );
        assert_eq!(serde_json::to_string(&Request::Sas).unwrap(), r#""sas""#);
        let start = Request::Start {
            args: s(&["--name", "x"]),
            signal_secret: None,
        };
        assert_eq!(
            serde_json::to_string(&start).unwrap(),
            r#"{"start":{"args":["--name","x"]}}"#
        );
        assert_eq!(
            serde_json::to_string(&Event::Line("hi".into())).unwrap(),
            r#"{"line":"hi"}"#
        );
        assert_eq!(
            serde_json::to_string(&Event::Exited { code: 1 }).unwrap(),
            r#"{"exited":{"code":1}}"#
        );
        assert!(serde_json::from_str::<Request>(r#"{"start":{"args":[],"extra":1}}"#).is_err());
    }
}
