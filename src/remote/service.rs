//! `RemoteService`: the one owner of this machine's remote endpoint (DEV-1160).
//!
//! It binds the transport only while something needs it, and shuts it down
//! when nothing does:
//!
//! ```text
//!   needed = Allow remote connections
//!          ∨ a pairing code is showing (or its request is in flight)
//!          ∨ this Mac is pairing with another (Add machine)
//! ```
//!
//! So the first pairing works with the switch still off, and turning the
//! switch off closes every remote connection and — once no pairing is in
//! progress — the endpoint itself.
//!
//! The UI talks to it through `SettingsAction`s and reads [`RemoteStatus`];
//! everything that touches the network runs on the shared tokio runtime.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::identity::LocalIdentity;
use super::iroh_transport::IrohTransport;
use super::listener::{self, Gate, SWITCHED_OFF_REASON};
use super::pairing::{
    pair_with, ClientProgress, HostPairing, HostView, PairingCode, PairingSecret, RequestId,
};
use super::peers::TrustedPeer;
use super::registry::PeerRegistry;
use super::transport::{PeerId, PeerTransport, TransportConfig};
use crate::errors::{AlleleError, Result};
use crate::platform::SecretStore;
use crate::repositories::{JsonFilePeersRepo, PeersRepository};
use crate::settings::RemoteSettings;

/// Binds the endpoint. Production loads the Keychain identity and binds iroh;
/// tests bind an in-memory transport.
#[async_trait]
pub(crate) trait Binder: Send + Sync {
    async fn bind(&self, config: &TransportConfig) -> Result<Arc<dyn PeerTransport>>;
}

struct IrohBinder {
    secrets: Arc<dyn SecretStore>,
}

#[async_trait]
impl Binder for IrohBinder {
    async fn bind(&self, config: &TransportConfig) -> Result<Arc<dyn PeerTransport>> {
        // The Keychain read can block on a macOS permission prompt.
        let secrets = Arc::clone(&self.secrets);
        let identity =
            tokio::task::spawn_blocking(move || LocalIdentity::for_this_instance(&*secrets))
                .await
                .map_err(|e| AlleleError::Remote(format!("identity task failed: {e}")))??;
        Ok(Arc::new(IrohTransport::bind(&identity, config).await?))
    }
}

/// What [`RemoteService::answer`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The click answered a live request (not a stale prompt).
    pub answered: bool,
    /// This was the first pairing, so the switch went on; the caller saves
    /// the setting.
    pub switch_turned_on: bool,
}

/// What the client half of the Machines pane shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientView {
    Idle,
    Connecting,
    AwaitingApproval { confirmation: String },
    Paired { machine_name: String },
    Failed(String),
}

/// One trusted peer, as the Machines pane lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerRow {
    pub peer: TrustedPeer,
    /// Whether it has a connection open right now.
    pub connected: bool,
}

/// Everything the Machines pane needs, read in one go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteStatus {
    /// The endpoint is bound and the gate is accepting.
    pub listening: bool,
    pub peers: Vec<PeerRow>,
    pub host: HostView,
    pub client: ClientView,
    /// The last thing that went wrong outside a pairing (a failed bind).
    pub error: Option<String>,
    /// `peers.json` could not be read; nobody is trusted until it is fixed.
    pub peers_error: Option<String>,
}

#[derive(Default)]
struct Life {
    transport: Option<Arc<dyn PeerTransport>>,
    accept: Option<JoinHandle<()>>,
}

struct Inner {
    rt: Handle,
    registry: PeerRegistry,
    pairing: Arc<HostPairing>,
    allow: Arc<AtomicBool>,
    binder: Arc<dyn Binder>,
    config: Mutex<Option<TransportConfig>>,
    life: tokio::sync::Mutex<Life>,
    listening: AtomicBool,
    client: Mutex<ClientView>,
    client_busy: AtomicBool,
    error: Mutex<Option<String>>,
    /// Bumped whenever the endpoint might need binding or releasing.
    wake: watch::Sender<u64>,
    /// Bumped on every change the UI shows.
    ui: watch::Sender<u64>,
}

