//! What an AI agent is shown of this PC (docs/MCP.md §13, G2): the foreground window's
//! elements through UI Automation, screenshots through GDI, and actions on elements
//! through UI Automation's patterns, as `mac_view` does with the Accessibility API.
//!
//! Points are physical pixels on Windows (`real_windows`), which is what UI Automation
//! and `GetCursorPos` give a per-monitor DPI-aware process. Every call joins the
//! multithreaded COM apartment of the blocking thread it runs on, so the elements kept
//! with the last tree can be used from whichever thread acts on them.

// UI Automation's constants keep their SDK names (`UIA_ButtonControlTypeId`).
#![allow(non_upper_case_globals)]

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use nexa_agent::{
    ScreenInfo,
    agent_view::{
        Act, AgentView, Element, Frontmost, PointRect, Rgba, UiNode, UiOptions, UiTree, fingerprint,
    },
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, POINT},
        System::{
            Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx},
            Threading::{
                OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            },
        },
        UI::{
            Accessibility::*,
            WindowsAndMessaging::{
                GetCursorPos, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
            },
        },
    },
    core::{BSTR, Interface, PWSTR},
};

/// Elements looked at in one walk, kept or not.
const MAX_VISITS: usize = 6000;
const MAX_DEPTH: u16 = 40;
const WALK_BUDGET: Duration = Duration::from_secs(3);
/// How long UI Automation waits for an app that does not answer.
const APP_TIMEOUT_MS: u32 = 1500;

/// UI Automation, in this thread's multithreaded apartment.
fn automation() -> Result<IUIAutomation, String> {
    // SAFETY: joining the MTA is harmless when the thread already has (S_FALSE); a
    // thread in another apartment gets an error, and CoCreateInstance says so.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let a: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("UI Automation is not available: {e}"))?;
        if let Ok(a2) = a.cast::<IUIAutomation2>() {
            let _ = a2.SetTransactionTimeout(APP_TIMEOUT_MS);
            let _ = a2.SetConnectionTimeout(APP_TIMEOUT_MS);
        }
        Ok(a)
    }
}

/// The short role an agent reads, and whether it acts on it.
fn role_of(kind: UIA_CONTROLTYPE_ID, password: bool) -> Option<(&'static str, bool)> {
    Some(match kind {
        UIA_WindowControlTypeId => ("window", false),
        UIA_ButtonControlTypeId | UIA_SplitButtonControlTypeId => ("button", true),
        UIA_CheckBoxControlTypeId => ("checkbox", true),
        UIA_RadioButtonControlTypeId => ("radio", true),
        UIA_EditControlTypeId if password => ("password", true),
        UIA_EditControlTypeId => ("textfield", true),
        UIA_ComboBoxControlTypeId => ("combobox", true),
        UIA_HyperlinkControlTypeId => ("link", true),
        UIA_ListItemControlTypeId | UIA_TreeItemControlTypeId | UIA_DataItemControlTypeId => {
            ("row", true)
        }
        UIA_MenuItemControlTypeId => ("menuitem", true),
        UIA_TabItemControlTypeId => ("tab", true),
        UIA_SliderControlTypeId => ("slider", true),
        UIA_SpinnerControlTypeId => ("stepper", true),
        UIA_HeaderItemControlTypeId => ("header", true),
        UIA_TextControlTypeId => ("text", false),
        UIA_ImageControlTypeId => ("image", false),
        UIA_MenuControlTypeId => ("menu", false),
        UIA_MenuBarControlTypeId => ("menubar", false),
        _ => return None,
    })
}

/// What one walk asks of each element, in one round trip per element's children.
struct Cache {
    request: IUIAutomationCacheRequest,
    /// The control view: what UI Automation itself thinks a person would see.
    view: IUIAutomationCondition,
}

