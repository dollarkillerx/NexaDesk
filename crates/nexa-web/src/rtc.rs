//! One browser's WebRTC connection: str0m, driven by a UDP socket, the encoded frames,
//! commands from signaling and str0m's own timers, all on one task.
//!
//! The host is ICE-lite with a single host candidate: the address the browser already
//! reached over HTTP. The browser checks that address and str0m learns the browser's from
//! its checks, so neither side needs STUN on a LAN or tailnet.

use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Once, atomic::Ordering::Relaxed},
    time::Instant,
};

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
    media::{Control, Frame},
};

static CRYPTO: Once = Once::new();

/// str0m needs a process-wide crypto provider; ours is pure Rust.
pub(crate) fn install_crypto() {
    CRYPTO.call_once(|| {
        let _ = str0m::crypto::from_feature_flags().install_process_default();
    });
}

/// A new connection, answered but not yet authorized.
pub(crate) struct Prepared {
    pub rtc: Rtc,
    pub socket: UdpSocket,
    pub answer: SdpAnswer,
    pub host_fingerprint: [u8; 32],
}

/// Answers `offer` with one host candidate on `ip`, the address the browser reached.
pub(crate) async fn prepare(
    offer: &str,
    ip: IpAddr,
    config: &WebConfig,
) -> Result<Prepared, String> {
    let offer = SdpOffer::from_sdp_string(offer).map_err(|e| format!("offer: {e}"))?;
    let ip = ip.to_canonical();
    let socket = UdpSocket::bind(SocketAddr::new(ip, 0))
        .await
        .map_err(|e| format!("udp: {e}"))?;
    let local = socket.local_addr().map_err(|e| e.to_string())?;
    let mut rtc = RtcConfig::new()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_h264(true)
        .enable_bwe(Some(Bitrate::bps(u64::from(config.start_bitrate_bps))))
        .build(Instant::now());
    let candidate = Candidate::host(local, "udp").map_err(|e| format!("candidate: {e}"))?;
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
        socket,
        answer,
        host_fingerprint,
    })
}

pub(crate) enum Cmd {
    /// Authorized: send video from `frames`, and carry out input if `input` is set.
    Start {
        frames: mpsc::Receiver<Frame>,
        control: Arc<Control>,
        input: Option<InputThread>,
    },
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
    mut rtc: Rtc,
    socket: UdpSocket,
    mut cmds: mpsc::Receiver<Cmd>,
    config: WebConfig,
) -> Ended {
    let local = match socket.local_addr() {
        Ok(a) => a,
        Err(e) => return Ended::Error(e.to_string()),
    };
    let mut buf = vec![0u8; 2000];
    let mut video: Option<Mid> = None;
    let mut input_channel = None;
    let mut frames: Option<mpsc::Receiver<Frame>> = None;
    let mut control: Option<Arc<Control>> = None;
    let mut input: Option<InputThread> = None;
    let mut connected = false;

    loop {
        // Drain str0m: transmit, handle events, until it names its next deadline.
        let deadline = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(e)) => match e {
                    Event::Connected => {
                        connected = true;
                        if let Some(c) = &control {
                            c.force_key.store(true, Relaxed);
                        }
                    }
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        return Ended::Disconnected;
                    }
                    Event::MediaAdded(m) if m.kind == MediaKind::Video => video = Some(m.mid),
                    Event::ChannelOpen(id, label) if label == "input" => input_channel = Some(id),
                    Event::ChannelData(d) if Some(d.id) == input_channel => {
                        if let (Some(input), Some(msg)) = (&input, Msg::parse(&d.data)) {
                            input.send(msg);
                        }
                    }
                    Event::KeyframeRequest(_) => {
                        if let Some(c) = &control {
                            c.force_key.store(true, Relaxed);
                        }
                    }
                    Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                        if let Some(c) = &control {
                            let bps = (estimate.as_u64() as f64 * 0.85) as u64;
                            let bps = bps.clamp(
                                u64::from(config.min_bitrate_bps),
                                u64::from(config.max_bitrate_bps),
                            );
                            c.bitrate.store(bps as u32, Relaxed);
                        }
                    }
                    _ => {}
                },
                Err(e) => return Ended::Error(e.to_string()),
            }
        };

        if let Some(c) = &control {
            if let Some(why) = c.failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return Ended::Error(why);
            }
        }

        tokio::select! {
            r = socket.recv_from(&mut buf) => {
                let Ok((n, source)) = r else { continue };
                let Ok(contents) = buf[..n].try_into() else { continue };
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
                    return Ended::Error("the capture thread stopped".into());
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
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Start { frames: f, control: c, input: i }) => {
                    c.force_key.store(true, Relaxed);
                    frames = Some(f);
                    control = Some(c);
                    input = i;
                }
                None => return Ended::Disconnected,
            },
            _ = sleep_until(deadline.into()) => {
                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    return Ended::Error(e.to_string());
                }
            }
        }
    }
}
