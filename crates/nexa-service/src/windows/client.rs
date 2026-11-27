//! The client side of the control pipe, for trying the service by hand (the app speaks
//! the same protocol).

use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
};

use crate::proto::{Event, PIPE, Request};

fn connect() -> Result<File, String> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE)
        .map_err(|e| format!("the service is not running ({e})"))
}

fn ask(pipe: &mut File, request: &Request) -> Result<(), String> {
    let mut line = serde_json::to_string(request).map_err(|e| e.to_string())?;
    line.push('\n');
    pipe.write_all(line.as_bytes()).map_err(|e| e.to_string())
}

fn events(pipe: File) -> impl Iterator<Item = Result<Event, String>> {
    BufReader::new(pipe).lines().map(|line| {
        let line = line.map_err(|e| e.to_string())?;
        serde_json::from_str(&line).map_err(|e| format!("not understood: {e}"))
    })
}

/// `nexa-service hello`
pub fn hello() -> Result<(), String> {
    let mut pipe = connect()?;
    ask(&mut pipe, &Request::Hello)?;
    match events(pipe).next() {
        Some(Ok(Event::Hello { protocol })) => {
            println!("the service is running (protocol {protocol})");
            Ok(())
        }
        Some(Ok(other)) => Err(format!("unexpected answer: {other:?}")),
        Some(Err(e)) => Err(e),
        None => Err("no answer".into()),
    }
}

/// `nexa-service start -- <host arguments>`: runs until the host ends; ending this program
/// (Ctrl+C) ends the host.
pub fn start(args: &[String]) -> Result<(), String> {
    let args = match args.first().map(String::as_str) {
        Some("--") => &args[1..],
        _ => args,
    };
    let mut pipe = connect()?;
    ask(
        &mut pipe,
        &Request::Start {
            args: args.to_vec(),
            signal_secret: std::env::var("NEXA_SIGNAL_SECRET").ok(),
        },
    )?;
    for event in events(pipe) {
        match event? {
            Event::Started { pid } => {
                println!("the host runs as SYSTEM in this session: pid {pid}")
            }
            Event::Line(line) => println!("{line}"),
            Event::Exited { code } => {
                println!("the host exited with code {code}");
                return Ok(());
            }
            Event::Error(e) => return Err(e),
            other => return Err(format!("unexpected answer: {other:?}")),
        }
    }
    Err("the service closed the connection".into())
}
