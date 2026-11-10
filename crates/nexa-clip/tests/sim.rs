//! Deterministic two-peer simulations of the clipboard protocol.
//!
//! Every message goes through encode/decode, so the wire format is exercised by
//! all of these scenarios as well.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::Bytes;
use nexa_clip::{
    ClipAnnounce, ClipApplied, ClipCommand, ClipConfig, ClipData, ClipEvent, ClipFetch, ClipKind,
    ClipMessage, ClipNotice, ClipPolicy, ClipStatus, ClipSync, ClipboardBackend, ContentHash,
    DeviceId, EventClass, FakeBackend, FetchFailure, FormatInfo, Gen, InputGate,
    ManualInstant as At,
};

const A_ID: DeviceId = DeviceId([1; 16]);
const B_ID: DeviceId = DeviceId([2; 16]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    A,
    B,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

/// How a peer's OS reports our own clipboard writes back to us.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Echo {
    /// The change event arrives and still carries the owner marker.
    OwnerMarker,
    /// A clipboard manager rewrote the content and dropped the marker.
    ManagerStripsMarker,
    /// No change event arrives (coalesced by polling).
    Silent,
}

struct Peer {
    sync: ClipSync<At>,
    os: FakeBackend,
    gate: InputGate<u32, At>,
    released: Vec<u32>,
    notices: Vec<ClipNotice>,
    failures: Vec<(Gen, ClipKind, FetchFailure)>,
    sent: Vec<ClipMessage>,
    reads: u32,
    echo: Echo,
}

impl Peer {
    fn new(id: DeviceId, config: ClipConfig) -> Self {
        Self {
            sync: ClipSync::new(id, config),
            os: FakeBackend::new(),
            gate: InputGate::new(),
            released: Vec::new(),
            notices: Vec::new(),
            failures: Vec::new(),
            sent: Vec::new(),
            reads: 0,
            echo: Echo::OwnerMarker,
        }
    }

    fn announces_sent(&self) -> usize {
        self.sent
            .iter()
            .filter(|m| matches!(m, ClipMessage::Announce(_)))
            .count()
    }

    fn applied_sent(&self) -> Vec<ClipApplied> {
        self.sent
            .iter()
            .filter_map(|m| match m {
                ClipMessage::Applied(a) => Some(*a),
                _ => None,
            })
            .collect()
    }

    fn data_sent(&self) -> Vec<ClipData> {
        self.sent
            .iter()
            .filter_map(|m| match m {
                ClipMessage::Data(d) => Some(d.clone()),
                _ => None,
            })
            .collect()
    }
}

struct Sim {
    a: Peer,
    b: Peer,
    to_a: Vec<ClipMessage>,
    to_b: Vec<ClipMessage>,
    now: At,
}

impl Sim {
    fn new() -> Self {
        Self::with_config(ClipConfig::default())
    }

    fn with_config(config: ClipConfig) -> Self {
        let mut sim = Self {
            a: Peer::new(A_ID, config),
            b: Peer::new(B_ID, config),
            to_a: Vec::new(),
            to_b: Vec::new(),
            now: At(0),
        };
        sim.event(Side::A, ClipEvent::Connected);
        sim.event(Side::B, ClipEvent::Connected);
        sim
    }

    fn peer(&mut self, side: Side) -> &mut Peer {
        match side {
            Side::A => &mut self.a,
            Side::B => &mut self.b,
        }
    }

    fn queue(&mut self, to: Side) -> &mut Vec<ClipMessage> {
        match to {
            Side::A => &mut self.to_a,
            Side::B => &mut self.to_b,
        }
    }

    fn advance(&mut self, ms: u64) {
        self.now = At(self.now.0 + ms);
    }

    /// Feeds an event to a peer and carries out the resulting commands the way
    /// the real driver would, including follow-up events.
    fn event(&mut self, side: Side, event: ClipEvent) {
        let now = self.now;
        let mut pending = VecDeque::from([event]);
        while let Some(event) = pending.pop_front() {
            let commands = self.peer(side).sync.handle(event, now);
            for command in commands {
                self.execute(side, command, &mut pending);
            }
        }
    }

    fn execute(&mut self, side: Side, command: ClipCommand, pending: &mut VecDeque<ClipEvent>) {
        let now = self.now;
        match command {
            ClipCommand::Send(message) => {
                self.peer(side).sent.push(message.clone());
                self.queue(side.other()).push(message);
            }
            ClipCommand::WritePromise {
                generation,
                formats,
                inline,
            } => {
                let peer = self.peer(side);
                let result = peer.os.write_promise(generation, &formats, &inline);
                let written = result.is_ok();
                pending.push_back(ClipEvent::WriteDone { generation, result });
                if written {
                    match peer.echo {
                        Echo::OwnerMarker => {}
                        Echo::ManagerStripsMarker => peer.os.strip_owner(),
                        Echo::Silent => return,
                    }
                    pending.push_back(ClipEvent::LocalChanged(peer.os.snapshot()));
                }
            }
            ClipCommand::ReadLocal { generation, kind } => {
                let peer = self.peer(side);
                peer.reads += 1;
                let result = peer.os.read(kind).map(Bytes::from);
                pending.push_back(ClipEvent::LocalReadDone {
                    generation,
                    kind,
                    result,
                });
            }
            ClipCommand::FulfillDeferred {
                generation,
                kind,
                data,
            } => {
                let peer = self.peer(side);
                if peer.sync.latest() == Some(generation) {
                    peer.os.fulfill(kind, data);
                }
            }
            ClipCommand::FailDeferred {
                generation,
                kind,
                reason,
            } => {
                let peer = self.peer(side);
                peer.failures.push((generation, kind, reason));
                if peer.sync.latest() == Some(generation) {
                    peer.os.abandon(kind);
                }
            }
            ClipCommand::GateRelease { generation } => {
                let peer = self.peer(side);
                peer.gate.on_applied(generation, now, &mut peer.released);
            }
            ClipCommand::Notify(notice) => self.peer(side).notices.push(notice),
        }
    }

