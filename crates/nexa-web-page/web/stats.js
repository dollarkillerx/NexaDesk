// How the connection is doing, from WebRTC's own statistics, once a second: which way
// it goes (LAN, Tailscale, relayed through the server, or straight across the
// internet), round trip, loss, bitrate, frame rate and size of the picture.

function v4(addr) {
  const p = addr.split(".").map(Number);
  return p.length === 4 && p.every((n) => n >= 0 && n < 256) ? p : null;
}

/** "lan", "tailscale" or "internet" for the address the media goes to. */
export function kindOf(address = "") {
  const a = address.toLowerCase();
  const p = v4(a);
  if (p) {
    if (p[0] === 100 && p[1] >= 64 && p[1] < 128) return "tailscale";
    if (p[0] === 10 || (p[0] === 172 && p[1] >= 16 && p[1] < 32) || (p[0] === 192 && p[1] === 168)) return "lan";
    if (p[0] === 169 && p[1] === 254) return "lan";
    if (p[0] === 127) return "lan";
    return "internet";
  }
  if (a === "::1") return "lan";
  if (a.startsWith("fd7a:115c:a1e0:")) return "tailscale";
  if (a.startsWith("fc") || a.startsWith("fd") || a.startsWith("fe80:")) return "lan";
  return "internet";
}

/** "good", "fair" or "poor": what the dot in the header shows. */
export function grade({ rttMs, lossPct }) {
  if (rttMs == null) return "fair";
  if (rttMs < 80 && lossPct < 2) return "good";
  if (rttMs < 200 && lossPct < 8) return "fair";
  return "poor";
}

/**
 * Reads `pc` every second and calls `update(reading)` with
 * { method, rttMs, lossPct, kbps, fps, width, height, grade }.
 * Returns a function that stops it.
 */
export function watch(pc, update) {
  let last = null;
  const tick = async () => {
    let stats;
    try { stats = await pc.getStats(); } catch { return; }
    let pair = null, video = null;
    stats.forEach((r) => {
      if (r.type === "transport" && r.selectedCandidatePairId) pair = stats.get(r.selectedCandidatePairId) ?? pair;
      if (!pair && r.type === "candidate-pair" && r.nominated && r.state === "succeeded") pair = r;
      if (r.type === "inbound-rtp" && r.kind === "video") video = r;
    });
    const reading = { method: "unknown", rttMs: null, lossPct: 0, kbps: 0, fps: 0, width: 0, height: 0 };
    if (pair) {
      const local = stats.get(pair.localCandidateId), remote = stats.get(pair.remoteCandidateId);
      if (local?.candidateType === "relay" || remote?.candidateType === "relay") reading.method = "relay";
      else reading.method = kindOf(remote?.address);
      if (pair.currentRoundTripTime != null) reading.rttMs = Math.round(pair.currentRoundTripTime * 1000);
    }
    if (video) {
      const now = performance.now();
      if (last) {
        const secs = (now - last.at) / 1000;
        const bytes = video.bytesReceived - last.bytes;
        const got = video.packetsReceived - last.packets, lost = video.packetsLost - last.lost;
        reading.kbps = secs > 0 ? Math.round((bytes * 8) / secs / 1000) : 0;
        reading.lossPct = got + lost > 0 ? Math.max(0, (lost / (got + lost)) * 100) : 0;
      }
      last = { at: now, bytes: video.bytesReceived, packets: video.packetsReceived, lost: video.packetsLost };
      reading.fps = Math.round(video.framesPerSecond ?? 0);
      reading.width = video.frameWidth ?? 0;
      reading.height = video.frameHeight ?? 0;
    }
    reading.grade = grade(reading);
    update(reading);
  };
  const timer = setInterval(tick, 1000);
  tick();
  return () => clearInterval(timer);
}
