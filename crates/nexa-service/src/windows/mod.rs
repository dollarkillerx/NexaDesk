mod client;
mod install;
mod log;
mod sas;
mod server;
mod service;

/// The service's name with the service control manager.
pub const SERVICE_NAME: &str = "NexaDeskService";

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("install") => install::install(),
        Some("uninstall") => install::uninstall(),
        Some("hello") => client::hello(),
        Some("start") => client::start(&args[1..]),
        Some("run") => service::run(),
        _ => Err(
            "usage: nexa-service install | uninstall | hello | start -- <host arguments>".into(),
        ),
    }
}
