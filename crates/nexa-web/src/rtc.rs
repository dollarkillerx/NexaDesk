//! One browser's WebRTC connection: str0m, driven by a UDP socket, the encoded frames,
//! commands from signaling and str0m's own timers, all on one task.
//!
//! The host is ICE-lite with a single candidate. For a browser that reached the host
//! itself, a host candidate on that same address: the browser checks it and str0m learns
//! the browser's address from its checks, so neither side needs STUN on a LAN or
//! tailnet. For a browser that came through the signaling server, the address of an
//! allocation on the server's TURN: both ends relay there, and nothing about this
//! machine's address is revealed until the person at the host lets the browser in.
//! Then this host's own addresses follow ([`Cmd::Direct`]): a browser on the same LAN
//! or tailnet checks them and moves the session off the relay, which stays as the
//! fallback.

use std::{
    collections::VecDeque,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Once, atomic::Ordering::Relaxed},
    time::{Duration, Instant},
};

use nexa_turn::{Client as TurnClient, ClientEvent};

use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig,
    bwe::{Bitrate, BweKind},
    change::{SdpAnswer, SdpOffer},
    format::{Codec, PayloadParams},
    media::{Frequency, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};
use tokio::{net::UdpSocket, sync::mpsc, time::sleep_until};

use crate::{
    WebConfig,
    input::{InputThread, Msg},
    media::{Control, Frame, Quality, Resolution, preset, sized},
};

/// Auto steps down when the estimate stays under this for [`STEP_DOWN_AFTER`]...
const POOR_BPS: u64 = 1_200_000;
const STEP_DOWN_AFTER: Duration = Duration::from_secs(3);
/// ...and back up when it stays over this for [`STEP_UP_AFTER`].
const GOOD_BPS: u64 = 3_500_000;
const STEP_UP_AFTER: Duration = Duration::from_secs(8);

/// `{t:"q", m:"auto"|"clear"|"balanced"|"weak"}` from the page.
fn quality_request(data: &[u8]) -> Option<Quality> {
    #[derive(serde::Deserialize)]
    struct Q {
        t: String,
        m: String,
    }
    let q: Q = serde_json::from_slice(data).ok()?;
    if q.t != "q" {
        return None;
    }
    Quality::parse(&q.m)
}

/// `{t:"s", m:"auto"|"native"|"1080"|"720", w, h}`: the picture size chosen in the page,
/// and its window in device pixels.
fn resolution_request(data: &[u8]) -> Option<(Resolution, Option<(u32, u32)>)> {
    #[derive(serde::Deserialize)]
    struct S {
        t: String,
        m: String,
        #[serde(default)]
        w: u32,
        #[serde(default)]
        h: u32,
    }
    let s: S = serde_json::from_slice(data).ok()?;
    if s.t != "s" {
        return None;
    }
    let window = (s.w > 0 && s.h > 0).then_some((s.w, s.h));
    Some((Resolution::parse(&s.m)?, window))
}

/// The quality in force, and what Auto made of the network.
struct Tuning {
    quality: Quality,
    resolution: Resolution,
    /// The browser's window in device pixels, for Auto.
    window: Option<(u32, u32)>,
    reduced: bool,
    poor_since: Option<Instant>,
    good_since: Option<Instant>,
}

/// The stream's rate on a LAN, where str0m's estimate is not used (see [`prepare`]). A
/// mostly still screen sends so little that one burst of jitter, a Wi-Fi radio waking
/// up, cut that estimate to a fraction of almost nothing, and str0m then paced every
/// frame at it: on a LAN at 3 ms with no loss the picture was 1280×720 at 0.3 Mbit/s,
/// and late. Here the rate grows every second the browser reports neither loss nor a
/// queue, and backs off when either lasts beyond one report.
pub(crate) struct LanRate {
    bps: f64,
    min_bps: f64,
    max_bps: f64,
    /// Round trips of the last minute, for the lowest.
    rtts: VecDeque<(Instant, Duration)>,
    /// Reports in a row that showed congestion.
    bad: u32,
}

impl LanRate {
    /// How often the browser's reports are read.
    pub(crate) const EVERY: Duration = Duration::from_secs(1);
    /// Per clean report: from the 8 Mbit/s start to 25 Mbit/s in three seconds.
    const GROWTH: f64 = 1.5;
    const BACKOFF: f64 = 0.7;
    /// Loss from this on is congestion, not noise.
    const LOSSY: f32 = 0.02;
    /// A round trip this much over the lowest of the last minute is a queue.
    const QUEUE: Duration = Duration::from_millis(60);
    const HISTORY: Duration = Duration::from_secs(60);

