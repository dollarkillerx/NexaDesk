//! Checks the terminal's WebKit bridge for real, off screen and without a host: the
//! page loads xterm.js and says its size, a keystroke posted the way xterm posts it
//! reaches Rust, and output written from Rust shows in xterm's buffer.
//!
//!     cargo run -p nexa-client --example terminal_selftest
//!
//! Prints `PASS` and exits 0, or says what failed and exits 1.

#[cfg(target_os = "macos")]
fn main() {
    use std::{ffi::c_void, sync::mpsc, thread, time::Duration};

    use dispatch2::DispatchQueue;
    use nexa_client::terminal::{TermCmd, TermLink, attach_with, eval, show};
    use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // Far off screen: nothing shows, but WebKit lays it out and runs it.
    let frame = NSRect::new(NSPoint::new(-5000.0, -5000.0), NSSize::new(800.0, 480.0));
    // SAFETY: a plain window with documented arguments.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the window stays alive for the whole run.
    unsafe { window.setReleasedWhenClosed(false) };
    window.orderFrontRegardless();
    let view = window.contentView().expect("content view");

    let (seen_tx, seen_rx) = mpsc::channel::<String>();
    let open = Box::new(move |link: TermLink| {
        let TermLink { id, size, mut cmds } = link;
        let _ = seen_tx.send(format!("ready {}x{}", size.cols, size.rows));
        let seen = seen_tx.clone();
        thread::spawn(move || {
            while let Some(cmd) = cmds.blocking_recv() {
                if let TermCmd::Data(d) = cmd {
                    let typed = String::from_utf8_lossy(&d).into_owned();
                    let _ = seen.send(format!("typed {typed:?}"));
                    show(id, format!("echo:{}\r\n", typed.trim_end()).into_bytes());
                }
            }
        });
    });
    // SAFETY: a live content view.
    let id = unsafe { attach_with(Retained::as_ptr(&view) as *mut c_void, open) }.expect("attach");

    thread::spawn(move || {
        let fail = |why: &str| {
            println!("FAIL: {why}");
            std::process::exit(1);
        };
        match seen_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(m) if m.starts_with("ready ") => println!("page {m}"),
            other => fail(&format!("the page never said it was ready: {other:?}")),
        }
        // A keystroke, posted exactly as the page's onData handler posts one.
        DispatchQueue::main().exec_async(move || {
            eval(
                id,
                r#"window.webkit.messageHandlers.nexa.postMessage(JSON.stringify({t: "d", d: "ls\r"})); "ok""#,
                |_| {},
            )
        });
        match seen_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(m) if m == r#"typed "ls\r""# => println!("rust got {m}"),
            other => fail(&format!("the keystroke did not reach Rust: {other:?}")),
        }
        thread::sleep(Duration::from_millis(500));
        let (line_tx, line_rx) = mpsc::channel();
        DispatchQueue::main().exec_async(move || {
            eval(
                id,
                "nexaTerm.buffer.active.getLine(0).translateToString(true)",
                move |r| {
                    let _ = line_tx.send(r);
                },
            )
        });
        match line_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(line)) if line == "echo:ls" => println!("xterm shows {line:?}"),
            other => fail(&format!("output did not show in xterm: {other:?}")),
        }
        println!("PASS");
        std::process::exit(0);
    });
    app.run();
}

#[cfg(not(target_os = "macos"))]
fn main() {}
