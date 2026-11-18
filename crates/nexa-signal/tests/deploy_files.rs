//! The installer (deploy/install.sh) carries its own copy of the systemd unit so that it can
//! be piped to `sh` from one URL. The copy must not drift from deploy/nexa-signal.service.

use std::path::PathBuf;

fn deploy(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// What the unit says, without comments and blank lines.
fn directives(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_installers_unit_is_the_service_file() {
    let script = deploy("install.sh");
    let start = script
        .find("cat >\"$UNIT\" <<'EOF'\n")
        .expect("the installer writes the unit with a heredoc")
        + "cat >\"$UNIT\" <<'EOF'\n".len();
    let end = start + script[start..].find("\nEOF\n").expect("the heredoc ends");
    assert_eq!(
        directives(&script[start..end]),
        directives(&deploy("nexa-signal.service")),
        "deploy/install.sh and deploy/nexa-signal.service must carry the same unit"
    );
}
