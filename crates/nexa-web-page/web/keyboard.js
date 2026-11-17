// The keyboard, into the host. Three ways in, one hidden text field to catch them all:
//
// - Physical keys go as HID usages (keys.js), so shortcuts and games work.
// - Symbols go as the character this keyboard prints on them (unless the person asks for
//   positions): a Japanese keyboard and an American one put @, [, : and the like on
//   different keys, so the key's position would type something else on the host.
// - Text an input method composes (Chinese, Japanese…) or a phone keyboard types goes as
//   text, which the host types as Unicode: the keys that made it mean nothing there.
// - A paste sends the browser's clipboard text first, then the paste keystroke, on the
//   same ordered channel, so the host pastes what was just copied here.
//
// The field holds focus while a session runs; an input method needs an editable
// element to work in. It sits where the pointer last was, so a candidate window opens
// near what is being typed into.

import { hidFor } from "./keys.js";

/** A Mac's (or an iPhone's or iPad's) browser: ⌘ is its shortcut key, and it sends no
 *  key-up for a key let go while ⌘ is held. */
export const MAC_BROWSER = /Mac OS X|Macintosh|iPhone|iPad/.test(navigator.userAgent);

const CTRL = 0xe0, SHIFT = 0xe1, ALT = 0xe2, META = 0xe3;
const MODIFIERS = [
  { flag: "ctrlKey", hids: [CTRL, 0xe4] },
  { flag: "shiftKey", hids: [SHIFT, 0xe5] },
  { flag: "altKey", hids: [ALT, 0xe6] },
  { flag: "metaKey", hids: [META, 0xe7] },
];
const IS_MODIFIER = new Set([CTRL, SHIFT, ALT, META, 0xe4, 0xe5, 0xe6, 0xe7]);
// Ctrl ↔ ⌘, both sides.
const SWAP = { 0xe0: 0xe3, 0xe4: 0xe7, 0xe3: 0xe0, 0xe7: 0xe4 };
const KEY_V = 0x19;
const ENTER = 0x28, BACKSPACE = 0x2a, DELETE = 0x4c;
// The host types text of up to 256 bytes per message.
const TEXT_CHUNK = 240;
// How long a paste keystroke waits for the browser's paste event.
const PASTE_WAIT = 400;

/** A symbol typed with no shortcut modifier: sent as the character, not the key.
 *  Letters, digits and space stay keys: every Latin layout agrees on them, and games
 *  and single-key shortcuts need them as keys. */
export function isSymbol(e) {
  if ([...e.key].length !== 1) return false;
  // ⌘ or Ctrl makes a shortcut; AltGr (Ctrl+Alt on Windows) makes a character.
  if ((e.metaKey || e.ctrlKey) && !e.getModifierState?.("AltGraph")) return false;
  return !/[\p{L}\p{N}\s]/u.test(e.key);
}

/** HID usage of a character on a US keyboard, with whether Shift makes it; or null. */
function hidForChar(ch) {
  if (/^[a-z]$/.test(ch)) return [0x04 + ch.charCodeAt(0) - 97, false];
  if (/^[A-Z]$/.test(ch)) return [0x04 + ch.charCodeAt(0) - 65, true];
  if (/^[1-9]$/.test(ch)) return [0x1e + ch.charCodeAt(0) - 49, false];
  if (ch === "0") return [0x27, false];
  if (ch === " ") return [0x2c, false];
  return null;
}

/** Splits text into pieces of at most `max` UTF-8 bytes, never inside a character. */
export function chunks(text, max = TEXT_CHUNK) {
  const out = [];
  let piece = "", bytes = 0;
  for (const ch of text) {
    const n = new TextEncoder().encode(ch).length;
    if (bytes + n > max) {
      out.push(piece);
      piece = "";
      bytes = 0;
    }
    piece += ch;
    bytes += n;
  }
  if (piece) out.push(piece);
  return out;
}

