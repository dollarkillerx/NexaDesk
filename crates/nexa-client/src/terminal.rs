//! The host's command line in the app (`nexa_session::terminal`): xterm.js, the web
//! page's own copy, in a WKWebView inside a view the UI hands over, wired to one
//! `Terminal` stream of the session.
//!
//! Keys and sizes come from the page through a script message handler; output goes
//! back with `evaluateJavaScript`. WebKit is only touched on the main thread; the
//! stream runs on the session's runtime and posts what arrives to the main queue.

use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    sync::{
        Arc, Once,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

use data_encoding::BASE64;
use dispatch2::DispatchQueue;
use nexa_session::{
    framed::open_bi,
    handshake::FLUSH_TIMEOUT,
    terminal::{CHUNK, TermBody, TermMsg, TermSize},
};
use nexa_wire::framing::StreamType;
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, define_class, msg_send,
    rc::{Allocated, Retained},
    runtime::{AnyClass, AnyObject, AnyProtocol, NSObject, NSObjectProtocol},
};
use objc2_core_foundation::CGRect;
use objc2_foundation::NSString;
use quinn::Connection;
use tokio::sync::mpsc;

use crate::client::Session;

#[link(name = "WebKit", kind = "framework")]
unsafe extern "C" {}

/// From the page to the stream.
pub enum TermCmd {
    Data(Vec<u8>),
    Resize(TermSize),
}

/// A terminal the page is ready for: the session opens its stream.
pub struct TermLink {
    pub id: u64,
    pub size: TermSize,
    pub cmds: mpsc::UnboundedReceiver<TermCmd>,
}

/// From the stream to the page.
enum TermOut {
    Data(Vec<u8>),
    Exit(String),
}

const HANDLER: &str = "nexa";
/// NSViewWidthSizable | NSViewHeightSizable: the terminal fills its view as it resizes.
const FILL: usize = 2 | 16;

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "NexaTerminalBridge"]
    #[ivars = u64]
    struct Bridge;

    unsafe impl NSObjectProtocol for Bridge {}

    impl Bridge {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &AnyObject, message: &AnyObject) {
            // SAFETY: WKScriptMessage's `body`, an object or nil.
            let body: Option<Retained<AnyObject>> = unsafe { msg_send![message, body] };
            // The page posts nothing but strings.
            if let Some(text) = body.and_then(|b| b.downcast::<NSString>().ok()) {
                on_message(*self.ivars(), &text.to_string());
            }
        }
    }
);

impl Bridge {
    fn new(id: u64) -> Retained<Self> {
        let this = Self::alloc().set_ivars(id);
        // SAFETY: NSObject's init takes no arguments.
        unsafe { msg_send![super(this), init] }
    }

    /// WebKit takes a handler that says it is a WKScriptMessageHandler.
    fn conform() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            if let Some(protocol) = AnyProtocol::get(c"WKScriptMessageHandler") {
                // SAFETY: a class this module defined, and a protocol the runtime knows.
                unsafe {
                    objc2::ffi::class_addProtocol(
                        Self::class() as *const AnyClass as *mut AnyClass,
                        protocol,
                    )
                };
            }
        });
    }
}

/// What starts the stream once the page is ready: the session's, or a test's.
pub type Opener = Box<dyn Fn(TermLink)>;

struct Pane {
    webview: Retained<AnyObject>,
    controller: Retained<AnyObject>,
    open: Opener,
    /// Set once the page said its size and the stream was asked for.
    cmds: Option<mpsc::UnboundedSender<TermCmd>>,
}