    fn user_copy(&mut self, side: Side, items: &[(ClipKind, &[u8])]) {
        self.peer(side).os.user_copy(items);
        let snapshot = self.peer(side).os.snapshot();
        self.event(side, ClipEvent::LocalChanged(snapshot));
    }

    fn copy_text(&mut self, side: Side, text: &str) {
        self.user_copy(side, &[(ClipKind::Text, text.as_bytes())]);
    }

    fn copy_sensitive(&mut self, side: Side, text: &str) {
        self.peer(side)
            .os
            .user_copy_sensitive(&[(ClipKind::Text, text.as_bytes())]);
        let snapshot = self.peer(side).os.snapshot();
        self.event(side, ClipEvent::LocalChanged(snapshot));
    }

    /// Delivers the message at `index` of the queue for `to`.
    fn deliver(&mut self, to: Side, index: usize) {
        let message = self.queue(to).remove(index);
        let decoded = ClipMessage::decode(&message.encode()).expect("message must decode");
        assert_eq!(decoded, message);
        self.event(to, ClipEvent::Received(decoded));
    }

    /// Delivers everything, in order, until both queues are empty.
    fn drain(&mut self) {
        while !self.to_a.is_empty() || !self.to_b.is_empty() {
            if !self.to_b.is_empty() {
                self.deliver(Side::B, 0);
            }
            if !self.to_a.is_empty() {
                self.deliver(Side::A, 0);
            }
        }
    }

    /// Drops the connection, losing everything in flight, and restores it.
    fn reconnect(&mut self) {
        self.disconnect();
        self.connect();
    }

    fn disconnect(&mut self) {
        self.to_a.clear();
        self.to_b.clear();
        self.event(Side::A, ClipEvent::Disconnected);
        self.event(Side::B, ClipEvent::Disconnected);
    }

    fn connect(&mut self) {
        self.event(Side::A, ClipEvent::Connected);
        self.event(Side::B, ClipEvent::Connected);
    }

    /// Asks the peer's OS to render every promised format, like a paste would.
    fn render_promises(&mut self, side: Side) {
        let Some(generation) = self.peer(side).sync.remote_generation() else {
            return;
        };
        for kind in self.peer(side).os.promised_kinds() {
            self.event(side, ClipEvent::FetchRequested { generation, kind });
        }
    }

    fn assert_converged(&self) {
        assert_eq!(self.a.sync.latest(), self.b.sync.latest());
        for kind in [ClipKind::Text, ClipKind::Html, ClipKind::Rtf, ClipKind::Png] {
            let digest = |peer: &Peer| peer.os.content(kind).map(|c| ContentHash::of(&c));
            assert_eq!(digest(&self.a), digest(&self.b), "{kind:?}");
        }
    }
}

fn text_of(peer: &Peer) -> Option<String> {
    peer.os
        .content(ClipKind::Text)
        .map(|b| String::from_utf8(b.to_vec()).unwrap())
}

fn large_png(len: usize, salt: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 ^ salt).collect()
}

// ---------------------------------------------------------------- basic flow

#[test]
fn copy_reaches_the_peer_and_is_confirmed() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "hello");
    sim.drain();
    assert_eq!(text_of(&sim.b).as_deref(), Some("hello"));
    assert_eq!(sim.a.sync.acked(), sim.a.sync.local_generation());
    assert_eq!(sim.a.sync.paste_requirement(), None);
    assert_eq!(sim.b.sync.remote_generation(), Some((1, A_ID)));
}

#[test]
fn lamport_follows_the_peer() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "1");
    sim.copy_text(Side::A, "2");
    sim.copy_text(Side::A, "3");
    sim.drain();
    assert_eq!(sim.b.sync.lamport(), 3);
    sim.copy_text(Side::B, "4");
    assert_eq!(sim.b.sync.local_generation(), Some((4, B_ID)));
    sim.drain();
    assert_eq!(text_of(&sim.a).as_deref(), Some("4"));
}

#[test]
fn identical_recopy_does_not_create_a_new_generation() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "same");
    sim.copy_text(Side::A, "same");
    assert_eq!(sim.a.announces_sent(), 1);
    assert_eq!(sim.a.sync.lamport(), 1);
}

#[test]
fn clearing_the_clipboard_announces_nothing() {
    let mut sim = Sim::new();
    sim.user_copy(Side::A, &[]);
    assert_eq!(sim.a.announces_sent(), 0);
    assert_eq!(sim.a.sync.local_generation(), None);
}

#[test]
fn inline_threshold_is_inclusive_and_shared_across_formats() {
    const LIMIT: usize = 64 * 1024;
    let mut sim = Sim::new();
    let exact = vec![b'a'; LIMIT];
    sim.user_copy(Side::A, &[(ClipKind::Text, &exact)]);
    let ClipMessage::Announce(first) = sim.a.sent[0].clone() else {
        panic!("expected an announcement");
    };
    assert_eq!(first.inline.len(), 1);

    let over = vec![b'b'; LIMIT + 1];
    sim.user_copy(Side::A, &[(ClipKind::Text, &over)]);
    let ClipMessage::Announce(second) = sim.a.sent[1].clone() else {
        panic!("expected an announcement");
    };
    assert!(second.inline.is_empty());

    let half = vec![b'c'; LIMIT / 2 + 1];
    let html = vec![b'd'; LIMIT / 2 + 1];
    sim.user_copy(Side::A, &[(ClipKind::Text, &half), (ClipKind::Html, &html)]);
    let ClipMessage::Announce(third) = sim.a.sent[2].clone() else {
        panic!("expected an announcement");
    };
    assert_eq!(third.inline.len(), 1);
    assert_eq!(third.inline[0].0, ClipKind::Text);
}