export class Keyboard {
  /**
   * @param field   the hidden <textarea>
   * @param send    sends one input message to the host (dropped unless input is allowed)
   * @param sendClip sends clipboard text to the host; returns false if not allowed
   * @param active  whether a session with input is showing
   */
  constructor({ field, send, sendClip, active }) {
    this.field = field;
    this.send = send;
    this.sendClip = sendClip;
    this.active = active;
    /** Ctrl and ⌘ trade places: for a keyboard whose shortcut key is not the host's (a
     *  PC's on a Mac, a Mac's on Windows or Linux). */
    this.swap = false;
    /** Whether the host's shortcuts use ⌘ (a Mac) rather than Ctrl. */
    this.hostMac = true;
    this.held = new Set();
    /** Keys that went down while ⌘ was held, on a Mac: their key-ups never come. */
    this.underMeta = new Set();
    /** Modifiers latched on the on-screen key bar, for the next key or character. */
    this.latched = new Set();
    this.onLatch = () => {};
    this.pendingPaste = null;
    this.swallowUp = new Set();
    /** Keys whose symbol went as text: their key-up is nobody's business. */
    this.typedUp = new Set();
    /** Symbols as printed on this keyboard (true) or as key positions (false). */
    this.symbols = true;
    this.composing = false;

    field.addEventListener("keydown", (e) => this.key(e, true));
    field.addEventListener("keyup", (e) => this.key(e, false));
    field.addEventListener("compositionstart", () => {
      this.composing = true;
      field.classList.add("composing");
    });
    field.addEventListener("compositionend", (e) => {
      this.composing = false;
      field.classList.remove("composing");
      if (e.data) this.text(e.data);
      field.value = "";
    });
    field.addEventListener("beforeinput", (e) => this.beforeInput(e));
    field.addEventListener("paste", (e) => this.paste(e));
  }

  /** Where the host has a usage for this key, with Ctrl and ⌘ swapped if asked. */
  hid(e) {
    const c = hidFor(e);
    return c !== undefined && this.swap && SWAP[c] ? SWAP[c] : c;
  }

  flags(e) {
    if (!this.swap) return e;
    return { ctrlKey: e.metaKey, shiftKey: e.shiftKey, altKey: e.altKey, metaKey: e.ctrlKey };
  }

  /** Brings the host's modifiers in line with what this key event says is held. */
  sync(e) {
    const f = this.flags(e);
    for (const m of MODIFIERS) {
      const down = m.hids.filter((h) => this.held.has(h));
      if (f[m.flag] && down.length === 0) this.down(m.hids[0]);
      else if (!f[m.flag]) for (const h of down) this.up(h);
    }
  }

  down(c, repeat = false) {
    this.send({ t: "k", c, d: true, r: repeat });
    this.held.add(c);
  }

  up(c) {
    this.send({ t: "k", c, d: false, r: false });
    this.held.delete(c);
  }

  /** Down and up, with the latched modifiers around it. */
  tap(c, extra = []) {
    const mods = [...new Set([...this.latched, ...extra])].filter((m) => !this.held.has(m));
    for (const m of mods) this.down(m);
    this.down(c);
    this.up(c);
    for (const m of mods.reverse()) this.up(m);
    this.clearLatch();
  }

  clearLatch() {
    if (this.latched.size === 0) return;
    this.latched.clear();
    this.onLatch(this.latched);
  }

  toggleLatch(mod) {
    if (this.latched.has(mod)) this.latched.delete(mod);
    else this.latched.add(mod);
    this.onLatch(this.latched);
  }

  isPaste(e, c) {
    // The host's paste (⌘V on a Mac, Ctrl+V elsewhere), however this keyboard spells it.
    const f = this.flags(e);
    return c === KEY_V && (this.hostMac ? f.metaKey : f.ctrlKey) && !e.altKey && !e.repeat;
  }

