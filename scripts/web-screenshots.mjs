// The README's web screenshots, taken in headless Google Chrome against the nexa-web demo:
// the connect page, a session with the network panel open, and a terminal-only session.
// Run by scripts/screenshots.sh, which starts the demo:
//   node scripts/web-screenshots.mjs <demo url> <demo log> <output dir>
// The code is read from the demo's log before each session: a used code is replaced.
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [url, log, out] = process.argv.slice(2);
const code = () => [...readFileSync(log, "utf8").matchAll(/^one-time code: (\S+)$/gm)].at(-1)?.[1].replace("-", "") ?? "";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const profile = mkdtempSync(join(tmpdir(), "nexa-shots-"));
const chrome = spawn("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", [
  "--headless=new", `--user-data-dir=${profile}`, "--remote-debugging-port=9334",
  // Only this throwaway profile, only for the demo's self-signed certificate.
  "--ignore-certificate-errors", "--lang=zh-CN", "--accept-lang=zh-CN", "--hide-scrollbars", "about:blank",
], { stdio: "ignore" });
const quit = () => {
  chrome.kill();
  // Chrome may still be writing its profile as it goes; a leftover temp dir is harmless.
  try { rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch {}
};
process.on("exit", quit);

let target;
for (let i = 0; i < 50 && !target; i++) {
  await sleep(200);
  try {
    target = (await (await fetch("http://127.0.0.1:9334/json")).json()).find((t) => t.type === "page");
  } catch {}
}
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((r) => ws.addEventListener("open", r, { once: true }));
let next = 0;
const waiting = new Map();
ws.addEventListener("message", (e) => {
  const m = JSON.parse(e.data);
  if (m.id && waiting.has(m.id)) {
    waiting.get(m.id)(m);
    waiting.delete(m.id);
  }
});
const cdp = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = ++next;
    waiting.set(id, (m) => (m.error ? reject(new Error(m.error.message)) : resolve(m.result)));
    ws.send(JSON.stringify({ id, method, params }));
  });
const js = async (expr) => (await cdp("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true })).result.value;
const until = async (expr, what) => {
  for (let i = 0; i < 100; i++) {
    if (await js(expr)) return;
    await sleep(200);
  }
  throw new Error("timed out: " + what);
};
const shot = async (name) => {
  const { data } = await cdp("Page.captureScreenshot", { format: "png" });
  writeFileSync(join(out, name), Buffer.from(data, "base64"));
  console.log(join(out, name));
};
const type = async (text) => {
  await cdp("Input.insertText", { text });
  await cdp("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" });
  await cdp("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
};
const connect = async (view) => {
  await cdp("Page.navigate", { url });
  await until(`document.readyState === "complete" && !!document.getElementById("view-choice")`, "page");
  await js(`localStorage.setItem("nexadesk.view", ${JSON.stringify(view)}); localStorage.setItem("nexadesk.lang", "zh-Hans"); location.reload()`);
  await sleep(500);
  await until(`document.readyState === "complete"`, "reload");
  await sleep(300);
  await js(`document.getElementById("code").value = ${JSON.stringify(code())}; document.getElementById("connect").click()`);
};

await cdp("Page.enable");
await cdp("Runtime.enable");
await cdp("Emulation.setDeviceMetricsOverride", { width: 1100, height: 760, deviceScaleFactor: 2, mobile: false });

// The connect page, with the screen / terminal choice.
await cdp("Page.navigate", { url });
await until(`document.readyState === "complete" && !!document.getElementById("view-choice")`, "page");
await js(`localStorage.setItem("nexadesk.view", "screen"); localStorage.setItem("nexadesk.lang", "zh-Hans"); location.reload()`);
await sleep(500);
await until(`document.readyState === "complete"`, "reload");
await sleep(500);
await shot("web.png");

// A session, with connection, quality and resolution open.
await cdp("Emulation.setDeviceMetricsOverride", { width: 1280, height: 760, deviceScaleFactor: 2, mobile: false });
await connect("screen");
await until(`document.getElementById("screen").videoWidth > 0`, "video");
await until(`/fps/.test(document.getElementById("net-text").textContent)`, "stats");
await js(`document.getElementById("keys-hint").hidden = true; document.getElementById("net").click()`);
await sleep(1500);
await shot("web-session.png");
await js(`document.getElementById("disconnect").click()`);
await sleep(500);

// Only the command line: a plain shell with a made-up prompt.
await cdp("Emulation.setDeviceMetricsOverride", { width: 1100, height: 520, deviceScaleFactor: 2, mobile: false });
await connect("terminal");
await until(`!document.getElementById("termpanel").hidden`, "terminal");
await sleep(1500);
await type(`PS1='demo@studio ~ $ '; clear`);
await sleep(500);
for (const command of [`echo "Hello from NexaDesk"`, `uname -sm`, `ls /`]) {
  await type(command);
  await sleep(600);
}
await shot("web-terminal.png");
await js(`document.getElementById("disconnect").click()`);
await sleep(300);
process.exit(0);