impl Cache {
    fn new(a: &IUIAutomation) -> Result<Self, String> {
        let fail = |e: windows::core::Error| format!("UI Automation: {e}");
        // SAFETY: plain COM calls on valid objects.
        unsafe {
            let request = a.CreateCacheRequest().map_err(fail)?;
            for property in [
                UIA_ControlTypePropertyId,
                UIA_NamePropertyId,
                UIA_HelpTextPropertyId,
                UIA_BoundingRectanglePropertyId,
                UIA_IsEnabledPropertyId,
                UIA_HasKeyboardFocusPropertyId,
                UIA_IsPasswordPropertyId,
            ] {
                request.AddProperty(property).map_err(fail)?;
            }
            for pattern in [
                UIA_ValuePatternId,
                UIA_TogglePatternId,
                UIA_SelectionItemPatternId,
            ] {
                request.AddPattern(pattern).map_err(fail)?;
            }
            let view = a.ControlViewCondition().map_err(fail)?;
            request.SetTreeFilter(&view).map_err(fail)?;
            Ok(Self { request, view })
        }
    }

    fn children(&self, e: &IUIAutomationElement) -> Vec<IUIAutomationElement> {
        // SAFETY: plain COM calls on valid objects.
        unsafe {
            let Ok(found) = e.FindAllBuildCache(TreeScope_Children, &self.view, &self.request)
            else {
                return Vec::new();
            };
            let n = found.Length().unwrap_or(0);
            (0..n).filter_map(|i| found.GetElement(i).ok()).collect()
        }
    }
}

fn text(b: windows::core::Result<BSTR>) -> String {
    b.map(|s| s.to_string().trim().to_owned())
        .unwrap_or_default()
}

fn bounds(e: &IUIAutomationElement) -> Option<PointRect> {
    // SAFETY: a cached property of a valid element.
    let r = unsafe { e.CachedBoundingRectangle() }.ok()?;
    Some(PointRect {
        x: f64::from(r.left),
        y: f64::from(r.top),
        w: f64::from(r.right - r.left),
        h: f64::from(r.bottom - r.top),
    })
}

/// An element kept with the last tree, to act on.
struct UiaElement(IUIAutomationElement);

// SAFETY: UI Automation's client objects live in the multithreaded apartment that
// every thread using them here has joined; one blocking thread uses them at a time.
unsafe impl Send for UiaElement {}
unsafe impl Sync for UiaElement {}

struct Walk<'a> {
    cache: &'a Cache,
    options: &'a UiOptions,
    nodes: Vec<UiNode>,
    visits: usize,
    deadline: Instant,
    cut_short: bool,
}

impl Walk<'_> {
    fn visit(&mut self, e: &IUIAutomationElement, depth: u16, kept_depth: u16) {
        if self.visits >= MAX_VISITS
            || depth > MAX_DEPTH
            || self.nodes.len() >= self.options.max_nodes
            || Instant::now() >= self.deadline
        {
            self.cut_short = true;
            return;
        }
        self.visits += 1;
        // SAFETY: cached properties of a valid element.
        let (kind, password) = unsafe {
            (
                e.CachedControlType().unwrap_or_default(),
                e.CachedIsPassword().is_ok_and(|b| b.as_bool()),
            )
        };
        let here = bounds(e);
        let mut next_depth = kept_depth;
        if let Some((short, interactive)) = role_of(kind, password) {
            let is_text = matches!(short, "text" | "image");
            let named = !text(unsafe { e.CachedName() }).is_empty();
            let interactive = interactive || (short == "image" && named);
            let wanted = depth == 0 || interactive || (!self.options.interactive_only && is_text);
            let shown = depth == 0
                || here.is_some_and(|b| b.w >= 1.0 && b.h >= 1.0 && b.overlaps(&self.options.area));
            if wanted && shown {
                self.nodes
                    .push(read_node(e, short, interactive, kept_depth, here));
                next_depth = kept_depth + 1;
            }
            if is_text && !interactive {
                return;
            }
        }
        // A part wholly off the display holds nothing on it (see `mac_view`).
        if depth > 0
            && here.is_some_and(|b| b.w >= 1.0 && b.h >= 1.0 && !b.overlaps(&self.options.area))
        {
            return;
        }
        for child in self.cache.children(e) {
            self.visit(&child, depth + 1, next_depth);
        }
    }
}

