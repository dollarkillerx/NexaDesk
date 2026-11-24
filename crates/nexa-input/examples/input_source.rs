//! Checks TIS by hand on a Mac, on the main thread as TIS wants:
//! no argument prints the selected input source; `list` the enabled ones; `next`,
//! `ascii` or `japanese` does what 🌐, 英数 or かな does; anything else selects that ID.

#[cfg(target_os = "macos")]
fn main() {
    use nexa_input::SwitchAction;
    use nexa_input::platform::macos::{
        current_input_source, enabled_input_sources, globe_changes_input_source,
        select_input_source, switch_input_source,
    };
    let result = match std::env::args().nth(1).as_deref() {
        None => Ok(()),
        Some("list") => {
            for s in enabled_input_sources() {
                println!("{}  ascii={} {:?}", s.id, s.ascii, s.languages);
            }
            println!(
                "globe changes input source: {}",
                globe_changes_input_source()
            );
            Ok(())
        }
        Some("next") => switch_input_source(SwitchAction::Next).map(drop),
        Some("ascii") => switch_input_source(SwitchAction::Ascii).map(drop),
        Some("japanese") => switch_input_source(SwitchAction::Japanese).map(drop),
        Some(id) => select_input_source(id),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
    println!(
        "{}",
        current_input_source().unwrap_or_else(|| "(none)".into())
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {}
