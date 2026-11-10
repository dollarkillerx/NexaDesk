//! The viewer half of §6.9: the host's cursor shown as the local cursor over the
//! video, at the size it has in the video, and hidden while game mode holds the lock.

use std::{collections::VecDeque, sync::Arc};

use nexa_session::proto::CursorMoved;
use nexa_wire::control::{CursorSet, CursorShape};

use crate::{
    deps::{CursorDisplay, LocalCursor},
    input::ViewGeometry,
};

const MAX_SHAPES: usize = 64;
/// Larger than any real cursor; a host sending more is ignored.
const MAX_PNG: usize = 1 << 20;

#[derive(Clone)]
struct Shape {
    id: u64,
    w: u32,
    h: u32,
    hot_x: i32,
    hot_y: i32,
    png: Arc<Vec<u8>>,
}

/// What the local cursor should be.
#[derive(Clone, Debug, PartialEq)]
enum Look {
    Default,
    Hidden,
    Host(LocalCursor),
}

/// Sizes a host cursor for the view: stream pixels → view pixels (the letterbox
/// scale) → points (the view's backing scale).
pub fn local_size(
    w: u32,
    h: u32,
    hot_x: i32,
    hot_y: i32,
    view: &ViewGeometry,
) -> Option<(f64, f64, f64, f64)> {
    let k = view.view_pixels_per_stream_pixel()? / f64::from(view.scale.max(0.01));
    Some((
        f64::from(w) * k,
        f64::from(h) * k,
        f64::from(hot_x) * k,
        f64::from(hot_y) * k,
    ))
}

#[derive(Default)]
pub struct CursorState {
    shapes: VecDeque<Shape>,
    /// The host's last `CursorSet`: `Some(None)` hidden, `None` nothing yet.
    host: Option<Option<u64>>,
    game: bool,
    locked: bool,
    view: ViewGeometry,
    display: Option<Box<dyn CursorDisplay>>,
    shown: Option<Look>,
    pub warps: u64,
}

impl CursorState {
    pub fn attach(&mut self, display: Box<dyn CursorDisplay>) {
        self.display = Some(display);
        self.shown = None;
        self.apply();
    }

    /// Returns the display so the caller can drop it on the right thread.
    pub fn detach(&mut self) -> Option<Box<dyn CursorDisplay>> {
        self.shown = None;
        self.display.take()
    }

    pub fn set_view(&mut self, view: ViewGeometry) {
        self.view = view;
        self.apply();
    }

    pub fn set_game(&mut self, game: bool) {
        self.game = game;
        self.apply();
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
        self.apply();
    }

    pub fn on_shape(&mut self, s: CursorShape) {
        if s.png.is_empty() || s.png.len() > MAX_PNG || s.w == 0 || s.h == 0 {
            return;
        }
        self.shapes.retain(|x| x.id != s.id);
        if self.shapes.len() == MAX_SHAPES {
            self.shapes.pop_front();
        }
        self.shapes.push_back(Shape {
            id: s.id,
            w: s.w,
            h: s.h,
            hot_x: s.hot_x,
            hot_y: s.hot_y,
            png: Arc::new(s.png),
        });
        self.apply();
    }

    pub fn on_set(&mut self, s: CursorSet) {
        self.host = Some((!s.hidden).then_some(s.id));
        self.apply();
    }

    /// The host moved its cursor. Returns the new position in view pixels when the
    /// local cursor was moved there, which only happens while input is captured.
    pub fn on_moved(&mut self, m: &CursorMoved, input_active: bool) -> Option<(f64, f64)> {
        if self.game || !input_active || m.display != self.view.display {
            return None;
        }
        let (x, y) = self.view.to_view(m.x, m.y)?;
        let scale = f64::from(self.view.scale.max(0.01));
        if let Some(d) = self.display.as_mut() {
            d.warp(x / scale, y / scale);
            self.warps += 1;
        }
        Some((x, y))
    }

    fn look(&self) -> Look {
        if self.locked {
            return Look::Hidden;
        }
        if self.game {
            return Look::Default;
        }
        match self.host {
            None => Look::Default,
            Some(None) => Look::Hidden,
            Some(Some(id)) => match self.shapes.iter().find(|s| s.id == id) {
                Some(s) => match local_size(s.w, s.h, s.hot_x, s.hot_y, &self.view) {
                    Some((width_pt, height_pt, hot_x_pt, hot_y_pt)) => Look::Host(LocalCursor {
                        id,
                        png: s.png.clone(),
                        width_pt,
                        height_pt,
                        hot_x_pt,
                        hot_y_pt,
                    }),
                    None => Look::Default,
                },
                None => Look::Default,
            },
        }
    }

    fn apply(&mut self) {
        let look = self.look();
        if self.shown.as_ref() == Some(&look) {
            return;
        }
        let Some(display) = self.display.as_mut() else {
            return;
        };
        match &look {
            Look::Default => display.reset(),
            Look::Hidden => display.show(None),
            Look::Host(c) => display.show(Some(c)),
        }
        self.shown = Some(look);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Show(Option<(u64, f64, f64, f64, f64)>),
        Reset,
        Warp(f64, f64),
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Vec<Call>>>);

