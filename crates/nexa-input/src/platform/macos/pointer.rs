//! Relative pointer capture for game mode (§7.4, DESIGN §8.5).
//!
//! The cursor is detached from the mouse, deltas are read from a listen-only
//! tap, and the running totals of `nexa_wire::datagram::Pointer` are emitted
//! for every event, unaggregated.

use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};

use nexa_wire::datagram::Pointer;
use objc2_core_graphics::{CGEvent, CGEventField, CGEventTapOptions, CGEventType};

use super::capture::CaptureError;
use super::inject::INJECTED_EVENT_MARKER;
use super::tap::{TapHandler, TapSpec, TapThread};

/// Units of `Pointer::wheel_*_total` per line of wheel movement (1/120 notch).
const WHEEL_UNITS_PER_LINE: f64 = 120.0;

/// Accumulates motion into the wire's running totals.
///
/// Totals wrap, so the receiver can always take differences. Fractions are
/// carried over between events rather than rounded away, so slow movement
/// still adds up.
#[derive(Clone, Debug)]
pub struct PointerAccumulator {
    pointer: Pointer,
    carry_x: f64,
    carry_y: f64,
    carry_wheel_h: f64,
    carry_wheel_v: f64,
}

impl PointerAccumulator {
    pub fn new(display: u8) -> Self {
        Self {
            pointer: Pointer {
                relative: true,
                display,
                ..Pointer::default()
            },
            carry_x: 0.0,
            carry_y: 0.0,
            carry_wheel_h: 0.0,
            carry_wheel_v: 0.0,
        }
    }

    fn next(&mut self) -> Pointer {
        self.pointer.seq = self.pointer.seq.wrapping_add(1);
        self.pointer
    }

    /// Adds a pointer delta and returns the updated totals.
    pub fn on_motion(&mut self, dx: f64, dy: f64) -> Pointer {
        self.pointer.x = advance(self.pointer.x, &mut self.carry_x, dx);
        self.pointer.y = advance(self.pointer.y, &mut self.carry_y, dy);
        self.next()
    }

    /// Adds wheel movement in lines. Positive vertical is up (away from the
    /// user) and positive horizontal is right, in the Windows convention.
    pub fn on_wheel(&mut self, horizontal: f64, vertical: f64) -> Pointer {
        self.pointer.wheel_h_total = advance(
            self.pointer.wheel_h_total,
            &mut self.carry_wheel_h,
            horizontal * WHEEL_UNITS_PER_LINE,
        );
        self.pointer.wheel_v_total = advance(
            self.pointer.wheel_v_total,
            &mut self.carry_wheel_v,
            vertical * WHEEL_UNITS_PER_LINE,
        );
        self.next()
    }
}

fn advance(total: i32, carry: &mut f64, delta: f64) -> i32 {
    *carry += delta;
    let whole = carry.trunc();
    *carry -= whole;
    total.wrapping_add(whole as i32)
}

/// Reads an event from the tap into the accumulator. Horizontal wheel is
/// negated: macOS reports positive as left, the wire uses positive as right.
pub(super) fn pointer_sample(
    kind: CGEventType,
    event: &CGEvent,
    acc: &mut PointerAccumulator,
) -> Option<Pointer> {
    let number = |field| CGEvent::double_value_field(Some(event), field);
    match kind {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => Some(acc.on_motion(
            number(CGEventField::MouseEventDeltaX),
            number(CGEventField::MouseEventDeltaY),
        )),
        CGEventType::ScrollWheel => Some(acc.on_wheel(
            -number(CGEventField::ScrollWheelEventFixedPtDeltaAxis2),
            number(CGEventField::ScrollWheelEventFixedPtDeltaAxis1),
        )),
        _ => None,
    }
}

struct PointerHandler {
    accumulator: Mutex<PointerAccumulator>,
    tx: Sender<Pointer>,
}

impl TapHandler for PointerHandler {
    fn on_event(&self, kind: CGEventType, event: NonNull<CGEvent>) -> *mut CGEvent {
        // SAFETY: the tap passes a valid event for the duration of the callback.
        let cg = unsafe { event.as_ref() };
        let injected = CGEvent::integer_value_field(Some(cg), CGEventField::EventSourceUserData)
            == INJECTED_EVENT_MARKER;
        if !injected
            && let Ok(mut acc) = self.accumulator.lock()
            && let Some(pointer) = pointer_sample(kind, cg, &mut acc)
        {
            let _ = self.tx.send(pointer);
        }
        event.as_ptr()
    }

