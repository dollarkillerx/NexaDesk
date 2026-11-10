//! Path scoring, switching and probe scheduling (§4.5).
//!
//! Sans-IO: the caller reports what it measured and calls [`PathManager::poll`]
//! with the current time, at least every 50 ms. Only the viewer decides; the
//! host's manager schedules probes and follows `PathSwitch` control messages.
//!
//! RTT and liveness fed in here must come from authenticated traffic: QUIC
//! packets, or STUN responses that passed `verify_integrity`. A STUN packet
//! can influence a score but can never name a path to switch to.

use std::time::{Duration, Instant};

/// Matches `PathSwitch.path_id` on the control stream.
pub type PathId = u32;

/// §4.5 path penalties, in score units (ms) 【初值】.
pub const PENALTY_DIRECT: f64 = 0.0;
pub const PENALTY_TAILSCALE_DIRECT: f64 = 2.0;
pub const PENALTY_OWN_RELAY: f64 = 5.0;
pub const PENALTY_DERP: f64 = 30.0;
pub const PENALTY_WSS_RELAY: f64 = 60.0;
/// Score units per percentage point of loss.
pub const LOSS_WEIGHT: f64 = 8.0;

/// A candidate must score at most this fraction of the current path.
pub const UPGRADE_RATIO: f64 = 0.85;
/// ...and be at least this much better in absolute terms.
pub const UPGRADE_MIN_DELTA_MS: f64 = 3.0;
pub const UPGRADE_HOLD: Duration = Duration::from_secs(3);
pub const FAILOVER_SILENCE: Duration = Duration::from_secs(1);

pub const PROBE_FAST_INTERVAL: Duration = Duration::from_secs(1);
pub const PROBE_FAST_PHASE: Duration = Duration::from_secs(10);
pub const PROBE_SLOW_INTERVAL: Duration = Duration::from_secs(5);
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Fresh qualifying RTT samples required during the hold. At the slow probe
/// cadence 3 s could otherwise pass on a single lucky sample, so a candidate
/// under evaluation is probed at the fast cadence, and any raw sample that
/// does not beat the current path restarts the hold.
const UPGRADE_MIN_SAMPLES: u32 = 3;
/// A standby with this many unanswered probes is not eligible for failover.
const PROBE_MISS_LIMIT: u32 = 3;
/// Time constant of the RTT filter. Sized against the 3 s hold so that
/// neighbouring noisy samples cannot complete a hold by themselves.
const RTT_SMOOTHING_TAU_S: f64 = 3.0;
/// A sample after a long gap weighs no more than one at the fast cadence, so
/// sparse probes cannot swing the filter on a single reading.
const RTT_SMOOTHING_MAX_DT_S: f64 = 1.0;

/// §4.2 candidate kinds, split where §4.5 charges different penalties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandKind {
    Host,
    Ipv6,
    Srflx,
    Portmap,
    TailnetDirect,
    TailnetDerp,
    Relay,
    WssRelay,
}

impl CandKind {
    pub const fn penalty(self) -> f64 {
        match self {
            CandKind::Host | CandKind::Ipv6 | CandKind::Srflx | CandKind::Portmap => PENALTY_DIRECT,
            CandKind::TailnetDirect => PENALTY_TAILSCALE_DIRECT,
            CandKind::Relay => PENALTY_OWN_RELAY,
            CandKind::TailnetDerp => PENALTY_DERP,
            CandKind::WssRelay => PENALTY_WSS_RELAY,
        }
    }
}

/// `score = rtt_ms + 8 × loss_pct + penalty(kind)`; lower is better.
pub fn score(rtt: Duration, loss_pct: f64, kind: CandKind) -> f64 {
    score_ms(rtt.as_secs_f64() * 1e3, loss_pct, kind)
}

fn score_ms(rtt_ms: f64, loss_pct: f64, kind: CandKind) -> f64 {
    rtt_ms + LOSS_WEIGHT * loss_pct + kind.penalty()
}

