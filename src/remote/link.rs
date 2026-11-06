//! `PeerLink` — keeps one peer connected for as long as the link lives.
//!
//! Detecting a dead peer is the transport's heartbeat ([`Heartbeat`]): a silent
//! connection is closed after the idle timeout, which resolves
//! [`PeerConnection::closed`]. The link watches for that and redials.
//!
//! **The stale connection is always closed before a redial.** In the DEV-1158
//! spike a redial after the peer restarted waited 34 s behind the dead
//! connection; closing it first brought that to 4 s. It is the one rule here
//! that must not be "simplified" away.
//!
//! The link runs on whichever tokio runtime it is spawned onto, owns no UI
//! state, and reports through a [`tokio::sync::watch`] channel, so the GPUI
//! side can poll [`PeerLink::state`] or await changes on a background task.
//!
//! [`Heartbeat`]: super::transport::Heartbeat

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::transport::{PeerConnection, PeerId, PeerTransport};

/// How a link retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Wait this long after the first failed dial; doubles per failure.
    pub initial_backoff: Duration,
    /// Never wait longer than this between dials.
    pub max_backoff: Duration,
    /// Give up on a single dial after this long.
    pub dial_timeout: Duration,
    /// A connection that lasted at least this long was healthy: after it
    /// drops, redial at once and reset the backoff. One that dropped sooner
    /// counts as a failure, so a peer that accepts then hangs up (say, one
    /// that no longer trusts us) is not redialed in a tight loop.
    pub stable_after: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(5),
            dial_timeout: Duration::from_secs(10),
            stable_after: Duration::from_secs(5),
        }
    }
}

/// Where a link is, as last reported.
#[derive(Clone)]
pub enum LinkState {
    /// Dialing. `attempt` counts dials since the last healthy connection.
    Connecting { attempt: u32 },
    /// Connected; streams can be opened on the connection.
    Connected(Arc<dyn PeerConnection>),
    /// The last dial failed or the connection dropped early; the next dial
    /// starts after `retry_in`.
    Waiting {
        attempt: u32,
        retry_in: Duration,
        last_error: String,
    },
    /// Shut down, or dropped. Terminal.
    Stopped,
}

impl LinkState {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected(_))
    }
}

impl fmt::Debug for LinkState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connecting { attempt } => write!(f, "Connecting {{ attempt: {attempt} }}"),
            Self::Connected(c) => write!(f, "Connected({:?})", c.remote_id()),
            Self::Waiting {
                attempt,
                retry_in,
                last_error,
            } => write!(
                f,
                "Waiting {{ attempt: {attempt}, retry_in: {retry_in:?}, last_error: {last_error:?} }}"
            ),
            Self::Stopped => f.write_str("Stopped"),
        }
    }
}

enum Command {
    /// Drop the current connection (or the current wait) and dial again now.
    Reconnect,
    Shutdown,
}

/// A supervised connection to one peer. Dropping the link stops it and closes
/// the connection.
pub struct PeerLink {
    peer: PeerId,
    state: watch::Receiver<LinkState>,
    commands: mpsc::UnboundedSender<Command>,
    task: JoinHandle<()>,
}

impl PeerLink {
    /// Start keeping `peer` connected over `transport`, on runtime `rt`.
    pub fn spawn(
        rt: &tokio::runtime::Handle,
        transport: Arc<dyn PeerTransport>,
        peer: PeerId,
        policy: ReconnectPolicy,
    ) -> Self {
        let (state_tx, state) = watch::channel(LinkState::Connecting { attempt: 1 });
        let (commands, command_rx) = mpsc::unbounded_channel();
        let task = rt.spawn(supervise(transport, peer, policy, state_tx, command_rx));
        Self {
            peer,
            state,
            commands,
            task,
        }
    }

    pub fn peer(&self) -> PeerId {
        self.peer
    }

    /// The latest state.
    pub fn state(&self) -> LinkState {
        self.state.borrow().clone()
    }

    /// A receiver that wakes on every state change.
    pub fn subscribe(&self) -> watch::Receiver<LinkState> {
        self.state.clone()
    }

    /// The live connection, if there is one right now.
    pub fn connection(&self) -> Option<Arc<dyn PeerConnection>> {
        match &*self.state.borrow() {
            LinkState::Connected(c) => Some(Arc::clone(c)),
            _ => None,
        }
    }