    pub(crate) fn new(config: &WebConfig) -> Self {
        Self {
            bps: f64::from(config.start_bitrate_bps),
            min_bps: f64::from(config.min_bitrate_bps),
            max_bps: f64::from(config.max_bitrate_bps),
            rtts: VecDeque::new(),
            bad: 0,
        }
    }

    /// One report: the loss toward the browser and the round trip, if it said. Returns
    /// the rate to send at.
    pub(crate) fn update(&mut self, loss: Option<f32>, rtt: Option<Duration>, now: Instant) -> u64 {
        if let Some(rtt) = rtt {
            self.rtts.push_back((now, rtt));
        }
        while self
            .rtts
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > Self::HISTORY)
        {
            self.rtts.pop_front();
        }
        let lowest = self.rtts.iter().map(|(_, r)| *r).min();
        let queued = rtt
            .zip(lowest)
            .is_some_and(|(r, low)| r > low + Self::QUEUE);
        let lossy = loss.is_some_and(|l| l >= Self::LOSSY);
        self.bad = if queued || lossy { self.bad + 1 } else { 0 };
        // One bad report on Wi-Fi is a hiccup; two in a row is the link.
        if self.bad >= 2 {
            self.bps *= Self::BACKOFF;
        } else if self.bad == 0 {
            self.bps *= Self::GROWTH;
        }
        self.bps = self.bps.clamp(self.min_bps, self.max_bps);
        self.bps as u64
    }
}

/// Whether the browser reached this host on a private address: a LAN, where bandwidth is
/// seldom what limits the stream. A Tailscale address (100.64/10) may cross the internet.
pub(crate) fn is_lan(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            // Unique local (fc00::/7) and link-local (fe80::/10).
            first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
    }
}

/// A new rate for the stream, from str0m's estimate or [`LanRate`]: Auto may step down
/// or back up, and the media thread takes it within the quality's range.
fn rate_changed(
    bps: u64,
    tuning: &mut Tuning,
    control: Option<&Arc<Control>>,
    rtc: &mut Rtc,
    channel: Option<str0m::channel::ChannelId>,
    config: &WebConfig,
) {
    if tuning.observe(bps, Instant::now()) {
        tuning.apply(control, rtc, channel, config);
    }
    if let Some(c) = control {
        let bps = bps.clamp(
            u64::from(config.min_bitrate_bps.min(300_000)),
            u64::from(config.max_bitrate_bps),
        );
        c.bitrate.store(bps as u32, Relaxed);
    }
}

impl Tuning {
    /// Sends the stream settings to the media thread and tells the page.
    fn apply(
        &self,
        control: Option<&Arc<Control>>,
        rtc: &mut Rtc,
        channel: Option<str0m::channel::ChannelId>,
        config: &WebConfig,
    ) {
        let p = sized(
            preset(self.quality, self.reduced, config),
            self.effective_resolution(),
            self.window,
        );
        if let Some(c) = control {
            *c.preset.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
        }
        // What the stream may grow to. Without it str0m never probes, and a mostly still
        // screen sends too little for the estimate to grow, or even to stay up: browser
        // streams stayed near the 8 Mbit/s start, and once the estimate sank Auto stepped
        // down to its small, slow preset on a fast network, whatever resolution was asked.
        rtc.bwe()
            .set_bitrate(Bitrate::ZERO, Bitrate::bps(u64::from(p.max_bps)));
        let note = serde_json::json!({
            "t": "q",
            "q": self.quality.name(),
            "res": self.effective_resolution().name(),
            "reduced": self.reduced,
            "fps": p.fps,
            "maxKbps": p.max_bps / 1000,
        })
        .to_string();
        if let Some(mut ch) = channel.and_then(|id| rtc.channel(id)) {
            let _ = ch.write(false, note.as_bytes());
        }
    }

    /// Clear is the sharpest picture: left on Auto, the resolution is the host's own.
    fn effective_resolution(&self) -> Resolution {
        match (self.quality, self.resolution) {
            (Quality::Clear, Resolution::Auto) => Resolution::Native,
            (_, r) => r,
        }
    }

    /// Auto's reading of a new bandwidth estimate: whether to step down or back up.
    fn observe(&mut self, bps: u64, now: Instant) -> bool {
        if self.quality != Quality::Auto {
            return false;
        }
        if bps < POOR_BPS {
            self.good_since = None;
            let since = *self.poor_since.get_or_insert(now);
            if !self.reduced && now.duration_since(since) >= STEP_DOWN_AFTER {
                self.reduced = true;
                return true;
            }
        } else if bps > GOOD_BPS {
            self.poor_since = None;
            let since = *self.good_since.get_or_insert(now);
            if self.reduced && now.duration_since(since) >= STEP_UP_AFTER {
                self.reduced = false;
                return true;
            }
        } else {
            self.poor_since = None;
            self.good_since = None;
        }
        false
    }
}

static CRYPTO: Once = Once::new();