#[test]
fn images_are_never_inlined() {
    let mut sim = Sim::new();
    sim.user_copy(Side::A, &[(ClipKind::Png, b"tiny")]);
    let ClipMessage::Announce(announce) = sim.a.sent[0].clone() else {
        panic!("expected an announcement");
    };
    assert!(announce.inline.is_empty());
}

// ---------------------------------------------------------- echo suppression

#[test]
fn applying_a_remote_clip_does_not_re_announce_with_owner_marker() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "from a");
    sim.drain();
    assert_eq!(text_of(&sim.b).as_deref(), Some("from a"));
    assert_eq!(sim.b.announces_sent(), 0);
}

#[test]
fn applying_a_remote_clip_does_not_re_announce_when_a_manager_strips_the_marker() {
    let mut sim = Sim::new();
    sim.b.echo = Echo::ManagerStripsMarker;
    sim.copy_text(Side::A, "from a");
    sim.drain();
    assert_eq!(text_of(&sim.b).as_deref(), Some("from a"));
    assert_eq!(sim.b.announces_sent(), 0);
    assert_eq!(sim.a.sync.lamport(), 1);
}

#[test]
fn hash_echo_does_not_swallow_a_genuine_later_copy_of_the_same_text() {
    let mut sim = Sim::new();
    sim.b.echo = Echo::ManagerStripsMarker;
    sim.copy_text(Side::A, "x");
    sim.drain();
    sim.copy_text(Side::B, "y");
    sim.drain();
    assert_eq!(sim.b.announces_sent(), 1);
    sim.copy_text(Side::B, "x");
    assert_eq!(sim.b.announces_sent(), 2);
    sim.drain();
    assert_eq!(text_of(&sim.a).as_deref(), Some("x"));
}

#[test]
fn owner_marked_echo_also_clears_the_hash() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "x");
    sim.drain();
    sim.copy_text(Side::B, "y");
    sim.copy_text(Side::B, "x");
    assert_eq!(sim.b.announces_sent(), 2);
}

#[test]
fn hash_that_never_echoed_stops_counting_after_the_ttl() {
    let mut sim = Sim::new();
    sim.b.echo = Echo::Silent;
    sim.copy_text(Side::A, "x");
    sim.copy_text(Side::A, "z");
    sim.drain();
    sim.copy_text(Side::B, "y");
    sim.drain();
    let before = sim.b.announces_sent();

    // Inside the TTL, "x" looks like the late echo of our earlier write.
    sim.b.os.user_copy(&[(ClipKind::Text, b"x")]);
    let snapshot = sim.b.os.snapshot();
    sim.event(Side::B, ClipEvent::LocalChanged(snapshot));
    assert_eq!(sim.b.announces_sent(), before);

    // A later copy of "z" is past the TTL and counts as a genuine copy.
    sim.advance(5_001);
    sim.copy_text(Side::B, "z");
    assert_eq!(sim.b.announces_sent(), before + 1);
}

#[test]
fn partial_hash_match_is_not_an_echo() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "shared");
    sim.drain();
    sim.b
        .os
        .user_copy(&[(ClipKind::Text, b"shared"), (ClipKind::Html, b"<b>new</b>")]);
    let snapshot = sim.b.os.snapshot();
    sim.event(Side::B, ClipEvent::LocalChanged(snapshot));
    assert_eq!(sim.b.announces_sent(), 1);
}

// ----------------------------------------------------- ordering and conflicts

#[test]
fn simultaneous_copy_converges_on_the_larger_device() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "from a");
    sim.copy_text(Side::B, "from b");
    sim.drain();
    assert_eq!(sim.a.sync.latest(), Some((1, B_ID)));
    assert_eq!(sim.b.sync.latest(), Some((1, B_ID)));
    assert_eq!(text_of(&sim.a).as_deref(), Some("from b"));
    assert_eq!(text_of(&sim.b).as_deref(), Some("from b"));
    assert!(
        sim.b
            .applied_sent()
            .iter()
            .any(|a| { a.generation == (1, A_ID) && a.status == ClipStatus::Stale })
    );
}

#[test]
fn simultaneous_copy_loser_does_not_gate_pastes() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "from a");
    sim.copy_text(Side::B, "from b");
    assert_eq!(sim.a.sync.paste_requirement(), Some((1, A_ID)));
    sim.drain();
    assert_eq!(sim.a.sync.paste_requirement(), None);
    assert_eq!(sim.b.sync.paste_requirement(), None);
}

#[test]
fn newer_announce_wins_when_delivered_first() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "old");
    sim.copy_text(Side::A, "new");
    sim.deliver(Side::B, 1); // reordered: the newer one first
    sim.deliver(Side::B, 0);
    sim.drain();
    assert_eq!(text_of(&sim.b).as_deref(), Some("new"));
    assert_eq!(sim.a.sync.acked(), Some((2, A_ID)));
}

#[test]
fn reordered_acknowledgements_never_move_acked_backwards() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "1");
    sim.copy_text(Side::A, "2");
    sim.deliver(Side::B, 0);
    sim.deliver(Side::B, 0);
    assert_eq!(sim.to_a.len(), 2);
    sim.deliver(Side::A, 1);
    sim.deliver(Side::A, 0);
    assert_eq!(sim.a.sync.acked(), Some((2, A_ID)));
    assert_eq!(sim.a.sync.paste_requirement(), None);
}