fn beats_current(candidate: f64, current: f64) -> bool {
    candidate <= current * UPGRADE_RATIO && current - candidate >= UPGRADE_MIN_DELTA_MS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchReason {
    /// First validated path: start QUIC now, without waiting for a better one.
    Initial,
    Upgrade,
    Failover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathDecision {
    Stay,
    /// The viewer sends `PathSwitch { to }` over the control stream.
    Switch {
        to: PathId,
        reason: SwitchReason,
    },
    /// The current path is gone and nothing else is alive. Reported once per outage.
    NoPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeKind {
    /// Standby RTT and liveness measurement.
    Probe,
    /// Keeps the NAT binding of the active path open.
    Keepalive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeRequest {
    pub path: PathId,
    pub kind: ProbeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathOutput {
    pub decision: PathDecision,
    pub probes: Vec<ProbeRequest>,
}

#[derive(Debug)]
struct PathState {
    id: PathId,
    kind: CandKind,
    rtt_ms: Option<f64>,
    rtt_at: Instant,
    loss_pct: f64,
    last_rx: Instant,
    unanswered_probes: u32,
    /// Declared dead by failover; ineligible until something arrives on it again.
    failed: bool,
    standby_since: Instant,
    next_probe: Instant,
    upgrade_since: Option<Instant>,
    upgrade_samples: u32,
}

impl PathState {
    fn score(&self) -> Option<f64> {
        self.rtt_ms
            .map(|rtt| score_ms(rtt, self.loss_pct, self.kind))
    }

    fn alive(&self) -> bool {
        !self.failed && self.unanswered_probes < PROBE_MISS_LIMIT
    }

    fn clear_upgrade(&mut self) {
        self.upgrade_since = None;
        self.upgrade_samples = 0;
    }

    fn demote(&mut self, now: Instant) {
        self.standby_since = now;
        self.next_probe = now + PROBE_FAST_INTERVAL;
        self.unanswered_probes = 0;
        self.clear_upgrade();
    }
}

#[derive(Debug)]
pub struct PathManager {
    role: Role,
    paths: Vec<PathState>,
    active: Option<PathId>,
    /// The silence timer starts no earlier than this, because a standby's
    /// last packet may be seconds old at the moment it is promoted.
    activated_at: Option<Instant>,
    ever_active: bool,
    no_path_reported: bool,
}

impl PathManager {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            paths: Vec::new(),
            active: None,
            activated_at: None,
            ever_active: false,
            no_path_reported: false,
        }
    }

    pub fn active(&self) -> Option<PathId> {
        self.active
    }

    /// Current score of a path, or `None` before its first RTT sample.
    pub fn path_score(&self, id: PathId) -> Option<f64> {
        self.path(id).and_then(PathState::score)
    }

    /// Registers a `Validated` path. Re-adding a known id is ignored.
    pub fn add_path(&mut self, id: PathId, kind: CandKind, now: Instant) {
        if self.path(id).is_some() {
            return;
        }
        self.paths.push(PathState {
            id,
            kind,
            rtt_ms: None,
            rtt_at: now,
            loss_pct: 0.0,
            last_rx: now,
            unanswered_probes: 0,
            failed: false,
            standby_since: now,
            next_probe: now + PROBE_FAST_INTERVAL,
            upgrade_since: None,
            upgrade_samples: 0,
        });
    }

    pub fn remove_path(&mut self, id: PathId) {
        self.paths.retain(|p| p.id != id);
        if self.active == Some(id) {
            self.active = None;
            self.activated_at = None;
        }
    }

    /// Any authenticated inbound packet on the path.
    pub fn on_inbound(&mut self, id: PathId, now: Instant) {
        if let Some(p) = self.path_mut(id) {
            p.last_rx = now;
            p.unanswered_probes = 0;
            p.failed = false;
        }
    }

    /// An RTT measurement. Also counts as inbound, since a response arrived.
    pub fn on_rtt(&mut self, id: PathId, rtt: Duration, now: Instant) {
        self.on_inbound(id, now);
        let current = self
            .active
            .filter(|&a| a != id)
            .and_then(|a| self.path_score(a));
        let Some(p) = self.path_mut(id) else { return };
        let sample = rtt.as_secs_f64() * 1e3;
        p.rtt_ms = Some(match p.rtt_ms {
            None => sample,
            Some(prev) => {
                let dt = now
                    .saturating_duration_since(p.rtt_at)
                    .as_secs_f64()
                    .min(RTT_SMOOTHING_MAX_DT_S);
                let alpha = 1.0 - (-dt / RTT_SMOOTHING_TAU_S).exp();
                prev + alpha * (sample - prev)
            }
        });
        p.rtt_at = now;
        if p.upgrade_since.is_some() {
            if current.is_some_and(|c| beats_current(score_ms(sample, p.loss_pct, p.kind), c)) {
                p.upgrade_samples += 1;
            } else {
                p.clear_upgrade();
            }
        }
    }

    /// Loss on the path in percent (0–100). Non-finite values are ignored.
    pub fn on_loss(&mut self, id: PathId, loss_pct: f64) {
        if let Some(p) = self.path_mut(id).filter(|_| loss_pct.is_finite()) {
            p.loss_pct = loss_pct.clamp(0.0, 100.0);
        }
    }

    /// Host only: apply a `PathSwitch` received on the authenticated control
    /// stream. Returns false for the viewer or an unknown path.
    pub fn apply_remote_switch(&mut self, id: PathId, now: Instant) -> bool {
        if self.role != Role::Host || self.path(id).is_none() {
            return false;
        }
        self.activate(id, now);
        true
    }

    /// Advances timers and reports what to do now.
    pub fn poll(&mut self, now: Instant) -> PathOutput {
        let decision = if self.role == Role::Viewer {
            self.decide(now)
        } else {
            PathDecision::Stay
        };
        let probes = self.due_probes(now);
        PathOutput { decision, probes }
    }

    fn path(&self, id: PathId) -> Option<&PathState> {
        self.paths.iter().find(|p| p.id == id)
    }

    fn path_mut(&mut self, id: PathId) -> Option<&mut PathState> {
        self.paths.iter_mut().find(|p| p.id == id)
    }

    fn activate(&mut self, id: PathId, now: Instant) {
        if let Some(old) = self.active.filter(|&old| old != id)
            && let Some(p) = self.path_mut(old)
        {
            p.demote(now);
        }
        for p in &mut self.paths {
            p.clear_upgrade();
        }
        if let Some(p) = self.path_mut(id) {
            p.next_probe = now + KEEPALIVE_INTERVAL;
            p.unanswered_probes = 0;
        }
        self.active = Some(id);
        self.activated_at = Some(now);
        self.ever_active = true;
    }

    fn decide(&mut self, now: Instant) -> PathDecision {
        let Some(active) = self.active else {
            return self.fail_over(now);
        };
        let silent_since = self
            .path(active)
            .map(|p| p.last_rx)
            .into_iter()
            .chain(self.activated_at)
            .max();
        if silent_since.is_none_or(|t| now.saturating_duration_since(t) >= FAILOVER_SILENCE) {
            return self.fail_over(now);
        }
        if std::mem::take(&mut self.no_path_reported) {
            // Traffic came back on the path we had given up on.
            return PathDecision::Switch {
                to: active,
                reason: SwitchReason::Failover,
            };
        }
        self.consider_upgrade(active, now)
    }

    fn fail_over(&mut self, now: Instant) -> PathDecision {
        let active = self.active;
        // The old path has just proven itself dead.
        if let Some(p) = active.and_then(|id| self.path_mut(id)) {
            p.failed = true;
        }
        let best = self
            .paths
            .iter()
            .filter(|p| Some(p.id) != active && p.alive())
            .min_by(|a, b| {
                a.score()
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&b.score().unwrap_or(f64::INFINITY))
            })
            .map(|p| p.id);
        match best {
            Some(to) => {
                let reason = if self.ever_active {
                    SwitchReason::Failover
                } else {
                    SwitchReason::Initial
                };
                self.activate(to, now);
                self.no_path_reported = false;
                PathDecision::Switch { to, reason }
            }
            None if !self.ever_active || self.no_path_reported => PathDecision::Stay,
            None => {
                self.no_path_reported = true;
                PathDecision::NoPath
            }
        }
    }

    fn consider_upgrade(&mut self, active: PathId, now: Instant) -> PathDecision {
        let Some(current) = self.path_score(active) else {
            return PathDecision::Stay;
        };
        let mut ready: Option<(PathId, f64)> = None;
        for p in self.paths.iter_mut().filter(|p| p.id != active) {
            let candidate = p
                .score()
                .filter(|&s| p.alive() && beats_current(s, current));
            let Some(s) = candidate else {
                p.clear_upgrade();
                continue;
            };
            let Some(since) = p.upgrade_since else {
                p.upgrade_since = Some(now);
                p.next_probe = p.next_probe.min(now + PROBE_FAST_INTERVAL);
                continue;
            };
            let held = now.saturating_duration_since(since) >= UPGRADE_HOLD
                && p.upgrade_samples >= UPGRADE_MIN_SAMPLES;
            if held && ready.is_none_or(|(_, best)| s < best) {
                ready = Some((p.id, s));
            }
        }
        match ready {
            Some((to, _)) => {
                self.activate(to, now);
                PathDecision::Switch {
                    to,
                    reason: SwitchReason::Upgrade,
                }
            }
            None => PathDecision::Stay,
        }
    }

    fn due_probes(&mut self, now: Instant) -> Vec<ProbeRequest> {
        let active = self.active;
        let mut out = Vec::new();
        for p in self.paths.iter_mut().filter(|p| now >= p.next_probe) {
            let (kind, interval) = if Some(p.id) == active {
                (ProbeKind::Keepalive, KEEPALIVE_INTERVAL)
            } else {
                p.unanswered_probes = p.unanswered_probes.saturating_add(1);
                let fast = p.upgrade_since.is_some()
                    || now.saturating_duration_since(p.standby_since) < PROBE_FAST_PHASE;
                (
                    ProbeKind::Probe,
                    if fast {
                        PROBE_FAST_INTERVAL
                    } else {
                        PROBE_SLOW_INTERVAL
                    },
                )
            };
            p.next_probe = now + interval;
            out.push(ProbeRequest { path: p.id, kind });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVE: PathId = 1;
    const OTHER: PathId = 2;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    /// A viewer whose path 1 is already active, with `rtt_a` and `rtt_b` measured.
    fn viewer(
        t: Instant,
        kind_a: CandKind,
        rtt_a: u64,
        kind_b: CandKind,
        rtt_b: u64,
    ) -> PathManager {
        let mut m = PathManager::new(Role::Viewer);
        m.add_path(ACTIVE, kind_a, t);
        assert_eq!(
            m.poll(t).decision,
            PathDecision::Switch {
                to: ACTIVE,
                reason: SwitchReason::Initial
            }
        );
        m.add_path(OTHER, kind_b, t);
        m.on_rtt(ACTIVE, ms(rtt_a), t);
        m.on_rtt(OTHER, ms(rtt_b), t);
        m
    }

    /// Runs 100 ms steps, keeping the active path fed and answering each probe
    /// with `rtt_of(path, step)`. Returns the first non-Stay decision and when.
    fn drive(
        m: &mut PathManager,
        t0: Instant,
        steps: u64,
        mut rtt_of: impl FnMut(PathId, u64) -> u64,
    ) -> Vec<(u64, PathDecision)> {
        let mut decisions = Vec::new();
        for step in 1..=steps {
            let now = t0 + ms(step * 100);
            let active = m.active().unwrap();
            m.on_rtt(active, ms(rtt_of(active, step)), now);
            let out = m.poll(now);
            for p in &out.probes {
                m.on_rtt(p.path, ms(rtt_of(p.path, step)), now);
            }
            if out.decision != PathDecision::Stay {
                decisions.push((step, out.decision));
            }
        }
        decisions
    }

    #[test]
    fn score_formula_and_penalties() {
        assert_eq!(score(ms(20), 0.0, CandKind::Host), 20.0);
        assert_eq!(score(ms(20), 1.5, CandKind::Srflx), 32.0);
        assert_eq!(score(ms(20), 0.0, CandKind::TailnetDirect), 22.0);
        assert_eq!(score(ms(20), 0.0, CandKind::Relay), 25.0);
        assert_eq!(score(ms(20), 0.0, CandKind::TailnetDerp), 50.0);
        assert_eq!(score(ms(20), 0.0, CandKind::WssRelay), 80.0);
        for k in [
            CandKind::Host,
            CandKind::Ipv6,
            CandKind::Srflx,
            CandKind::Portmap,
        ] {
            assert_eq!(k.penalty(), 0.0);
        }
    }

    #[test]
    fn first_validated_path_starts_immediately() {
        let t = Instant::now();
        let mut m = PathManager::new(Role::Viewer);
        assert_eq!(m.poll(t).decision, PathDecision::Stay);
        m.add_path(7, CandKind::WssRelay, t);
        assert_eq!(
            m.poll(t).decision,
            PathDecision::Switch {
                to: 7,
                reason: SwitchReason::Initial
            }
        );
        assert_eq!(m.active(), Some(7));
        assert_eq!(m.poll(t).decision, PathDecision::Stay);
    }

    #[test]
    fn upgrades_only_after_sustained_three_seconds() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 30, CandKind::Host, 10);
        let got = drive(&mut m, t, 100, |p, _| if p == ACTIVE { 30 } else { 10 });
        // The advantage is first observed at step 1, and the hold runs from there.
        assert_eq!(
            got,
            vec![(
                31,
                PathDecision::Switch {
                    to: OTHER,
                    reason: SwitchReason::Upgrade
                }
            )]
        );
        assert_eq!(m.active(), Some(OTHER));
    }

    #[test]
    fn improvement_below_fifteen_percent_does_not_upgrade() {
        let t = Instant::now();
        // 14% better and 7 ms apart: passes the absolute rule, fails the ratio.
        let mut m = viewer(t, CandKind::Host, 50, CandKind::Host, 43);
        assert!(drive(&mut m, t, 200, |p, _| if p == ACTIVE { 50 } else { 43 }).is_empty());
    }

    #[test]
    fn exactly_fifteen_percent_upgrades() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 100, CandKind::Host, 85);
        assert_eq!(
            drive(&mut m, t, 40, |p, _| if p == ACTIVE { 100 } else { 85 }).len(),
            1
        );
    }

    #[test]
    fn small_absolute_gain_does_not_upgrade() {
        let t = Instant::now();
        // 40% better but only 2 ms apart.
        let mut m = viewer(t, CandKind::Host, 5, CandKind::Host, 3);
        assert!(drive(&mut m, t, 200, |p, _| if p == ACTIVE { 5 } else { 3 }).is_empty());
    }

    #[test]
    fn penalty_counts_against_upgrade() {
        let t = Instant::now();
        // 20 ms direct against 15 ms over our relay: 20 vs 20.
        let mut m = viewer(t, CandKind::Host, 20, CandKind::Relay, 15);
        assert!(drive(&mut m, t, 200, |p, _| if p == ACTIVE { 20 } else { 15 }).is_empty());
    }

    #[test]
    fn loss_counts_toward_upgrade() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 30, CandKind::Host, 30);
        m.on_loss(ACTIVE, 2.0); // Score 46 against 30.
        assert_eq!(drive(&mut m, t, 40, |_, _| 30).len(), 1);
    }

    #[test]
    fn interrupted_advantage_restarts_the_hold() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 30, CandKind::Host, 10);
        // The candidate is good for 2 s, then bad for 5 s, then good again.
        let rtt = |p: PathId, step: u64| {
            if p == ACTIVE {
                30
            } else if (20..70).contains(&step) {
                100
            } else {
                10
            }
        };
        let got = drive(&mut m, t, 300, rtt);
        assert_eq!(got.len(), 1);
        assert!(got[0].0 > 100, "switched at step {}", got[0].0);
    }

    #[test]
    fn sparse_probes_cannot_pass_the_hold_on_one_sample() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 30, CandKind::Host, 28);
        let mut replied_at = Vec::new();
        for step in 1..=400u64 {
            let now = t + ms(step * 100);
            m.on_rtt(ACTIVE, ms(30), now);
            let out = m.poll(now);
            assert_eq!(out.decision, PathDecision::Stay, "step {step}");
            for _ in out.probes.iter().filter(|p| p.path == OTHER) {
                // Past the fast phase the candidate answers once with a lucky reading.
                m.on_rtt(OTHER, ms(if step == 150 { 5 } else { 28 }), now);
                replied_at.push(step);
            }
        }
        let after: Vec<u64> = replied_at
            .into_iter()
            .filter(|&s| s >= 150)
            .take(2)
            .collect();
        assert!(
            after[1] - after[0] <= 11,
            "probe cadence must speed up under evaluation: {after:?}"
        );
    }

    #[test]
    fn sparse_probes_upgrade_once_confirmed() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 30, CandKind::Host, 28);
        let got = drive(&mut m, t, 400, |p, step| match (p, step) {
            (ACTIVE, _) => 30,
            (_, s) if s < 150 => 28,
            _ => 5,
        });
        assert_eq!(got.len(), 1, "{got:?}");
        let (step, _) = got[0];
        assert!((180..=195).contains(&step), "switched at step {step}");
    }

    #[test]
    fn fails_over_after_one_second_of_silence() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 10, CandKind::Relay, 40);
        m.on_inbound(ACTIVE, t);
        assert_eq!(m.poll(t + ms(999)).decision, PathDecision::Stay);
        m.on_inbound(OTHER, t + ms(999));
        assert_eq!(
            m.poll(t + ms(1000)).decision,
            PathDecision::Switch {
                to: OTHER,
                reason: SwitchReason::Failover
            }
        );
    }

    #[test]
    fn any_inbound_packet_resets_the_silence_timer() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 10, CandKind::Relay, 40);
        for i in 1..=20 {
            let now = t + ms(i * 900);
            m.on_inbound(ACTIVE, now);
            m.on_inbound(OTHER, now);
            assert_eq!(m.poll(now).decision, PathDecision::Stay);
        }
    }

    #[test]
    fn failover_picks_the_best_alive_path() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 10, CandKind::WssRelay, 10);
        m.add_path(3, CandKind::Relay, t);
        m.on_rtt(3, ms(20), t);
        m.add_path(4, CandKind::Host, t);
        m.on_rtt(4, ms(1), t); // Best score, but it stops answering probes.
        for s in 1..=3 {
            let now = t + ms(s * 1000);
            for p in [ACTIVE, OTHER, 3] {
                m.on_inbound(p, now);
            }
            assert_eq!(m.poll(now).decision, PathDecision::Stay);
        }
        assert_eq!(
            m.poll(t + ms(4000)).decision,
            PathDecision::Switch {
                to: 3,
                reason: SwitchReason::Failover
            }
        );
    }

    #[test]
    fn promoted_path_gets_a_full_silence_budget() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 10, CandKind::Relay, 40);
        // The standby last spoke at t; it was promoted well after that.
        let promoted = t + ms(1000);
        assert!(matches!(
            m.poll(promoted).decision,
            PathDecision::Switch { to: OTHER, .. }
        ));
        assert_eq!(m.poll(promoted + ms(999)).decision, PathDecision::Stay);
    }

    #[test]
    fn failed_path_is_not_chosen_until_it_speaks_again() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 10, CandKind::Relay, 40);
        assert!(matches!(
            m.poll(t + ms(1000)).decision,
            PathDecision::Switch { to: OTHER, .. }
        ));
        // The promoted path dies too: path 1 is still marked failed.
        assert_eq!(m.poll(t + ms(2000)).decision, PathDecision::NoPath);
        assert_eq!(
            m.poll(t + ms(2500)).decision,
            PathDecision::Stay,
            "reported once"
        );

        m.on_inbound(ACTIVE, t + ms(2600));
        assert_eq!(
            m.poll(t + ms(2600)).decision,
            PathDecision::Switch {
                to: ACTIVE,
                reason: SwitchReason::Failover
            }
        );
    }

    #[test]
    fn recovery_on_the_current_path_is_reported_after_no_path() {
        let t = Instant::now();
        let mut m = PathManager::new(Role::Viewer);
        m.add_path(ACTIVE, CandKind::Host, t);
        let _ = m.poll(t);
        assert_eq!(m.poll(t + ms(1000)).decision, PathDecision::NoPath);
        m.on_inbound(ACTIVE, t + ms(1500));
        assert_eq!(
            m.poll(t + ms(1500)).decision,
            PathDecision::Switch {
                to: ACTIVE,
                reason: SwitchReason::Failover
            }
        );
        assert_eq!(m.poll(t + ms(1600)).decision, PathDecision::Stay);
    }

    #[test]
    fn removing_the_active_path_fails_over() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 10, CandKind::Relay, 40);
        m.remove_path(ACTIVE);
        assert_eq!(
            m.poll(t + ms(1)).decision,
            PathDecision::Switch {
                to: OTHER,
                reason: SwitchReason::Failover
            }
        );
    }

    /// Switches per seed on two paths with equal mean RTT of 40 ms and uniform
    /// noise of ±`spread` ms, over 10 simulated minutes per seed.
    fn switches_under_noise(spread: u64, spike_mask: u64) -> Vec<usize> {
        (1..=20u64)
            .map(|seed| {
                let t = Instant::now();
                let mut m = viewer(t, CandKind::Host, 40, CandKind::Host, 40);
                let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
                let noisy = move |_: PathId, _: u64| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let base = 40 - spread + x % (2 * spread + 1);
                    if x >> 32 & spike_mask == 0 {
                        base * 3
                    } else {
                        base
                    }
                };
                drive(&mut m, t, 6000, noisy).len()
            })
            .collect()
    }

    #[test]
    fn no_flapping_on_noisy_rtt() {
        let moderate = switches_under_noise(6, u64::MAX);
        assert_eq!(
            moderate.iter().sum::<usize>(),
            0,
            "±15% noise: {moderate:?}"
        );
        // ±25% with 3x spikes can occasionally look 15% better for 3 s, which is
        // a legitimate one-way switch; it must not oscillate.
        let harsh = switches_under_noise(10, 0x3F);
        assert!(harsh.iter().all(|&n| n <= 1), "{harsh:?}");
        assert!(harsh.iter().sum::<usize>() <= 2, "{harsh:?}");
    }

    #[test]
    fn marginal_noisy_advantage_settles_without_oscillating() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 40, CandKind::Host, 36);
        let mut x = 0xDEAD_BEEF_CAFE_F00Du64;
        let mut noisy = move |p: PathId, _: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let mean = if p == ACTIVE { 40 } else { 36 };
            mean - 6 + x % 13
        };
        let got = drive(&mut m, t, 6000, &mut noisy);
        assert!(got.len() <= 1, "{got:?}");
    }

    #[test]
    fn switch_back_requires_its_own_hysteresis() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 30, CandKind::Host, 10);
        let got = drive(&mut m, t, 300, |p, step| {
            if step < 50 {
                if p == ACTIVE { 30 } else { 10 }
            } else if p == ACTIVE {
                11
            } else {
                10
            }
        });
        assert_eq!(got.len(), 1);
        assert_eq!(m.active(), Some(OTHER));
    }

    #[test]
    fn host_never_decides_and_follows_control_messages() {
        let t = Instant::now();
        let mut m = PathManager::new(Role::Host);
        m.add_path(ACTIVE, CandKind::Host, t);
        m.add_path(OTHER, CandKind::Host, t);
        assert_eq!(m.poll(t).decision, PathDecision::Stay);
        assert_eq!(m.active(), None);

        assert!(m.apply_remote_switch(ACTIVE, t));
        m.on_rtt(ACTIVE, ms(50), t);
        m.on_rtt(OTHER, ms(5), t);
        for s in 1..=100 {
            let now = t + ms(s * 100);
            m.on_rtt(OTHER, ms(5), now);
            assert_eq!(m.poll(now).decision, PathDecision::Stay);
        }
        assert_eq!(m.active(), Some(ACTIVE));
        assert!(!m.apply_remote_switch(99, t));

        let mut viewer = PathManager::new(Role::Viewer);
        viewer.add_path(ACTIVE, CandKind::Host, t);
        assert!(!viewer.apply_remote_switch(ACTIVE, t));
    }

    #[test]
    fn standby_probe_cadence() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 10, CandKind::Host, 10);
        let mut sent = Vec::new();
        for s in 1..=300u64 {
            let now = t + ms(s * 100);
            m.on_inbound(ACTIVE, now);
            m.on_inbound(OTHER, now);
            for p in m.poll(now).probes {
                if p.path == OTHER {
                    assert_eq!(p.kind, ProbeKind::Probe);
                    sent.push(s / 10);
                }
            }
        }
        assert_eq!(sent, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 15, 20, 25, 30]);
    }

    #[test]
    fn active_path_gets_keepalives_every_fifteen_seconds() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 10, CandKind::Host, 10);
        let mut sent = Vec::new();
        for s in 1..=460u64 {
            let now = t + ms(s * 100);
            m.on_inbound(ACTIVE, now);
            m.on_inbound(OTHER, now);
            for p in m.poll(now).probes {
                if p.path == ACTIVE {
                    assert_eq!(p.kind, ProbeKind::Keepalive);
                    sent.push(s / 10);
                }
            }
        }
        assert_eq!(sent, vec![15, 30, 45]);
    }

    #[test]
    fn demoted_path_restarts_fast_probing() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 30, CandKind::Host, 10);
        let got = drive(&mut m, t, 31, |p, _| if p == ACTIVE { 30 } else { 10 });
        assert_eq!(got.len(), 1);
        let switched = t + ms(3100);
        m.on_inbound(OTHER, switched + ms(999));
        let out = m.poll(switched + ms(1000));
        assert_eq!(
            out.probes,
            vec![ProbeRequest {
                path: ACTIVE,
                kind: ProbeKind::Probe
            }]
        );
    }

    #[test]
    fn invalid_loss_is_ignored() {
        let t = Instant::now();
        let mut m = viewer(t, CandKind::Host, 10, CandKind::Host, 10);
        m.on_loss(ACTIVE, f64::NAN);
        assert_eq!(m.path_score(ACTIVE), Some(10.0));
        m.on_loss(ACTIVE, 500.0);
        assert_eq!(m.path_score(ACTIVE), Some(810.0));
    }
}