fn read_node(
    e: &IUIAutomationElement,
    short: &'static str,
    interactive: bool,
    depth: u16,
    bounds: Option<PointRect>,
) -> UiNode {
    // SAFETY: cached properties and patterns of a valid element.
    unsafe {
        let mut name = text(e.CachedName());
        let mut value = None;
        let mut states = Vec::new();
        match short {
            // Never a password's text.
            "password" => {}
            "textfield" | "combobox" => {
                if let Ok(p) = e.GetCachedPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                {
                    value = Some(text(p.CachedValue()));
                }
            }
            _ => {}
        }
        if let Ok(p) = e.GetCachedPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) {
            match p.CachedToggleState() {
                Ok(ToggleState_On) => states.push("checked"),
                Ok(ToggleState_Indeterminate) => states.push("mixed"),
                _ => {}
            }
        }
        if let Ok(p) =
            e.GetCachedPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            && p.CachedIsSelected().is_ok_and(|b| b.as_bool())
        {
            states.push("selected");
        }
        if e.CachedHasKeyboardFocus().is_ok_and(|b| b.as_bool()) {
            states.push("focused");
        }
        if e.CachedIsEnabled().is_ok_and(|b| !b.as_bool()) {
            states.push("disabled");
        }
        if name.is_empty() && interactive {
            name = text(e.CachedHelpText());
        }
        UiNode {
            depth,
            role: short.to_owned(),
            name,
            value,
            states,
            bounds,
            interactive,
            element: interactive.then(|| Element(Arc::new(UiaElement(e.clone())))),
        }
    }
}