thread_local! {
    /// Main thread only: the panes showing, by id.
    static PANES: RefCell<HashMap<u64, Pane>> = RefCell::new(HashMap::new());
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Shows a terminal in `parent` (an NSView) for `session`. Main thread only.
/// The shell starts once the page has laid itself out and knows its size.
///
/// # Safety
/// `parent` must be a live NSView.
pub(crate) unsafe fn attach(session: Arc<Session>, parent: *mut c_void) -> Result<u64, String> {
    // SAFETY: per the contract.
    unsafe { attach_with(parent, Box::new(move |link| session.open_terminal(link))) }
}

/// [`attach`] with whatever should open the stream, for the self-test example.
///
/// # Safety
/// `parent` must be a live NSView.
pub unsafe fn attach_with(parent: *mut c_void, open: Opener) -> Result<u64, String> {
    MainThreadMarker::new().ok_or("not on the main thread")?;
    if parent.is_null() {
        return Err("no view".into());
    }
    // SAFETY: a live NSView, per the contract.
    let parent: &AnyObject = unsafe { &*(parent as *const AnyObject) };
    let config_class = AnyClass::get(c"WKWebViewConfiguration").ok_or("WebKit is missing")?;
    let webview_class = AnyClass::get(c"WKWebView").ok_or("WebKit is missing")?;
    Bridge::conform();
    let id = NEXT_ID.fetch_add(1, Relaxed);
    let bridge = Bridge::new(id);
    // SAFETY: documented WebKit and AppKit messages with the argument types they take.
    let (webview, controller) = unsafe {
        let config: Retained<AnyObject> = msg_send![config_class, new];
        let controller: Retained<AnyObject> = msg_send![&*config, userContentController];
        let name = NSString::from_str(HANDLER);
        let _: () = msg_send![&*controller, addScriptMessageHandler: &*bridge, name: &*name];
        let frame: CGRect = msg_send![parent, bounds];
        let allocated: Allocated<AnyObject> = msg_send![webview_class, alloc];
        let webview: Retained<AnyObject> =
            msg_send![allocated, initWithFrame: frame, configuration: &*config];
        let _: () = msg_send![&*webview, setAutoresizingMask: FILL];
        let _: () = msg_send![parent, addSubview: &*webview];
        let html = NSString::from_str(&page());
        let none: Option<&AnyObject> = None;
        let _: Option<Retained<AnyObject>> =
            msg_send![&*webview, loadHTMLString: &*html, baseURL: none];
        (webview, controller)
    };
    PANES.with(|p| {
        p.borrow_mut().insert(
            id,
            Pane {
                webview,
                controller,
                open,
                cmds: None,
            },
        )
    });
    Ok(id)
}

/// Takes the terminal away; its stream ends, and the shell with it. Main thread only.
pub fn detach(id: u64) {
    let Some(pane) = PANES.with(|p| p.borrow_mut().remove(&id)) else {
        return;
    };
    let name = NSString::from_str(HANDLER);
    // SAFETY: documented messages; the controller holds the bridge until this.
    unsafe {
        let _: () = msg_send![&*pane.controller, removeScriptMessageHandlerForName: &*name];
        let _: () = msg_send![&*pane.webview, removeFromSuperview];
    }
    drop(pane.cmds);
}

/// What the page says: `{t:"ready"|"r", c, r}` its size, `{t:"d", d}` typed text.
fn on_message(id: u64, text: &str) {
    let Ok(m) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let size = || TermSize {
        cols: m["c"].as_u64().unwrap_or(80).min(1000) as u32,
        rows: m["r"].as_u64().unwrap_or(24).min(500) as u32,
    };
    PANES.with(|p| {
        let mut panes = p.borrow_mut();
        let Some(pane) = panes.get_mut(&id) else {
            return;
        };
        match m["t"].as_str() {
            Some("ready") if pane.cmds.is_none() => {
                let (tx, rx) = mpsc::unbounded_channel();
                pane.cmds = Some(tx);
                (pane.open)(TermLink {
                    id,
                    size: size(),
                    cmds: rx,
                });
            }
            Some("d") => {
                if let (Some(tx), Some(d)) = (&pane.cmds, m["d"].as_str()) {
                    let _ = tx.send(TermCmd::Data(d.as_bytes().to_vec()));
                }
            }
            Some("r") => {
                if let Some(tx) = &pane.cmds {
                    let _ = tx.send(TermCmd::Resize(size()));
                }
            }
            _ => {}
        }
    });
}

/// Shows `data` as if the host had sent it (the self-test example's echo).
pub fn show(id: u64, data: Vec<u8>) {
    post(id, TermOut::Data(data));
}

/// Runs `js` in the terminal's page and hands its result, as text, to `done` (main thread).
pub fn eval(id: u64, js: &str, done: impl Fn(Option<String>) + 'static) {
    PANES.with(|p| {
        let panes = p.borrow();
        let Some(pane) = panes.get(&id) else {
            return done(None);
        };
        let js = NSString::from_str(js);
        let block = block2::RcBlock::new(move |result: *mut AnyObject, _error: *mut AnyObject| {
            // SAFETY: WebKit hands a valid object or nil for the duration of the call.
            let text = unsafe { result.as_ref() }
                .and_then(|r| r.downcast_ref::<NSString>())
                .map(|s| s.to_string());
            done(text);
        });
        // SAFETY: a documented message, with a completion block of the documented type.
        unsafe {
            let _: () =
                msg_send![&*pane.webview, evaluateJavaScript: &*js, completionHandler: &*block];
        }
    });
}

fn post(id: u64, out: TermOut) {
    DispatchQueue::main().exec_async(move || deliver(id, out));
}

fn deliver(id: u64, out: TermOut) {
    let js = match out {
        TermOut::Data(d) => format!("nexaWrite('{}')", BASE64.encode(&d)),
        TermOut::Exit(why) => format!("nexaExit({})", serde_json::Value::String(why)),
    };
    PANES.with(|p| {
        let panes = p.borrow();
        let Some(pane) = panes.get(&id) else { return };
        let js = NSString::from_str(&js);
        let done: Option<&block2::Block<dyn Fn(*mut AnyObject, *mut AnyObject)>> = None;
        // SAFETY: a documented message; no completion handler.
        unsafe {
            let _: () =
                msg_send![&*pane.webview, evaluateJavaScript: &*js, completionHandler: done];
        }
    });
}

/// The stream for one terminal, on the session's runtime, until either side ends it.
pub(crate) async fn run(conn: Connection, link: TermLink) {
    let TermLink { id, size, mut cmds } = link;
    let (mut send, mut recv) = match open_bi(&conn, StreamType::Terminal).await {
        Ok(s) => s,
        Err(e) => return post(id, TermOut::Exit(e.to_string())),
    };
    if let Err(e) = send.send(&TermMsg::from(TermBody::Open(size))).await {
        return post(id, TermOut::Exit(e.to_string()));
    }
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(TermCmd::Data(d)) => {
                    for piece in d.chunks(CHUNK) {
                        if send.send(&TermMsg::from(TermBody::Data(piece.to_vec()))).await.is_err() {
                            break;
                        }
                    }
                }
                Some(TermCmd::Resize(s)) => {
                    let _ = send.send(&TermMsg::from(TermBody::Resize(s))).await;
                }
                // The terminal was closed here.
                None => break,
            },
            msg = recv.next::<TermMsg>() => match msg {
                Ok(Some(TermMsg { body: Some(TermBody::Data(d)) })) => post(id, TermOut::Data(d)),
                Ok(Some(TermMsg { body: Some(TermBody::Exit(why)) })) => {
                    post(id, TermOut::Exit(why));
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    post(id, TermOut::Exit("closed".into()));
                    break;
                }
                Err(e) => {
                    post(id, TermOut::Exit(e.to_string()));
                    break;
                }
            },
        }
    }
    send.finish_and_flush(FLUSH_TIMEOUT).await;
}