/// str0m needs a process-wide crypto provider; ours is pure Rust.
pub(crate) fn install_crypto() {
    CRYPTO.call_once(|| {
        str0m::crypto::from_feature_flags().install_process_default();
    });
}

/// How media reaches the browser.
pub(crate) enum Path {
    /// It reached this address over HTTP, and can reach it over UDP too.
    Direct(IpAddr),
    /// Through TURN on the signaling server, with these credentials.
    Turn {
        server: SocketAddr,
        creds: nexa_turn::Credentials,
    },
}

/// The socket media goes over, and how.
pub(crate) enum Net {
    Direct {
        socket: UdpSocket,
        local: SocketAddr,
    },
    Turn {
        socket: UdpSocket,
        client: Box<TurnClient>,
        relayed: SocketAddr,
    },
}

impl Net {
    fn socket(&self) -> &UdpSocket {
        match self {
            Net::Direct { socket, .. } | Net::Turn { socket, .. } => socket,
        }
    }

    /// Sends what the TURN client has queued for its server.
    async fn flush(&mut self) {
        if let Net::Turn { socket, client, .. } = self {
            let server = client.server();
            while let Some(d) = client.poll_transmit() {
                let _ = socket.send_to(&d, server).await;
            }
        }
    }
}

/// A new connection, answered but not yet authorized.
pub(crate) struct Prepared {
    pub rtc: Rtc,
    pub net: Net,
    pub answer: SdpAnswer,
    pub host_fingerprint: [u8; 32],
    /// The browser is on a LAN: the stream runs on [`LanRate`], not str0m's estimate.
    pub lan: bool,
}

/// How long the TURN server has to give us an allocation.
const ALLOCATE_TIMEOUT: Duration = Duration::from_secs(6);

/// Answers `offer` with one candidate: on `ip` for a direct browser, else relayed.
pub(crate) async fn prepare(
    offer: &str,
    path: Path,
    config: &WebConfig,
) -> Result<Prepared, String> {
    let offer = SdpOffer::from_sdp_string(offer).map_err(|e| format!("offer: {e}"))?;
    let lan = matches!(path, Path::Direct(ip) if is_lan(ip));
    let (net, candidate) = match path {
        Path::Direct(ip) => {
            let ip = ip.to_canonical();
            let socket = UdpSocket::bind(SocketAddr::new(ip, 0))
                .await
                .map_err(|e| format!("udp: {e}"))?;
            let local = socket.local_addr().map_err(|e| e.to_string())?;
            let candidate = Candidate::host(local, "udp").map_err(|e| format!("candidate: {e}"))?;
            (Net::Direct { socket, local }, candidate)
        }
        Path::Turn { server, creds } => {
            let any: IpAddr = if server.is_ipv4() {
                Ipv4Addr::UNSPECIFIED.into()
            } else {
                Ipv6Addr::UNSPECIFIED.into()
            };
            let socket = UdpSocket::bind(SocketAddr::new(any, 0))
                .await
                .map_err(|e| format!("udp: {e}"))?;
            let client = Box::new(TurnClient::new(server, creds, Instant::now()));
            let mut net = Net::Turn {
                socket,
                client,
                relayed: server,
            };
            let relayed = allocate(&mut net).await?;
            if let Net::Turn {
                client, relayed: r, ..
            } = &mut net
            {
                *r = relayed;
                // The browser relays on the same server, so its address is the server's.
                client.permit(server.ip(), Instant::now());
            }
            net.flush().await;
            // ICE-lite only offers host candidates; to the browser a candidate is just
            // an address to check, and this one is ours on the relay.
            let candidate =
                Candidate::host(relayed, "udp").map_err(|e| format!("candidate: {e}"))?;
            (net, candidate)
        }
    };
    let rtc_config = RtcConfig::new()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_h264(true);
    // On a LAN no estimate, and so no pacing at it: the browser's reports drive the rate
    // (`LanRate`). Elsewhere str0m's estimate, which a slow or shared path needs.
    let rtc_config = if lan {
        rtc_config.set_stats_interval(Some(LanRate::EVERY))
    } else {
        rtc_config.enable_bwe(Some(Bitrate::bps(u64::from(config.start_bitrate_bps))))
    };
    let mut rtc = rtc_config.build(Instant::now());
    rtc.add_local_candidate(candidate);
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| format!("answer: {e}"))?;
    let fp = rtc.direct_api().local_dtls_fingerprint().clone();
    let host_fingerprint: [u8; 32] = fp
        .bytes
        .as_slice()
        .try_into()
        .ok()
        .filter(|_| fp.hash_func.eq_ignore_ascii_case("sha-256"))
        .ok_or("local fingerprint is not sha-256")?;
    Ok(Prepared {
        rtc,
        net,
        answer,
        host_fingerprint,
        lan,
    })
}