    fn wants_enabled(&self) -> bool {
        true
    }
}

/// Connects or disconnects the mouse from the cursor.
pub trait MouseAssociation: Send + 'static {
    fn set_associated(&self, associated: bool);
}

/// The real `CGAssociateMouseAndMouseCursorPosition`.
#[cfg(not(test))]
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemAssociation;

#[cfg(not(test))]
static DISSOCIATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(not(test))]
impl MouseAssociation for SystemAssociation {
    fn set_associated(&self, associated: bool) {
        DISSOCIATED.store(!associated, std::sync::atomic::Ordering::SeqCst);
        let _ = objc2_core_graphics::CGAssociateMouseAndMouseCursorPosition(associated);
    }
}

/// Restores the mouse on any panic, on any thread, before unwinding starts. A
/// [`PointerCapture`] held by another thread could not otherwise be reached.
#[cfg(not(test))]
fn install_panic_restore() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if DISSOCIATED.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = objc2_core_graphics::CGAssociateMouseAndMouseCursorPosition(true);
            }
            previous(info);
        }));
    });
}

/// Re-associates when dropped, including during unwinding.
struct AssociationGuard<A: MouseAssociation>(A);

impl<A: MouseAssociation> Drop for AssociationGuard<A> {
    fn drop(&mut self) {
        self.0.set_associated(true);
    }
}

/// An active game-mode pointer capture. The mouse is re-associated when this
/// is stopped or dropped.
pub struct PointerCapture<A: MouseAssociation> {
    // Field order is drop order: the mouse comes back before the tap stops.
    _guard: AssociationGuard<A>,
    _tap: TapThread<PointerHandler>,
}

#[cfg(not(test))]
impl PointerCapture<SystemAssociation> {
    /// Detaches the cursor from the mouse and starts emitting totals.
    pub fn start(display: u8) -> Result<(Self, Receiver<Pointer>), CaptureError> {
        install_panic_restore();
        Self::start_with(SystemAssociation, display)
    }
}

impl<A: MouseAssociation> PointerCapture<A> {
    /// [`PointerCapture::start`] with an explicit association implementation.
    pub fn start_with(
        association: A,
        display: u8,
    ) -> Result<(Self, Receiver<Pointer>), CaptureError> {
        let (tx, rx) = mpsc::channel();
        let spec = TapSpec {
            name: "input-pointer",
            options: CGEventTapOptions::ListenOnly,
            mask: [
                CGEventType::MouseMoved,
                CGEventType::LeftMouseDragged,
                CGEventType::RightMouseDragged,
                CGEventType::OtherMouseDragged,
                CGEventType::ScrollWheel,
            ]
            .into_iter()
            .fold(0, |mask, kind| mask | (1 << kind.0)),
        };
        let handler = PointerHandler {
            accumulator: Mutex::new(PointerAccumulator::new(display)),
            tx,
        };
        let tap = TapThread::spawn(spec, handler)?;
        // Only after the tap exists: no capture must be left detached if it fails.
        association.set_associated(false);
        let guard = AssociationGuard(association);
        Ok((
            Self {
                _guard: guard,
                _tap: tap,
            },
            rx,
        ))
    }

