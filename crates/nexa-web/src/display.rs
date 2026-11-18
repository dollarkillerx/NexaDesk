//! Which of the host's displays a session shows, and switching to another. One at a
//! time: a switch replaces the capture (`media`), and input (`input`) and the cursor
//! (`cursor`) follow it, so a click lands on the display the page shows.
//!
//! On the `input` data channel:
//!
//! ```text
//! page → host  {t:"d", i, id?}                       show display i: its place in
//!                                                    WebHost::displays, as §3.6; id
//!                                                    (from the list) pins which one,
//!                                                    so a monitor plugged or unplugged
//!                                                    meanwhile can not shift it
//! host → page  {t:"d", i, pts:[w,h], list?, err?}    the display shown now, after a
//!                                                    switch or a refused one; list is
//!                                                    [{n, w, h, id}] (name, pixels), err
//!                                                    "unavailable" or "failed"
//! ```
//!
//! The page learns the list and the first display from `accepted` (`displays`,
//! `display`). Anyone watching may switch: it is about the picture, not input.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::{WebDisplay, WebScreen};

/// More than anyone has; the rest are not offered.
pub(crate) const MAX_DISPLAYS: usize = 16;

/// The display a session shows, shared by its capture, input and cursor threads.
pub(crate) struct Shown(Mutex<(u32, WebScreen)>);

impl Shown {
    pub(crate) fn new(index: u32, screen: WebScreen) -> Arc<Self> {
        Arc::new(Self(Mutex::new((index, screen))))
    }

    fn get(&self) -> (u32, WebScreen) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn screen(&self) -> WebScreen {
        self.get().1
    }

    pub(crate) fn index(&self) -> u32 {
        self.get().0
    }

    /// Only once the new display's capture is running.
    pub(crate) fn set(&self, index: u32, screen: WebScreen) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = (index, screen);
    }
}

/// A display the page asked for: its place in the list it has, and, when the list said,
/// which display that was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Wanted {
    pub index: u32,
    pub id: Option<u32>,
}

impl Wanted {
    /// Where it is in `displays` now: by its id when the page knew it (the list may have
    /// changed since the page got it), else by its place.
    pub(crate) fn find(self, displays: &[WebDisplay]) -> Option<(u32, &WebDisplay)> {
        match self.id {
            Some(id) => displays
                .iter()
                .enumerate()
                .find(|(_, d)| d.screen.display == id)
                .map(|(i, d)| (i as u32, d)),
            None => displays.get(self.index as usize).map(|d| (self.index, d)),
        }
    }
}

/// `{t:"d", i, id?}` from the page.
pub(crate) fn request(data: &[u8]) -> Option<Wanted> {
    #[derive(serde::Deserialize)]
    struct D {
        t: String,
        i: u32,
        id: Option<u32>,
    }
    let d: D = serde_json::from_slice(data).ok()?;
    (d.t == "d").then_some(Wanted {
        index: d.i,
        id: d.id,
    })
}

/// The displays as the page lists them: a name and the size in pixels.
pub(crate) fn list(displays: &[WebDisplay]) -> Value {
    displays
        .iter()
        .take(MAX_DISPLAYS)
        .map(|d| {
            let name: String = d
                .name
                .chars()
                .filter(|c| !c.is_control())
                .take(60)
                .collect();
            json!({
                "n": name.trim(),
                "w": d.screen.size_pixels.0,
                "h": d.screen.size_pixels.1,
                "id": d.screen.display,
            })
        })
        .collect()
}

/// What the page is told after a switch, made or refused. `displays` empty: the list
/// could not be read, and the page keeps the one it has.
pub(crate) fn note(
    index: u32,
    screen: &WebScreen,
    displays: &[WebDisplay],
    err: Option<&str>,
) -> String {
    let mut m = json!({
        "t": "d",
        "i": index,
        // The screen in points, the size the page draws the cursor at.
        "pts": [screen.size_points.0, screen.size_points.1],
    });
    if !displays.is_empty() {
        m["list"] = list(displays);
    }
    if let Some(err) = err {
        m["err"] = err.into();
    }
    m.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::display;

    #[test]
    fn switch_requests_parse() {
        let w = |index, id| Some(Wanted { index, id });
        assert_eq!(request(br#"{"t":"d","i":1}"#), w(1, None));
        assert_eq!(request(br#"{"t":"d","i":1,"id":5}"#), w(1, Some(5)));
        assert_eq!(request(br#"{"t":"d","i":-1}"#), None);
        assert_eq!(request(br#"{"t":"d"}"#), None);
        assert_eq!(request(br#"{"t":"q","m":"auto"}"#), None);
        assert_eq!(request(b"nonsense"), None);
    }

    #[test]
    fn notes_say_what_is_shown_and_what_there_is() {
        let displays = [
            display(1, "Built-in\u{7} Retina Display ", (0.0, 0.0), (3024, 1964)),
            display(5, "DELL U2720Q", (1512.0, -200.0), (3840, 2160)),
        ];
        let m: Value =
            serde_json::from_str(&note(1, &displays[1].screen, &displays, None)).unwrap();
        assert_eq!(m["t"], "d");
        assert_eq!(m["i"], 1);
        assert_eq!(m["pts"], json!([1920.0, 1080.0]));
        assert_eq!(
            m["list"],
            json!([
                {"n": "Built-in Retina Display", "w": 3024, "h": 1964, "id": 1},
                {"n": "DELL U2720Q", "w": 3840, "h": 2160, "id": 5},
            ])
        );
        assert!(m.get("err").is_none());
        // Refused, and the list could not be read: the page keeps its own.
        let m: Value =
            serde_json::from_str(&note(0, &displays[0].screen, &[], Some("unavailable"))).unwrap();
        assert_eq!(m["i"], 0);
        assert_eq!(m["err"], "unavailable");
        assert!(m.get("list").is_none());
    }

    /// The page picked the DELL from [Built-in, DELL, LG]; the DELL was unplugged before
    /// the request arrived. By place, index 1 would now be the LG.
    #[test]
    fn a_pick_follows_its_display_not_its_place() {
        let now = [
            display(1, "Built-in", (0.0, 0.0), (3024, 1964)),
            display(7, "LG", (1512.0, 0.0), (2560, 1440)),
        ];
        let dell = Wanted {
            index: 1,
            id: Some(5),
        };
        assert!(dell.find(&now).is_none());
        let lg = Wanted {
            index: 2,
            id: Some(7),
        };
        assert_eq!(
            lg.find(&now).map(|(i, d)| (i, d.screen.display)),
            Some((1, 7))
        );
        // An older page without ids: by place.
        let by_place = Wanted { index: 1, id: None };
        assert_eq!(by_place.find(&now).map(|(i, _)| i), Some(1));
    }

    #[test]
    fn the_shown_display_changes_for_everyone_holding_it() {
        let a = display(1, "", (0.0, 0.0), (200, 200)).screen;
        let b = display(2, "", (100.0, 0.0), (400, 200)).screen;
        let shown = Shown::new(0, a);
        let other = shown.clone();
        shown.set(1, b);
        assert_eq!((other.index(), other.screen()), (1, b));
    }
}