  key(e, isDown) {
    if (!this.active()) return;
    // An input method is working on this key: leave it alone, its result comes as text.
    if (e.isComposing || this.composing || e.keyCode === 229 || e.key === "Process") return;
    // Phone keyboards say nothing useful here; what they type arrives as beforeinput.
    if (e.key === "Unidentified") return;
    const c = this.hid(e);
    if (c === undefined) return;
    if (MAC_BROWSER && isDown && e.metaKey && !IS_MODIFIER.has(c)) this.underMeta.add(c);
    // ⌘ itself comes up: so do the keys pressed under it, before it.
    if (MAC_BROWSER && !isDown && e.key === "Meta") this.releaseUnderMeta();

    if (!isDown && (this.swallowUp.delete(c) || this.typedUp.delete(c))) {
      e.preventDefault();
      return;
    }
    if (isDown && this.symbols && !this.latched.size && isSymbol(e)) {
      e.preventDefault();
      this.typedUp.add(c);
      this.send({ t: "s", s: e.key });
      return;
    }
    if (isDown && this.isPaste(e, c)) {
      this.sync(e);
      // Let the browser fire its paste event, which carries the clipboard's text.
      this.pendingPaste = setTimeout(() => this.finishPaste(), PASTE_WAIT);
      this.swallowUp.add(c);
      return;
    }
    e.preventDefault();
    if (!IS_MODIFIER.has(c)) this.sync(e);
    if (isDown) {
      if (this.latched.size && !IS_MODIFIER.has(c)) return this.tap(c);
      this.down(c, e.repeat);
    } else {
      this.up(c);
    }
  }

  paste(e) {
    e.preventDefault();
    // Sent even when only watching: the clipboard is a permission of its own.
    const text = e.clipboardData?.getData("text/plain") ?? "";
    if (text) this.sendClip(text);
    if (this.active() && this.pendingPaste !== null) this.finishPaste();
  }

  finishPaste() {
    clearTimeout(this.pendingPaste);
    this.pendingPaste = null;
    this.tap(KEY_V);
  }

  /** What a phone keyboard (or anything else that edits the field) typed. */
  beforeInput(e) {
    if (!this.active()) return;
    if (e.isComposing || this.composing || e.inputType.startsWith("insertComposition")) return;
    e.preventDefault();
    switch (e.inputType) {
      case "insertText":
      case "insertReplacementText":
        if (e.data) this.text(e.data);
        break;
      case "insertLineBreak":
      case "insertParagraph":
        this.tap(ENTER);
        break;
      case "deleteContentBackward":
      case "deleteWordBackward":
        this.tap(BACKSPACE);
        break;
      case "deleteContentForward":
        this.tap(DELETE);
        break;
    }
  }

  /** Text: a shortcut if a modifier is latched and it is one key, else typed as text. */
  text(s) {
    if (this.latched.size && [...s].length === 1) {
      const k = hidForChar(s);
      if (k) return this.tap(k[0], k[1] ? [SHIFT] : []);
    }
    this.clearLatch();
    for (const piece of chunks(s)) this.send({ t: "s", s: piece });
  }

  /** Lets go of the keys pressed under ⌘ on a Mac, whose key-ups the browser keeps. A
   *  pasted V's key-up, swallowed otherwise, would be waited for until the next V. */
  releaseUnderMeta() {
    for (const c of this.underMeta) {
      this.swallowUp.delete(c);
      if (this.held.has(c)) this.up(c);
    }
    this.underMeta.clear();
  }

  /** The keys held on the host for this page, as HID usages. */
  keyState() {
    return [...this.held];
  }

  /** Lets go of everything: the page lost the keyboard. */
  release() {
    clearTimeout(this.pendingPaste);
    this.pendingPaste = null;
    this.held.clear();
    this.underMeta.clear();
    this.swallowUp.clear();
    this.typedUp.clear();
    this.clearLatch();
    this.send({ t: "r" });
  }

  /** Keeps the field, and so an input method's window, near (x, y) on the page. */
  placeAt(x, y) {
    this.field.style.left = `${Math.round(x)}px`;
    this.field.style.top = `${Math.round(y)}px`;
  }

  focus() {
    this.field.focus({ preventScroll: true });
  }
}