    /// Wait until connected. `None` if the link stopped first.
    pub async fn wait_connected(&self) -> Option<Arc<dyn PeerConnection>> {
        let mut rx = self.state.clone();
        loop {
            match &*rx.borrow_and_update() {
                LinkState::Connected(c) => return Some(Arc::clone(c)),
                LinkState::Stopped => return None,
                _ => {}
            }
            rx.changed().await.ok()?;
        }
    }

    /// Close the current connection and redial now — for a caller that has
    /// evidence the connection is bad before the heartbeat does (a request
    /// timed out, the Mac woke from sleep). Also cuts short a backoff wait.
    pub fn reconnect_now(&self) {
        let _ = self.commands.send(Command::Reconnect);
    }

    /// Stop the link, close its connection, and wait for it to finish.
    pub async fn shutdown(self) {
        let _ = self.commands.send(Command::Shutdown);
        let _ = self.task.await;
    }
}

/// What ended a wait: time ran out, a redial was asked for, or stop.
enum Wake {
    Elapsed,
    Reconnect,
    Stop,
}

/// Read one command, mapping a closed channel (the `PeerLink` was dropped) to
/// `Stop`.
fn wake_for(cmd: Option<Command>) -> Wake {
    match cmd {
        Some(Command::Reconnect) => Wake::Reconnect,
        Some(Command::Shutdown) | None => Wake::Stop,
    }
}

async fn wait(duration: Duration, commands: &mut mpsc::UnboundedReceiver<Command>) -> Wake {
    tokio::select! {
        _ = tokio::time::sleep(duration) => Wake::Elapsed,
        cmd = commands.recv() => wake_for(cmd),
    }
}

