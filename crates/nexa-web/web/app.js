// NexaDesk in the browser: signaling and pairing over the host's WebSocket, then video
// and input over WebRTC. The protocol is described in crates/nexa-web/src/session.rs.
import { apply, t } from "./i18n.js";
import { hidFor } from "./keys.js";

const $ = (id) => document.getElementById(id);
const CODE_ALPHABET = /^[A-Z2-7]{8}$/;

// ---- Pairing (WebAssembly from crates/nexa-web-pake) ----

let pake = null;

async function loadPake() {
  if (pake) return pake;
  const { instance } = await WebAssembly.instantiateStreaming(fetch("/pake.wasm"), {});
  pake = instance.exports;
  return pake;
}

function bytesIn(p, bytes) {
  const ptr = p.nx_alloc(bytes.length);
  new Uint8Array(p.memory.buffer, ptr, bytes.length).set(bytes);
  return ptr;
}

/** SPAKE2 answer: returns {msg, confirm} or throws with a status code. */
function pakeAnswer(p, code, browserFp, hostFp, salt, hostMsg) {
  const seed = crypto.getRandomValues(new Uint8Array(32));
  const codeBytes = new TextEncoder().encode(code);
  const out = p.nx_alloc(65);
  const status = p.nx_answer(
    bytesIn(p, codeBytes), codeBytes.length,
    bytesIn(p, browserFp), bytesIn(p, hostFp),
    bytesIn(p, salt), bytesIn(p, hostMsg), bytesIn(p, seed), out,
  );
  seed.fill(0);
  if (status !== 0) throw new Error(status === 1 ? "code" : "protocol");
  const both = new Uint8Array(p.memory.buffer, out, 65).slice();
  return { msg: both.slice(0, 33), confirm: both.slice(33) };
}

function pakeHostOk(p, confirm) {
  return confirm.length === 32 && p.nx_check_host(bytesIn(p, confirm)) === 0;
}

const b64 = (bytes) => btoa(String.fromCharCode(...bytes));
const unb64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));

/** The sha-256 DTLS fingerprint written in an SDP. */
function fingerprintOf(sdp) {
  const m = /^a=fingerprint:sha-256 ([0-9A-Fa-f:]+)\s*$/m.exec(sdp);
  if (!m) return null;
  const bytes = m[1].split(":").map((h) => parseInt(h, 16));
  return bytes.length === 32 && bytes.every((b) => b >= 0 && b < 256) ? Uint8Array.from(bytes) : null;
}

function describeBrowser() {
  const ua = navigator.userAgent;
  const name = /Edg\//.test(ua) ? "Edge" : /Firefox\//.test(ua) ? "Firefox"
    : /Chrome\//.test(ua) ? "Chrome" : /Safari\//.test(ua) ? "Safari" : "Browser";
  const os = /iPhone|iPad/.test(ua) ? "iOS" : /Android/.test(ua) ? "Android"
    : /Mac OS X/.test(ua) ? "macOS" : /Windows/.test(ua) ? "Windows" : /Linux/.test(ua) ? "Linux" : "";
  return os ? `${name} · ${os}` : name;
}

// ---- Connect page ----

function showStatus(text, { error = false, busy = false } = {}) {
  $("status").hidden = false;
  $("status").classList.toggle("error", error);
  $("spinner").hidden = !busy;
  $("status-text").textContent = text;
}

function setConnecting(on) {
  $("connect").disabled = on;
  $("code").disabled = on;
}

function normalize(input) {
  return input.replace(/[\s-]/g, "").toUpperCase();
}

// ---- Session ----

let session = null;

