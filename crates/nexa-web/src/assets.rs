//! The page, built into the binary so the host serves exactly what it was shipped with.
//! `pake.wasm` is `crates/nexa-web-pake` built by `scripts/build-web-pake.sh`.

pub(crate) struct Asset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub body: &'static [u8],
}

pub(crate) const ASSETS: &[Asset] = &[
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
        path: "/i18n.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/i18n.js"),
    },
    Asset {
        path: "/style.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../web/style.css"),
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

pub(crate) fn find(path: &str) -> Option<&'static Asset> {
    let path = if path == "/index.html" { "/" } else { path };
    ASSETS.iter().find(|a| a.path == path)
}
