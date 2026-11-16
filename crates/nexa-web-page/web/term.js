// The host's command line in the page: xterm.js on a WebRTC data channel named "term".
// Typed bytes go to the host as binary messages, its output comes back the same way;
// {t:"r", c, r} says the terminal changed size, and the host's {t:"x", why} that the
// shell has ended. Closing the channel ends the shell on the host.

import { Terminal } from "/vendor/xterm.mjs";
import { FitAddon } from "/vendor/addon-fit.mjs";

const encoder = new TextEncoder();

export class RemoteTerminal {
  /** Opens a shell on the host in `element`. `onEnd(why)` runs once when it is over. */
  constructor(pc, element, onEnd) {
    this.term = new Terminal({
      cursorBlink: true,
      fontFamily: '"IBM Plex Mono", Menlo, "SF Mono", Consolas, monospace',
      fontSize: 14,
      scrollback: 5000,
      theme: { background: "#161616", foreground: "#f4f4f4", cursor: "#f4f4f4", selectionBackground: "#4589ff66" },
    });
    this.fit = new FitAddon();
    this.term.loadAddon(this.fit);
    this.term.open(element);
    this.onEnd = onEnd;
    this.ended = false;

    this.dc = pc.createDataChannel("term", { ordered: true });
    this.dc.binaryType = "arraybuffer";
    this.dc.onopen = () => {
      this.resize();
      this.term.focus();
    };
    this.dc.onmessage = (e) => {
      if (typeof e.data === "string") {
        let m;
        try { m = JSON.parse(e.data); } catch { return; }
        if (m?.t === "x") this.end(m.why || "exited");
        return;
      }
      this.term.write(new Uint8Array(e.data));
    };
    this.dc.onclose = () => this.end("closed");
    const send = (bytes) => { if (this.dc.readyState === "open") this.dc.send(bytes); };
    this.term.onData((d) => send(encoder.encode(d)));
    this.term.onBinary((d) => send(Uint8Array.from(d, (c) => c.charCodeAt(0) & 0xff)));
    this.term.onResize(({ cols, rows }) => {
      if (this.dc.readyState === "open") this.dc.send(JSON.stringify({ t: "r", c: cols, r: rows }));
    });
    this.observer = new ResizeObserver(() => this.resize());
    this.observer.observe(element);
  }

  resize() {
    try { this.fit.fit(); } catch {}
    const { cols, rows } = this.term;
    if (this.dc.readyState === "open") this.dc.send(JSON.stringify({ t: "r", c: cols, r: rows }));
  }

  focus() { this.term.focus(); }

  end(why) {
    if (this.ended) return;
    this.ended = true;
    this.onEnd?.(why);
  }

  /** Closes the terminal; the host ends the shell. */
  close() {
    this.observer.disconnect();
    try { this.dc.close(); } catch {}
    this.term.dispose();
  }
}