#[test]
fn applied_for_a_generation_never_issued_is_ignored() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "1");
    sim.event(
        Side::A,
        ClipEvent::Received(ClipMessage::Applied(ClipApplied {
            generation: (99, A_ID),
            status: ClipStatus::Ok,
        })),
    );
    assert_eq!(sim.a.sync.acked(), None);
    sim.event(
        Side::A,
        ClipEvent::Received(ClipMessage::Applied(ClipApplied {
            generation: (1, B_ID),
            status: ClipStatus::Ok,
        })),
    );
    assert_eq!(sim.a.sync.acked(), None);
    assert_eq!(sim.a.sync.paste_requirement(), Some((1, A_ID)));
}

// ------------------------------------------------------- loss and retransmit

#[test]
fn lost_announce_is_recovered_by_reconnect() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "payload");
    sim.reconnect();
    sim.drain();
    assert_eq!(text_of(&sim.b).as_deref(), Some("payload"));
    assert_eq!(sim.a.sync.acked(), Some((1, A_ID)));
}

#[test]
fn lost_ack_is_recovered_by_retransmit_without_rewriting() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "payload");
    sim.deliver(Side::B, 0);
    assert_eq!(sim.a.sync.acked(), None);
    let writes = sim.b.os.change_count();
    sim.reconnect();
    sim.drain();
    assert_eq!(sim.a.sync.acked(), Some((1, A_ID)));
    assert_eq!(sim.b.os.change_count(), writes);
}

#[test]
fn copy_while_disconnected_is_announced_on_reconnect() {
    let mut sim = Sim::new();
    sim.disconnect();
    sim.copy_text(Side::A, "offline");
    assert_eq!(sim.a.announces_sent(), 0);
    assert_eq!(sim.a.sync.paste_requirement(), None);
    sim.connect();
    sim.drain();
    assert_eq!(text_of(&sim.b).as_deref(), Some("offline"));
}

#[test]
fn reconnect_with_divergent_entries_picks_the_same_winner() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "a");
    sim.copy_text(Side::B, "b");
    sim.reconnect();
    sim.drain();
    assert_eq!(sim.a.sync.latest(), Some((1, B_ID)));
    sim.assert_converged();
}

#[test]
fn only_the_origin_of_the_winning_entry_re_announces() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "a");
    sim.drain();
    sim.reconnect();
    assert_eq!(sim.b.announces_sent(), 0);
    assert_eq!(sim.a.announces_sent(), 2);
}

#[test]
fn busy_write_reports_busy_and_retries_on_reconnect() {
    let mut sim = Sim::new();
    sim.b.os.fail_next_writes(1);
    sim.copy_text(Side::A, "payload");
    sim.drain();
    assert_eq!(
        sim.b.applied_sent().last().map(|a| a.status),
        Some(ClipStatus::Busy)
    );
    assert_eq!(sim.a.sync.acked(), None);
    assert_eq!(sim.a.sync.paste_requirement(), None);
    assert!(sim.a.notices.iter().any(|n| matches!(
        n,
        ClipNotice::PeerRejected {
            status: ClipStatus::Busy,
            ..
        }
    )));

    sim.reconnect();
    assert_eq!(sim.a.sync.paste_requirement(), Some((1, A_ID)));
    sim.drain();
    assert_eq!(text_of(&sim.b).as_deref(), Some("payload"));
    assert_eq!(sim.a.sync.acked(), Some((1, A_ID)));
}

#[test]
fn failed_write_does_not_leave_a_hash_that_swallows_a_real_copy() {
    let mut sim = Sim::new();
    sim.b.os.fail_next_writes(1);
    sim.copy_text(Side::A, "x");
    sim.drain();
    sim.copy_text(Side::B, "x");
    assert_eq!(sim.b.announces_sent(), 0, "matches the latest entry");
    sim.copy_text(Side::B, "y");
    assert_eq!(sim.b.announces_sent(), 1);
    sim.copy_text(Side::B, "x");
    assert_eq!(sim.b.announces_sent(), 2);
}

#[test]
fn applied_is_not_sent_until_the_write_finished() {
    let mut sync: ClipSync<At> = ClipSync::new(B_ID, ClipConfig::default());
    sync.handle(ClipEvent::Connected, At(0));
    let data = Bytes::from_static(b"hi");
    let generation = (1, A_ID);
    let commands = sync.handle(
        ClipEvent::Received(ClipMessage::Announce(ClipAnnounce {
            generation,
            origin: A_ID,
            formats: vec![FormatInfo::of(ClipKind::Text, &data)],
            inline: vec![(ClipKind::Text, data.clone())],
        })),
        At(0),
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, ClipCommand::WritePromise { .. }))
    );
    assert!(!commands.iter().any(|c| matches!(c, ClipCommand::Send(_))));

    let commands = sync.handle(
        ClipEvent::WriteDone {
            generation,
            result: Ok(()),
        },
        At(1),
    );
    assert_eq!(
        commands,
        [ClipCommand::Send(ClipMessage::Applied(ClipApplied {
            generation,
            status: ClipStatus::Ok,
        }))]
    );
}

#[test]
fn duplicate_announce_while_writing_does_not_write_twice() {
    let mut sync: ClipSync<At> = ClipSync::new(B_ID, ClipConfig::default());
    sync.handle(ClipEvent::Connected, At(0));
    let data = Bytes::from_static(b"hi");
    let announce = ClipAnnounce {
        generation: (1, A_ID),
        origin: A_ID,
        formats: vec![FormatInfo::of(ClipKind::Text, &data)],
        inline: vec![(ClipKind::Text, data.clone())],
    };
    sync.handle(
        ClipEvent::Received(ClipMessage::Announce(announce.clone())),
        At(0),
    );
    let again = sync.handle(ClipEvent::Received(ClipMessage::Announce(announce)), At(1));
    assert!(again.is_empty());
}

