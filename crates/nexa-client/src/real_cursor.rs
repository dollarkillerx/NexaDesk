//! The host's cursor as the real NSCursor over the video view (§6.9). Reached only
//! from `RealDeps`; tests use fakes and never change the cursor.
//!
//! A tracking area on the video's NSView sends `cursorUpdate:` to an owner object
//! here, which sets the current host cursor; leaving the view restores the arrow.
//! All AppKit work happens on the main queue; this type only holds plain data.

use std::{cell::RefCell, collections::HashMap, ffi::c_void};

use dispatch2::DispatchQueue;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol},
};
use objc2_app_kit::{
    NSCursor, NSEvent, NSImage, NSScreen, NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGAssociateMouseAndMouseCursorPosition, CGWarpMouseCursorPosition};
use objc2_foundation::NSData;

use crate::deps::{CursorDisplay, LocalCursor};

struct OwnerState {
    cursor: Option<Retained<NSCursor>>,
    inside: bool,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "NexaCursorOwner"]
    #[ivars = RefCell<OwnerState>]
    struct Owner;

    unsafe impl NSObjectProtocol for Owner {}

    impl Owner {
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {
            self.apply();
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.ivars().borrow_mut().inside = true;
            self.apply();
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().borrow_mut().inside = false;
            NSCursor::arrowCursor().set();
        }
    }
);

impl Owner {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(RefCell::new(OwnerState {
            cursor: None,
            inside: false,
        }));
        // SAFETY: NSObject's init takes no arguments.
        unsafe { msg_send![super(this), init] }
    }

    fn apply(&self) {
        let state = self.ivars().borrow();
        match &state.cursor {
            Some(c) => c.set(),
            None => NSCursor::arrowCursor().set(),
        }
    }
}

struct Entry {
    view: Retained<NSView>,
    area: Retained<NSTrackingArea>,
    owner: Retained<Owner>,
    /// Built cursors by (id, size), so switching back is free.
    cache: HashMap<(u64, u64, u64), Retained<NSCursor>>,
    hidden: Option<Retained<NSCursor>>,
}

thread_local! {
    /// Main thread only: the views with a host cursor.
    static ENTRIES: RefCell<HashMap<u64, Entry>> = RefCell::new(HashMap::new());
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Relaxed)
}

fn make_cursor(png: &[u8], w: f64, h: f64, hot_x: f64, hot_y: f64) -> Option<Retained<NSCursor>> {
    let data = NSData::with_bytes(png);
    let image = NSImage::initWithData(NSImage::alloc(), &data)?;
    image.setSize(CGSize::new(w.max(1.0), h.max(1.0)));
    let hot = CGPoint::new(hot_x.clamp(0.0, w), hot_y.clamp(0.0, h));
    Some(NSCursor::initWithImage_hotSpot(
        NSCursor::alloc(),
        &image,
        hot,
    ))
}

fn with_entry(id: u64, f: impl FnOnce(&mut Entry)) {
    ENTRIES.with(|e| {
        if let Some(entry) = e.borrow_mut().get_mut(&id) {
            f(entry);
        }
    });
}

fn set_current(entry: &Entry, cursor: Option<Retained<NSCursor>>) {
    let inside = {
        let mut state = entry.owner.ivars().borrow_mut();
        state.cursor = cursor;
        state.inside
    };
    if inside {
        entry.owner.apply();
    }
}

/// Handed to the session; every method posts to the main queue.
pub struct MacCursorDisplay {
    id: u64,
}

impl MacCursorDisplay {
    /// # Safety
    /// Main thread; `view` must be a valid NSView.
    pub unsafe fn attach(view: *mut c_void) -> Option<Self> {
        MainThreadMarker::new()?;
        // SAFETY: the caller passes a valid NSView, which this retains.
        let view: Retained<NSView> = unsafe { Retained::retain(view.cast())? };
        let owner = Owner::new();
        let options = NSTrackingAreaOptions::CursorUpdate
            | NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::ActiveInKeyWindow
            | NSTrackingAreaOptions::InVisibleRect;
        let owner_object: &AnyObject = &owner;
        // SAFETY: the rect is ignored with InVisibleRect; owner outlives the area,
        // both are kept in the entry until the area is removed.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                CGRect::default(),
                options,
                Some(owner_object),
                None,
            )
        };
        view.addTrackingArea(&area);
        let id = next_id();
        let hidden = make_cursor(
            &nexa_session::cursor::encode_png(1, 1, &[0, 0, 0, 0]),
            1.0,
            1.0,
            0.0,
            0.0,
        );
        ENTRIES.with(|e| {
            e.borrow_mut().insert(
                id,
                Entry {
                    view,
                    area,
                    owner,
                    cache: HashMap::new(),
                    hidden,
                },
            )
        });
        Some(Self { id })
    }
}

impl CursorDisplay for MacCursorDisplay {
    fn show(&mut self, cursor: Option<&LocalCursor>) {
        let id = self.id;
        let cursor = cursor.cloned();
        DispatchQueue::main().exec_async(move || {
            with_entry(id, |entry| {
                let built = match &cursor {
                    None => entry.hidden.clone(),
                    Some(c) => {
                        let key = (c.id, c.width_pt.to_bits(), c.height_pt.to_bits());
                        match entry.cache.get(&key) {
                            Some(hit) => Some(hit.clone()),
                            None => {
                                let made = make_cursor(
                                    &c.png,
                                    c.width_pt,
                                    c.height_pt,
                                    c.hot_x_pt,
                                    c.hot_y_pt,
                                );
                                if let Some(m) = &made {
                                    if entry.cache.len() > 64 {
                                        entry.cache.clear();
                                    }
                                    entry.cache.insert(key, m.clone());
                                }
                                made
                            }
                        }
                    }
                };
                set_current(entry, built);
            })
        });
    }

    fn reset(&mut self) {
        let id = self.id;
        DispatchQueue::main().exec_async(move || with_entry(id, |entry| set_current(entry, None)));
    }

    fn warp(&mut self, x: f64, y: f64) {
        let id = self.id;
        DispatchQueue::main().exec_async(move || {
            with_entry(id, |entry| {
                let Some(mtm) = MainThreadMarker::new() else {
                    return;
                };
                let Some(window) = entry.view.window() else {
                    return;
                };
                let bounds = entry.view.bounds();
                let local = if entry.view.isFlipped() {
                    CGPoint::new(x, y)
                } else {
                    CGPoint::new(x, bounds.size.height - y)
                };
                let in_window = entry.view.convertPoint_toView(local, None);
                let on_screen = window.convertPointToScreen(in_window);
                // CoreGraphics counts from the top of the primary screen.
                let Some(primary) = NSScreen::screens(mtm).firstObject() else {
                    return;
                };
                let global = CGPoint::new(on_screen.x, primary.frame().size.height - on_screen.y);
                let _ = CGWarpMouseCursorPosition(global);
                // A warp otherwise freezes local mouse events for a quarter second.
                let _ = CGAssociateMouseAndMouseCursorPosition(true);
            })
        });
    }
}

impl Drop for MacCursorDisplay {
    fn drop(&mut self) {
        let id = self.id;
        DispatchQueue::main().exec_async(move || {
            let entry = ENTRIES.with(|e| e.borrow_mut().remove(&id));
            if let Some(entry) = entry {
                entry.view.removeTrackingArea(&entry.area);
                if entry.owner.ivars().borrow().inside {
                    NSCursor::arrowCursor().set();
                }
            }
        });
    }
}
