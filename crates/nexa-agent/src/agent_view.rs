//! What an AI agent is shown of this computer (docs/MCP.md §13): the frontmost
//! window's elements as text, with a ref for each one it can act on, and a lossless
//! screenshot when it asks. The platform reads them ([`AgentView`]); this module turns
//! them into the agent's pixels and the text it reads.
//!
//! The text, one element a line, indented by depth:
//!
//! ```text
//! window "Untitled — TextEdit" (0,0 1568×980)
//!   @e1 button "Close" (14,10 16×16)
//!   @e2 textarea value="hello" focused (0,52 1568×900)
//!   text "Ready" (20,960 60×14)
//! ```
//!
//! Refs number the elements an agent can act on, in reading order, and hold only for
//! the look they came with.

use crate::ScreenInfo;

/// A rectangle in global points: the platform's own coordinates, as `ScreenInfo` has
/// them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// One element of the frontmost window, as the platform read it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct UiNode {
    /// 0 is the window itself.
    pub depth: u16,
    /// A short lower-case role: `button`, `textfield`, `checkbox`, `text`…
    pub role: String,
    /// What it is called: its title, label or description.
    pub name: String,
    /// What it holds: a field's text, a slider's position. Never a password's.
    pub value: Option<String>,
    /// `focused`, `disabled`, `checked`, `selected`.
    pub states: Vec<&'static str>,
    pub bounds: Option<PointRect>,
    /// Something to click, type into or choose: it gets a ref.
    pub interactive: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frontmost {
    /// The frontmost application's name.
    pub app: String,
    /// Its focused window's title.
    pub window: String,
    /// The element with the keyboard focus, in a few words.
    pub focused: String,
    /// A fingerprint of what that element holds ([`fingerprint`] of its text, or for
    /// a password field only of its length). Never shown to the agent; a look
    /// compares it with the last look's, so that typing counts as a change when the
    /// picture barely moves (one character, next to a blinking caret, is below what
    /// tells pictures apart).
    pub focused_state: u64,
}

/// A short stand-in for `value`, to tell whether it changed without keeping it.
pub fn fingerprint(value: impl std::hash::Hash) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BuildHasherDefault::<DefaultHasher>::default().hash_one(value)
}

/// A picture, straight RGBA, top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct UiOptions {
    pub interactive_only: bool,
    /// At most this many elements besides the windows and menus at the top.
    pub max_nodes: usize,
    /// The display shown, in global points. Elements wholly off it are not kept and
    /// do not count against `max_nodes` (what is inside them is still looked at).
    pub area: PointRect,
}

/// What a platform read of the frontmost window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiTree {
    /// Window first, then anything else at the top (open menus, the menu bar), each
    /// followed by its elements in reading order.
    pub nodes: Vec<UiNode>,
    /// The walk stopped before it had seen everything (too many elements, too deep,
    /// too slow): there may be more on the screen than `nodes` has.
    pub cut_short: bool,
}

impl From<Vec<UiNode>> for UiTree {
    fn from(nodes: Vec<UiNode>) -> Self {
        Self {
            nodes,
            cut_short: false,
        }
    }
}

impl PointRect {
    /// Whether the two share any area.
    pub fn overlaps(&self, other: &PointRect) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

/// Reads the screen for an agent. Calls block; they run on a blocking thread.
pub trait AgentView: Send + Sync {
    /// `screen` at `size` pixels, the pointer drawn in.
    fn screenshot(&self, screen: &ScreenInfo, size: (u32, u32)) -> Result<Rgba, String>;
    fn frontmost(&self) -> Frontmost;
    /// The frontmost window's elements, window first, in reading order.
    fn ui(&self, options: &UiOptions) -> Result<UiTree, String>;
    /// Where the pointer is, in global points.
    fn pointer(&self) -> Option<(f64, f64)>;
}

/// Maps global points on `screen` to the agent's pixels of it, `space`.
#[derive(Clone, Copy, Debug)]
pub struct AgentSpace<'a> {
    pub screen: &'a ScreenInfo,
    pub space: (u32, u32),
}