#[test]
fn invalid_announcements_are_rejected() {
    let mut sync: ClipSync<At> = ClipSync::new(B_ID, ClipConfig::default());
    sync.handle(ClipEvent::Connected, At(0));
    let data = Bytes::from_static(b"hi");
    let good_format = FormatInfo::of(ClipKind::Text, &data);

    let wrong_origin = ClipAnnounce {
        generation: (1, A_ID),
        origin: DeviceId([9; 16]),
        formats: vec![good_format],
        inline: vec![],
    };
    let tampered = ClipAnnounce {
        generation: (2, A_ID),
        origin: A_ID,
        formats: vec![good_format],
        inline: vec![(ClipKind::Text, Bytes::from_static(b"no"))],
    };
    let no_formats = ClipAnnounce {
        generation: (3, A_ID),
        origin: A_ID,
        formats: vec![],
        inline: vec![],
    };
    for announce in [wrong_origin, tampered, no_formats] {
        let generation = announce.generation;
        let commands = sync.handle(ClipEvent::Received(ClipMessage::Announce(announce)), At(0));
        assert_eq!(
            commands,
            [ClipCommand::Send(ClipMessage::Applied(ClipApplied {
                generation,
                status: ClipStatus::Invalid,
            }))]
        );
    }
    assert_eq!(sync.remote_generation(), None);
}

// ------------------------------------------------------------- deferred fetch

#[test]
fn large_format_is_promised_then_fetched_in_chunks() {
    let mut sim = Sim::new();
    let png = large_png(5 * 1024 * 1024 + 7, 3);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    assert_eq!(sim.b.os.promised_kinds(), [ClipKind::Png]);
    assert_eq!(
        sim.a.sync.acked(),
        Some((1, A_ID)),
        "gate need not wait for data"
    );

    sim.render_promises(Side::B);
    sim.drain();
    assert_eq!(sim.b.os.content(ClipKind::Png).unwrap(), png);
    assert_eq!(sim.a.reads, 1, "the OS clipboard is read once per format");
    let chunks = sim.a.data_sent();
    assert!(chunks.iter().all(|d| d.bytes.len() <= 64 * 1024));
    assert_eq!(chunks.iter().filter(|d| d.eof).count(), 1);
}

#[test]
fn zero_byte_promise_needs_no_round_trip() {
    let mut sim = Sim::new();
    sim.user_copy(Side::A, &[(ClipKind::Png, b"")]);
    sim.drain();
    sim.render_promises(Side::B);
    assert!(sim.to_a.is_empty());
    assert_eq!(sim.b.os.content(ClipKind::Png).unwrap().len(), 0);
}

fn tiny_window() -> ClipConfig {
    ClipConfig {
        chunk_size: 512,
        fetch_window: 2048,
        ..ClipConfig::default()
    }
}

#[test]
fn fetch_tolerates_reordered_chunks() {
    let mut sim = Sim::with_config(tiny_window());
    let png = large_png(5000, 11);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.render_promises(Side::B);
    sim.deliver(Side::A, 0);
    while !sim.to_a.is_empty() || !sim.to_b.is_empty() {
        if !sim.to_b.is_empty() {
            let last = sim.to_b.len() - 1;
            sim.deliver(Side::B, last);
        }
        if !sim.to_a.is_empty() {
            sim.deliver(Side::A, 0);
        }
    }
    assert_eq!(sim.b.os.content(ClipKind::Png).unwrap(), png);
}

#[test]
fn fetch_resumes_from_received_offset_after_reconnect() {
    let mut sim = Sim::with_config(tiny_window());
    let png = large_png(5000, 13);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.render_promises(Side::B);
    sim.deliver(Side::A, 0);
    sim.deliver(Side::B, 0);
    sim.deliver(Side::B, 0);

    sim.reconnect();
    let fetch = sim
        .to_a
        .iter()
        .find_map(|m| match m {
            ClipMessage::Fetch(f) => Some(*f),
            _ => None,
        })
        .expect("the fetch is re-issued");
    assert_eq!(fetch.offset, 1024);
    sim.drain();
    assert_eq!(sim.b.os.content(ClipKind::Png).unwrap(), png);
}

#[test]
fn fetch_times_out_without_progress_but_progress_extends_it() {
    let mut sim = Sim::with_config(tiny_window());
    let png = large_png(5000, 17);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.render_promises(Side::B);
    sim.deliver(Side::A, 0);

    sim.advance(9_000);
    sim.deliver(Side::B, 0);
    assert_eq!(sim.b.sync.next_deadline(), Some(At(19_000)));

    sim.advance(9_999);
    sim.event(Side::B, ClipEvent::Tick);
    assert!(sim.b.failures.is_empty());

    sim.advance(1);
    sim.event(Side::B, ClipEvent::Tick);
    assert_eq!(sim.b.failures.len(), 1);
    assert_eq!(sim.b.failures[0].2, FetchFailure::Timeout);
    assert_eq!(sim.b.sync.next_deadline(), None);
}

#[test]
fn fetch_for_a_superseded_generation_is_answered_stale() {
    let mut sim = Sim::new();
    let png = large_png(100 * 1024, 19);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.render_promises(Side::B);
    sim.copy_text(Side::A, "newer");
    sim.deliver(Side::A, 0);
    assert_eq!(sim.a.reads, 0);
    assert_eq!(sim.a.data_sent()[0].status, ClipStatus::Stale);
    sim.drain();
    assert_eq!(text_of(&sim.b).as_deref(), Some("newer"));
    assert_eq!(sim.b.failures[0].2, FetchFailure::Stale);
}

#[test]
fn promise_written_empty_after_a_timeout_is_restored_by_a_retransmit() {
    let mut sim = Sim::with_config(tiny_window());
    let png = large_png(5000, 71);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.render_promises(Side::B);
    sim.to_a.clear();
    sim.advance(10_000);
    sim.event(Side::B, ClipEvent::Tick);
    assert_eq!(sim.b.os.content(ClipKind::Png), None);
    assert!(sim.b.os.promised_kinds().is_empty(), "the promise is gone");

    sim.reconnect();
    sim.drain();
    assert_eq!(sim.b.os.promised_kinds(), [ClipKind::Png]);
    sim.render_promises(Side::B);
    sim.drain();
    assert_eq!(sim.b.os.content(ClipKind::Png).unwrap(), png);
}