/// The remote endpoint's owner. Cheap to clone.
#[derive(Clone)]
pub struct RemoteService {
    inner: Arc<Inner>,
}

impl RemoteService {
    pub(crate) fn new(
        rt: Handle,
        repo: Arc<dyn PeersRepository>,
        binder: Arc<dyn Binder>,
        settings: &RemoteSettings,
    ) -> Self {
        let (config, error) = match TransportConfig::from_settings(settings) {
            Ok(c) => (Some(c), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let inner = Arc::new(Inner {
            rt,
            registry: PeerRegistry::load(repo),
            pairing: Arc::new(HostPairing::default()),
            allow: Arc::new(AtomicBool::new(settings.allow_connections)),
            binder,
            config: Mutex::new(config),
            life: tokio::sync::Mutex::new(Life::default()),
            listening: AtomicBool::new(false),
            client: Mutex::new(ClientView::Idle),
            client_busy: AtomicBool::new(false),
            error: Mutex::new(error),
            wake: watch::channel(0).0,
            ui: watch::channel(0).0,
        });
        inner.rt.spawn(supervise(Arc::downgrade(&inner)));
        Self { inner }
    }

    /// The app's service: `peers.json` under this data root, the Keychain
    /// identity, and iroh. Starts listening at once if the switch is on.
    pub(crate) fn production(secrets: Arc<dyn SecretStore>, settings: &RemoteSettings) -> Self {
        Self::new(
            crate::rt::handle(),
            Arc::new(JsonFilePeersRepo::production()),
            Arc::new(IrohBinder { secrets }),
            settings,
        )
    }

    /// A service that never touches the network or the disk: an in-memory
    /// peers list and a binder that always fails. For `AppState` fixtures.
    #[cfg(test)]
    pub(crate) fn offline(settings: &RemoteSettings) -> Self {
        struct Offline;
        #[async_trait]
        impl Binder for Offline {
            async fn bind(&self, _: &TransportConfig) -> Result<Arc<dyn PeerTransport>> {
                Err(AlleleError::Remote("offline".into()))
            }
        }
        Self::new(
            crate::rt::handle(),
            Arc::new(crate::repositories::fakes::InMemoryPeersRepository::default()),
            Arc::new(Offline),
            settings,
        )
    }

    pub fn registry(&self) -> &PeerRegistry {
        &self.inner.registry
    }

    /// Resolve on the next change the UI shows.
    pub async fn changed(&self) {
        let mut ui = self.inner.ui.subscribe();
        let mut peers = self.inner.registry.subscribe();
        let mut pairing = self.inner.pairing.subscribe();
        tokio::select! {
            _ = ui.changed() => {}
            _ = peers.changed() => {}
            _ = pairing.changed() => {}
        }
    }

    pub fn status(&self) -> RemoteStatus {
        let inner = &self.inner;
        let peers = inner
            .registry
            .list()
            .into_iter()
            .map(|peer| PeerRow {
                connected: inner.registry.live_count(peer.id) > 0,
                peer,
            })
            .collect();
        RemoteStatus {
            listening: inner.listening.load(Ordering::SeqCst),
            peers,
            host: inner.pairing.view(Instant::now()),
            client: lock(&inner.client).clone(),
            error: lock(&inner.error).clone(),
            peers_error: inner.registry.load_error(),
        }
    }

    pub fn allows_connections(&self) -> bool {
        self.inner.allow.load(Ordering::SeqCst)
    }

    /// The *Allow remote connections* switch. Off closes every remote
    /// connection now; the endpoint follows once no pairing needs it.
    pub fn set_allow(&self, on: bool) {
        // Store first, then sweep: `admit` reads the switch under the lock the
        // sweep takes, so a racing connection is either refused or swept.
        self.inner.allow.store(on, Ordering::SeqCst);
        if !on {
            self.inner.registry.close_all(SWITCHED_OFF_REASON);
        }
        tracing::info!("remote connections {}", if on { "allowed" } else { "off" });
        self.inner.wake_up();
    }

    /// New relay settings. Applies the next time the endpoint binds.
    pub fn set_settings(&self, settings: &RemoteSettings) {
        match TransportConfig::from_settings(settings) {
            Ok(c) => *lock(&self.inner.config) = Some(c),
            Err(e) => self.inner.set_error(Some(e.to_string())),
        }
    }

    /// *Pair a new machine*: bind if need be, then show a fresh code.
    pub fn start_pairing(&self, host_name: String) {
        let inner = Arc::clone(&self.inner);
        self.inner.rt.spawn(async move {
            let life = inner.life.lock().await;
            let transport = match inner.bound(life).await {
                Ok(t) => t,
                Err(e) => return inner.set_error(Some(e.to_string())),
            };
            let secret = match PairingSecret::generate() {
                Ok(s) => s,
                Err(e) => return inner.set_error(Some(e.to_string())),
            };
            let code = PairingCode {
                host: transport.local_id(),
                secret,
            };
            inner.set_error(None);
            inner.pairing.open(code, host_name, Instant::now());
        });
    }

    pub fn cancel_pairing(&self) {
        self.inner.pairing.cancel();
    }

    pub fn dismiss_pairing(&self) {
        self.inner.pairing.dismiss();
    }

    /// The host user's click on an approval prompt.
    ///
    /// Approving this machine's **first** pairing also asks for the *Allow
    /// remote connections* switch to go on — otherwise the new peer could not
    /// connect. Later pairings never touch the switch: if the user turned it
    /// off (say, to shut out a lost laptop), approving a new machine must not
    /// quietly let the old one back in.
    pub fn answer(&self, request: RequestId, approve: bool) -> Answer {
        let first = self.inner.registry.is_empty();
        let answered = self.inner.pairing.answer(request, approve);
        let turn_on = answered && approve && first && !self.allows_connections();
        if turn_on {
            self.set_allow(true);
        }
        Answer {
            answered,
            switch_turned_on: turn_on,
        }
    }

    /// *Add machine*: pair with the host named in `code_text`.
    pub fn add_machine(&self, code_text: &str, machine_name: String) {
        let code = match PairingCode::parse(code_text) {
            Ok(c) => c,
            Err(e) => {
                self.inner.set_client(ClientView::Failed(e.to_string()));
                return;
            }
        };
        if self.inner.client_busy.swap(true, Ordering::SeqCst) {
            return; // one at a time
        }
        self.inner.set_client(ClientView::Connecting);
        let inner = Arc::clone(&self.inner);
        self.inner.rt.spawn(async move {
            let result = async {
                let transport = inner.bound(inner.life.lock().await).await?;
                let progress = |p| {
                    inner.set_client(match p {
                        ClientProgress::Connecting => ClientView::Connecting,
                        ClientProgress::AwaitingApproval { confirmation } => {
                            ClientView::AwaitingApproval { confirmation }
                        }
                    })
                };
                pair_with(&*transport, &code, &machine_name, &inner.registry, progress).await
            }
            .await;
            inner.set_client(match result {
                Ok(host) => {
                    tracing::info!(
                        "paired with \u{201c}{}\u{201d} ({})",
                        host.machine_name,
                        host.id.short()
                    );
                    ClientView::Paired {
                        machine_name: host.machine_name,
                    }
                }
                Err(e) => ClientView::Failed(e.to_string()),
            });
            inner.client_busy.store(false, Ordering::SeqCst);
            inner.wake_up();
        });
    }

    pub fn dismiss_client(&self) {
        if !self.inner.client_busy.load(Ordering::SeqCst) {
            self.inner.set_client(ClientView::Idle);
        }
    }

    pub fn set_control(&self, peer: PeerId, allowed: bool) -> Result<()> {
        self.inner.registry.set_control(peer, allowed)?;
        tracing::info!(
            "{} {} control",
            peer.short(),
            if allowed { "granted" } else { "lost" }
        );
        Ok(())
    }

    /// *Remove*: revoke the peer and close its connections now.
    pub fn remove_peer(&self, peer: PeerId) -> Result<bool> {
        self.inner.registry.remove(peer)
    }

    /// Bind now and return the transport. Tests drive the service with it.
    #[cfg(test)]
    pub(crate) async fn transport(&self) -> Result<Arc<dyn PeerTransport>> {
        self.inner.bound(self.inner.life.lock().await).await
    }

    /// Run one reconcile pass and wait for it. Tests only.
    #[cfg(test)]
    pub(crate) async fn reconcile_now(&self) {
        self.inner.reconcile().await;
    }

    #[cfg(test)]
    pub(crate) fn pairing(&self) -> &Arc<HostPairing> {
        &self.inner.pairing
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Inner {
    fn wake_up(&self) {
        self.wake.send_modify(|n| *n = n.wrapping_add(1));
        self.bump_ui();
    }

    fn bump_ui(&self) {
        self.ui.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn set_error(&self, error: Option<String>) {
        if let Some(e) = &error {
            tracing::warn!("remote: {e}");
        }
        *lock(&self.error) = error;
        self.bump_ui();
    }

    fn set_client(&self, view: ClientView) {
        *lock(&self.client) = view;
        self.bump_ui();
    }

    fn needed(&self) -> bool {
        self.allow.load(Ordering::SeqCst)
            || self.client_busy.load(Ordering::SeqCst)
            || self.pairing.in_progress(Instant::now())
    }

    /// The bound transport, binding (and starting the gate) if there is none.
    /// Takes the lifecycle guard so a concurrent reconcile cannot release the
    /// endpoint between the bind and the caller's use of it.
    async fn bound(
        &self,
        mut life: tokio::sync::MutexGuard<'_, Life>,
    ) -> Result<Arc<dyn PeerTransport>> {
        if let Some(t) = &life.transport {
            return Ok(Arc::clone(t));
        }
        let config = lock(&self.config).clone().ok_or_else(|| {
            AlleleError::Remote(
                "the relay setting is invalid; fix it in Settings → Machines".into(),
            )
        })?;
        let transport = self.binder.bind(&config).await?;
        let gate = Gate::new(
            self.registry.clone(),
            Arc::clone(&self.pairing),
            Arc::clone(&self.allow),
        );
        life.accept = Some(self.rt.spawn(listener::run(Arc::clone(&transport), gate)));
        life.transport = Some(Arc::clone(&transport));
        self.listening.store(true, Ordering::SeqCst);
        tracing::info!("remote endpoint up as {}", transport.local_id().short());
        self.bump_ui();
        Ok(transport)
    }

    /// Bind if something needs the endpoint; release it if nothing does.
    async fn reconcile(&self) {
        let mut life = self.life.lock().await;
        let needed = self.needed();
        if needed && life.transport.is_none() {
            if let Err(e) = self.bound(life).await {
                self.set_error(Some(format!("could not start the remote endpoint: {e}")));
            }
        } else if !needed {
            if let Some(transport) = life.transport.take() {
                if let Some(task) = life.accept.take() {
                    task.abort();
                }
                self.registry.close_all(SWITCHED_OFF_REASON);
                transport.shutdown().await;
                self.listening.store(false, Ordering::SeqCst);
                tracing::info!("remote endpoint down");
                self.bump_ui();
            }
        }
    }
}

/// Reconcile whenever something changes, and when a pairing deadline passes.
/// Ends when the service is dropped.
async fn supervise(weak: std::sync::Weak<Inner>) {
    let Some(inner) = weak.upgrade() else { return };
    let mut wake = inner.wake.subscribe();
    let mut pairing = inner.pairing.subscribe();
    drop(inner);
    loop {
        let Some(inner) = weak.upgrade() else { return };
        inner.reconcile().await;
        let deadline = inner.pairing.next_deadline();
        drop(inner);
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = wake.changed() => if r.is_err() { return },
            r = pairing.changed() => if r.is_err() { return },
            _ = sleep => {
                if let Some(inner) = weak.upgrade() {
                    inner.pairing.in_progress(Instant::now()); // expires the code
                }
            }
        }
    }
}
