//! User-space impairment proxy, so the spike needs no root / dummynet.

use std::{
    collections::VecDeque,
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{Rng, bind_udp, set_realtime_qos};

#[derive(Clone, Copy, Debug)]
pub enum Loss {
    None,
    Random(f64),
    /// Gilbert-Elliott: P(good→bad), P(bad→good), loss rate while bad.
    Burst { enter: f64, exit: f64, bad_loss: f64 },
}

#[derive(Clone, Copy, Debug)]
pub struct NetemCfg {
    pub one_way_delay: Duration,
    pub loss: Loss,
}

#[derive(Default)]
pub struct NetemCtl {
    pub blackhole: AtomicBool,
    pub stop: AtomicBool,
    pub max_udp_len: AtomicUsize,
    pub forwarded: AtomicU64,
    pub dropped: AtomicU64,
}

pub struct Netem {
    pub a: SocketAddr,
    pub b: SocketAddr,
    pub ctl: Arc<NetemCtl>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl Netem {
    /// `a` faces the viewer, `b` faces the host.
    pub fn start(cfg: NetemCfg, viewer: SocketAddr, host: SocketAddr, seed: u64) -> anyhow::Result<Self> {
        let sa = bind_udp()?;
        let sb = bind_udp()?;
        let (a, b) = (sa.local_addr()?, sb.local_addr()?);
        let ctl = Arc::new(NetemCtl::default());
        let mut threads = Vec::new();
        for (rx, tx, dest, s) in [
            (sa.try_clone()?, sb.try_clone()?, host, seed),
            (sb, sa, viewer, seed ^ 0xABCD),
        ] {
            let ctl = ctl.clone();
            threads.push(thread::spawn(move || pump(rx, tx, dest, cfg, ctl, s)));
        }
        Ok(Self { a, b, ctl, threads })
    }

    pub fn stop(self) {
        self.ctl.stop.store(true, Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

fn pump(rx: UdpSocket, tx: UdpSocket, dest: SocketAddr, cfg: NetemCfg, ctl: Arc<NetemCtl>, seed: u64) {
    set_realtime_qos();
    rx.set_nonblocking(true).unwrap();
    let mut rng = Rng::new(seed);
    let mut q: VecDeque<(Instant, Vec<u8>)> = VecDeque::new();
    let mut buf = [0u8; 2048];
    let mut bad = false;
    while !ctl.stop.load(Relaxed) {
        let mut idle = true;
        while let Ok((n, _)) = rx.recv_from(&mut buf) {
            idle = false;
            ctl.max_udp_len.fetch_max(n, Relaxed);
            let lost = match cfg.loss {
                Loss::None => false,
                Loss::Random(p) => rng.f64() < p,
                Loss::Burst { enter, exit, bad_loss } => {
                    bad = if bad { rng.f64() >= exit } else { rng.f64() < enter };
                    bad && rng.f64() < bad_loss
                }
            };
            if lost || ctl.blackhole.load(Relaxed) {
                ctl.dropped.fetch_add(1, Relaxed);
                continue;
            }
            q.push_back((Instant::now() + cfg.one_way_delay, buf[..n].to_vec()));
        }
        let now = Instant::now();
        while q.front().is_some_and(|(t, _)| *t <= now) {
            let (_, p) = q.pop_front().unwrap();
            let _ = tx.send_to(&p, dest);
            ctl.forwarded.fetch_add(1, Relaxed);
            idle = false;
        }
        if idle {
            std::hint::spin_loop();
        }
    }
}