/// Runs the TURN client until it has a relayed address.
async fn allocate(net: &mut Net) -> Result<SocketAddr, String> {
    let deadline = Instant::now() + ALLOCATE_TIMEOUT;
    let mut buf = vec![0u8; 2048];
    loop {
        net.flush().await;
        let Net::Turn { socket, client, .. } = net else {
            return Err("not relayed".into());
        };
        while let Some(ev) = client.poll_event() {
            match ev {
                ClientEvent::Allocated(a) => return Ok(a),
                ClientEvent::Failed(e) => return Err(format!("TURN: {e}")),
                ClientEvent::Data { .. } => {}
            }
        }
        let wake = client.next_timeout().unwrap_or(deadline).min(deadline);
        tokio::select! {
            r = socket.recv_from(&mut buf) => {
                if let Ok((n, from)) = r && from == client.server() {
                    client.handle(&buf[..n], Instant::now());
                }
            }
            _ = sleep_until(wake.into()) => {
                if Instant::now() >= deadline {
                    return Err("the TURN server did not answer".into());
                }
                client.handle_timeout(Instant::now());
            }
        }
    }
}

pub(crate) enum Cmd {
    /// Also offer these sockets, one per local address, as candidates: the browser may
    /// reach one directly and leave the relay.
    Direct(Vec<UdpSocket>),
    /// Authorized: send video from `frames`, hand what arrives on the input channel to
    /// `input`, and send what `outbound` yields on it (host clipboard text).
    Start {
        /// None for a terminal-only session: no picture at all.
        frames: Option<mpsc::Receiver<Frame>>,
        control: Option<Arc<Control>>,
        input: Option<InputThread>,
        outbound: mpsc::Receiver<String>,
        /// Granted the terminal: the host that starts the shell when the page opens it.
        terminal: Option<Arc<dyn crate::WebHost>>,
    },
}

/// Shell output waiting to go out is held back above this, so a flood of output slows
/// the shell down instead of piling up.
const TERM_BUFFER: usize = 256 * 1024;

/// `{t:"r", c, r}`: the terminal in the page changed size.
fn term_resize(data: &[u8]) -> Option<(u16, u16)> {
    #[derive(serde::Deserialize)]
    struct R {
        t: String,
        c: u16,
        r: u16,
    }
    let r: R = serde_json::from_slice(data).ok()?;
    (r.t == "r").then_some((r.c, r.r))
}

/// The shell of this session, while the page has its terminal open.
struct Term {
    pty: nexa_term::Pty,
    channel: str0m::channel::ChannelId,
    /// Tells this shell's output from an earlier one's, still draining.
    generation: u64,
}

#[derive(Debug)]
pub(crate) enum Ended {
    Disconnected,
    Error(String),
}

/// Prefer High profile with packetization mode 1: what the encoder produces.
fn pick_h264(params: &[PayloadParams]) -> Option<&PayloadParams> {
    let h264 = |p: &&PayloadParams| p.spec().codec == Codec::H264;
    let mode1 = |p: &&PayloadParams| p.spec().format.packetization_mode == Some(1);
    let high = |p: &&PayloadParams| {
        p.spec()
            .format
            .profile_level_id
            .is_some_and(|id| id >> 16 == 0x64)
    };
    params
        .iter()
        .filter(h264)
        .filter(mode1)
        .find(high)
        .or_else(|| params.iter().filter(h264).find(mode1))
        .or_else(|| params.iter().find(h264))
}

/// Runs until the browser disconnects, `cmds` closes, or something fails.
pub(crate) async fn drive(
    rtc: Rtc,
    mut net: Net,
    lan: bool,
    cmds: mpsc::Receiver<Cmd>,
    config: WebConfig,
) -> Ended {
    let lan = lan.then(|| LanRate::new(&config));
    let ended = run(rtc, &mut net, lan, cmds, config).await;
    // Give the relay back rather than leave it to expire.
    if let Net::Turn { client, .. } = &mut net {
        client.close(Instant::now());
    }
    net.flush().await;
    ended
}