#[test]
fn newer_entry_fails_an_older_fetch_immediately() {
    let mut sim = Sim::new();
    let png = large_png(100 * 1024, 23);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.render_promises(Side::B);
    sim.copy_text(Side::A, "newer");
    sim.deliver(Side::B, 0);
    assert_eq!(sim.b.failures[0].2, FetchFailure::Stale);
}

#[test]
fn fetch_with_no_matching_promise_fails_locally() {
    let mut sim = Sim::new();
    sim.event(
        Side::B,
        ClipEvent::FetchRequested {
            generation: (9, A_ID),
            kind: ClipKind::Png,
        },
    );
    assert_eq!(sim.b.failures[0].2, FetchFailure::Stale);
    assert!(sim.to_a.is_empty());
}

#[test]
fn corrupt_data_is_never_handed_to_the_os() {
    let mut sim = Sim::with_config(tiny_window());
    let png = large_png(1000, 29);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.render_promises(Side::B);
    sim.to_a.clear();
    let generation = (1, A_ID);
    sim.event(
        Side::B,
        ClipEvent::Received(ClipMessage::Data(ClipData {
            generation,
            kind: ClipKind::Png,
            offset: 0,
            bytes: Bytes::from(vec![0u8; 1000]),
            eof: true,
            status: ClipStatus::Ok,
        })),
    );
    assert_eq!(sim.b.failures[0].2, FetchFailure::Corrupt);
    assert_eq!(sim.b.os.content(ClipKind::Png), None);
}

#[test]
fn oversized_or_short_data_fails_the_fetch() {
    let mut sim = Sim::with_config(tiny_window());
    let png = large_png(1000, 31);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.render_promises(Side::B);
    sim.to_a.clear();
    sim.event(
        Side::B,
        ClipEvent::Received(ClipMessage::Data(ClipData {
            generation: (1, A_ID),
            kind: ClipKind::Png,
            offset: 0,
            bytes: Bytes::from(vec![0u8; 10]),
            eof: true,
            status: ClipStatus::Ok,
        })),
    );
    assert_eq!(sim.b.failures[0].2, FetchFailure::Corrupt);
}

#[test]
fn server_caps_a_fetch_at_one_window() {
    let mut sim = Sim::with_config(tiny_window());
    let png = large_png(10_000, 37);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.event(
        Side::A,
        ClipEvent::Received(ClipMessage::Fetch(ClipFetch {
            generation: (1, A_ID),
            kind: ClipKind::Png,
            offset: 0,
            len: u64::MAX,
        })),
    );
    let total: usize = sim.a.data_sent().iter().map(|d| d.bytes.len()).sum();
    assert_eq!(total, 2048);
}

// -------------------------------------------------------------- policy/privacy

#[test]
fn sensitive_content_is_not_announced_by_default() {
    let mut sim = Sim::new();
    sim.copy_sensitive(Side::A, "hunter2");
    assert_eq!(sim.a.announces_sent(), 0);
    assert_eq!(sim.a.sync.local_generation(), None);
    assert_eq!(sim.a.notices, [ClipNotice::SensitiveKeptLocal]);
}

#[test]
fn sensitive_content_can_be_allowed_explicitly() {
    let mut sim = Sim::with_config(ClipConfig {
        policy: ClipPolicy {
            sync_sensitive: true,
            ..ClipPolicy::default()
        },
        ..ClipConfig::default()
    });
    sim.copy_sensitive(Side::A, "hunter2");
    assert_eq!(sim.a.announces_sent(), 1);
}

#[test]
fn fetch_after_a_sensitive_copy_cannot_leak_it() {
    let mut sim = Sim::new();
    let png = large_png(100 * 1024, 41);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.copy_sensitive(Side::A, "hunter2");

    sim.render_promises(Side::B);
    sim.drain();
    assert_eq!(sim.a.reads, 0, "the OS clipboard must not be read at all");
    assert_eq!(sim.a.data_sent()[0].status, ClipStatus::Stale);
    assert_eq!(sim.b.failures[0].2, FetchFailure::Stale);
}

#[test]
fn read_racing_an_unseen_clipboard_change_is_not_sent() {
    let mut sim = Sim::new();
    let png = large_png(100 * 1024, 43);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    // The OS clipboard changes, but the change event has not been delivered.
    sim.a
        .os
        .user_copy_sensitive(&[(ClipKind::Png, b"secret screenshot")]);

    sim.render_promises(Side::B);
    sim.drain();
    assert_eq!(sim.a.reads, 1);
    let sent = sim.a.data_sent();
    assert!(sent.iter().all(|d| d.bytes.is_empty()));
    assert_eq!(sent[0].status, ClipStatus::Stale);
}

#[test]
fn recopying_the_announced_content_makes_it_servable_again() {
    let mut sim = Sim::new();
    let png = large_png(100 * 1024, 47);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.copy_sensitive(Side::A, "hunter2");
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    assert_eq!(sim.a.announces_sent(), 1);

    sim.render_promises(Side::B);
    sim.drain();
    assert_eq!(sim.b.os.content(ClipKind::Png).unwrap(), png);
}

#[test]
fn outbound_switch_blocks_announce_reannounce_and_fetch() {
    let mut sim = Sim::new();
    let png = large_png(100 * 1024, 53);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    let off = ClipPolicy {
        outbound: false,
        ..ClipPolicy::default()
    };
    sim.event(Side::A, ClipEvent::PolicyChanged(off));

    sim.render_promises(Side::B);
    sim.drain();
    assert_eq!(sim.a.reads, 0);
    assert_eq!(sim.a.data_sent()[0].status, ClipStatus::Denied);
    assert_eq!(sim.b.failures[0].2, FetchFailure::Denied);

    sim.copy_text(Side::A, "private");
    assert_eq!(sim.a.announces_sent(), 1);
    sim.reconnect();
    assert_eq!(sim.a.announces_sent(), 1);
}

