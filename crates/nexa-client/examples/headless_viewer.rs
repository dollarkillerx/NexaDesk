//! Connects to an agent through the real C ABI and prints what happens, without a view.
//! Video is decoded and thrown away; no window opens, input is never captured.
//! The real viewer is the Avalonia app; this exists to try a host before it does.
//!
//! `cargo run -p nexa-client --example headless_viewer -- <ip:port> <one-time-code>`
//! `cargo run -p nexa-client --example headless_viewer -- <ip:port> known <host-key-hex>`

use std::ffi::c_void;

use nexa_client::{
    abi,
    proto::{ConnectAuth, ConnectRequest, Event, EventBody},
};
use nexa_wire::control::Mode;
use prost::Message;

unsafe extern "C" fn on_event(_: *mut c_void, pb: *const u8, len: usize) {
    // SAFETY: valid for the duration of the call.
    let bytes = unsafe { std::slice::from_raw_parts(pb, len) };
    let Ok(event) = Event::decode(bytes) else {
        return;
    };
    match event.body {
        Some(EventBody::Stats(s)) => println!(
            "decoded={} presented={} lost={} rtt={}us",
            s.decoded, s.presented, s.lost, s.rtt_us
        ),
        Some(EventBody::Disconnected(d)) => {
            println!("disconnected: reason {} ({})", d.reason, d.message);
            std::process::exit(0);
        }
        Some(body) => println!("{body:?}"),
        None => {}
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let auth = match args.as_slice() {
        [_, code] => ConnectAuth::Code(code.clone()),
        [_, kind, key] if kind == "known" => ConnectAuth::KnownHost(key.clone()),
        _ => {
            eprintln!(
                "usage: headless_viewer <ip:port> <one-time-code> | <ip:port> known <host-key>"
            );
            std::process::exit(2);
        }
    };
    let mut client = std::ptr::null_mut();
    // SAFETY: valid pointers; the callback ignores `user`.
    let status = unsafe {
        abi::nexa_init(
            std::ptr::null(),
            0,
            Some(on_event),
            std::ptr::null_mut(),
            &mut client,
        )
    };
    assert_eq!(status, abi::NEXA_OK);
    let request = ConnectRequest {
        address: args[0].clone(),
        tag: 1,
        mode: Mode::Balanced as i32,
        fps: 0,
        auth: Some(auth),
    }
    .encode_to_vec();
    let mut session = std::ptr::null_mut();
    // SAFETY: valid buffers and handle.
    let status =
        unsafe { abi::nexa_connect(client, request.as_ptr(), request.len(), &mut session) };
    assert_eq!(status, abi::NEXA_OK);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