function connect(code) {
  const pc = new RTCPeerConnection({ iceServers: [] });
  const ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`);
  const s = { pc, ws, dc: null, input: false, ended: false };
  session = s;

  const finish = (key, { error = true } = {}) => {
    if (s.ended) return;
    s.ended = true;
    try { s.dc?.close(); } catch {}
    try { pc.close(); } catch {}
    try { ws.send(JSON.stringify({ type: "bye" })); } catch {}
    try { ws.close(); } catch {}
    showConnect();
    // A code that got as far as a session is used up.
    if (s.started) $("code").value = "";
    showStatus(t(key), { error });
    setConnecting(false);
    if (session === s) session = null;
  };
  s.finish = finish;

  pc.addTransceiver("video", { direction: "recvonly" });
  s.dc = pc.createDataChannel("input", { ordered: true });
  pc.ontrack = (e) => {
    const video = $("screen");
    video.srcObject = e.streams[0] ?? new MediaStream([e.track]);
    // Show frames as they come: this is remote control, not a film.
    try { e.receiver.jitterBufferTarget = 0; } catch {}
    try { e.receiver.playoutDelayHint = 0; } catch {}
  };
  pc.onconnectionstatechange = () => {
    if (pc.connectionState === "failed") finish(s.started ? "error.network" : "error.webrtc");
  };

  ws.onopen = async () => {
    await pc.setLocalDescription(await pc.createOffer());
    ws.send(JSON.stringify({ type: "offer", sdp: pc.localDescription.sdp, browser: describeBrowser() }));
  };
  ws.onerror = () => finish(s.started ? "error.network" : "error.webrtc");
  ws.onclose = () => { if (!s.ended) finish(s.started ? "ended" : "error.protocol", { error: !s.started }); };
  ws.onmessage = async (ev) => {
    let m;
    try { m = JSON.parse(ev.data); } catch { return finish("error.protocol"); }
    switch (m.type) {
      case "answer": {
        showStatus(t("verifying"), { busy: true });
        await pc.setRemoteDescription({ type: "answer", sdp: m.sdp });
        const mine = fingerprintOf(pc.localDescription.sdp);
        const theirs = fingerprintOf(m.sdp);
        if (!mine || !theirs) return finish("error.protocol");
        try {
          const p = await loadPake();
          const r = pakeAnswer(p, code, mine, theirs, unb64(m.salt), unb64(m.msg));
          ws.send(JSON.stringify({ type: "auth", msg: b64(r.msg), confirm: b64(r.confirm) }));
        } catch (e) {
          return finish(e.message === "code" ? "error.format" : "error.wasm");
        }
        break;
      }
      case "waiting":
        showStatus(t("waiting"), { busy: true });
        break;
      case "accepted":
        // The host proves it knows the code for the same two fingerprints, or we leave.
        if (!pakeHostOk(pake, unb64(m.confirm))) return finish("error.host");
        s.input = !!m.input;
        s.started = true;
        showSession(s);
        break;
      case "denied":
        finish("error.denied");
        break;
      case "ended":
        finish("ended", { error: false });
        break;
      case "error":
        finish("error." + (m.code || "protocol"));
        break;
    }
  };
}

function showConnect() {
  $("session-view").hidden = true;
  $("connect-view").hidden = false;
  $("disconnect").hidden = true;
  $("fullscreen").hidden = true;
  $("mode-tag").hidden = true;
  const v = $("screen");
  v.srcObject = null;
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
}

function showSession(s) {
  $("connect-view").hidden = true;
  $("session-view").hidden = false;
  $("session-view").classList.toggle("control", s.input);
  $("disconnect").hidden = false;
  $("fullscreen").hidden = !document.fullscreenEnabled;
  const tag = $("mode-tag");
  tag.hidden = false;
  tag.textContent = t(s.input ? "control" : "viewOnly");
  tag.classList.toggle("control", s.input);
  $("keys-hint").hidden = !s.input;
  setTimeout(() => { $("keys-hint").hidden = true; }, 6000);
  $("screen").focus();
}

// ---- Input ----

function send(msg) {
  const s = session;
  if (!s?.started || !s.input || s.dc?.readyState !== "open") return;
  s.dc.send(JSON.stringify(msg));
}

/** Where a pointer event falls in the picture, as fractions; null outside it. */
function fraction(e) {
  const v = $("screen");
  if (!v.videoWidth || !v.videoHeight) return null;
  const r = v.getBoundingClientRect();
  const scale = Math.min(r.width / v.videoWidth, r.height / v.videoHeight);
  const w = v.videoWidth * scale, h = v.videoHeight * scale;
  const x = (e.clientX - r.left - (r.width - w) / 2) / w;
  const y = (e.clientY - r.top - (r.height - h) / 2) / h;
  if (x < 0 || x > 1 || y < 0 || y > 1) return null;
  return { x, y };
}

const BUTTON = { 0: 0, 2: 1, 1: 2 }; // DOM left, right, middle → host 0, 1, 2

// Modifiers can be down without their own keydown reaching the page (held before it
// had focus, or swallowed by the browser). Each key event carries the truth in its flags,
// so the host is brought in line before the key itself, as the native client does.
const MODIFIERS = [
  { flag: "ctrlKey", hids: [0xe0, 0xe4] },
  { flag: "shiftKey", hids: [0xe1, 0xe5] },
  { flag: "altKey", hids: [0xe2, 0xe6] },
  { flag: "metaKey", hids: [0xe3, 0xe7] },
];
const held = new Set();

function syncModifiers(e) {
  for (const m of MODIFIERS) {
    const down = m.hids.filter((h) => held.has(h));
    if (e[m.flag] && down.length === 0) {
      send({ t: "k", c: m.hids[0], d: true, r: false });
      held.add(m.hids[0]);
    } else if (!e[m.flag]) {
      for (const h of down) {
        send({ t: "k", c: h, d: false, r: false });
        held.delete(h);
      }
    }
  }
}
let pendingMove = null;
let wheelCarry = { x: 0, y: 0 };

function setupInput() {
  const v = $("screen");
  v.addEventListener("contextmenu", (e) => e.preventDefault());
  v.addEventListener("pointermove", (e) => {
    const f = fraction(e);
    if (!f) return;
    if (!pendingMove) requestAnimationFrame(() => { if (pendingMove) send({ t: "m", ...pendingMove }); pendingMove = null; });
    pendingMove = f;
  });
  v.addEventListener("pointerdown", (e) => {
    const f = fraction(e);
    const b = BUTTON[e.button];
    if (!f || b === undefined) return;
    e.preventDefault();
    v.focus();
    v.setPointerCapture(e.pointerId);
    send({ t: "b", b, d: true, ...f });
  });
  v.addEventListener("pointerup", (e) => {
    const b = BUTTON[e.button];
    if (b === undefined) return;
    const f = fraction(e) ?? { x: 0.5, y: 0.5 };
    send({ t: "b", b, d: false, x: Math.min(1, Math.max(0, f.x)), y: Math.min(1, Math.max(0, f.y)) });
  });
  v.addEventListener("wheel", (e) => {
    if (!session?.started) return;
    e.preventDefault();
    // Pixels (or lines) to 120ths of a notch, about 100 px per notch; up is positive.
    const k = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1;
    wheelCarry.x += (-e.deltaX * k * 120) / 100;
    wheelCarry.y += (-e.deltaY * k * 120) / 100;
    const dx = Math.trunc(wheelCarry.x), dy = Math.trunc(wheelCarry.y);
    wheelCarry.x -= dx;
    wheelCarry.y -= dy;
    if (dx || dy) send({ t: "w", dx: Math.max(-4800, Math.min(4800, dx)), dy: Math.max(-4800, Math.min(4800, dy)) });
  }, { passive: false });

  const key = (down) => (e) => {
    if (!session?.started || $("session-view").hidden) return;
    const c = hidFor(e);
    if (c === undefined) return;
    e.preventDefault();
    if (!MODIFIERS.some((m) => m.hids.includes(c))) syncModifiers(e);
    send({ t: "k", c, d: down, r: down && e.repeat });
    if (down) held.add(c); else held.delete(c);
  };
  window.addEventListener("keydown", key(true));
  window.addEventListener("keyup", key(false));
  // Nothing stays held when the page loses the keyboard.
  const release = () => { held.clear(); send({ t: "r" }); };
  window.addEventListener("blur", release);
  document.addEventListener("visibilitychange", () => { if (document.hidden) release(); });
}

// ---- Start ----

function init() {
  apply();
  setupInput();
  const code = $("code");
  code.addEventListener("input", () => {
    code.classList.remove("invalid");
    $("code-help").classList.remove("invalid");
    $("code-help").textContent = t("codeHelp");
  });
  $("connect-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const c = normalize(code.value);
    if (!CODE_ALPHABET.test(c)) {
      code.classList.add("invalid");
      $("code-help").classList.add("invalid");
      $("code-help").textContent = t("error.format");
      return;
    }
    setConnecting(true);
    showStatus(t("connecting"), { busy: true });
    loadPake().then(() => connect(c), () => {
      setConnecting(false);
      showStatus(t("error.wasm"), { error: true });
    });
  });
  $("disconnect").addEventListener("click", () => session?.finish("ended", { error: false }));
  $("fullscreen").addEventListener("click", () => $("session-view").requestFullscreen?.().catch(() => {}));
  window.addEventListener("pagehide", () => session?.finish("ended", { error: false }));
  code.focus();
}

init();