#[test]
fn outbound_off_then_on_does_not_serve_the_intervening_copy() {
    let mut sim = Sim::new();
    let png = large_png(100 * 1024, 59);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    let off = ClipPolicy {
        outbound: false,
        ..ClipPolicy::default()
    };
    sim.event(Side::A, ClipEvent::PolicyChanged(off));
    sim.user_copy(Side::A, &[(ClipKind::Png, b"kept private")]);
    sim.event(Side::A, ClipEvent::PolicyChanged(ClipPolicy::default()));

    sim.render_promises(Side::B);
    sim.drain();
    assert_eq!(sim.a.reads, 0);
    assert_eq!(sim.a.data_sent()[0].status, ClipStatus::Stale);
}

#[test]
fn inbound_switch_refuses_the_write_but_still_answers_and_follows_lamport() {
    let mut sim = Sim::new();
    let off = ClipPolicy {
        inbound: false,
        ..ClipPolicy::default()
    };
    sim.event(Side::B, ClipEvent::PolicyChanged(off));
    sim.copy_text(Side::A, "x");
    sim.drain();
    assert_eq!(text_of(&sim.b), None);
    assert_eq!(sim.b.applied_sent()[0].status, ClipStatus::Denied);
    assert_eq!(sim.b.sync.lamport(), 1);
    assert_eq!(sim.a.sync.paste_requirement(), None);
}

#[test]
fn turning_inbound_off_cancels_running_fetches() {
    let mut sim = Sim::new();
    let png = large_png(100 * 1024, 61);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    sim.drain();
    sim.render_promises(Side::B);
    let off = ClipPolicy {
        inbound: false,
        ..ClipPolicy::default()
    };
    sim.event(Side::B, ClipEvent::PolicyChanged(off));
    assert_eq!(sim.b.failures[0].2, FetchFailure::Denied);
}

#[test]
fn oversized_formats_are_dropped_and_the_rest_still_sync() {
    let mut sim = Sim::with_config(ClipConfig {
        max_format_size: 10,
        ..ClipConfig::default()
    });
    sim.user_copy(
        Side::A,
        &[(ClipKind::Text, b"short"), (ClipKind::Png, &[1u8; 11])],
    );
    sim.drain();
    assert_eq!(text_of(&sim.b).as_deref(), Some("short"));
    assert_eq!(sim.b.os.content(ClipKind::Png), None);
    assert!(sim.a.notices.iter().any(|n| matches!(
        n,
        ClipNotice::TooLarge {
            kind: ClipKind::Png,
            size: 11
        }
    )));
}

#[test]
fn entirely_oversized_copy_is_not_announced() {
    let mut sim = Sim::with_config(ClipConfig {
        max_format_size: 10,
        ..ClipConfig::default()
    });
    sim.user_copy(Side::A, &[(ClipKind::Png, &[1u8; 11])]);
    assert_eq!(sim.a.announces_sent(), 0);
    assert_eq!(sim.a.sync.local_generation(), None);
}

#[test]
fn receiver_enforces_its_own_size_limit() {
    let mut sync: ClipSync<At> = ClipSync::new(
        B_ID,
        ClipConfig {
            max_format_size: 10,
            ..ClipConfig::default()
        },
    );
    sync.handle(ClipEvent::Connected, At(0));
    let commands = sync.handle(
        ClipEvent::Received(ClipMessage::Announce(ClipAnnounce {
            generation: (1, A_ID),
            origin: A_ID,
            formats: vec![FormatInfo {
                kind: ClipKind::Png,
                size: 11,
                hash: ContentHash::of(b"x"),
            }],
            inline: vec![],
        })),
        At(0),
    );
    assert_eq!(
        commands,
        [ClipCommand::Send(ClipMessage::Applied(ClipApplied {
            generation: (1, A_ID),
            status: ClipStatus::TooLarge,
        }))]
    );
}

// ------------------------------------------------------- gate + sync together

fn paste(sim: &mut Sim, side: Side) {
    let now = sim.now;
    let peer = sim.peer(side);
    let required = peer.sync.paste_requirement();
    peer.gate.on_paste(required, now, &mut peer.released);
}

fn key(sim: &mut Sim, side: Side, event: u32) {
    let now = sim.now;
    let peer = sim.peer(side);
    peer.gate
        .on_event(event, EventClass::Ordered, now, &mut peer.released);
}

#[test]
fn the_race_from_the_design_copy_then_immediate_paste() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "fresh");
    paste(&mut sim, Side::A);
    key(&mut sim, Side::A, 7);
    assert!(sim.a.released.is_empty());
    sim.deliver(Side::B, 0);
    assert_eq!(text_of(&sim.b).as_deref(), Some("fresh"));
    assert!(sim.a.released.is_empty(), "held until the ack arrives");
    sim.deliver(Side::A, 0);
    assert_eq!(sim.a.released, [7]);
}

#[test]
fn paste_after_ack_is_not_held() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "fresh");
    sim.drain();
    paste(&mut sim, Side::A);
    key(&mut sim, Side::A, 7);
    assert_eq!(sim.a.released, [7]);
}

#[test]
fn losing_a_simultaneous_copy_releases_a_held_paste() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "from a");
    sim.copy_text(Side::B, "from b");
    paste(&mut sim, Side::A);
    key(&mut sim, Side::A, 7);
    assert!(sim.a.released.is_empty());
    sim.deliver(Side::A, 0); // B's announcement beats ours
    assert_eq!(sim.a.released, [7]);
}

