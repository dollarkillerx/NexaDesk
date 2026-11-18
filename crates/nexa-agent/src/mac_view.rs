//! What an AI agent is shown of this Mac (docs/MCP.md §13.4): the frontmost window's
//! elements through the Accessibility API, and screenshots from ScreenCaptureKit.
//!
//! The Accessibility permission the agent already holds to inject input is the one
//! reading other apps' elements needs. Every app is given a one-second messaging
//! timeout, so one that hangs cannot hold a look up; a walk also stops after
//! [`MAX_VISITS`] elements or [`WALK_BUDGET`].

use std::{
    ffi::c_void,
    ptr,
    time::{Duration, Instant},
};

use nexa_agent::{
    ScreenInfo,
    agent_view::{AgentView, Frontmost, PointRect, Rgba, UiNode, UiOptions},
};
use objc2_core_foundation::{CFNumber, CFString};

type CFTypeRef = *const c_void;
type AXError = i32;
const AX_SUCCESS: AXError = 0;
const AX_VALUE_CG_POINT: u32 = 1;
const AX_VALUE_CG_SIZE: u32 = 2;

#[repr(C)]
#[derive(Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        e: CFTypeRef,
        attr: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(e: CFTypeRef, attr: CFTypeRef, value: CFTypeRef) -> AXError;
    fn AXUIElementSetMessagingTimeout(e: CFTypeRef, seconds: f32) -> AXError;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> bool;
    fn AXValueGetTypeID() -> usize;
    fn AXUIElementGetTypeID() -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(b: CFTypeRef) -> bool;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(a: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CFTypeRef, i: isize) -> CFTypeRef;
    static kCFBooleanTrue: CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: CFTypeRef) -> CFTypeRef;
    fn CGEventGetLocation(event: CFTypeRef) -> CGPoint;
}

/// An owned CoreFoundation reference, released when dropped.
struct Cf(CFTypeRef);

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: owned (+1) since it was created or copied.
        unsafe { CFRelease(self.0) }
    }
}

impl Cf {
    /// Takes ownership of a +1 reference; `None` for null.
    fn owned(r: CFTypeRef) -> Option<Self> {
        (!r.is_null()).then_some(Self(r))
    }

    fn type_id(&self) -> usize {
        // SAFETY: a valid CF object.
        unsafe { CFGetTypeID(self.0) }
    }
}

/// An attribute's name, made once per use.
fn name(s: &str) -> objc2_core_foundation::CFRetained<CFString> {
    CFString::from_str(s)
}

fn attr(e: &Cf, attribute: &str) -> Option<Cf> {
    let key = name(attribute);
    let mut out: CFTypeRef = ptr::null();
    // SAFETY: `e` is an AXUIElement, the key a CFString, `out` a valid slot; on success
    // `out` holds a +1 reference.
    let status =
        unsafe { AXUIElementCopyAttributeValue(e.0, (&*key as *const CFString).cast(), &mut out) };
    if status == AX_SUCCESS {
        Cf::owned(out)
    } else {
        None
    }
}

fn string(e: &Cf, attribute: &str) -> Option<String> {
    let v = attr(e, attribute)?;
    // SAFETY: type checked; the reference is alive while `v` is.
    unsafe {
        if v.type_id() == CFStringGetTypeID() {
            let s = &*(v.0 as *const CFString);
            Some(s.to_string()).filter(|s| !s.is_empty())
        } else if v.type_id() == CFNumberGetTypeID() {
            let n = &*(v.0 as *const CFNumber);
            n.as_f64().map(|f| {
                if f.fract() == 0.0 && f.abs() < 1e15 {
                    format!("{}", f as i64)
                } else {
                    format!("{f:.2}")
                }
            })
        } else {
            None
        }
    }
}

fn boolean(e: &Cf, attribute: &str) -> Option<bool> {
    let v = attr(e, attribute)?;
    // SAFETY: type checked.
    unsafe {
        if v.type_id() == CFBooleanGetTypeID() {
            Some(CFBooleanGetValue(v.0))
        } else if v.type_id() == CFNumberGetTypeID() {
            (*(v.0 as *const CFNumber)).as_i64().map(|n| n != 0)
        } else {
            None
        }
    }
}

fn number(e: &Cf, attribute: &str) -> Option<i64> {
    let v = attr(e, attribute)?;
    // SAFETY: type checked.
    unsafe {
        if v.type_id() == CFNumberGetTypeID() {
            (*(v.0 as *const CFNumber)).as_i64()
        } else if v.type_id() == CFBooleanGetTypeID() {
            Some(i64::from(CFBooleanGetValue(v.0)))
        } else {
            None
        }
    }
}

fn bounds(e: &Cf) -> Option<PointRect> {
    let (p, s) = (attr(e, "AXPosition")?, attr(e, "AXSize")?);
    let mut point = CGPoint::default();
    let mut size = CGSize::default();
    // SAFETY: AXValues of the checked kinds, written into matching structs.
    unsafe {
        let value_type = AXValueGetTypeID();
        if p.type_id() != value_type || s.type_id() != value_type {
            return None;
        }
        if !AXValueGetValue(p.0, AX_VALUE_CG_POINT, (&mut point as *mut CGPoint).cast())
            || !AXValueGetValue(s.0, AX_VALUE_CG_SIZE, (&mut size as *mut CGSize).cast())
        {
            return None;
        }
    }
    Some(PointRect {
        x: point.x,
        y: point.y,
        w: size.width,
        h: size.height,
    })
}

