//! The host's cursor, sent beside the picture instead of drawn into it, so the page can
//! show it at the browser's own pointer with no delay (as the native viewer does, §6.9).
//!
//! On the `input` data channel, host → browser:
//!
//! ```text
//! {t:"cs", id, w, h, hx, hy, s, png}   a shape, once per id: PNG (base64), hot spot and
//!                                      size in image pixels, s = image pixels per point
//! {t:"cp", x, y, v, id}                where it is now: x, y as fractions of the screen,
//!                                      v = visible, id = the shape in use
//! ```
//!
//! The page tells its own moves from anyone else's by remembering where it put the
//! cursor; only those it did not make are drawn where the host says.

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use data_encoding::BASE64;
use nexa_media::{CursorImage, CursorSource};
use serde_json::json;
use tokio::sync::mpsc;

use crate::{WebScreen, display::Shown};

/// 60 Hz, as the native viewer's cursor thread.
const POLL: Duration = Duration::from_millis(16);
/// Larger shapes are not sent: browsers refuse cursor images much past 128 points.
const MOST_PIXELS: u32 = 256;

pub(crate) struct CursorFeed {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl CursorFeed {
    /// `resync` is set when the page's channel (re)opens: everything is said again.
    /// Positions are fractions of the display `shown` is at the time.
    pub(crate) fn spawn(
        mut source: Box<dyn CursorSource>,
        shown: Arc<Shown>,
        resync: Arc<AtomicBool>,
        out: mpsc::Sender<String>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let join = thread::Builder::new()
            .name("web-cursor".into())
            .spawn(move || {
                let mut sent: HashSet<String> = HashSet::new();
                let mut shape: Option<(Arc<CursorImage>, String)> = None;
                let mut last: Option<String> = None;
                // A message that did not fit in the queue: tried again on the next tick,
                // unless something newer replaces it.
                let mut pending: Option<String> = None;
                while !s.load(Relaxed) {
                    thread::sleep(POLL);
                    if resync.swap(false, Relaxed) {
                        sent.clear();
                        last = None;
                    }
                    if let Some(p) = pending.take()
                        && let Err(mpsc::error::TrySendError::Full(p)) = out.try_send(p)
                    {
                        pending = Some(p);
                        continue;
                    }
                    let sample = source.sample();
                    if let Some(image) = sample.image {
                        let same = shape
                            .as_ref()
                            .is_some_and(|(l, _)| Arc::ptr_eq(l, &image) || **l == *image);
                        if !same {
                            shape = Some((image.clone(), shape_id(&image)));
                        }
                    }
                    let Some((image, id)) = &shape else { continue };
                    if !sent.contains(id) {
                        let Some(message) = shape_message(image, id) else {
                            continue;
                        };
                        // A shape must arrive: wait for room rather than drop it.
                        if out.blocking_send(message).is_err() {
                            return;
                        }
                        sent.insert(id.clone());
                    }
                    let screen = shown.screen();
                    let Some((x, y)) = sample.position.map(|p| fraction(&screen, p)) else {
                        continue;
                    };
                    let now = json!({
                        "t": "cp",
                        "x": (x * 100_000.0).round() / 100_000.0,
                        "y": (y * 100_000.0).round() / 100_000.0,
                        "v": sample.visible,
                        "id": id,
                    })
                    .to_string();
                    if last.as_deref() == Some(now.as_str()) {
                        continue;
                    }
                    last = Some(now.clone());
                    match out.try_send(now) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(m)) => pending = Some(m),
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            })
            .expect("spawn cursor thread");
        Self {
            stop,
            join: Some(join),
        }
    }
}

impl Drop for CursorFeed {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Global host points to fractions of `screen`; off the screen is kept, the page clamps.
fn fraction(screen: &WebScreen, (x, y): (f64, f64)) -> (f64, f64) {
    let (ox, oy) = screen.origin_points;
    let (w, h) = screen.size_points;
    ((x - ox) / w.max(1.0), (y - oy) / h.max(1.0))
}

/// The same image always gets the same id, across samples and sessions.
fn shape_id(image: &CursorImage) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(&image.width.to_le_bytes());
    eat(&image.height.to_le_bytes());
    eat(&image.hot_x.to_le_bytes());
    eat(&image.hot_y.to_le_bytes());
    eat(&image.scale.to_le_bytes());
    eat(&image.rgba);
    format!("{h:016x}")
}

fn shape_message(image: &CursorImage, id: &str) -> Option<String> {
    let (w, h) = (image.width, image.height);
    if w == 0 || h == 0 || w > MOST_PIXELS || h > MOST_PIXELS {
        return None;
    }
    if image.rgba.len() != (w * h * 4) as usize {
        return None;
    }
    let png = nexa_session::cursor::encode_png(w, h, &image.rgba);
    Some(
        json!({
            "t": "cs",
            "id": id,
            "w": w,
            "h": h,
            "hx": image.hot_x,
            "hy": image.hot_y,
            "s": if image.scale > 0.0 { image.scale } else { 1.0 },
            "png": BASE64.encode(&png),
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(fill: u8) -> CursorImage {
        CursorImage {
            width: 4,
            height: 4,
            rgba: vec![fill; 64],
            hot_x: 1.0,
            hot_y: 2.0,
            scale: 2.0,
        }
    }

    #[test]
    fn shape_ids_follow_the_pixels() {
        assert_eq!(shape_id(&image(1)), shape_id(&image(1)));
        assert_ne!(shape_id(&image(1)), shape_id(&image(2)));
    }

    #[test]
    fn positions_are_fractions_of_the_screen() {
        let screen = WebScreen {
            display: 1,
            origin_points: (100.0, 0.0),
            size_points: (1000.0, 500.0),
            size_pixels: (2000, 1000),
        };
        assert_eq!(fraction(&screen, (600.0, 250.0)), (0.5, 0.5));
        assert_eq!(fraction(&screen, (100.0, 0.0)), (0.0, 0.0));
    }

    #[test]
    fn a_shape_is_a_png_with_its_hot_spot() {
        let m: serde_json::Value =
            serde_json::from_str(&shape_message(&image(9), "x").unwrap()).unwrap();
        assert_eq!(m["t"], "cs");
        assert_eq!(m["hx"], 1.0);
        assert_eq!(m["s"], 2.0);
        let png = BASE64
            .decode(m["png"].as_str().unwrap().as_bytes())
            .unwrap();
        assert_eq!(&png[1..4], b"PNG");
        let mut big = image(0);
        big.width = 1000;
        assert!(shape_message(&big, "y").is_none());
    }

    #[test]
    fn feed_sends_the_shape_then_where_it_is() {
        struct Fixed;
        impl CursorSource for Fixed {
            fn sample(&mut self) -> nexa_media::CursorSample {
                nexa_media::CursorSample {
                    image: Some(Arc::new(image(5))),
                    visible: true,
                    position: Some((50.0, 25.0)),
                }
            }
        }
        let screen = WebScreen {
            display: 1,
            origin_points: (0.0, 0.0),
            size_points: (100.0, 100.0),
            size_pixels: (200, 200),
        };
        let (tx, mut rx) = mpsc::channel(8);
        let shown = Shown::new(0, screen);
        let feed = CursorFeed::spawn(
            Box::new(Fixed),
            shown.clone(),
            Arc::new(AtomicBool::new(false)),
            tx,
        );
        let first: serde_json::Value = serde_json::from_str(&rx.blocking_recv().unwrap()).unwrap();
        let second: serde_json::Value = serde_json::from_str(&rx.blocking_recv().unwrap()).unwrap();
        assert_eq!(first["t"], "cs");
        assert_eq!(second["t"], "cp");
        assert_eq!(second["x"], 0.5);
        assert_eq!(second["y"], 0.25);
        assert_eq!(second["id"], first["id"]);
        // Nothing changed: nothing more was said.
        thread::sleep(POLL * 3);
        assert!(rx.try_recv().is_err());
        // Another display shown: the same spot, as a fraction of that one.
        shown.set(
            1,
            WebScreen {
                display: 2,
                origin_points: (50.0, 0.0),
                size_points: (100.0, 50.0),
                size_pixels: (200, 100),
            },
        );
        let third: serde_json::Value = serde_json::from_str(&rx.blocking_recv().unwrap()).unwrap();
        drop(feed);
        assert_eq!(third["t"], "cp");
        assert_eq!(
            (third["x"].clone(), third["y"].clone()),
            (0.0.into(), 0.5.into())
        );
    }
}
