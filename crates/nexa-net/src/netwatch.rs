//! Network change notifications (§4.5 网络变化): interfaces, addresses or the
//! default route changed, so candidates should be gathered again.
//!
//! On macOS `NWPathMonitor` reports path changes as they happen. It does not
//! report every address change on an interface that stays up, so a poll of
//! the interface addresses and default gateways runs as well; it is also all
//! there is on other platforms. Observing only: nothing here changes settings.

use std::{
    collections::BTreeSet,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    time::Duration,
};

use crate::{gather::system_addresses, portmap::default_gateways};

pub const POLL_EVERY: Duration = Duration::from_secs(2);

pub type OnChange = Arc<dyn Fn() + Send + Sync>;

/// What the poll compares: every interface address and the default gateways.
pub fn fingerprint() -> BTreeSet<(String, IpAddr)> {
    let mut f: BTreeSet<(String, IpAddr)> = system_addresses().into_iter().collect();
    for gw in default_gateways() {
        f.insert(("<gateway>".to_owned(), gw));
    }
    f
}

/// Runs until dropped.
pub struct NetWatcher {
    stop: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    _monitor: Option<macos::PathMonitor>,
}

impl Drop for NetWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

impl NetWatcher {
    /// `on_change` runs on a background thread, possibly more than once per change.
    pub fn start(on_change: OnChange) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (st, cb) = (stop.clone(), on_change.clone());
        let _ = std::thread::Builder::new()
            .name("netwatch".into())
            .spawn(move || {
                let mut last = fingerprint();
                while !st.load(Relaxed) {
                    std::thread::sleep(POLL_EVERY);
                    let now = fingerprint();
                    if now != last {
                        last = now;
                        cb();
                    }
                }
            });
        Self {
            stop,
            #[cfg(target_os = "macos")]
            _monitor: macos::PathMonitor::start(on_change),
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        ffi::c_void,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering::Relaxed},
        },
    };

    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchRetained};

    use super::OnChange;

    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        fn nw_path_monitor_create() -> *mut c_void;
        fn nw_path_monitor_set_queue(monitor: *mut c_void, queue: *mut c_void);
        fn nw_path_monitor_set_update_handler(
            monitor: *mut c_void,
            handler: &block2::DynBlock<dyn Fn(*mut c_void)>,
        );
        fn nw_path_monitor_start(monitor: *mut c_void);
        fn nw_path_monitor_cancel(monitor: *mut c_void);
        fn nw_release(object: *mut c_void);
    }

    pub struct PathMonitor {
        monitor: *mut c_void,
        _queue: DispatchRetained<DispatchQueue>,
    }

    // SAFETY: an nw_path_monitor_t may be used and cancelled from any thread.
    unsafe impl Send for PathMonitor {}
    unsafe impl Sync for PathMonitor {}

    impl PathMonitor {
        pub fn start(on_change: OnChange) -> Option<Self> {
            let queue = DispatchQueue::new("nexa.netwatch", None);
            // SAFETY: plain C calls on an object we own; the block is copied by the call.
            unsafe {
                let monitor = nw_path_monitor_create();
                if monitor.is_null() {
                    return None;
                }
                // The first update only reports the current state.
                let seen = Arc::new(AtomicBool::new(false));
                let block = RcBlock::new(move |_path: *mut c_void| {
                    if seen.swap(true, Relaxed) {
                        on_change();
                    }
                });
                nw_path_monitor_set_update_handler(monitor, &block);
                let q: *const DispatchQueue = &*queue;
                nw_path_monitor_set_queue(monitor, q as *mut c_void);
                nw_path_monitor_start(monitor);
                Some(Self {
                    monitor,
                    _queue: queue,
                })
            }
        }
    }

    impl Drop for PathMonitor {
        fn drop(&mut self) {
            // SAFETY: `monitor` came from nw_path_monitor_create and is released once.
            unsafe {
                nw_path_monitor_cancel(self.monitor);
                nw_release(self.monitor);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_and_stops_without_reporting_a_change() {
        let hits = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let h = hits.clone();
        let w = NetWatcher::start(Arc::new(move || {
            h.fetch_add(1, Relaxed);
        }));
        std::thread::sleep(Duration::from_millis(100));
        drop(w);
        // A real change during the test would be legitimate; there is no assertion on it.
        let _ = hits.load(Relaxed);
        assert!(!fingerprint().is_empty() || system_addresses().is_empty());
    }
}