    /// Ends the capture, re-associating the mouse.
    pub fn stop(self) {
        drop(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI32, Ordering};

    #[derive(Clone, Default)]
    struct Fake(Arc<AtomicI32>);
    impl MouseAssociation for Fake {
        fn set_associated(&self, associated: bool) {
            self.0
                .store(if associated { 1 } else { -1 }, Ordering::SeqCst);
        }
    }

    #[test]
    fn motion_accumulates_totals_and_counts_events() {
        let mut acc = PointerAccumulator::new(2);
        let first = acc.on_motion(3.0, -4.0);
        assert_eq!((first.x, first.y, first.seq, first.display), (3, -4, 1, 2));
        assert!(first.relative);
        let second = acc.on_motion(-1.0, 10.0);
        assert_eq!((second.x, second.y, second.seq), (2, 6, 2));
    }

    #[test]
    fn totals_wrap_rather_than_overflow() {
        let mut acc = PointerAccumulator::new(0);
        acc.pointer.x = i32::MAX;
        acc.pointer.seq = u16::MAX;
        let p = acc.on_motion(1.0, 0.0);
        assert_eq!(p.x, i32::MIN);
        assert_eq!(p.seq, 0);
    }

    #[test]
    fn fractions_carry_between_events() {
        let mut acc = PointerAccumulator::new(0);
        assert_eq!(acc.on_motion(0.75, 0.0).x, 0);
        assert_eq!(acc.on_motion(0.75, 0.0).x, 1);
        assert_eq!(acc.on_motion(0.5, 0.0).x, 2);
        assert_eq!(acc.on_motion(-2.5, 0.0).x, 0);
    }

    #[test]
    fn wheel_lines_scale_to_120ths_with_carry() {
        let mut acc = PointerAccumulator::new(0);
        let p = acc.on_wheel(0.0, 1.0);
        assert_eq!((p.wheel_v_total, p.wheel_h_total), (120, 0));
        let p = acc.on_wheel(0.0, 0.0041);
        assert_eq!(p.wheel_v_total, 120, "under one unit is carried");
        let p = acc.on_wheel(0.0, 0.0041);
        assert_eq!(p.wheel_v_total, 120, "0.98 units still carried");
        let p = acc.on_wheel(0.0, 0.0041);
        assert_eq!(p.wheel_v_total, 121);
    }

    fn scroll_event() -> objc2_core_foundation::CFRetained<CGEvent> {
        CGEvent::new_scroll_wheel_event2(
            None,
            objc2_core_graphics::CGScrollEventUnit::Line,
            2,
            2,
            -1,
            0,
        )
        .unwrap()
    }

    fn mouse_event(kind: CGEventType) -> objc2_core_foundation::CFRetained<CGEvent> {
        let at = objc2_core_foundation::CGPoint::new(0.0, 0.0);
        CGEvent::new_mouse_event(None, kind, at, objc2_core_graphics::CGMouseButton::Left).unwrap()
    }

    #[test]
    fn delta_fields_are_read_for_moves_and_drags() {
        for kind in [
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
        ] {
            let event = mouse_event(kind);
            CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventDeltaX, 7);
            CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventDeltaY, -9);
            let mut acc = PointerAccumulator::new(0);
            let p = pointer_sample(kind, &event, &mut acc).unwrap();
            assert_eq!((p.x, p.y), (7, -9), "{kind:?}");
        }
    }

    #[test]
    fn wheel_fields_are_read_with_the_horizontal_sign_flipped() {
        let event = scroll_event();
        let mut acc = PointerAccumulator::new(0);
        let p = pointer_sample(CGEventType::ScrollWheel, &event, &mut acc).unwrap();
        assert_eq!(p.wheel_v_total, 240);
        assert_eq!(p.wheel_h_total, 120, "macOS -1 (right) is wire +");
    }

    #[test]
    fn other_event_types_are_ignored() {
        let event = mouse_event(CGEventType::MouseMoved);
        let mut acc = PointerAccumulator::new(0);
        assert_eq!(pointer_sample(CGEventType::KeyDown, &event, &mut acc), None);
    }

    #[test]
    fn guard_restores_on_drop() {
        let fake = Fake::default();
        fake.set_associated(false);
        drop(AssociationGuard(fake.clone()));
        assert_eq!(fake.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn guard_restores_during_panic() {
        let fake = Fake::default();
        fake.set_associated(false);
        let inner = fake.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = AssociationGuard(inner);
            panic!("simulated failure in game mode");
        }));
        assert!(result.is_err());
        assert_eq!(fake.0.load(Ordering::SeqCst), 1);
    }

    /// A real capture lifecycle using a fake association, so the real mouse is
    /// untouched. Its tap is listen-only, mouse only, and short-lived.
    #[test]
    fn capture_lifecycle_dissociates_then_restores() {
        let fake = Fake::default();
        let (capture, rx) = match PointerCapture::start_with(fake.clone(), 1) {
            Ok(pair) => pair,
            Err(CaptureError::PermissionDenied) => return, // headless test run
            Err(other) => panic!("{other:?}"),
        };
        assert_eq!(fake.0.load(Ordering::SeqCst), -1);
        drop(rx);
        capture.stop();
        assert_eq!(fake.0.load(Ordering::SeqCst), 1);
    }
}
