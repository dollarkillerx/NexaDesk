// Fingers on the picture, as a mouse the host understands. Touch has no hover and no
// buttons, so gestures stand in for them:
//
//   tap                  click where the finger is
//   long press           right click
//   drag (one finger)    hold the left button and move: select, drag a window
//   two-finger tap       right click
//   two-finger drag      scroll
//   pinch                zoom this view in (and two fingers then pan it); the host
//                        sees nothing of it
//
// Mice and pens keep the ordinary pointer handling in app.js.

const MOVE_SLOP = 10;       // px before a finger counts as moving
const LONG_PRESS = 550;     // ms
const TWO_TAP = 300;        // ms both fingers may stay down for a two-finger tap
const PINCH_SLOP = 24;      // px of spread before two fingers count as a pinch
const MAX_ZOOM = 5;
const NOTCH_PX = 100;       // finger travel per wheel notch, as for a trackpad

/**
 * @param video   the <video> showing the host
 * @param actions { at(point) → fraction|null, move(f), button(b, down, f), wheel(dx, dy), active() }
 */
export function setupTouch(video, actions) {
  const fingers = new Map();
  let g = null;                   // the gesture under way
  const view = { scale: 1, x: 0, y: 0 };
  let moveQueued = null;

  const apply = () => {
    video.style.transformOrigin = "0 0";
    video.style.transform = view.scale === 1 && view.x === 0 && view.y === 0
      ? "" : `translate(${view.x}px, ${view.y}px) scale(${view.scale})`;
  };
  const clampView = () => {
    const r = video.parentElement.getBoundingClientRect();
    const w = r.width * view.scale, h = r.height * view.scale;
    view.x = Math.min(0, Math.max(r.width - w, view.x));
    view.y = Math.min(0, Math.max(r.height - h, view.y));
    if (view.scale <= 1.001) Object.assign(view, { scale: 1, x: 0, y: 0 });
  };
  const local = (p) => {
    const r = video.parentElement.getBoundingClientRect();
    return { x: p.x - r.left, y: p.y - r.top };
  };
  const mid = () => {
    const [a, b] = [...fingers.values()];
    return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
  };
  const spread = () => {
    const [a, b] = [...fingers.values()];
    return Math.hypot(a.x - b.x, a.y - b.y);
  };
  const point = (p) => actions.at({ clientX: p.x, clientY: p.y });
  const sendMove = (f) => {
    if (!moveQueued) requestAnimationFrame(() => { if (moveQueued) actions.move(moveQueued); moveQueued = null; });
    moveQueued = f;
  };
  const click = (b, p) => {
    const f = point(p);
    if (!f) return;
    actions.move(f);
    actions.button(b, true, f);
    actions.button(b, false, f);
  };

  video.addEventListener("pointerdown", (e) => {
    if (e.pointerType !== "touch" || !actions.active()) return;
    e.preventDefault();
    video.setPointerCapture(e.pointerId);
    fingers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (fingers.size === 1) {
      const start = { x: e.clientX, y: e.clientY };
      g = { kind: "one", start, at: start, long: null, fired: false };
      g.long = setTimeout(() => {
        if (g?.kind === "one") {
          g.fired = true;
          click(1, g.start);
        }
      }, LONG_PRESS);
    } else if (fingers.size === 2) {
      if (g?.long) clearTimeout(g.long);
      if (g?.kind === "drag") {
        const f = point(g.at);
        if (f) actions.button(0, false, f);
      }
      g = {
        kind: "two", t0: performance.now(), d0: spread(), m0: mid(), last: mid(),
        scale0: view.scale, x0: view.x, y0: view.y, decided: null, carry: { x: 0, y: 0 },
      };
    }
  });

  video.addEventListener("pointermove", (e) => {
    if (e.pointerType !== "touch" || !fingers.has(e.pointerId) || !g) return;
    fingers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (g.kind === "one" || g.kind === "drag") {
      const p = { x: e.clientX, y: e.clientY };
      g.at = p;
      if (g.kind === "one") {
        if (g.fired || Math.hypot(p.x - g.start.x, p.y - g.start.y) < MOVE_SLOP) return;
        clearTimeout(g.long);
        const f0 = point(g.start);
        if (!f0) return;
        g.kind = "drag";
        actions.move(f0);
        actions.button(0, true, f0);
      }
      const f = point(p);
      if (f) sendMove(f);
    } else if (g.kind === "two" && fingers.size === 2) {
      const m = mid(), d = spread();
      if (!g.decided) {
        if (Math.abs(d - g.d0) > PINCH_SLOP) g.decided = "pinch";
        else if (Math.hypot(m.x - g.m0.x, m.y - g.m0.y) > MOVE_SLOP) g.decided = view.scale > 1 ? "pan" : "scroll";
      }
      if (g.decided === "pinch") {
        // Zoom about the fingers' midpoint, so what is under them stays under them.
        const scale = Math.min(MAX_ZOOM, Math.max(1, g.scale0 * d / g.d0));
        const a = local(g.m0), b = local(m);
        view.x = b.x - (a.x - g.x0) * (scale / g.scale0);
        view.y = b.y - (a.y - g.y0) * (scale / g.scale0);
        view.scale = scale;
        clampView();
        apply();
      } else if (g.decided === "pan") {
        view.x += m.x - g.last.x;
        view.y += m.y - g.last.y;
        clampView();
        apply();
      } else if (g.decided === "scroll") {
        // Content follows the fingers, as on a trackpad with natural scrolling.
        g.carry.x += ((m.x - g.last.x) * 120) / NOTCH_PX;
        g.carry.y += ((m.y - g.last.y) * 120) / NOTCH_PX;
        const dx = Math.trunc(g.carry.x), dy = Math.trunc(g.carry.y);
        g.carry.x -= dx;
        g.carry.y -= dy;
        if (dx || dy) actions.wheel(dx, dy);
      }
      g.last = m;
    }
  });

  const end = (e) => {
    if (e.pointerType !== "touch" || !fingers.has(e.pointerId)) return;
    const m = fingers.size === 2 ? mid() : null;
    fingers.delete(e.pointerId);
    if (!g) return;
    if (g.kind === "one") {
      clearTimeout(g.long);
      if (!g.fired) click(0, g.start);
      g = null;
    } else if (g.kind === "drag") {
      const f = point(g.at);
      if (f) actions.button(0, false, f);
      g = null;
    } else if (g.kind === "two") {
      if (!g.decided && m && performance.now() - g.t0 < TWO_TAP) click(1, m);
      // The other finger lifting ends it; it does not start a one-finger gesture.
      g = fingers.size ? { kind: "lifting" } : null;
    } else if (fingers.size === 0) {
      g = null;
    }
  };
  video.addEventListener("pointerup", end);
  video.addEventListener("pointercancel", end);

  return {
    /** Back to the whole picture. */
    reset() {
      Object.assign(view, { scale: 1, x: 0, y: 0 });
      apply();
    },
  };
}
