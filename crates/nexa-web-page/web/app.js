// NexaDesk in the browser: signaling and pairing over the host's WebSocket, then video
// and input over WebRTC. The protocol is described in crates/nexa-web/src/session.rs.
import { apply, lang, LANGS, setLang, t } from "./i18n.js";
import { Keyboard } from "./keyboard.js";
import { setupTouch } from "./touch.js";
import { watch } from "./stats.js";
import { RemoteTerminal } from "./term.js";
import * as saved from "./saved.js";

const $ = (id) => document.getElementById(id);
const CODE_ALPHABET = /^[A-Z2-7]{8}$/;
const DEVICE_ID = /^[1-9][0-9]{8}$/;

/** "direct" when the host itself served this page; "server" when the signaling server
 *  did, for a host anywhere: then a device ID is needed, and media goes through TURN. */
let mode = "direct";

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

/** SPAKE2 answer: returns {msg, confirm} or throws with a status code. With the fixed
 *  password, Argon2id runs first, as the host did when it was set: a moment's work. */
function pakeAnswer(p, secret, method, browserFp, hostFp, salt, hostMsg) {
  const seed = crypto.getRandomValues(new Uint8Array(32));
  const codeBytes = new TextEncoder().encode(secret);
  const out = p.nx_alloc(65);
  const answer = method === "password" ? p.nx_answer_password : p.nx_answer;
  const status = answer(
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
  // On a phone the form is taller than the screen: keep the news where it is seen.
  $("status").scrollIntoView({ block: "nearest" });
}

function setConnecting(on) {
  $("connect").disabled = on;
  $("code").disabled = on;
  for (const b of document.querySelectorAll("#saved-list button")) b.disabled = on;
}

function normalize(input) {
  return input.replace(/[\s-]/g, "").toUpperCase();
}

// ---- Session ----

let session = null;

/** What was typed: the one-time code if it can be one, else the fixed password. */
function readSecret(raw) {
  const c = normalize(raw);
  if (CODE_ALPHABET.test(c)) return { secret: c, method: "code" };
  if ([...raw].length >= 8) return { secret: raw, method: "password" };
  return null;
}

const VIEW_KEY = "nexadesk.view";

/** "screen" or "terminal": what the person chose to open, remembered in this browser. */
function chosenView() {
  try {
    return localStorage.getItem(VIEW_KEY) === "terminal" ? "terminal" : "screen";
  } catch {
    return "screen";
  }
}

function showViewChoice() {
  const view = chosenView();
  for (const b of document.querySelectorAll("#view-choice button")) {
    b.setAttribute("aria-checked", String(b.dataset.view === view));
  }
  $("view-help").textContent = t("viewHelp." + view);
}

/** `opts.save`: keep this connection in the browser once it is let in. `opts.from`: the
 *  saved connection this was started from. */
function connect(code, deviceId, method = "code", opts = {}) {
  const pc = new RTCPeerConnection({ iceServers: [] });
  const path = mode === "server" ? "/web" : "/ws";
  const ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}${path}`);
  const s = { pc, ws, dc: null, input: false, clipboard: false, terminal: false, hostOs: "", ended: false };
  session = s;

  const finish = (key, { error = true } = {}) => {
    if (s.ended) return;
    s.ended = true;
    // A kept password the host no longer takes: drop it and ask for it again.
    if (key === "error.password" && opts.from?.sealed) {
      saved.forgetPassword(opts.from.id);
      key = "saved.stale";
      if (mode === "server") $("device").value = spaced(deviceId);
      setTimeout(() => $("code").focus(), 0);
    }
    s.stopStats?.();
    closeTerminal();
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
  // The host sends text copied there, and what it made of the quality asked for, on
  // the same channel.
  s.dc.onmessage = (e) => {
    let m;
    try { m = JSON.parse(e.data); } catch { return; }
    if (m?.t === "c" && typeof m.s === "string" && s.clipboard) receiveClip(m.s);
    if (m?.t === "q") showQualityNote(m);
  };
  s.dc.onopen = () => {
    sendQuality();
    sendResolution();
  };
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

  const offer = async () => {
    await pc.setLocalDescription(await pc.createOffer());
    ws.send(JSON.stringify({ type: "offer", sdp: pc.localDescription.sdp, browser: describeBrowser(), auth: method, view: chosenView() }));
  };
  ws.onopen = () => {
    // Through the server: say which host first; the offer follows once routed.
    if (mode === "server") ws.send(JSON.stringify({ type: "hello", id: deviceId }));
    else offer();
  };
  ws.onerror = () => finish(s.started ? "error.network" : "error.webrtc");
  ws.onclose = () => { if (!s.ended) finish(s.started ? "ended" : "error.protocol", { error: !s.started }); };
  ws.onmessage = async (ev) => {
    let m;
    try { m = JSON.parse(ev.data); } catch { return finish("error.protocol"); }
    switch (m.type) {
      case "routed":
        // The host starts on the server's relay; once it lets us in it also offers its
        // own addresses, and on the same network the browser moves to one of those.
        pc.setConfiguration({ iceServers: [m.ice], iceTransportPolicy: "all" });
        showStatus(t("connecting"), { busy: true });
        offer();
        break;
      case "answer": {
        if (method === "request") {
          // Nothing to prove: the person at the host decides.
          await pc.setRemoteDescription({ type: "answer", sdp: m.sdp });
          showStatus(t("requesting"), { busy: true });
          break;
        }
        showStatus(t(method === "password" ? "verifyingPassword" : "verifying"), { busy: true });
        await pc.setRemoteDescription({ type: "answer", sdp: m.sdp });
        const mine = fingerprintOf(pc.localDescription.sdp);
        const theirs = fingerprintOf(m.sdp);
        if (!mine || !theirs) return finish("error.protocol");
        try {
          const p = await loadPake();
          const r = pakeAnswer(p, code, method, mine, theirs, unb64(m.salt), unb64(m.msg));
          ws.send(JSON.stringify({ type: "auth", msg: b64(r.msg), confirm: b64(r.confirm) }));
        } catch (e) {
          return finish(e.message === "code" ? "error.format" : "error.wasm");
        }
        break;
      }
      case "waiting":
        showStatus(t(method === "request" ? "requesting" : "waiting"), { busy: true });
        break;
      case "accepted":
        // The host proves it knows the code for the same two fingerprints, or we leave.
        if (method !== "request" && !pakeHostOk(pake, unb64(m.confirm))) return finish("error.host");
        s.input = !!m.input;
        s.clipboard = !!m.clipboard;
        s.terminal = !!m.terminal;
        // No picture: the host gave only the command line.
        s.video = m.video !== false;
        s.hostOs = typeof m.os === "string" ? m.os : "";
        s.started = true;
        keep(m, code, deviceId, method, opts);
        showSession(s);
        break;
      case "candidates":
        for (const candidate of Array.isArray(m.list) ? m.list : []) {
          if (typeof candidate !== "string") continue;
          pc.addIceCandidate({ candidate, sdpMLineIndex: 0 }).catch(() => {});
        }
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

/** Saves the connection just let in, if asked, or brings a saved one up to date. */
function keep(accepted, code, deviceId, method, opts) {
  const name = typeof accepted.name === "string" ? accepted.name : "";
  const id = mode === "server" ? deviceId : "";
  // Through the server the device ID is worth keeping on its own; the host that served
  // this page needs its password, or there is nothing to save.
  const worth = mode === "server" || method === "password";
  if (opts.save && worth) {
    saved.save({ deviceId: id, name, view: chosenView(), password: method === "password" ? code : undefined }).then(showSaved);
  } else if (opts.from) {
    saved.save({ deviceId: id, name, view: chosenView() }).then(showSaved);
  }
}

const spaced = (id) => id.replace(/(\d{3})(?=\d)/g, "$1 ");

/** The saved connections that fit this page: device IDs through the server, the one
 *  host itself when it served the page. */
function showSaved() {
  const items = saved.list().filter((e) => (mode === "server" ? DEVICE_ID.test(e.deviceId) : e.deviceId === ""));
  $("saved").hidden = items.length === 0;
  const ul = $("saved-list");
  ul.replaceChildren();
  for (const e of items) {
    const li = document.createElement("li");
    li.className = "saved-item";
    const go = document.createElement("button");
    go.type = "button";
    go.className = "saved-go";
    const name = document.createElement("span");
    name.className = "saved-name";
    name.textContent = e.name || (e.deviceId ? `${t("saved.device")} ${spaced(e.deviceId)}` : t("saved.thisHost"));
    const meta = document.createElement("span");
    meta.className = "saved-meta";
    const where = e.deviceId ? spaced(e.deviceId) : location.host;
    meta.textContent = [where, t(e.sealed ? "saved.password" : e.deviceId ? "saved.ask" : "saved.typePassword"), t("view." + (e.view === "terminal" ? "terminal" : "screen"))].join(" · ");
    go.append(name, meta);
    go.addEventListener("click", () => connectSaved(e.id));
    const rm = document.createElement("button");
    rm.type = "button";
    rm.className = "saved-remove";
    rm.setAttribute("aria-label", `${t("saved.remove")} ${name.textContent}`);
    rm.textContent = "✕";
    // Two taps: the first asks, the second removes.
    rm.addEventListener("click", () => {
      if (rm.classList.contains("armed")) {
        saved.remove(e.id);
        showSaved();
        return;
      }
      rm.classList.add("armed");
      rm.textContent = t("saved.confirmRemove");
      setTimeout(() => {
        if (!rm.isConnected) return;
        rm.classList.remove("armed");
        rm.textContent = "✕";
      }, 4000);
    });
    li.append(go, rm);
    ul.append(li);
  }
  if (session) setConnecting(true);
}

/** One tap on a saved connection: its password if one is kept, else the host is asked. */
async function connectSaved(id) {
  if (session) return;
  const e = saved.find(id);
  if (!e) return showSaved();
  try { localStorage.setItem(VIEW_KEY, e.view === "terminal" ? "terminal" : "screen"); } catch {}
  showViewChoice();
  const password = await saved.passwordOf(e);
  if (!password && mode !== "server") {
    showStatus(t("saved.noPassword"), { error: true });
    $("code").focus();
    return;
  }
  setConnecting(true);
  showStatus(t("connecting"), { busy: true });
  if (!password) return connect("", e.deviceId, "request", { from: e });
  loadPake().then(() => connect(password, e.deviceId, "password", { from: e }), () => {
    setConnecting(false);
    showStatus(t("error.wasm"), { error: true });
  });
}

const REMEMBER_KEY = "nexadesk.remember";

/** What ticking "save" would keep, for what is typed now. */
function showRememberHelp() {
  const raw = $("code").value;
  const typed = raw.trim() === "" ? null : readSecret(raw);
  let key;
  if (mode === "server" && raw.trim() === "") key = "rememberHelp.request";
  else if (typed?.method === "password") key = saved.canKeepPasswords ? "rememberHelp.password" : "rememberHelp.noCrypto";
  else key = mode === "server" ? "rememberHelp.code" : "rememberHelp.codeDirect";
  $("remember-help").textContent = t(key);
  $("remember-help").hidden = !$("remember").checked;
}

function showConnect() {
  document.body.classList.remove("in-session");
  showSaved();
  $("session-view").hidden = true;
  $("connect-view").hidden = false;
  $("disconnect").hidden = true;
  $("fullscreen").hidden = true;
  $("mode-tag").hidden = true;
  $("keyboard").hidden = true;
  $("terminal").hidden = true;
  $("swap").hidden = true;
  $("net").hidden = true;
  $("netpanel").hidden = true;
  $("keybar").hidden = true;
  $("toast").hidden = true;
  touch?.reset();
  const v = $("screen");
  v.srcObject = null;
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
}

function showSession(s) {
  document.body.classList.add("in-session");
  $("connect-view").hidden = true;
  $("session-view").hidden = false;
  $("session-view").classList.toggle("control", s.input);
  $("disconnect").hidden = false;
  $("fullscreen").hidden = !document.fullscreenEnabled;
  const tag = $("mode-tag");
  tag.hidden = false;
  tag.textContent = t(s.input ? "control" : "viewOnly");
  tag.classList.toggle("control", s.input);
  // A keyboard that is not a Mac's, on a Mac: Ctrl does what ⌘ does there.
  const swapUseful = s.input && s.hostOs === "macos" && !MAC_BROWSER;
  keyboard.swap = swapUseful;
  $("swap").hidden = !swapUseful;
  showSwap();
  $("keyboard").hidden = !(s.input && TOUCH);
  $("terminal").hidden = !s.terminal || !s.video;
  $("terminal").setAttribute("aria-pressed", "false");
  $("session-view").classList.toggle("terminal-only", !s.video);
  if (!s.video) {
    tag.textContent = t("view.terminal");
    $("keys-hint").hidden = true;
    $("keyboard").hidden = true;
    $("swap").hidden = true;
    $("net").hidden = false;
    $("net-text").textContent = "…";
    s.stopStats = watch(s.pc, showNet);
    openTerminal();
    return;
  }
  const hint = $("keys-hint");
  hint.textContent = t(TOUCH ? "touchHint" : "keysHint");
  hint.hidden = !s.input;
  setTimeout(() => { hint.hidden = true; }, 7000);
  setSoftKeyboard(false);
  $("net").hidden = false;
  $("net-text").textContent = "…";
  showQualityChoice();
  showResolutionChoice();
  s.stopStats = watch(s.pc, showNet);
  keyboard.focus();
}

// ---- Command line ----

let terminal = null;

/** Opens the host's command line over the picture, or brings it back. */
function openTerminal() {
  const s = session;
  if (!s?.started || !s.terminal) return;
  $("termpanel").hidden = false;
  $("terminal").setAttribute("aria-pressed", "true");
  $("term-note").textContent = "";
  if (terminal && !terminal.ended) {
    terminal.focus();
    return;
  }
  closeTerminal({ keepPanel: true });
  terminal = new RemoteTerminal(s.pc, $("term-body"), (why) => {
    $("term-note").textContent = t("term." + why) || why;
    // The command line was the whole session.
    if (!s.video && why === "exited") s.finish("ended", { error: false });
  });
}

/** Hides the command line; the shell keeps running until the session ends or it is closed. */
function hideTerminal() {
  $("termpanel").hidden = true;
  $("terminal").setAttribute("aria-pressed", "false");
  keyboard.focus();
}

/** Ends the shell on the host and takes the panel away. */
function closeTerminal({ keepPanel = false } = {}) {
  terminal?.close();
  terminal = null;
  $("term-body").replaceChildren();
  if (!keepPanel) {
    $("termpanel").hidden = true;
    $("terminal").setAttribute("aria-pressed", "false");
  }
}

// ---- Network and quality ----

const QUALITY_KEY = "nexadesk.quality";
const QUALITIES = ["auto", "clear", "balanced", "weak"];

function quality() {
  try {
    const q = localStorage.getItem(QUALITY_KEY);
    return QUALITIES.includes(q) ? q : "auto";
  } catch {
    return "auto";
  }
}

/** Tells the host what to send. Allowed when only watching: it is not input. */
function sendQuality() {
  const s = session;
  if (s?.dc?.readyState === "open") s.dc.send(JSON.stringify({ t: "q", m: quality() }));
}

function showQualityChoice() {
  for (const b of document.querySelectorAll("#quality button")) {
    b.setAttribute("aria-checked", String(b.dataset.q === quality()));
  }
}

// ---- Resolution ----

const RES_KEY = "nexadesk.resolution";
const RESOLUTIONS = ["auto", "native", "1080", "720"];

function resolution() {
  try {
    const r = localStorage.getItem(RES_KEY);
    return RESOLUTIONS.includes(r) ? r : "auto";
  } catch {
    return "auto";
  }
}

/** The picture's area in device pixels, rounded so small resizes change nothing. */
function windowPixels() {
  const stage = $("session-view");
  const dpr = window.devicePixelRatio || 1;
  const round = (v) => Math.max(64, Math.round((v * dpr) / 64) * 64);
  return { w: round(stage.clientWidth || window.innerWidth), h: round(stage.clientHeight || window.innerHeight) };
}

let sentWindow = "";

/** Tells the host how large a picture to send; Auto also says how large this window is. */
function sendResolution() {
  const s = session;
  if (s?.dc?.readyState !== "open" || s.video === false) return;
  const m = { t: "s", m: resolution(), ...windowPixels() };
  sentWindow = `${m.m} ${m.w}x${m.h}`;
  s.dc.send(JSON.stringify(m));
}

function showResolutionChoice() {
  const r = resolution();
  for (const b of document.querySelectorAll("#resolution button")) {
    b.setAttribute("aria-checked", String(b.dataset.r === r));
  }
  $("res-note").textContent = t("resHelp." + r);
}

let resizeTimer = 0;
/** The window changed size: with Auto, the host follows once it has settled. */
function onWindowResized() {
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(() => {
    if (resolution() !== "auto" || !session?.started) return;
    const { w, h } = windowPixels();
    if (`auto ${w}x${h}` !== sentWindow) sendResolution();
  }, 600);
}

function showQualityNote(m) {
  $("quality-note").textContent = m.reduced ? t("q.reduced") : "";
}

function showNet(r) {
  const method = t("net." + r.method);
  $("net-dot").className = "dot " + r.grade;
  const parts = [method];
  if (r.rttMs != null) parts.push(`${r.rttMs} ms`);
  if (r.fps > 0) parts.push(`${r.fps} fps`);
  $("net-text").textContent = parts.join(" · ");
  $("net").title = method;
  $("net-method").textContent = method;
  $("net-rtt").textContent = r.rttMs == null ? "—" : `${r.rttMs} ms`;
  $("net-loss").textContent = `${r.lossPct.toFixed(1)} %`;
  $("net-kbps").textContent = r.kbps >= 1000 ? `${(r.kbps / 1000).toFixed(1)} Mbit/s` : `${r.kbps} kbit/s`;
  $("net-fps").textContent = String(r.fps);
  $("net-size").textContent = r.width ? `${r.width} × ${r.height}` : "—";
}

function showSwap() {
  const b = $("swap");
  b.textContent = t(keyboard.swap ? "swapOn" : "swapOff");
  b.setAttribute("aria-pressed", String(keyboard.swap));
  b.title = t("swapTip");
}

// ---- Input ----

const MAC_BROWSER = /Mac OS X|Macintosh|iPhone|iPad/.test(navigator.userAgent);
const TOUCH = matchMedia("(pointer: coarse)").matches;

function send(msg) {
  const s = session;
  if (!s?.started || !s.input || s.dc?.readyState !== "open") return;
  s.dc.send(JSON.stringify(msg));
}

// Keep under the DataChannel's 256 KiB once encoded.
const MAX_CLIP_JSON = 255 * 1024;

/** Sends the browser's clipboard text to the host; false if it may not, or cannot. */
function sendClip(text) {
  const s = session;
  if (!s?.started || !s.clipboard || s.dc?.readyState !== "open") return false;
  const msg = JSON.stringify({ t: "c", s: text });
  if (new TextEncoder().encode(msg).length > MAX_CLIP_JSON) {
    toast(t("clip.tooLarge"));
    return false;
  }
  s.dc.send(msg);
  return true;
}

/** Text copied on the host: straight onto this clipboard if the browser lets us. */
async function receiveClip(text) {
  if (window.isSecureContext && navigator.clipboard?.writeText && document.hasFocus()) {
    try {
      await navigator.clipboard.writeText(text);
      toast(t("clip.copied"));
      return;
    } catch {}
  }
  // Some browsers only write the clipboard during a click.
  toast(t("clip.ready").replace("{n}", String([...text].length)), {
    action: t("clip.copy"),
    run: () => copyNow(text),
  });
}

async function copyNow(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Without a secure context: the old way, through a selection.
    const area = document.createElement("textarea");
    area.value = text;
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
  }
  toast(t("clip.copied"));
  keyboard.focus();
}

let toastTimer = null;
function toast(text, { action = null, run = null } = {}) {
  const el = $("toast");
  $("toast-text").textContent = text;
  const b = $("toast-action");
  b.hidden = !action;
  b.textContent = action ?? "";
  b.onclick = run ? () => { el.hidden = true; run(); } : null;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.hidden = true; }, action ? 15000 : 3000);
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
const inSession = () => !!session?.started && !$("session-view").hidden;

let keyboard = null;
let touch = null;
let pendingMove = null;
let wheelCarry = { x: 0, y: 0 };

/** The phone's own keyboard: shown only when asked for, so taps stay clicks. */
function setSoftKeyboard(on) {
  const field = $("kbd");
  field.inputMode = on || !TOUCH ? "text" : "none";
  $("keyboard").setAttribute("aria-pressed", String(on));
  $("keybar").hidden = !on;
  if (on) {
    $("keys-hint").hidden = true;
    field.blur();
    keyboard.focus();
  }
}

/** Keeps the key bar on top of the phone's keyboard, which covers the page's bottom. */
function followVisualViewport() {
  const vv = window.visualViewport;
  if (!vv) return;
  const place = () => {
    const covered = Math.max(0, window.innerHeight - vv.height - vv.offsetTop);
    $("keybar").style.bottom = `${covered}px`;
  };
  vv.addEventListener("resize", place);
  vv.addEventListener("scroll", place);
}

function setupInput() {
  const v = $("screen");
  keyboard = new Keyboard({
    field: $("kbd"),
    send,
    sendClip,
    active: () => inSession() && !!session?.input,
  });
  keyboard.onLatch = (latched) => {
    for (const b of document.querySelectorAll("#keybar [data-mod]")) {
      b.setAttribute("aria-pressed", String(latched.has(Number(b.dataset.mod))));
    }
  };
  touch = setupTouch(v, {
    at: fraction,
    move: (f) => send({ t: "m", ...f }),
    button: (b, d, f) => send({ t: "b", b, d, ...f }),
    wheel: (dx, dy) => send({ t: "w", dx, dy }),
    active: () => inSession() && !!session?.input,
  });

  v.addEventListener("contextmenu", (e) => e.preventDefault());
  v.addEventListener("pointermove", (e) => {
    if (e.pointerType === "touch") return;
    keyboard.placeAt(e.clientX, e.clientY);
    const f = fraction(e);
    if (!f) return;
    if (!pendingMove) requestAnimationFrame(() => { if (pendingMove) send({ t: "m", ...pendingMove }); pendingMove = null; });
    pendingMove = f;
  });
  v.addEventListener("pointerdown", (e) => {
    keyboard.placeAt(e.clientX, e.clientY);
    if (e.pointerType === "touch") return;
    const f = fraction(e);
    const b = BUTTON[e.button];
    if (!f || b === undefined) return;
    e.preventDefault();
    keyboard.focus();
    v.setPointerCapture(e.pointerId);
    send({ t: "b", b, d: true, ...f });
  });
  v.addEventListener("pointerup", (e) => {
    if (e.pointerType === "touch") return;
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

  // The on-screen key bar: keys a phone keyboard lacks. Buttons never take focus, so
  // the phone keyboard stays up.
  const bar = $("keybar");
  bar.addEventListener("pointerdown", (e) => e.preventDefault());
  bar.addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b || !inSession()) return;
    if (b.dataset.mod) keyboard.toggleLatch(Number(b.dataset.mod));
    else if (b.dataset.key) keyboard.tap(Number(b.dataset.key));
  });
  $("keyboard").addEventListener("click", () => {
    setSoftKeyboard($("keyboard").getAttribute("aria-pressed") !== "true");
  });
  $("net").addEventListener("click", () => {
    const panel = $("netpanel");
    panel.hidden = !panel.hidden;
    $("net").setAttribute("aria-expanded", String(!panel.hidden));
  });
  $("quality").addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b?.dataset.q) return;
    try { localStorage.setItem(QUALITY_KEY, b.dataset.q); } catch {}
    showQualityChoice();
    $("quality-note").textContent = "";
    sendQuality();
    keyboard.focus();
  });
  $("resolution").addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b?.dataset.r) return;
    try { localStorage.setItem(RES_KEY, b.dataset.r); } catch {}
    showResolutionChoice();
    sendResolution();
    keyboard.focus();
  });
  window.addEventListener("resize", onWindowResized);
  document.addEventListener("fullscreenchange", onWindowResized);
  $("swap").addEventListener("click", () => {
    keyboard.release();
    keyboard.swap = !keyboard.swap;
    showSwap();
    keyboard.focus();
  });

  followVisualViewport();

  // Nothing stays held when the page loses the keyboard.
  const release = () => { if (session?.started) keyboard.release(); };
  window.addEventListener("blur", release);
  document.addEventListener("visibilitychange", () => { if (document.hidden) release(); });
}

// ---- Start ----

/** Fills the language menu and redoes the texts set from code when it changes. */
function setupLanguage() {
  const menu = $("lang");
  for (const [code, name] of LANGS) menu.add(new Option(name, code, false, code === lang));
  menu.addEventListener("change", () => {
    setLang(menu.value);
    if (mode === "server") {
      $("security-note").textContent = `${t("securityServer")} ${t("securityRequest")}`;
      $("code-help").textContent = t("codeHelpServer");
    }
    $("reveal").textContent = t($("code").type === "text" ? "hide" : "show");
    showViewChoice();
    showSaved();
    showRememberHelp();
    const s = session;
    if (s?.started) {
      $("mode-tag").textContent = t(s.input ? "control" : "viewOnly");
      $("keys-hint").textContent = t(TOUCH ? "touchHint" : "keysHint");
      showSwap();
      showResolutionChoice();
    }
  });
}

function init() {
  apply();
  setupLanguage();
  setupInput();
  const code = $("code");
  try { $("remember").checked = localStorage.getItem(REMEMBER_KEY) === "1"; } catch {}
  $("remember").addEventListener("change", () => {
    try { localStorage.setItem(REMEMBER_KEY, $("remember").checked ? "1" : "0"); } catch {}
    showRememberHelp();
  });
  code.addEventListener("input", showRememberHelp);
  code.addEventListener("input", () => {
    code.classList.remove("invalid");
    $("code-help").classList.remove("invalid");
    $("code-help").textContent = t(mode === "server" ? "codeHelpServer" : "codeHelp");
  });
  $("connect-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const id = $("device").value.replace(/\D/g, "");
    if (mode === "server" && !DEVICE_ID.test(id)) {
      $("device").classList.add("invalid");
      $("device-help").classList.add("invalid");
      $("device-help").textContent = t("error.deviceFormat");
      return;
    }
    // Through the server, an empty code asks the person at the host instead.
    if (mode === "server" && code.value.trim() === "") {
      setConnecting(true);
      showStatus(t("connecting"), { busy: true });
      connect("", id, "request", { save: $("remember").checked });
      return;
    }
    const typed = readSecret(code.value);
    if (!typed) {
      code.classList.add("invalid");
      $("code-help").classList.add("invalid");
      $("code-help").textContent = t("error.format");
      return;
    }
    setConnecting(true);
    showStatus(t("connecting"), { busy: true });
    const save = $("remember").checked;
    loadPake().then(() => connect(typed.secret, id, typed.method, { save }), () => {
      setConnecting(false);
      showStatus(t("error.wasm"), { error: true });
    });
  });
  $("reveal").addEventListener("click", () => {
    const shown = code.type === "text";
    code.type = shown ? "password" : "text";
    $("reveal").textContent = t(shown ? "show" : "hide");
    $("reveal").setAttribute("aria-pressed", String(!shown));
    code.focus();
  });
  $("device").addEventListener("input", () => {
    $("device").classList.remove("invalid");
    $("device-help").classList.remove("invalid");
    $("device-help").textContent = t("deviceHelp");
  });
  fetch("/mode").then((r) => r.json()).then((m) => {
    if (m?.mode !== "server") return;
    mode = "server";
    $("device-row").hidden = false;
    $("code").required = false;
    $("code-help").textContent = t("codeHelpServer");
    $("security-note").textContent = `${t("securityServer")} ${t("securityRequest")}`;
    $("device").focus();
  }).catch(() => {}).finally(() => {
    showSaved();
    showRememberHelp();
  });
  $("disconnect").addEventListener("click", () => session?.finish("ended", { error: false }));
  $("terminal").addEventListener("click", () => ($("termpanel").hidden ? openTerminal() : hideTerminal()));
  for (const b of document.querySelectorAll("#view-choice button")) {
    b.addEventListener("click", () => {
      try { localStorage.setItem(VIEW_KEY, b.dataset.view); } catch {}
      showViewChoice();
    });
  }
  showViewChoice();
  $("term-close").addEventListener("click", () => {
    closeTerminal();
    keyboard.focus();
  });
  $("fullscreen").addEventListener("click", () => $("session-view").requestFullscreen?.().catch(() => {}));
  window.addEventListener("pagehide", () => session?.finish("ended", { error: false }));
  code.focus();
}

init();