async fn run(
    mut rtc: Rtc,
    net: &mut Net,
    mut lan: Option<LanRate>,
    mut cmds: mpsc::Receiver<Cmd>,
    config: WebConfig,
) -> Ended {
    let mut buf = vec![0u8; 2048];
    let mut video: Option<Mid> = None;
    let mut input_channel = None;
    let mut frames: Option<mpsc::Receiver<Frame>> = None;
    let mut control: Option<Arc<Control>> = None;
    let mut input: Option<InputThread> = None;
    let mut outbound: Option<mpsc::Receiver<String>> = None;
    let mut connected = false;
    // Sockets offered after the browser was let in, and what arrives on them.
    let mut direct: Vec<(Arc<UdpSocket>, SocketAddr)> = Vec::new();
    let (direct_tx, mut direct_rx) = mpsc::channel::<(SocketAddr, SocketAddr, Vec<u8>)>(256);
    let mut readers = Readers(Vec::new());
    let mut terminal_host: Option<Arc<dyn crate::WebHost>> = None;
    let mut term: Option<Term> = None;
    // Output from the shell's reader thread; None when it has ended.
    let (term_tx, mut term_rx) = mpsc::channel::<(u64, Option<Vec<u8>>)>(64);
    let mut term_gen: u64 = 0;
    let mut tuning = Tuning {
        quality: Quality::Auto,
        resolution: Resolution::Auto,
        window: None,
        reduced: false,
        poor_since: None,
        good_since: None,
    };

    loop {
        // Drain str0m: transmit, handle events, until it names its next deadline.
        let deadline = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    if let Some((socket, _)) = direct.iter().find(|(_, local)| *local == t.source) {
                        let _ = socket.send_to(&t.contents, t.destination).await;
                        continue;
                    }
                    match net {
                        Net::Direct { socket, .. } => {
                            let _ = socket.send_to(&t.contents, t.destination).await;
                        }
                        Net::Turn { client, .. } => {
                            client.send(t.destination, &t.contents, Instant::now());
                            net.flush().await;
                        }
                    }
                }
                Ok(Output::Event(e)) => match e {
                    Event::Connected => {
                        connected = true;
                        if let Some(c) = &control {
                            c.force_key.store(true, Relaxed);
                        }
                    }
                    // No connectivity checks from the browser for 15 s: its network went away.
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        return Ended::Error("ICE: the browser stopped answering".into());
                    }
                    Event::MediaAdded(m) if m.kind == MediaKind::Video => video = Some(m.mid),
                    Event::ChannelOpen(id, label) if label == "input" => {
                        input_channel = Some(id);
                        // What was said before it opened was lost.
                        if let Some(c) = &control {
                            c.cursor_resync.store(true, Relaxed);
                        }
                    }
                    // The page opened its terminal: a shell, only if it was granted.
                    Event::ChannelOpen(id, label) if label == "term" => {
                        let Some(host) = &terminal_host else {
                            if let Some(mut ch) = rtc.channel(id) {
                                let _ = ch.write(false, br#"{"t":"x","why":"denied"}"#);
                            }
                            continue;
                        };
                        // One shell per session: a new one replaces the old.
                        term = None;
                        match host.terminal(80, 24) {
                            Ok(pty) => {
                                match pty.reader() {
                                    Ok(mut reader) => {
                                        let tx = term_tx.clone();
                                        term_gen += 1;
                                        let generation = term_gen;
                                        std::thread::Builder::new()
                                            .name("web-terminal".into())
                                            .spawn(move || {
                                                use std::io::Read;
                                                let mut buf = vec![0u8; 16 * 1024];
                                                loop {
                                                    match reader.read(&mut buf) {
                                                        Ok(n) if n > 0 => {
                                                            if tx
                                                                .blocking_send((
                                                                    generation,
                                                                    Some(buf[..n].to_vec()),
                                                                ))
                                                                .is_err()
                                                            {
                                                                return;
                                                            }
                                                        }
                                                        _ => break,
                                                    }
                                                }
                                                let _ = tx.blocking_send((generation, None));
                                            })
                                            .ok();
                                    }
                                    Err(_) => continue,
                                }
                                if let Some(mut ch) = rtc.channel(id) {
                                    ch.set_buffered_amount_low_threshold(TERM_BUFFER / 2);
                                }
                                term = Some(Term {
                                    pty,
                                    channel: id,
                                    generation: term_gen,
                                });
                            }
                            Err(why) => {
                                if let Some(mut ch) = rtc.channel(id) {
                                    let note =
                                        serde_json::json!({"t": "x", "why": why}).to_string();
                                    let _ = ch.write(false, note.as_bytes());
                                }
                            }
                        }
                    }
                    Event::ChannelData(d) if term.as_ref().is_some_and(|t| t.channel == d.id) => {
                        if let Some(t) = &term {
                            if d.binary {
                                let _ = t.pty.write(&d.data);
                            } else if let Some((c, r)) = term_resize(&d.data) {
                                let _ = t.pty.resize(c, r);
                            }
                        }
                    }
                    // Closing the terminal in the page ends the shell.
                    Event::ChannelClose(id) if term.as_ref().is_some_and(|t| t.channel == id) => {
                        term = None;
                    }
                    Event::ChannelData(d) if Some(d.id) == input_channel => {
                        // Binary messages are pieces of a pasted picture; quality is for
                        // anyone watching, input only with permission.
                        if d.binary {
                            if let (Some(input), Some(msg)) = (&input, Msg::picture_piece(d.data)) {
                                input.send(msg);
                            }
                        } else if let Some(q) = quality_request(&d.data) {
                            tuning.quality = q;
                            tuning.reduced = false;
                            tuning.poor_since = None;
                            tuning.good_since = None;
                            tuning.apply(control.as_ref(), &mut rtc, input_channel, &config);
                        } else if let Some((resolution, window)) = resolution_request(&d.data) {
                            // Also for anyone watching: it is about the picture, not input.
                            tuning.resolution = resolution;
                            if window.is_some() {
                                tuning.window = window;
                            }
                            tuning.apply(control.as_ref(), &mut rtc, input_channel, &config);
                        } else if let (Some(input), Some(msg)) = (&input, Msg::parse(&d.data)) {
                            input.send(msg);
                        }
                    }
                    Event::KeyframeRequest(_) => {
                        if let Some(c) = &control {
                            c.force_key.store(true, Relaxed);
                        }
                    }
                    Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                        let bps = (estimate.as_u64() as f64 * 0.85) as u64;
                        rate_changed(
                            bps,
                            &mut tuning,
                            control.as_ref(),
                            &mut rtc,
                            input_channel,
                            &config,
                        );
                    }
                    Event::PeerStats(stats) => {
                        if let Some(lan) = lan.as_mut() {
                            let bps =
                                lan.update(stats.egress_loss_fraction, stats.rtt, Instant::now());
                            rate_changed(
                                bps,
                                &mut tuning,
                                control.as_ref(),
                                &mut rtc,
                                input_channel,
                                &config,
                            );
                        }
                    }
                    _ => {}
                },
                Err(e) => return Ended::Error(e.to_string()),
            }
        };

        if let Some(c) = &control
            && let Some(why) = c.failed.lock().unwrap_or_else(|e| e.into_inner()).take()
        {
            return Ended::Error(why);
        }

        let turn_wake = match net {
            Net::Turn { client, .. } => client.next_timeout(),
            Net::Direct { .. } => None,
        };
        let wake = turn_wake.map_or(deadline, |t| t.min(deadline));

        tokio::select! {
            r = net.socket().recv_from(&mut buf) => {
                let Ok((n, source)) = r else { continue };
                match net {
                    Net::Direct { local, .. } => {
                        let Ok(contents) = buf[..n].try_into() else { continue };
                        let input = Input::Receive(
                            Instant::now(),
                            Receive { proto: Protocol::Udp, source, destination: *local, contents },
                        );
                        if let Err(e) = rtc.handle_input(input) {
                            return Ended::Error(e.to_string());
                        }
                    }
                    Net::Turn { client, relayed, .. } => {
                        if source != client.server() {
                            continue;
                        }
                        let now = Instant::now();
                        client.handle(&buf[..n], now);
                        while let Some(ev) = client.poll_event() {
                            match ev {
                                ClientEvent::Data { from, data } => {
                                    let Ok(contents) = data.as_slice().try_into() else { continue };
                                    let input = Input::Receive(
                                        now,
                                        Receive { proto: Protocol::Udp, source: from, destination: *relayed, contents },
                                    );
                                    if let Err(e) = rtc.handle_input(input) {
                                        return Ended::Error(e.to_string());
                                    }
                                }
                                ClientEvent::Failed(e) => return Ended::Error(format!("TURN: {e}")),
                                ClientEvent::Allocated(_) => {}
                            }
                        }
                        net.flush().await;
                    }
                }
            }
            out = term_rx.recv(), if term.is_some() && term_room(&mut rtc, term.as_ref()) => {
                let Some(t) = &term else { continue };
                let Some((generation, out)) = out else { continue };
                if generation != t.generation {
                    continue;
                }
                let Some(mut ch) = rtc.channel(t.channel) else { continue };
                match out {
                    Some(bytes) => {
                        let _ = ch.write(true, &bytes);
                    }
                    // The shell exited: the page says so and closes its terminal.
                    None => {
                        let _ = ch.write(false, br#"{"t":"x","why":"exited"}"#);
                        term = None;
                    }
                }
            }
            Some((local, source, data)) = direct_rx.recv() => {
                let Ok(contents) = data.as_slice().try_into() else { continue };
                let input = Input::Receive(
                    Instant::now(),
                    Receive { proto: Protocol::Udp, source, destination: local, contents },
                );
                if let Err(e) = rtc.handle_input(input) {
                    return Ended::Error(e.to_string());
                }
            }
            frame = async { frames.as_mut()?.recv().await }, if frames.is_some() => {
                let Some(frame) = frame else {
                    // Its own reason, if it gave one.
                    let why = control
                        .as_ref()
                        .and_then(|c| c.failed.lock().unwrap_or_else(|e| e.into_inner()).take());
                    return Ended::Error(why.unwrap_or_else(|| "the capture thread stopped".into()));
                };
                if !connected {
                    continue;
                }
                let Some(mid) = video else { continue };
                let Some(writer) = rtc.writer(mid) else { continue };
                let params: Vec<PayloadParams> = writer.payload_params().cloned().collect();
                let Some(pt) = pick_h264(&params).map(|p| p.pt()) else {
                    return Ended::Error("the browser cannot receive H.264".into());
                };
                let ticks = (frame.at.as_micros() * 90 / 1000) as u64;
                let time = MediaTime::new(ticks, Frequency::NINETY_KHZ);
                if let Err(e) = writer.write(pt, Instant::now(), time, frame.data) {
                    return Ended::Error(e.to_string());
                }
            }
            text = async { outbound.as_mut()?.recv().await }, if outbound.is_some() => {
                let Some(text) = text else {
                    outbound = None;
                    continue;
                };
                if let Some(mut ch) = input_channel.and_then(|id| rtc.channel(id)) {
                    let _ = ch.write(false, text.as_bytes());
                }
            }
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Direct(sockets)) => {
                    for socket in sockets {
                        let Ok(local) = socket.local_addr() else { continue };
                        let Ok(candidate) = Candidate::host(local, "udp") else { continue };
                        rtc.add_local_candidate(candidate);
                        let socket = Arc::new(socket);
                        let (reader, tx) = (socket.clone(), direct_tx.clone());
                        readers.0.push(tokio::spawn(async move {
                            let mut buf = vec![0u8; 2048];
                            while let Ok((n, from)) = reader.recv_from(&mut buf).await {
                                if tx.send((local, from, buf[..n].to_vec())).await.is_err() {
                                    break;
                                }
                            }
                        }));
                        direct.push((socket, local));
                    }
                }
                Some(Cmd::Start { frames: f, control: c, input: i, outbound: o, terminal: th }) => {
                    terminal_host = th;
                    if let Some(c) = &c {
                        c.force_key.store(true, Relaxed);
                    }
                    frames = f;
                    control = c;
                    input = i;
                    outbound = Some(o);
                    // A choice made before the stream started.
                    if tuning.quality != Quality::Auto
                        || tuning.resolution != Resolution::Auto
                        || tuning.window.is_some()
                    {
                        tuning.apply(control.as_ref(), &mut rtc, input_channel, &config);
                    }
                }
                None => return Ended::Disconnected,
            },
            _ = sleep_until(wake.into()) => {
                let now = Instant::now();
                if let Net::Turn { client, .. } = net {
                    client.handle_timeout(now);
                    net.flush().await;
                }
                if now >= deadline
                    && let Err(e) = rtc.handle_input(Input::Timeout(now))
                {
                    return Ended::Error(e.to_string());
                }
            }
        }
    }
}