impl AgentSpace<'_> {
    fn scale(&self) -> (f64, f64) {
        let (w, h) = self.screen.size_points;
        (
            f64::from(self.space.0) / w.max(1.0),
            f64::from(self.space.1) / h.max(1.0),
        )
    }

    /// A point, if it is on this screen.
    pub fn point(&self, x: f64, y: f64) -> Option<(i32, i32)> {
        let (rx, ry) = (
            x - self.screen.origin_points.0,
            y - self.screen.origin_points.1,
        );
        let (w, h) = self.screen.size_points;
        if !(0.0..w).contains(&rx) || !(0.0..h).contains(&ry) {
            return None;
        }
        let (kx, ky) = self.scale();
        Some(((rx * kx).floor() as i32, (ry * ky).floor() as i32))
    }

    /// A rectangle clipped to this screen; `None` when nothing of it is on it.
    pub fn rect(&self, r: &PointRect) -> Option<(i32, i32, u32, u32)> {
        let (ox, oy) = self.screen.origin_points;
        let (w, h) = self.screen.size_points;
        let x0 = (r.x - ox).max(0.0);
        let y0 = (r.y - oy).max(0.0);
        let x1 = (r.x + r.w - ox).min(w);
        let y1 = (r.y + r.h - oy).min(h);
        if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
            return None;
        }
        let (kx, ky) = self.scale();
        let (px, py) = ((x0 * kx).round() as i32, (y0 * ky).round() as i32);
        let pw = ((x1 - x0) * kx).round().max(1.0) as u32;
        let ph = ((y1 - y0) * ky).round().max(1.0) as u32;
        Some((px, py, pw, ph))
    }
}

/// Text in quotes on one line, at most `max` characters.
fn quoted(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max) + 2);
    out.push('"');
    for (n, c) in s.chars().enumerate() {
        if n == max {
            out.push('…');
            break;
        }
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' | '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

const NAME_MAX: usize = 80;
const VALUE_MAX: usize = 200;

/// The text an agent reads, and each ref's centre in its pixels (`refs[i]` is
/// `@e{i+1}`). Elements off this screen are left out; so is anything past
/// `max_nodes` lines, which a last line says, as it says a walk that was cut short.
pub fn render(tree: &UiTree, at: AgentSpace<'_>, max_nodes: usize) -> (String, Vec<(i32, i32)>) {
    let nodes = &tree.nodes;
    let mut text = String::new();
    let mut refs = Vec::new();
    let mut lines = 0;
    let mut left_out = 0;
    for node in nodes {
        let rect = node.bounds.as_ref().and_then(|b| at.rect(b));
        // The window always; an element only where it can be seen on this screen.
        if node.depth > 0 && rect.is_none() {
            continue;
        }
        if lines >= max_nodes {
            left_out += 1;
            continue;
        }
        let indent = usize::from(node.depth.min(12)) * 2;
        text.extend(std::iter::repeat_n(' ', indent));
        if node.interactive && node.depth > 0 {
            let (x, y, w, h) = rect.expect("checked above");
            refs.push((x + (w / 2) as i32, y + (h / 2) as i32));
            text.push_str(&format!("@e{} ", refs.len()));
        }
        text.push_str(&node.role);
        if !node.name.is_empty() {
            text.push(' ');
            text.push_str(&quoted(&node.name, NAME_MAX));
        }
        if let Some(v) = &node.value {
            text.push_str(" value=");
            text.push_str(&quoted(v, VALUE_MAX));
        }
        for s in &node.states {
            text.push(' ');
            text.push_str(s);
        }
        if let Some((x, y, w, h)) = rect {
            text.push_str(&format!(" ({x},{y} {w}×{h})"));
        }
        text.push('\n');
        lines += 1;
    }
    if left_out > 0 {
        text.push_str(&format!(
            "… {left_out} more not shown (raise max_nodes, or look at a screenshot)\n"
        ));
    }
    if tree.cut_short {
        text.push_str(
            "… the window has more elements than could be read: some may be missing (look \
             at a screenshot)\n",
        );
    }
    (text, refs)
}

/// Whether two thumbnails show the same thing: few enough pixels differ by much. A
/// blinking caret or a cursor's tick changes a handful; anything that moves, opens or
/// redraws changes more.
pub fn same_picture(a: &Rgba, b: &Rgba) -> bool {
    if (a.width, a.height) != (b.width, b.height) || a.data.len() != b.data.len() {
        return false;
    }
    let pixels = (a.width as usize * a.height as usize).max(1);
    let differing = a
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.data.as_chunks::<4>().0)
        .filter(|(p, q)| (0..3).any(|i| p[i].abs_diff(q[i]) > 24))
        .count();
    // 0.2%: at 160×100, 32 pixels.
    differing * 500 <= pixels
}

