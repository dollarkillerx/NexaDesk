// Connections saved in this browser, so the next time is one tap: a host's device ID,
// its name and what to open, and, only if the person asks, its fixed password.
//
// A password is never kept as text. It is sealed with AES-GCM under a key this browser
// generated and keeps in IndexedDB as non-extractable: page script can use the key but
// cannot read it out, and the sealed bytes in localStorage are useless without it.
// One-time codes are never saved: each one works once.

const KEY = "nexadesk.saved";
const MOST = 20;

/** Whether this browser can seal a password (WebCrypto needs a secure context). */
export const canKeepPasswords = !!(globalThis.crypto?.subtle && globalThis.indexedDB);

function read() {
  try {
    const list = JSON.parse(localStorage.getItem(KEY) ?? "[]");
    return Array.isArray(list) ? list.filter((e) => e && typeof e.id === "string") : [];
  } catch {
    return [];
  }
}

function write(list) {
  try {
    localStorage.setItem(KEY, JSON.stringify(list.slice(0, MOST)));
    return true;
  } catch {
    return false;
  }
}

/** The saved connections, the one used last first. */
export function list() {
  return read().sort((a, b) => (b.last ?? 0) - (a.last ?? 0));
}

export function find(id) {
  return read().find((e) => e.id === id) ?? null;
}

/** Saves or updates the connection to `deviceId` ("" for the host that served this
 *  page). `password`: the fixed password to keep, `null` to keep none, `undefined` to
 *  leave whatever is kept as it is. */
export async function save({ deviceId, name, view, password }) {
  const all = read();
  let entry = all.find((e) => e.deviceId === deviceId);
  if (!entry) {
    entry = { id: crypto.randomUUID?.() ?? String(Date.now()) + Math.random(), deviceId };
    all.unshift(entry);
  }
  if (name) entry.name = String(name).slice(0, 64);
  if (view) entry.view = view;
  entry.last = Date.now();
  if (password === null) delete entry.sealed;
  else if (password !== undefined && canKeepPasswords) {
    try {
      entry.sealed = await seal(password);
    } catch {
      delete entry.sealed;
    }
  }
  write(all);
  return entry;
}

export function remove(id) {
  write(read().filter((e) => e.id !== id));
}

/** Drops a kept password that no longer works, keeping the rest. */
export function forgetPassword(id) {
  const all = read();
  const e = all.find((x) => x.id === id);
  if (e) delete e.sealed;
  write(all);
}

/** The kept password, or null if none is kept or it cannot be opened here. */
export async function passwordOf(entry) {
  if (!entry?.sealed || !canKeepPasswords) return null;
  try {
    const key = await sealingKey(false);
    if (!key) return null;
    const iv = unb64(entry.sealed.iv);
    const plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv }, key, unb64(entry.sealed.ct));
    return new TextDecoder().decode(plain);
  } catch {
    return null;
  }
}

async function seal(password) {
  const key = await sealingKey(true);
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, new TextEncoder().encode(password));
  return { iv: b64(iv), ct: b64(new Uint8Array(ct)) };
}

// ---- The sealing key, in IndexedDB ----

function openDb() {
  return new Promise((resolve, reject) => {
    const r = indexedDB.open("nexadesk", 1);
    r.onupgradeneeded = () => r.result.createObjectStore("keys");
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

function request(db, mode, run) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction("keys", mode);
    const r = run(tx.objectStore("keys"));
    tx.oncomplete = () => resolve(r.result);
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });
}

async function sealingKey(create) {
  const db = await openDb();
  try {
    let key = await request(db, "readonly", (s) => s.get("seal"));
    if (!key && create) {
      key = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
      await request(db, "readwrite", (s) => s.put(key, "seal"));
    }
    return key ?? null;
  } finally {
    db.close();
  }
}

const b64 = (bytes) => btoa(String.fromCharCode(...bytes));
const unb64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
