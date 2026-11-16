// The host's cursor, drawn by the page instead of waiting for it in the video
// (crates/nexa-web/src/cursor.rs). With a mouse, the browser's own pointer takes the
// host's shape and moves at once. Where the host's cursor went without us (someone at
// the host, another viewer, a program), or on a touch screen with no pointer of its
// own, an image is drawn where the host says it is.

/** How long a position we sent is remembered, to know the host's reports of it. */
const MEMORY_MS = 2000;
/** How close, in screen pixels, a report must be to a position we sent. */
const NEAR_PX = 4;
/** Browsers ignore cursor images larger than this, in CSS pixels. */
const MOST_CSS_PX = 128;

export class RemoteCursor {
  /** `video`: the picture; `overlay`: an <img> over it; `touch`: no pointer of our own. */
  constructor(video, overlay, { touch }) {
    this.video = video;
    this.overlay = overlay;
    this.touch = touch;
    this.reset();
  }

  /** A new connection: nothing is known yet, and nothing is drawn. */
  reset() {
    this.on = false;
    this.points = null;
    this.shapes = new Map();
    this.state = null;
    this.sentAt = [];
    this.cssKey = "";
    // Hidden by the host (as a Mac does while you type) until our pointer moves again.
    this.hidden = false;
    this.video.style.cursor = "";
    this.overlay.hidden = true;
  }

  /** The host let us in: `on` when it sends its cursor; `points` its screen in points.
   *  What it sent before this (the channel may open first) is kept. */
  enable(on, points) {
    this.on = on;
    this.points = Array.isArray(points) && points[0] > 0 ? points : null;
    this.cssKey = "";
    // An arrow until the host's own shape arrives: never no pointer at all.
    if (on && !this.touch) this.video.style.cursor = "default";
    this.draw();
  }

  /** The page put the host's cursor at (x, y), fractions of the screen. */
  sent(x, y) {
    const now = performance.now();
    this.sentAt.push({ x, y, t: now });
    while (this.sentAt.length && now - this.sentAt[0].t > MEMORY_MS) this.sentAt.shift();
    // Moving brings it back, as on the Mac itself. The host may never say so: a Mac
    // keeps reporting the cursor hidden after typing even once injected moves come.
    if (this.hidden) {
      this.hidden = false;
      this.draw();
    }
  }

  /** A message from the host; false if it is not about the cursor. */
  message(m) {
    if (m.t === "cs") {
      if (typeof m.id !== "string" || typeof m.png !== "string") return true;
      const img = new Image();
      const shape = { img, w: m.w, h: m.h, hx: m.hx, hy: m.hy, s: m.s > 0 ? m.s : 1, url: "", ready: false };
      img.onload = () => {
        shape.ready = true;
        if (this.state?.id === m.id) this.draw();
      };
      shape.url = `data:image/png;base64,${m.png}`;
      img.src = shape.url;
      this.shapes.set(m.id, shape);
      return true;
    }
    if (m.t === "cp") {
      if (typeof m.x !== "number" || typeof m.y !== "number") return true;
      const v = m.v !== false;
      // Only the host hiding it hides it; staying hidden does not hide it again.
      if (!v && (this.state === null || this.state.v)) this.hidden = true;
      if (v) this.hidden = false;
      this.state = { x: m.x, y: m.y, v, id: m.id };
      this.draw();
      return true;
    }
    return false;
  }

  /** The picture moved or changed size: redraw at the new scale. */
  layout() {
    this.cssKey = "";
    this.draw();
  }

  /** Where the picture is on the page and how large, as `fraction()` in app.js sees it. */
  rect() {
    const v = this.video;
    if (!v.videoWidth || !v.videoHeight) return null;
    const r = v.getBoundingClientRect();
    const scale = Math.min(r.width / v.videoWidth, r.height / v.videoHeight);
    const w = v.videoWidth * scale, h = v.videoHeight * scale;
    return { left: r.left + (r.width - w) / 2, top: r.top + (r.height - h) / 2, w, h };
  }

  /** CSS pixels per host point. */
  pointScale(rect) {
    return this.points ? rect.w / this.points[0] : 1;
  }

  ours(x, y, rect) {
    const now = performance.now();
    return this.sentAt.some((p) => now - p.t <= MEMORY_MS
      && Math.abs(p.x - x) * rect.w <= NEAR_PX && Math.abs(p.y - y) * rect.h <= NEAR_PX);
  }

  draw() {
    if (!this.on) return;
    const st = this.state;
    const rect = this.rect();
    const shape = st && this.shapes.get(st.id);
    if (!st || !rect || !shape?.ready) return;
    // At the picture's scale, but never so small it is lost: a phone shrinks a whole
    // desktop into a few inches, and its cursor with it.
    const k = Math.max(this.pointScale(rect), this.touch ? 1 : 0.75);
    // Its size on the page: host points at the picture's scale, within what browsers take.
    let cw = (shape.w / shape.s) * k, ch = (shape.h / shape.s) * k;
    const fit = Math.min(1, MOST_CSS_PX / Math.max(cw, ch));
    cw *= fit;
    ch *= fit;
    const hx = (shape.hx / shape.w) * cw, hy = (shape.hy / shape.h) * ch;

    const visible = !this.hidden;
    if (!this.touch) this.pointerShape(st, visible, shape, cw, ch, hx, hy);

    // Drawn where the host says only when the browser's pointer is not already there.
    const show = visible && (this.touch || !this.ours(st.x, st.y, rect));
    this.overlay.hidden = !show;
    if (!show) return;
    const stage = this.overlay.offsetParent?.getBoundingClientRect() ?? { left: 0, top: 0 };
    if (this.overlay.src !== shape.url) this.overlay.src = shape.url;
    this.overlay.style.width = `${cw}px`;
    this.overlay.style.height = `${ch}px`;
    this.overlay.style.transform = `translate(${rect.left - stage.left + st.x * rect.w - hx}px, ${rect.top - stage.top + st.y * rect.h - hy}px)`;
  }

  /** The browser's own pointer over the picture takes the host's shape, at its scale. */
  pointerShape(st, visible, shape, cw, ch, hx, hy) {
    const key = visible ? `${st.id}:${Math.round(cw)}` : "hidden";
    if (key === this.cssKey) return;
    this.cssKey = key;
    if (!visible) {
      this.video.style.cursor = "none";
      return;
    }
    const dpr = window.devicePixelRatio || 1;
    const image = (density) => {
      const canvas = document.createElement("canvas");
      canvas.width = Math.max(1, Math.round(cw * density));
      canvas.height = Math.max(1, Math.round(ch * density));
      const g = canvas.getContext("2d");
      g.imageSmoothingQuality = "high";
      g.drawImage(shape.img, 0, 0, canvas.width, canvas.height);
      return canvas.toDataURL("image/png");
    };
    const at = `${Math.round(hx)} ${Math.round(hy)}`;
    const style = this.video.style;
    // Sharp on a high-density screen where image-set() is understood (a value the
    // browser does not take leaves the property empty); 1x everywhere else.
    if (dpr > 1) {
      const url = image(dpr);
      for (const fn of ["image-set", "-webkit-image-set"]) {
        style.cursor = "";
        style.cursor = `${fn}(url(${url}) ${dpr}x) ${at}, default`;
        if (style.cursor) return;
      }
    }
    style.cursor = `url(${image(1)}) ${at}, default`;
  }
}