/// The page: xterm.js and its fit addon, loaded from strings (no files, no server).
fn page() -> String {
    const XTERM: &str = include_str!("../../nexa-web-page/web/vendor/xterm.mjs");
    const FIT: &str = include_str!("../../nexa-web-page/web/vendor/addon-fit.mjs");
    const CSS: &str = include_str!("../../nexa-web-page/web/vendor/xterm.css");
    let json = |s: &str| serde_json::Value::String(s.to_owned()).to_string();
    format!(
        r##"<!doctype html><html><head><meta charset="utf-8"><style>{CSS}
html,body{{margin:0;height:100%;background:#161616;overflow:hidden}}
#t{{position:absolute;inset:6px 0 0 8px}}
</style></head><body><div id="t"></div><script type="module">
const send = (m) => window.webkit.messageHandlers.{HANDLER}.postMessage(JSON.stringify(m));
const load = (src) => import(URL.createObjectURL(new Blob([src], {{ type: "text/javascript" }})));
const {{ Terminal }} = await load({xterm});
const {{ FitAddon }} = await load({fit});
const term = new Terminal({{
  fontFamily: "SF Mono, Menlo, monospace", fontSize: 13, cursorBlink: true, macOptionIsMeta: true,
  theme: {{ background: "#161616", foreground: "#f4f4f4", cursor: "#f4f4f4" }},
}});
const fit = new FitAddon();
term.loadAddon(fit);
term.open(document.getElementById("t"));
window.nexaTerm = term;
fit.fit();
term.onData((d) => send({{ t: "d", d }}));
term.onResize(({{ cols, rows }}) => send({{ t: "r", c: cols, r: rows }}));
new ResizeObserver(() => fit.fit()).observe(document.body);
window.nexaWrite = (b64) => term.write(Uint8Array.from(atob(b64), (c) => c.charCodeAt(0)));
window.nexaExit = (why) => term.write("\r\n\x1b[2m[" + why + "]\x1b[0m\r\n");
send({{ t: "ready", c: term.cols, r: term.rows }});
term.focus();
</script></body></html>"##,
        xterm = json(XTERM),
        fit = json(FIT),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_page_carries_xterm_and_its_bridge() {
        let page = super::page();
        assert!(page.contains("messageHandlers.nexa.postMessage"));
        assert!(page.contains("nexaWrite") && page.contains("nexaExit"));
        // The libraries go in as JSON strings: no stray script end can cut the page.
        assert_eq!(page.matches("</script>").count(), 1);
    }
}
