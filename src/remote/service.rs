//! `RemoteService`: the one owner of this machine's remote endpoint (DEV-1160).
//!
//! It binds the transport only while something needs it, and shuts it down
//! when nothing does:
//!
//! ```text
//!   needed = Allow remote connections
//!          ∨ a pairing code is showing (or its request is in flight)
//!          ∨ this Mac is pairing with another (Add machine)
//!          ∨ there are paired machines to follow (DEV-1162)
//! ```
//!
//! Following is outbound: while the endpoint is up, a [`Follower`] per trusted
//! peer keeps a link to it and its status feed in [`HostFeeds`]. That is
//! independent of the switch, which only decides whether *others* may connect
//! *here* — so a laptop with the switch off still sees its desktop.
//!
//! So the first pairing works with the switch still off, and turning the
//! switch off closes every remote connection and — once no pairing is in
//! progress — the endpoint itself.
//!
//! The UI talks to it through `SettingsAction`s and reads [`RemoteStatus`];
//! everything that touches the network runs on the shared tokio runtime.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::control::{ControlContext, HostName, RemoteDispatch};
use super::follow::{Follower, HostFeeds};
use super::identity::LocalIdentity;
use super::iroh_transport::IrohTransport;
use super::link::ReconnectPolicy;
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
use crate::status_feed::StatusFeed;

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
    /// "Does this number match the other Mac?" — Confirm / Cancel.
    AwaitingConfirmation {
        confirmation: String,
    },
    /// Confirmed here; the other Mac's user is still deciding.
    Confirmed {
        confirmation: String,
    },
    Paired {
        machine_name: String,
    },
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
    /// One per trusted peer while the endpoint is up.
    followers: HashMap<PeerId, Follower>,
}