async fn supervise(
    transport: Arc<dyn PeerTransport>,
    peer: PeerId,
    policy: ReconnectPolicy,
    state: watch::Sender<LinkState>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let mut attempt: u32 = 0;
    let mut backoff = policy.initial_backoff;

    'link: loop {
        attempt += 1;
        let _ = state.send(LinkState::Connecting { attempt });

        let dialed = tokio::select! {
            r = tokio::time::timeout(policy.dial_timeout, transport.dial(peer)) => r,
            cmd = commands.recv() => match wake_for(cmd) {
                Wake::Stop => break 'link,
                _ => continue 'link,
            },
        };

        let error = match dialed {
            Ok(Ok(conn)) => {
                let since = Instant::now();
                tracing::info!("remote: connected to {}", peer.short());
                let _ = state.send(LinkState::Connected(Arc::clone(&conn)));

                let wake = tokio::select! {
                    reason = conn.closed() => {
                        tracing::info!("remote: connection to {} closed: {reason}", peer.short());
                        Wake::Elapsed
                    }
                    cmd = commands.recv() => wake_for(cmd),
                };
                // Close our end before anything else — see the module docs.
                conn.close(match wake {
                    Wake::Stop => "shutdown",
                    _ => "reconnect",
                });
                match wake {
                    Wake::Stop => break 'link,
                    Wake::Reconnect => {
                        attempt = 0;
                        backoff = policy.initial_backoff;
                        continue 'link;
                    }
                    Wake::Elapsed if since.elapsed() >= policy.stable_after => {
                        attempt = 0;
                        backoff = policy.initial_backoff;
                        continue 'link;
                    }
                    Wake::Elapsed => "connection dropped right after it opened".to_string(),
                }
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!("dial timed out after {:?}", policy.dial_timeout),
        };

        tracing::debug!(
            "remote: link to {} waiting {backoff:?}: {error}",
            peer.short()
        );
        let _ = state.send(LinkState::Waiting {
            attempt,
            retry_in: backoff,
            last_error: error,
        });
        match wait(backoff, &mut commands).await {
            Wake::Stop => break 'link,
            // Fresh evidence the network is back (a wake, an interface
            // switch): start over as if from a healthy connection.
            Wake::Reconnect => {
                attempt = 0;
                backoff = policy.initial_backoff;
            }
            Wake::Elapsed => backoff = (backoff * 2).min(policy.max_backoff),
        }
    }

    let _ = state.send(LinkState::Stopped);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{AlleleError, Result};
    use crate::remote::transport::PeerStream;
    use crate::remote::LocalIdentity;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio::sync::Notify;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        Dial,
        Close(usize, String),
    }

    struct FakeConn {
        n: usize,
        peer: PeerId,
        closed: Notify,
        is_closed: Mutex<bool>,
        log: Arc<Mutex<Vec<Event>>>,
    }

    impl FakeConn {
        /// The peer vanishes: what the heartbeat's idle timeout reports.
        fn drop_from_remote(&self) {
            *self.is_closed.lock().unwrap() = true;
            self.closed.notify_waiters();
        }
    }

    #[async_trait]
    impl PeerConnection for FakeConn {
        fn remote_id(&self) -> PeerId {
            self.peer
        }
        async fn open_stream(&self) -> Result<PeerStream> {
            Err(AlleleError::Remote("fake".into()))
        }
        async fn accept_stream(&self) -> Result<PeerStream> {
            Err(AlleleError::Remote("fake".into()))
        }
        async fn closed(&self) -> String {
            let notified = self.closed.notified();
            if *self.is_closed.lock().unwrap() {
                return "closed".into();
            }
            notified.await;
            "closed".into()
        }
        fn is_closed(&self) -> bool {
            *self.is_closed.lock().unwrap()
        }
        fn close(&self, reason: &str) {
            self.log
                .lock()
                .unwrap()
                .push(Event::Close(self.n, reason.to_string()));
            *self.is_closed.lock().unwrap() = true;
            self.closed.notify_waiters();
        }
    }

    /// A transport whose dials follow a script: `true` succeeds, `false` fails.
    /// Once the script runs out, every dial succeeds.
    struct FakeTransport {
        me: PeerId,
        script: Mutex<VecDeque<bool>>,
        conns: Mutex<Vec<Arc<FakeConn>>>,
        log: Arc<Mutex<Vec<Event>>>,
    }

    impl FakeTransport {
        fn new(script: &[bool]) -> Arc<Self> {
            Arc::new(Self {
                me: LocalIdentity::ephemeral().id(),
                script: Mutex::new(script.iter().copied().collect()),
                conns: Mutex::default(),
                log: Arc::default(),
            })
        }
        fn conn(&self, n: usize) -> Arc<FakeConn> {
            Arc::clone(&self.conns.lock().unwrap()[n])
        }
        fn log(&self) -> Vec<Event> {
            self.log.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PeerTransport for FakeTransport {
        fn local_id(&self) -> PeerId {
            self.me
        }
        async fn dial(&self, peer: PeerId) -> Result<Arc<dyn PeerConnection>> {
            self.log.lock().unwrap().push(Event::Dial);
            if !self.script.lock().unwrap().pop_front().unwrap_or(true) {
                return Err(AlleleError::Remote("unreachable".into()));
            }
            let mut conns = self.conns.lock().unwrap();
            let conn = Arc::new(FakeConn {
                n: conns.len(),
                peer,
                closed: Notify::new(),
                is_closed: Mutex::new(false),
                log: Arc::clone(&self.log),
            });
            conns.push(Arc::clone(&conn));
            Ok(conn)
        }
        async fn accept(&self) -> Option<Arc<dyn PeerConnection>> {
            None
        }
        async fn network_changed(&self) {}
        async fn shutdown(&self) {}
    }

    fn peer() -> PeerId {
        LocalIdentity::ephemeral().id()
    }

    async fn wait_for(link: &PeerLink, pred: impl Fn(&LinkState) -> bool) -> LinkState {
        let mut rx = link.subscribe();
        loop {
            let s = rx.borrow_and_update().clone();
            if pred(&s) {
                return s;
            }
            rx.changed().await.unwrap();
        }
    }

    fn link(transport: &Arc<FakeTransport>, policy: ReconnectPolicy) -> PeerLink {
        PeerLink::spawn(
            &tokio::runtime::Handle::current(),
            Arc::clone(transport) as Arc<dyn PeerTransport>,
            peer(),
            policy,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn connects_and_reports_it() {
        let t = FakeTransport::new(&[]);
        let l = link(&t, ReconnectPolicy::default());
        let conn = l.wait_connected().await.unwrap();
        assert_eq!(conn.remote_id(), l.peer());
        assert!(l.connection().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_connection_is_closed_before_the_redial() {
        let t = FakeTransport::new(&[]);
        let l = link(&t, ReconnectPolicy::default());
        l.wait_connected().await.unwrap();
        // Healthy for a while, then the heartbeat gives up on the peer.
        tokio::time::sleep(Duration::from_secs(10)).await;
        t.conn(0).drop_from_remote();
        wait_for(&l, |_| t.conns.lock().unwrap().len() == 2).await;
        wait_for(&l, LinkState::is_connected).await;
        assert_eq!(
            t.log(),
            vec![
                Event::Dial,
                Event::Close(0, "reconnect".into()),
                Event::Dial
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_now_closes_the_live_connection_then_redials() {
        let t = FakeTransport::new(&[]);
        let l = link(&t, ReconnectPolicy::default());
        l.wait_connected().await.unwrap();
        l.reconnect_now();
        wait_for(&l, |_| t.conns.lock().unwrap().len() == 2).await;
        assert_eq!(
            t.log(),
            vec![
                Event::Dial,
                Event::Close(0, "reconnect".into()),
                Event::Dial
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_dials_back_off_doubling_up_to_the_cap() {
        let t = FakeTransport::new(&[false; 6]);
        let policy = ReconnectPolicy {
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(1),
            ..ReconnectPolicy::default()
        };
        let l = link(&t, policy);
        let mut waits = Vec::new();
        let mut rx = l.subscribe();
        while waits.len() < 6 {
            rx.changed().await.unwrap();
            if let LinkState::Waiting { retry_in, .. } = &*rx.borrow_and_update() {
                waits.push(*retry_in);
            }
        }
        let ms: Vec<u128> = waits.iter().map(Duration::as_millis).collect();
        assert_eq!(ms, vec![250, 500, 1000, 1000, 1000, 1000]);
        // And it gets there once the peer answers.
        l.wait_connected().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_now_during_backoff_restarts_from_the_initial_backoff() {
        let t = FakeTransport::new(&[false; 5]);
        let policy = ReconnectPolicy {
            max_backoff: Duration::from_secs(60),
            ..ReconnectPolicy::default()
        };
        let l = link(&t, policy);
        // Let the backoff grow past its starting value.
        wait_for(&l, |s| {
            matches!(s, LinkState::Waiting { retry_in, .. } if *retry_in >= Duration::from_secs(2))
        })
        .await;
        l.reconnect_now();
        let s = wait_for(&l, |s| {
            matches!(s, LinkState::Waiting { retry_in, .. } if *retry_in < Duration::from_secs(2))
        })
        .await;
        match s {
            LinkState::Waiting {
                attempt, retry_in, ..
            } => {
                assert_eq!(retry_in, policy.initial_backoff);
                assert_eq!(attempt, 1);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_drops_at_once_backs_off_instead_of_spinning() {
        let t = FakeTransport::new(&[]);
        let l = link(&t, ReconnectPolicy::default());
        l.wait_connected().await.unwrap();
        t.conn(0).drop_from_remote();
        let s = wait_for(&l, |s| matches!(s, LinkState::Waiting { .. })).await;
        match s {
            LinkState::Waiting { retry_in, .. } => {
                assert_eq!(retry_in, ReconnectPolicy::default().initial_backoff)
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_closes_the_connection_and_stops() {
        let t = FakeTransport::new(&[]);
        let l = link(&t, ReconnectPolicy::default());
        l.wait_connected().await.unwrap();
        let rx = l.subscribe();
        l.shutdown().await;
        assert!(matches!(*rx.borrow(), LinkState::Stopped));
        assert_eq!(
            t.log(),
            vec![Event::Dial, Event::Close(0, "shutdown".into())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_link_stops_it() {
        let t = FakeTransport::new(&[]);
        let l = link(&t, ReconnectPolicy::default());
        l.wait_connected().await.unwrap();
        let mut rx = l.subscribe();
        drop(l);
        while !matches!(*rx.borrow_and_update(), LinkState::Stopped) {
            rx.changed().await.unwrap();
        }
        assert_eq!(t.log().last(), Some(&Event::Close(0, "shutdown".into())));
    }
}
