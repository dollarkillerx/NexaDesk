//! Prints the selected keyboard input source's ID; with an ID, selects that source
//! first. For checking TIS by hand on a Mac (both run on the main thread, as TIS wants).

#[cfg(target_os = "macos")]
fn main() {
    use nexa_input::platform::macos::{current_input_source, select_input_source};
    if let Some(id) = std::env::args().nth(1)
        && let Err(e) = select_input_source(&id)
    {
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