struct Inner {
    rt: Handle,
    registry: PeerRegistry,
    pairing: Arc<HostPairing>,
    allow: Arc<AtomicBool>,
    binder: Arc<dyn Binder>,
    /// What admitted peers' requests are answered by (DEV-1161).
    control: ControlContext,
    feeds: HostFeeds,
    follow_policy: ReconnectPolicy,
    config: Mutex<Option<TransportConfig>>,
    life: tokio::sync::Mutex<Life>,
    listening: AtomicBool,
    client: Mutex<ClientView>,
    client_busy: AtomicBool,
    /// The running *Add machine*'s Confirm / Cancel channel.
    client_confirm: Mutex<Option<oneshot::Sender<bool>>>,
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
        control: ControlContext,
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
            control,
            feeds: HostFeeds::default(),
            follow_policy: ReconnectPolicy::default(),
            config: Mutex::new(config),
            life: tokio::sync::Mutex::new(Life::default()),
            listening: AtomicBool::new(false),
            client: Mutex::new(ClientView::Idle),
            client_busy: AtomicBool::new(false),
            client_confirm: Mutex::new(None),
            error: Mutex::new(error),
            wake: watch::channel(0).0,
            ui: watch::channel(0).0,
        });
        inner.rt.spawn(supervise(Arc::downgrade(&inner)));
        Self { inner }
    }

    /// The app's service: `peers.json` under this data root, the Keychain
    /// identity, and iroh. Starts listening at once if the switch is on.
    ///
    /// `dispatch` answers paired machines' requests — in the app, the same
    /// queue `control.sock` feeds. `computer_name` is this Mac's name, looked
    /// up only when a peer first needs it.
    pub(crate) fn production(
        secrets: Arc<dyn SecretStore>,
        settings: &RemoteSettings,
        dispatch: Arc<dyn RemoteDispatch>,
        status_feed: StatusFeed,
        computer_name: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        let control = ControlContext {
            dispatch,
            host_name: HostName::new(settings.machine_name.clone(), computer_name),
            status_feed,
        };
        Self::new(
            crate::rt::handle(),
            Arc::new(JsonFilePeersRepo::production()),
            Arc::new(IrohBinder { secrets }),
            settings,
            control,
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
            ControlContext::unavailable(),
        )
    }

    /// The paired machines, in pairing order. Cheaper than [`Self::status`]
    /// for a caller that draws every frame (the MACHINES sidebar, DEV-1163).
    pub fn paired(&self) -> Vec<TrustedPeer> {
        self.inner.registry.list()
    }

    #[cfg(test)]
    pub fn registry(&self) -> &PeerRegistry {
        &self.inner.registry
    }

    /// Resolve on the next change the UI shows.
    #[allow(dead_code)] // for a UI that waits rather than polls `status`
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

    /// What every paired host last told us about its sessions (DEV-1162).
    /// Read by the Machines sidebar (DEV-1163) and remote attention
    /// (DEV-1164).
    pub fn hosts(&self) -> &HostFeeds {
        &self.inner.feeds
    }

    /// The name a trusted peer gave when it paired, or `None` if it is not
    /// (or no longer) trusted. Display only; peer-chosen text.
    pub fn peer_name(&self, id: PeerId) -> Option<String> {
        self.inner.registry.get(id).map(|p| p.machine_name)
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

    /// New relay or machine-name settings. The name applies to the next
    /// control stream; the relay the next time the endpoint binds.
    pub fn set_settings(&self, settings: &RemoteSettings) {
        self.inner
            .control
            .host_name
            .set(settings.machine_name.clone());
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
        let (confirm_tx, confirm) = oneshot::channel();
        *lock(&self.inner.client_confirm) = Some(confirm_tx);
        let inner = Arc::clone(&self.inner);
        self.inner.rt.spawn(async move {
            let result = async {
                let transport = inner.bound(inner.life.lock().await).await?;
                let progress = |p| {
                    inner.set_client(match p {
                        ClientProgress::Connecting => ClientView::Connecting,
                        ClientProgress::AwaitingConfirmation { confirmation } => {
                            ClientView::AwaitingConfirmation { confirmation }
                        }
                        ClientProgress::Confirmed { confirmation } => {
                            ClientView::Confirmed { confirmation }
                        }
                    })
                };
                pair_with(
                    &*transport,
                    &code,
                    &machine_name,
                    &inner.registry,
                    confirm,
                    progress,
                )
                .await
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
            lock(&inner.client_confirm).take();
            inner.client_busy.store(false, Ordering::SeqCst);
            inner.wake_up();
        });
    }

    /// This Mac's user's answer to "does the number match?" during *Add
    /// machine*. Cancel ends the pairing with nothing saved on either Mac.
    pub fn confirm_client(&self, confirmed: bool) {
        if let Some(tx) = lock(&self.inner.client_confirm).take() {
            let _ = tx.send(confirmed);
        }
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
            || !self.registry.is_empty()
    }

    /// One follower per trusted peer: start the new, stop the removed.
    async fn sync_followers(&self, life: &mut Life) {
        let Some(transport) = life.transport.clone() else {
            return;
        };
        let trusted: Vec<PeerId> = self.registry.list().into_iter().map(|p| p.id).collect();
        let gone: Vec<PeerId> = life
            .followers
            .keys()
            .filter(|id| !trusted.contains(id))
            .copied()
            .collect();
        for id in gone {
            if let Some(f) = life.followers.remove(&id) {
                f.stop().await;
            }
            self.feeds.forget(id);
        }
        for id in trusted {
            life.followers.entry(id).or_insert_with(|| {
                Follower::spawn(
                    &self.rt,
                    Arc::clone(&transport),
                    id,
                    self.follow_policy,
                    self.feeds.clone(),
                    Arc::clone(&self.control.host_name),
                )
            });
        }
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
            self.control.clone(),
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
                return;
            }
            life = self.life.lock().await;
        }
        if needed {
            self.sync_followers(&mut life).await;
        } else {
            // Unpairing the last peer is one way to get here: a peer that is
            // no longer trusted is forgotten, not left listed as unreachable.
            let trusted = self.registry.list();
            for (id, f) in life.followers.drain() {
                f.stop().await;
                if trusted.iter().any(|p| p.id == id) {
                    self.feeds.lost(id);
                } else {
                    self.feeds.forget(id);
                }
            }
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
    // A peer paired or removed changes who to follow, and whether anything
    // needs the endpoint at all.
    let mut peers = inner.registry.subscribe();
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
            r = peers.changed() => if r.is_err() { return },
            _ = sleep => {
                if let Some(inner) = weak.upgrade() {
                    inner.pairing.in_progress(Instant::now()); // expires the code
                }
            }
        }
    }
}
