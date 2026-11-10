//! The agent with the desktop UI as its consent surface (`--consent ui`).
//!
//! [`UiChain`] asks the attached UI, or else a fallback (the controlling terminal) if
//! there is one, or else nobody, which refuses. It also keeps §9.3's promise that a
//! session is never left running unseen: when the last surface goes away the
//! session is ended, and a session that starts with no surface is ended at once.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use nexa_auth::{DeviceIdentity, PairingStore};
use nexa_session::handshake::{Consent, ConsentAnswer, ConsentRequest};
use thiserror::Error;

use crate::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentUi, Platform,
    ipc::{IpcConfig, IpcError, IpcHub, IpcListener, IpcServer},
    start,
};

pub struct UiChain {
    ipc: Arc<IpcHub>,
    fallback: Option<Arc<dyn AgentUi>>,
    end_session: OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl UiChain {
    pub fn new(ipc: Arc<IpcHub>, fallback: Option<Arc<dyn AgentUi>>) -> Arc<Self> {
        Arc::new(Self {
            ipc,
            fallback,
            end_session: OnceLock::new(),
        })
    }

    /// How to end the running session when nobody could see it any more.
    pub fn set_end_session(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.end_session.set(Arc::new(f));
    }

    /// Ends the session if no surface is left to show it.
    pub fn check_presence(&self) {
        if !self.can_present()
            && let Some(end) = self.end_session.get()
        {
            end();
        }
    }
}

impl Consent for UiChain {
    fn can_present(&self) -> bool {
        self.ipc.can_present() || self.fallback.as_ref().is_some_and(|f| f.can_present())
    }

    fn ask(&self, request: &ConsentRequest, timeout: Duration) -> ConsentAnswer {
        if self.ipc.can_present() {
            return self.ipc.ask(request, timeout);
        }
        match &self.fallback {
            Some(f) if f.can_present() => f.ask(request, timeout),
            _ => ConsentAnswer::deny(),
        }
    }
}

impl AgentUi for UiChain {
    fn event(&self, event: AgentEvent) {
        let started = matches!(event, AgentEvent::SessionStarted(_));
        if let Some(f) = &self.fallback {
            f.event(event.clone());
        }
        self.ipc.event(event);
        if started {
            self.check_presence();
        }
    }
}

#[derive(Debug, Error)]
pub enum UiHostError {
    #[error(transparent)]
    Ipc(#[from] IpcError),
    #[error(transparent)]
    Agent(#[from] AgentError),
}

/// A running agent with its IPC server. Dropping it removes the socket; call
/// `agent.shutdown()` to stop the agent itself.
pub struct UiHost {
    pub agent: Arc<Agent>,
    pub hub: Arc<IpcHub>,
    pub chain: Arc<UiChain>,
    pub server: IpcServer,
}

/// Binds the IPC socket first, so a second agent fails before touching the network,
/// then starts the agent and begins accepting UI connections. Must run inside a
/// multi-threaded tokio runtime.
pub async fn start_with_ui(
    config: AgentConfig,
    identity: DeviceIdentity,
    store: PairingStore,
    platform: Arc<dyn Platform>,
    fallback: Option<Arc<dyn AgentUi>>,
    ipc: IpcConfig,
    on_shutdown: Arc<dyn Fn() + Send + Sync>,
) -> Result<UiHost, UiHostError> {
    let listener = IpcListener::bind(ipc)?;
    let hub = IpcHub::new();
    {
        let platform = platform.clone();
        hub.set_input_available(Arc::new(move || platform.input_available()));
    }
    let chain = UiChain::new(hub.clone(), fallback);
    let agent = Arc::new(
        start(
            config,
            identity,
            store,
            platform,
            chain.clone() as Arc<dyn AgentUi>,
        )
        .await?,
    );
    {
        let agent = Arc::downgrade(&agent);
        chain.set_end_session(move || {
            if let Some(agent) = agent.upgrade() {
                agent.disconnect();
            }
        });
    }
    {
        let chain = Arc::downgrade(&chain);
        hub.set_on_detached(move || {
            if let Some(chain) = chain.upgrade() {
                chain.check_presence();
            }
        });
    }
    let server = listener.serve(hub.clone(), agent.clone(), on_shutdown);
    Ok(UiHost {
        agent,
        hub,
        chain,
        server,
    })
}