fn children(e: &Cf) -> Vec<Cf> {
    let Some(array) = attr(e, "AXChildren") else {
        return Vec::new();
    };
    // SAFETY: type checked; each element is retained before the array goes.
    unsafe {
        if array.type_id() != CFArrayGetTypeID() {
            return Vec::new();
        }
        let element = AXUIElementGetTypeID();
        (0..CFArrayGetCount(array.0))
            .filter_map(|i| {
                let v = CFArrayGetValueAtIndex(array.0, i);
                (!v.is_null() && CFGetTypeID(v) == element).then(|| Cf(CFRetain(v)))
            })
            .collect()
    }
}

/// The short role an agent reads, and whether it acts on it.
fn role_of(role: &str, subrole: Option<&str>) -> Option<(&'static str, bool)> {
    Some(match (role, subrole) {
        ("AXWindow", _) => ("window", false),
        ("AXSheet", _) => ("sheet", false),
        ("AXButton", _) => ("button", true),
        ("AXCheckBox", Some("AXSwitch")) => ("switch", true),
        ("AXCheckBox", _) => ("checkbox", true),
        ("AXRadioButton", Some("AXTabButton")) => ("tab", true),
        ("AXRadioButton", _) => ("radio", true),
        ("AXTextField", Some("AXSecureTextField")) => ("password", true),
        ("AXTextField", Some("AXSearchField")) => ("searchfield", true),
        ("AXTextField", _) => ("textfield", true),
        ("AXTextArea", _) => ("textarea", true),
        ("AXComboBox", _) => ("combobox", true),
        ("AXPopUpButton", _) => ("popup", true),
        ("AXMenuButton", _) => ("menubutton", true),
        ("AXLink", _) => ("link", true),
        ("AXMenuItem", _) => ("menuitem", true),
        ("AXMenuBarItem", _) => ("menubaritem", true),
        ("AXSlider", _) => ("slider", true),
        ("AXIncrementor", _) => ("stepper", true),
        ("AXDisclosureTriangle", _) => ("disclosure", true),
        ("AXColorWell", _) => ("colorwell", true),
        ("AXDateField", _) => ("datefield", true),
        ("AXStaticText", _) => ("text", false),
        ("AXHeading", _) => ("heading", false),
        _ => return None,
    })
}

/// Holds a walk to its limits.
struct Walk<'a> {
    options: &'a UiOptions,
    nodes: Vec<UiNode>,
    visits: usize,
    deadline: Instant,
}

/// Elements looked at in one walk, kept or not.
const MAX_VISITS: usize = 6000;
const MAX_DEPTH: u16 = 40;
const WALK_BUDGET: Duration = Duration::from_secs(3);

impl Walk<'_> {
    fn visit(&mut self, e: &Cf, depth: u16, kept_depth: u16) {
        if self.visits >= MAX_VISITS
            || depth > MAX_DEPTH
            || self.nodes.len() >= self.options.max_nodes
            || Instant::now() >= self.deadline
        {
            return;
        }
        self.visits += 1;
        let role = string(e, "AXRole").unwrap_or_default();
        let subrole = string(e, "AXSubrole");
        let mut next_depth = kept_depth;
        if let Some((short, interactive)) = role_of(&role, subrole.as_deref()) {
            let is_text = matches!(short, "text" | "heading");
            let keep = depth == 0 || interactive || (!self.options.interactive_only && is_text);
            if keep {
                self.nodes
                    .push(read_node(e, short, interactive, kept_depth));
                next_depth = kept_depth + 1;
            }
            // Text has no children worth a line.
            if is_text {
                return;
            }
        }
        for child in children(e) {
            self.visit(&child, depth + 1, next_depth);
        }
    }
}

fn read_node(e: &Cf, short: &'static str, interactive: bool, depth: u16) -> UiNode {
    let is_field = matches!(
        short,
        "textfield" | "textarea" | "searchfield" | "combobox" | "datefield"
    );
    let mut name = string(e, "AXTitle")
        .or_else(|| string(e, "AXDescription"))
        .or_else(|| attr(e, "AXTitleUIElement").and_then(|t| string(&t, "AXValue")))
        .unwrap_or_default();
    let mut value = None;
    let mut states = Vec::new();
    match short {
        "text" | "heading" => {
            if name.is_empty() {
                name = string(e, "AXValue").unwrap_or_default();
            }
        }
        "checkbox" | "radio" | "switch" | "tab" | "disclosure" => match number(e, "AXValue") {
            Some(1) => states.push("checked"),
            Some(2) => states.push("mixed"),
            _ => {}
        },
        // Never a password's text.
        "password" => {}
        "slider" | "stepper" => value = string(e, "AXValue"),
        _ if is_field => {
            value = Some(string(e, "AXValue").unwrap_or_default());
            if name.is_empty() {
                name = string(e, "AXPlaceholderValue").unwrap_or_default();
            }
        }
        _ => {}
    }
    if name.is_empty() && interactive {
        // A button with only an icon still says what it is ("close button").
        name = string(e, "AXHelp")
            .or_else(|| string(e, "AXRoleDescription").filter(|d| d != short))
            .unwrap_or_default();
    }
    if boolean(e, "AXFocused") == Some(true) {
        states.push("focused");
    }
    if boolean(e, "AXSelected") == Some(true) {
        states.push("selected");
    }
    if boolean(e, "AXEnabled") == Some(false) {
        states.push("disabled");
    }
    UiNode {
        depth,
        role: short.to_owned(),
        name,
        value,
        states,
        bounds: bounds(e),
        interactive,
    }
}

