//! The page, built into the binary so whoever serves it (the host, or the signaling
//! server for browsers across the internet) serves exactly what it was shipped with.
//! `pake.wasm` is `crates/nexa-web-pake` built by `scripts/build-web-pake.sh`.

pub struct Asset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub body: &'static [u8],
}

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/",
        content_type: "text/html; charset=utf-8",
        body: include_bytes!("../web/index.html"),
    },
    Asset {
        path: "/app.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/app.js"),
    },
    Asset {
        path: "/keys.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/keys.js"),
    },
    Asset {
        path: "/keyboard.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/keyboard.js"),
    },
    Asset {
        path: "/stats.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/stats.js"),
    },
    Asset {
        path: "/touch.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/touch.js"),
    },
    Asset {
        path: "/i18n.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/i18n.js"),
    },
    Asset {
        path: "/style.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../web/style.css"),
    },
    // The terminal: xterm.js and its fit addon (web/vendor/README.txt).
    Asset {
        path: "/vendor/xterm.mjs",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/vendor/xterm.mjs"),
    },
    Asset {
        path: "/vendor/addon-fit.mjs",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/vendor/addon-fit.mjs"),
    },
    Asset {
        path: "/vendor/xterm.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../web/vendor/xterm.css"),
    },
    Asset {
        path: "/term.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/term.js"),
    },
    Asset {
        path: "/pake.wasm",
        content_type: "application/wasm",
        body: include_bytes!("../web/pake.wasm"),
    },
    Asset {
        path: "/icon.png",
        content_type: "image/png",
        body: include_bytes!("../web/icon.png"),
    },
];

pub fn find(path: &str) -> Option<&'static Asset> {
    let path = if path == "/index.html" { "/" } else { path };
    ASSETS.iter().find(|a| a.path == path)
}