    impl CursorDisplay for Fake {
        fn show(&mut self, c: Option<&LocalCursor>) {
            self.0.lock().unwrap().push(Call::Show(
                c.map(|c| (c.id, c.width_pt, c.height_pt, c.hot_x_pt, c.hot_y_pt)),
            ));
        }
        fn reset(&mut self) {
            self.0.lock().unwrap().push(Call::Reset);
        }
        fn warp(&mut self, x: f64, y: f64) {
            self.0.lock().unwrap().push(Call::Warp(x, y));
        }
    }

    fn shape(id: u64) -> CursorShape {
        CursorShape {
            id,
            w: 32,
            h: 48,
            hot_x: 2,
            hot_y: 4,
            png: vec![1, 2, 3],
        }
    }

    /// A 2000×1000 view at Retina scale showing a 4000×2000 stream: half a view pixel
    /// per stream pixel, a quarter point.
    fn view() -> ViewGeometry {
        ViewGeometry {
            view_w: 2000,
            view_h: 1000,
            stream_w: 4000,
            stream_h: 2000,
            scale: 2.0,
            display: 0,
        }
    }

    fn state() -> (CursorState, Fake) {
        let fake = Fake::default();
        let mut s = CursorState::default();
        s.set_view(view());
        s.attach(Box::new(fake.clone()));
        fake.0.lock().unwrap().clear();
        (s, fake)
    }

    #[test]
    fn shapes_are_sized_for_the_view_and_shown_once() {
        let (mut s, fake) = state();
        s.on_shape(shape(7));
        s.on_set(CursorSet {
            id: 7,
            hidden: false,
        });
        s.on_set(CursorSet {
            id: 7,
            hidden: false,
        });
        // 32×48 stream px at 0.25 pt per stream px.
        assert_eq!(
            fake.0.lock().unwrap().clone(),
            vec![Call::Show(Some((7, 8.0, 12.0, 0.5, 1.0)))]
        );
    }

    #[test]
    fn a_resized_view_resizes_the_cursor() {
        let (mut s, fake) = state();
        s.on_shape(shape(7));
        s.on_set(CursorSet {
            id: 7,
            hidden: false,
        });
        s.set_view(ViewGeometry {
            view_w: 4000,
            view_h: 2000,
            ..view()
        });
        let calls = fake.0.lock().unwrap().clone();
        assert_eq!(
            calls.last(),
            Some(&Call::Show(Some((7, 16.0, 24.0, 1.0, 2.0))))
        );
    }

    #[test]
    fn hidden_unknown_game_and_locked() {
        let (mut s, fake) = state();
        s.on_set(CursorSet {
            id: 9,
            hidden: false,
        });
        s.on_set(CursorSet {
            id: 0,
            hidden: true,
        });
        s.on_shape(shape(9));
        s.on_set(CursorSet {
            id: 9,
            hidden: false,
        });
        s.set_game(true);
        s.set_locked(true);
        s.set_locked(false);
        s.set_game(false);
        assert_eq!(
            fake.0.lock().unwrap().clone(),
            vec![
                // Default (no Reset call: the default was already shown) → hidden.
                Call::Show(None),
                Call::Show(Some((9, 8.0, 12.0, 0.5, 1.0))),
                Call::Reset,
                Call::Show(None),
                Call::Reset,
                Call::Show(Some((9, 8.0, 12.0, 0.5, 1.0))),
            ]
        );
    }

    #[test]
    fn host_moves_warp_only_while_controlling_outside_game_mode() {
        let (mut s, fake) = state();
        let m = CursorMoved {
            x: 2000,
            y: 1000,
            display: 0,
        };
        assert_eq!(s.on_moved(&m, false), None);
        assert_eq!(
            s.on_moved(
                &CursorMoved {
                    display: 1,
                    ..m.clone()
                },
                true
            ),
            None
        );
        assert_eq!(s.on_moved(&m, true), Some((1000.25, 500.25)));
        s.set_game(true);
        assert_eq!(s.on_moved(&m, true), None);
        let warps: Vec<Call> = fake
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|c| matches!(c, Call::Warp(..)))
            .cloned()
            .collect();
        // The middle of stream pixel (2000, 1000), in points at 2×.
        assert_eq!(warps, vec![Call::Warp(500.125, 250.125)]);
    }

    #[test]
    fn absurd_shapes_are_ignored() {
        let (mut s, fake) = state();
        s.on_shape(CursorShape {
            png: vec![],
            ..shape(1)
        });
        s.on_shape(CursorShape { w: 0, ..shape(2) });
        s.on_set(CursorSet {
            id: 1,
            hidden: false,
        });
        assert!(
            fake.0.lock().unwrap().is_empty(),
            "still the default cursor"
        );
    }
}