fn system() -> Option<Cf> {
    // SAFETY: returns a +1 reference.
    Cf::owned(unsafe { AXUIElementCreateSystemWide() })
}

/// The frontmost app, asked to answer within a second, and (for Electron apps) to
/// build its accessibility tree.
fn front_app() -> Option<Cf> {
    let app = attr(&system()?, "AXFocusedApplication")?;
    let key = name("AXManualAccessibility");
    // SAFETY: valid element, key and CF boolean; failures (most apps) are harmless.
    unsafe {
        AXUIElementSetMessagingTimeout(app.0, 1.0);
        AXUIElementSetAttributeValue(app.0, (&*key as *const CFString).cast(), kCFBooleanTrue);
    }
    Some(app)
}

fn front_window(app: &Cf) -> Option<Cf> {
    attr(app, "AXFocusedWindow").or_else(|| attr(app, "AXMainWindow"))
}

pub struct MacView;

impl AgentView for MacView {
    fn screenshot(&self, screen: &ScreenInfo, size: (u32, u32)) -> Result<Rgba, String> {
        nexa_media::macos::screenshot_rgba(screen.display_id, size.0, size.1)
            .map(|(width, height, data)| Rgba {
                width,
                height,
                data,
            })
            .map_err(|e| e.to_string())
    }

    fn frontmost(&self) -> Frontmost {
        let Some(app) = front_app() else {
            return Frontmost::default();
        };
        let window = front_window(&app);
        let focused = attr(&app, "AXFocusedUIElement").map(|f| {
            let role = string(&f, "AXRole").unwrap_or_default();
            let short = role_of(&role, string(&f, "AXSubrole").as_deref()).map_or_else(
                || role.trim_start_matches("AX").to_lowercase(),
                |r| r.0.to_owned(),
            );
            // Never a password's text: only its length.
            let value = if short == "password" {
                number(&f, "AXNumberOfCharacters").map_or_else(String::new, |n| n.to_string())
            } else {
                string(&f, "AXValue").unwrap_or_default()
            };
            let label = string(&f, "AXTitle")
                .or_else(|| string(&f, "AXDescription"))
                .or_else(|| string(&f, "AXPlaceholderValue"))
                .unwrap_or_default();
            let described = if label.is_empty() {
                short
            } else {
                format!("{short} \"{label}\"")
            };
            (described, value)
        });
        Frontmost {
            app: string(&app, "AXTitle").unwrap_or_default(),
            window: window
                .as_ref()
                .and_then(|w| string(w, "AXTitle"))
                .unwrap_or_default(),
            focused_value: focused.as_ref().map(|f| f.1.clone()).unwrap_or_default(),
            focused: focused.map(|f| f.0).unwrap_or_default(),
        }
    }

    fn ui(&self, options: &UiOptions) -> Result<Vec<UiNode>, String> {
        if !nexa_input::platform::macos::accessibility_granted() {
            return Err("this Mac has not given NexaDesk the Accessibility permission".into());
        }
        let app = front_app().ok_or("no app is in front")?;
        let window = front_window(&app).ok_or("the app in front has no window")?;
        let mut walk = Walk {
            options,
            nodes: Vec::new(),
            visits: 0,
            deadline: Instant::now() + WALK_BUDGET,
        };
        walk.visit(&window, 0, 0);
        Ok(walk.nodes)
    }

    fn pointer(&self) -> Option<(f64, f64)> {
        // SAFETY: a fresh event from no source is +1, and only its location is read.
        let event = Cf::owned(unsafe { CGEventCreate(ptr::null()) })?;
        let p = unsafe { CGEventGetLocation(event.0) };
        Some((p.x, p.y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_read_short_and_say_what_acts() {
        assert_eq!(role_of("AXButton", None), Some(("button", true)));
        assert_eq!(
            role_of("AXTextField", Some("AXSecureTextField")),
            Some(("password", true))
        );
        assert_eq!(
            role_of("AXRadioButton", Some("AXTabButton")),
            Some(("tab", true))
        );
        assert_eq!(role_of("AXStaticText", None), Some(("text", false)));
        assert_eq!(role_of("AXGroup", None), None);
    }
}