/// Encodes `rgba` as an 8-bit RGB PNG (the screen has no transparency).
pub fn png(rgba: &Rgba) -> Result<Vec<u8>, String> {
    let mut rgb = Vec::with_capacity(rgba.data.len() / 4 * 3);
    for p in rgba.data.as_chunks::<4>().0 {
        rgb.extend_from_slice(&p[..3]);
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, rgba.width, rgba.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgb).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> ScreenInfo {
        ScreenInfo {
            display_id: 1,
            origin_points: (0.0, 0.0),
            size_points: (1440.0, 900.0),
            size_pixels: (2880, 1800),
            refresh_hz: 60,
            name: "Built-in".into(),
        }
    }

    fn node(
        depth: u16,
        role: &str,
        name: &str,
        interactive: bool,
        r: (f64, f64, f64, f64),
    ) -> UiNode {
        UiNode {
            depth,
            role: role.into(),
            name: name.into(),
            interactive,
            bounds: Some(PointRect {
                x: r.0,
                y: r.1,
                w: r.2,
                h: r.3,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn elements_get_refs_and_agent_pixels() {
        let s = screen();
        let at = AgentSpace {
            screen: &s,
            space: (1568, 980),
        };
        let mut field = node(1, "textfield", "Search", true, (100.0, 50.0, 200.0, 24.0));
        field.value = Some("he said \"hi\"\nthen left".into());
        field.states = vec!["focused"];
        let nodes = vec![
            node(0, "window", "Untitled", false, (0.0, 0.0, 1440.0, 900.0)),
            node(1, "button", "Close", true, (10.0, 10.0, 14.0, 14.0)),
            field,
            node(1, "text", "Ready", false, (20.0, 880.0, 60.0, 14.0)),
            // Off this screen: on a display to its right.
            node(1, "button", "Elsewhere", true, (1500.0, 10.0, 40.0, 20.0)),
        ];
        let tree = UiTree {
            nodes,
            cut_short: false,
        };
        let (text, refs) = render(&tree, at, 100);
        assert_eq!(
            text,
            "window \"Untitled\" (0,0 1568×980)\n\
             \x20\x20@e1 button \"Close\" (11,11 15×15)\n\
             \x20\x20@e2 textfield \"Search\" value=\"he said \\\"hi\\\" then left\" focused (109,54 218×26)\n\
             \x20\x20text \"Ready\" (22,958 65×15)\n"
        );
        assert_eq!(refs, vec![(18, 18), (218, 67)]);
    }

    #[test]
    fn too_many_lines_are_counted_not_listed() {
        let s = screen();
        let at = AgentSpace {
            screen: &s,
            space: (1440, 900),
        };
        let mut nodes = vec![node(0, "window", "W", false, (0.0, 0.0, 1440.0, 900.0))];
        for i in 0..10 {
            nodes.push(node(
                1,
                "button",
                &format!("B{i}"),
                true,
                (f64::from(i) * 20.0, 0.0, 10.0, 10.0),
            ));
        }
        let mut tree = UiTree {
            nodes,
            cut_short: false,
        };
        let (text, refs) = render(&tree, at, 4);
        assert_eq!(refs.len(), 3);
        assert!(
            text.ends_with("… 7 more not shown (raise max_nodes, or look at a screenshot)\n"),
            "{text}"
        );
        // A walk that stopped early says so too.
        tree.cut_short = true;
        let (text, _) = render(&tree, at, 100);
        assert!(
            text.ends_with("some may be missing (look at a screenshot)\n"),
            "{text}"
        );
    }

    #[test]
    fn a_second_display_maps_from_its_origin() {
        let right = ScreenInfo {
            origin_points: (1440.0, 0.0),
            size_points: (1920.0, 1080.0),
            ..screen()
        };
        let at = AgentSpace {
            screen: &right,
            space: (1568, 882),
        };
        assert_eq!(at.point(1440.0 + 960.0, 540.0), Some((784, 441)));
        assert_eq!(at.point(100.0, 100.0), None);
        assert_eq!(
            at.rect(&PointRect {
                x: 1400.0,
                y: 0.0,
                w: 100.0,
                h: 10.0
            }),
            Some((0, 0, 49, 8)),
            "clipped to the display"
        );
    }

    #[test]
    fn a_caret_is_the_same_picture_and_a_dialog_is_not() {
        let blank = Rgba {
            width: 160,
            height: 100,
            data: vec![200; 160 * 100 * 4],
        };
        let mut caret = blank.clone();
        for y in 40..60 {
            let i = (y * 160 + 80) * 4;
            caret.data[i..i + 3].copy_from_slice(&[0, 0, 0]);
        }
        assert!(same_picture(&blank, &caret));
        let mut dialog = blank.clone();
        for y in 30..70 {
            for x in 40..120 {
                let i = (y * 160 + x) * 4;
                dialog.data[i..i + 3].copy_from_slice(&[40, 40, 40]);
            }
        }
        assert!(!same_picture(&blank, &dialog));
    }

    #[test]
    fn png_is_rgb_of_the_right_size() {
        let p = png(&Rgba {
            width: 3,
            height: 2,
            data: vec![9; 24],
        })
        .unwrap();
        assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(p[16..20].try_into().unwrap()), 3);
        assert_eq!(p[25], 2, "colour type RGB");
    }
}