/// The foreground window's program, by its file name ("notepad").
fn program_of(window: HWND) -> String {
    // SAFETY: plain Win32 calls; the process handle is closed after use.
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&mut pid));
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(process);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn title_of(window: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: a plain Win32 call into a buffer of the given size.
    let n = unsafe { GetWindowTextW(window, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub struct WinView;

impl AgentView for WinView {
    fn screenshot(&self, screen: &ScreenInfo, size: (u32, u32)) -> Result<Rgba, String> {
        let rect = (
            screen.origin_points.0 as i32,
            screen.origin_points.1 as i32,
            screen.size_pixels.0,
            screen.size_pixels.1,
        );
        nexa_media::windows::screenshot_rgba(rect, size.0, size.1)
            .map(|(width, height, data)| Rgba {
                width,
                height,
                data,
            })
            .map_err(|e| e.to_string())
    }

    fn frontmost(&self) -> Frontmost {
        // SAFETY: a plain Win32 call.
        let window = unsafe { GetForegroundWindow() };
        if window.is_invalid() {
            return Frontmost::default();
        }
        let mut front = Frontmost {
            app: program_of(window),
            window: title_of(window),
            ..Frontmost::default()
        };
        let Ok(a) = automation() else {
            return front;
        };
        // SAFETY: plain COM calls on valid objects.
        unsafe {
            if let Ok(f) = a.GetFocusedElement() {
                let kind = f.CurrentControlType().unwrap_or_default();
                let password = f.CurrentIsPassword().is_ok_and(|b| b.as_bool());
                let short = role_of(kind, password).map_or("element", |r| r.0);
                let name = text(f.CurrentName());
                front.focused = if name.is_empty() {
                    short.to_owned()
                } else {
                    format!("{short} \"{name}\"")
                };
                front.focused_state = if password {
                    0
                } else {
                    f.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                        .map_or(0, |p| fingerprint(text(p.CurrentValue())))
                };
            }
        }
        front
    }

    fn ui(&self, options: &UiOptions) -> Result<UiTree, String> {
        let a = automation()?;
        let cache = Cache::new(&a)?;
        // SAFETY: plain Win32 and COM calls on valid objects.
        let (window, menus) = unsafe {
            let foreground = GetForegroundWindow();
            let window = (!foreground.is_invalid())
                .then(|| {
                    a.ElementFromHandleBuildCache(foreground, &cache.request)
                        .ok()
                })
                .flatten();
            // An open context menu is a window of its own, at the desktop's top level.
            let menus: Vec<IUIAutomationElement> = a
                .GetRootElementBuildCache(&cache.request)
                .map(|root| cache.children(&root))
                .unwrap_or_default()
                .into_iter()
                .filter(|m| {
                    m.CachedControlType()
                        .is_ok_and(|k| k == UIA_MenuControlTypeId)
                })
                .collect();
            (window, menus)
        };
        if window.is_none() && menus.is_empty() {
            return Err("no window is in front".into());
        }
        let mut walk = Walk {
            cache: &cache,
            options,
            nodes: Vec::new(),
            visits: 0,
            deadline: Instant::now() + WALK_BUDGET,
            cut_short: false,
        };
        if let Some(w) = &window {
            walk.visit(w, 0, 0);
        }
        for m in &menus {
            walk.visit(m, 0, 0);
        }
        Ok(UiTree {
            nodes: walk.nodes,
            cut_short: walk.cut_short,
        })
    }

    fn pointer(&self) -> Option<(f64, f64)> {
        let mut p = POINT::default();
        // SAFETY: a plain Win32 call.
        unsafe { GetCursorPos(&mut p) }.ok()?;
        Some((f64::from(p.x), f64::from(p.y)))
    }

    fn act(&self, element: &Element, act: &Act) -> Result<(), String> {
        let e = &element
            .0
            .downcast_ref::<UiaElement>()
            .ok_or("not an element of this PC")?
            .0;
        // Joins this thread to the apartment the element lives in.
        let _ = automation()?;
        let gone = |e: windows::core::Error| {
            format!("could not do it ({e}): the element may be gone; call nexa_ui again")
        };
        // SAFETY: plain COM calls on a valid element and its patterns.
        unsafe {
            match act {
                Act::Focus => e.SetFocus().map_err(gone),
                Act::SetValue(t) => {
                    let p = e
                        .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                        .map_err(|_| "this element has no value to set: type into it instead")?;
                    if p.CurrentIsReadOnly().is_ok_and(|b| b.as_bool()) {
                        return Err("this field is read-only".into());
                    }
                    p.SetValue(&BSTR::from(t.as_str())).map_err(gone)
                }
                Act::Press => {
                    // The element's own action, in the order a person would expect.
                    if let Ok(p) =
                        e.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                    {
                        return p.Invoke().map_err(gone);
                    }
                    if let Ok(p) =
                        e.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                    {
                        return p.Toggle().map_err(gone);
                    }
                    if let Ok(p) = e.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    ) {
                        return p.Select().map_err(gone);
                    }
                    if let Ok(p) = e.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    ) {
                        return match p.CurrentExpandCollapseState() {
                            Ok(ExpandCollapseState_Expanded) => p.Collapse(),
                            _ => p.Expand(),
                        }
                        .map_err(gone);
                    }
                    if let Ok(p) = e.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                        UIA_LegacyIAccessiblePatternId,
                    ) {
                        return p.DoDefaultAction().map_err(gone);
                    }
                    Err("this element cannot be pressed: click it instead".into())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_read_short_and_say_what_acts() {
        assert_eq!(
            role_of(UIA_ButtonControlTypeId, false),
            Some(("button", true))
        );
        assert_eq!(
            role_of(UIA_EditControlTypeId, true),
            Some(("password", true))
        );
        assert_eq!(
            role_of(UIA_ListItemControlTypeId, false),
            Some(("row", true))
        );
        assert_eq!(role_of(UIA_TextControlTypeId, false), Some(("text", false)));
        assert_eq!(role_of(UIA_PaneControlTypeId, false), None);
    }
}