#[test]
fn rejected_generation_releases_a_held_paste() {
    let mut sim = Sim::new();
    sim.b.os.fail_next_writes(1);
    sim.copy_text(Side::A, "x");
    paste(&mut sim, Side::A);
    key(&mut sim, Side::A, 7);
    sim.drain();
    assert_eq!(sim.a.released, [7]);
}

#[test]
fn large_clipboard_does_not_delay_the_paste_past_the_announcement() {
    let mut sim = Sim::new();
    let png = large_png(20 * 1024 * 1024, 67);
    sim.user_copy(Side::A, &[(ClipKind::Png, &png)]);
    paste(&mut sim, Side::A);
    key(&mut sim, Side::A, 7);
    sim.drain();
    assert_eq!(sim.a.released, [7]);
    assert_eq!(sim.b.os.promised_kinds(), [ClipKind::Png]);
}

#[test]
fn deadline_releases_when_the_peer_never_answers() {
    let mut sim = Sim::new();
    sim.copy_text(Side::A, "x");
    paste(&mut sim, Side::A);
    key(&mut sim, Side::A, 7);
    sim.advance(800);
    let now = sim.now;
    sim.a.gate.poll(now, &mut sim.a.released);
    assert_eq!(sim.a.released, [7]);
}

// ----------------------------------------------------------------- randomised

/// A small deterministic generator; failures print the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

fn random_side(rng: &mut Rng) -> Side {
    if rng.below(2) == 0 { Side::A } else { Side::B }
}

fn run_random_session(seed: u64) -> Sim {
    let mut rng = Rng(seed.wrapping_mul(2).wrapping_add(0x9e37_79b9_7f4a_7c15));
    let mut sim = Sim::with_config(ClipConfig {
        chunk_size: 256,
        fetch_window: 1024,
        inline_threshold: 2048,
        ..ClipConfig::default()
    });
    if rng.below(2) == 0 {
        sim.b.echo = Echo::ManagerStripsMarker;
    }
    let mut connected = true;
    let mut copies = 0u32;

    for _ in 0..(50 + rng.below(250)) {
        sim.advance(rng.below(50) as u64);
        match rng.below(100) {
            0..=19 => {
                copies += 1;
                let side = random_side(&mut rng);
                if rng.below(3) == 0 {
                    let len = 3000 + rng.below(4000);
                    let png = large_png(len, copies as u8);
                    sim.user_copy(side, &[(ClipKind::Png, &png)]);
                } else {
                    let text = format!("copy {copies} seed {seed}");
                    sim.copy_text(side, &text);
                }
            }
            20..=69 => {
                let to = random_side(&mut rng);
                let len = sim.queue(to).len();
                if len > 0 {
                    let index = rng.below(len);
                    sim.deliver(to, index);
                }
            }
            70..=77 => {
                let to = random_side(&mut rng);
                let len = sim.queue(to).len();
                if len > 0 {
                    // Duplicate delivery of a message that stays queued.
                    let index = rng.below(len);
                    let message = sim.queue(to)[index].clone();
                    sim.event(to, ClipEvent::Received(message));
                }
            }
            78..=81 => {
                let side = random_side(&mut rng);
                sim.render_promises(side);
            }
            82..=85 => {
                let side = random_side(&mut rng);
                sim.peer(side).os.fail_next_writes(1);
            }
            86..=90 => {
                sim.advance(11_000);
                sim.event(Side::A, ClipEvent::Tick);
                sim.event(Side::B, ClipEvent::Tick);
            }
            91..=95 => {
                if connected {
                    sim.disconnect();
                } else {
                    sim.connect();
                }
                connected = !connected;
            }
            _ => {
                if connected {
                    sim.reconnect();
                }
            }
        }
    }

    // Quiescence: a final clean connection, then everything is delivered.
    sim.a.os.fail_next_writes(0);
    sim.b.os.fail_next_writes(0);
    sim.reconnect();
    while !sim.to_a.is_empty() || !sim.to_b.is_empty() {
        let to = random_side(&mut rng);
        let len = sim.queue(to).len();
        if len > 0 {
            let index = rng.below(len);
            sim.deliver(to, index);
        }
    }
    sim.render_promises(Side::A);
    sim.render_promises(Side::B);
    sim.drain();
    sim
}

#[test]
fn peers_converge_after_quiescence_under_random_schedules() {
    for seed in 1..=500u64 {
        let sim = run_random_session(seed);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sim.assert_converged();
            let winner = sim.a.sync.latest();
            assert_eq!(sim.a.sync.paste_requirement(), None);
            assert_eq!(sim.b.sync.paste_requirement(), None);
            if let Some(winner) = winner {
                let origin = if winner.1 == A_ID { &sim.a } else { &sim.b };
                assert_eq!(origin.sync.local_generation(), Some(winner));
                let other = if winner.1 == A_ID { &sim.b } else { &sim.a };
                assert_eq!(other.sync.remote_generation(), Some(winner));
                assert_eq!(origin.sync.acked(), Some(winner));
                assert!(other.os.promised_kinds().is_empty());
            }
        }));
        if result.is_err() {
            panic!("convergence failed for seed {seed}");
        }
    }
}

#[test]
fn random_schedules_are_reproducible() {
    let first = run_random_session(77);
    let second = run_random_session(77);
    assert_eq!(first.a.sync.latest(), second.a.sync.latest());
    assert_eq!(first.a.sent, second.a.sent);
    assert_eq!(first.b.sent, second.b.sent);
}

#[test]
fn defaults_match_the_design() {
    let config = ClipConfig::default();
    assert_eq!(config.fetch_timeout, Duration::from_secs(10));
    assert_eq!(config.inline_threshold, 64 * 1024);
    assert_eq!(config.max_format_size, 32 * 1024 * 1024);
    assert!(!config.policy.sync_sensitive);
}