/// Whether the terminal's channel has room for more output.
fn term_room(rtc: &mut Rtc, term: Option<&Term>) -> bool {
    term.and_then(|t| rtc.channel(t.channel))
        .is_some_and(|mut ch| ch.buffered_amount() < TERM_BUFFER)
}

/// Stops the direct sockets' readers with the session.
struct Readers(Vec<tokio::task::JoinHandle<()>>);

impl Drop for Readers {
    fn drop(&mut self) {
        for r in &self.0 {
            r.abort();
        }
    }
}

/// This host's addresses a browser on the same network could reach directly: private
/// IPv4, the tailnet, and IPv6 other than link-local. Loopback and link-local are left
/// out, and so are public IPv4 addresses, which the relay already covers.
pub(crate) fn direct_addresses(ips: impl IntoIterator<Item = IpAddr>) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = ips
        .into_iter()
        .map(|ip| ip.to_canonical())
        .filter(|ip| match ip {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                v4.is_private() || (o[0] == 100 && (64..128).contains(&o[1]))
            }
            IpAddr::V6(v6) => {
                !v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local()
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out.truncate(8);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_steps_down_on_a_poor_network_and_back_up_slowly() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut t = Tuning {
            quality: Quality::Auto,
            resolution: Resolution::Auto,
            window: None,
            reduced: false,
            poor_since: None,
            good_since: None,
        };
        // A short dip changes nothing.
        assert!(!t.observe(800_000, at(0)));
        assert!(!t.observe(5_000_000, at(1)));
        assert!(!t.observe(800_000, at(2)));
        assert!(!t.observe(800_000, at(4)));
        // Three seconds of it does.
        assert!(t.observe(800_000, at(5)));
        assert!(t.reduced);
        // Back up only after a good while.
        assert!(!t.observe(5_000_000, at(6)));
        assert!(!t.observe(5_000_000, at(12)));
        assert!(t.observe(5_000_000, at(14)));
        assert!(!t.reduced);
        // A fixed choice is left alone.
        t.quality = Quality::Clear;
        assert!(!t.observe(100_000, at(20)));
        assert!(!t.observe(100_000, at(40)));
    }

    #[test]
    fn only_addresses_a_nearby_browser_could_reach_are_offered() {
        let ips: Vec<IpAddr> = [
            "127.0.0.1",
            "192.168.0.70",
            "10.1.2.3",
            "100.104.200.119",
            "8.8.8.8",
            "169.254.1.1",
            "fe80::1",
            "fd7a:115c:a1e0::1",
            "192.168.0.70",
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        let got: Vec<String> = direct_addresses(ips)
            .iter()
            .map(|i| i.to_string())
            .collect();
        assert_eq!(
            got,
            [
                "10.1.2.3",
                "100.104.200.119",
                "192.168.0.70",
                "fd7a:115c:a1e0::1"
            ]
        );
    }

    #[test]
    fn clear_means_the_host_s_own_pixels_unless_a_resolution_is_chosen() {
        let mut t = Tuning {
            quality: Quality::Clear,
            resolution: Resolution::Auto,
            window: Some((1512, 945)),
            reduced: false,
            poor_since: None,
            good_since: None,
        };
        assert_eq!(t.effective_resolution(), Resolution::Native);
        t.resolution = Resolution::Lines(720);
        assert_eq!(t.effective_resolution(), Resolution::Lines(720));
        t.quality = Quality::Balanced;
        t.resolution = Resolution::Auto;
        assert_eq!(t.effective_resolution(), Resolution::Auto);
    }

    #[test]
    fn resolution_requests_parse() {
        assert_eq!(
            resolution_request(br#"{"t":"s","m":"auto","w":3024,"h":1890}"#),
            Some((Resolution::Auto, Some((3024, 1890))))
        );
        assert_eq!(
            resolution_request(br#"{"t":"s","m":"720"}"#),
            Some((Resolution::Lines(720), None))
        );
        assert_eq!(resolution_request(br#"{"t":"s","m":"16k"}"#), None);
        assert_eq!(resolution_request(br#"{"t":"q","m":"weak"}"#), None);
    }

    #[test]
    fn private_addresses_are_a_lan_and_tailscale_is_not() {
        for lan in [
            "192.168.0.86",
            "10.1.2.3",
            "172.20.0.1",
            "169.254.1.1",
            "fe80::1",
            "fd7a::1",
        ] {
            assert!(is_lan(lan.parse().unwrap()), "{lan}");
        }
        for far in [
            "100.101.102.103",
            "8.8.8.8",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_lan(far.parse().unwrap()), "{far}");
        }
        assert!(is_lan("::ffff:192.168.1.2".parse().unwrap()));
    }

    #[test]
    fn a_lan_stream_grows_while_clean_and_shrugs_off_one_hiccup() {
        let config = WebConfig::default();
        let mut lan = LanRate::new(&config);
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let ms = Duration::from_millis;
        // Clean reports: from the start to the ceiling within a few seconds.
        let mut bps = 0;
        for s in 0..4 {
            bps = lan.update(Some(0.0), Some(ms(3)), at(s));
        }
        assert_eq!(bps, u64::from(config.max_bitrate_bps));
        // One Wi-Fi spike: held, not cut.
        assert_eq!(lan.update(Some(0.0), Some(ms(180)), at(4)), bps);
        assert_eq!(lan.update(Some(0.0), Some(ms(4)), at(5)), bps);
        // Loss that lasts: cut, and cut again.
        let first = lan.update(Some(0.05), Some(ms(3)), at(6));
        assert_eq!(first, bps);
        let cut = lan.update(Some(0.05), Some(ms(3)), at(7));
        assert!(cut < bps, "{cut} < {bps}");
        assert!(lan.update(Some(0.05), Some(ms(3)), at(8)) < cut);
        // A queue that lasts is congestion too.
        let mut queued = LanRate::new(&config);
        for s in 0..4 {
            queued.update(Some(0.0), Some(ms(3)), at(s));
        }
        queued.update(None, Some(ms(120)), at(4));
        assert!(queued.update(None, Some(ms(120)), at(5)) < u64::from(config.max_bitrate_bps));
        // Never under the floor.
        for s in 10..60 {
            bps = lan.update(Some(0.5), None, at(s));
        }
        assert_eq!(bps, u64::from(config.min_bitrate_bps));
    }

    #[test]
    fn quality_requests_parse() {
        assert_eq!(
            quality_request(br#"{"t":"q","m":"weak"}"#),
            Some(Quality::Weak)
        );
        assert_eq!(quality_request(br#"{"t":"q","m":"max"}"#), None);
        assert_eq!(quality_request(br#"{"t":"k","c":4,"d":true}"#), None);
    }
}
