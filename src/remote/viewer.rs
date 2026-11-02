//! A remote session held open in a tab (DEV-1167): the client side that keeps
//! one host session mirrored for as long as its tab exists.
//!
//! ```text
//!   no link ──► Unreachable ◄──────────────── link dropped
//!      │ link up                                   ▲
//!      ▼                                           │
//!   Attaching ──attach──► Live ──stream ended──► (link still up?)
//!      │ refused                                   │ yes
//!      ▼                                           ▼
//!   Unavailable(msg) ◄──────────────────────── Unavailable
//!      └── retried every RETRY, and at once on a new link
//! ```
//!
//! The last grid is kept through every state but the first, so a tab whose
//! host drops keeps showing what it last saw under an overlay. Reattaching is
//! automatic: each new link (a reconnect) starts a fresh attach, and the new
//! grid replaces the old one only once its first frame has arrived.
//!
//! Dropping a [`RemoteScreen`] drops its attachment, which closes the screen
//! stream; the host takes that as a detach. It never touches the session.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use alacritty_terminal::sync::FairMutex;
use tokio::sync::watch;
use tokio::time::sleep;

use super::follow::ScreenLink;
use super::screen::{AbortOnDrop, Attachment};
use crate::terminal::mirror::MirroredGrid;

/// How long to wait before attaching again after the host refused or ended
/// an attach while the connection stayed up.
const RETRY: Duration = Duration::from_secs(3);

/// After a screen stream ends, how long to wait for the link to drop too
/// before deciding the host ended it on purpose. A dying connection ends its
/// streams a moment before the follower notices.
const SETTLE: Duration = Duration::from_millis(500);

/// What a remote tab is showing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScreenPhase {
    /// Asking the host for the screen.
    Attaching,
    /// Mirroring.
    Live,
    /// No connection to the host; showing the last frame, if any.
    Unreachable,
    /// Connected, but the host would not mirror this session, or stopped:
    /// its message. Retried.
    Unavailable(String),
}

struct Shared {
    phase: ScreenPhase,
    /// The newest grid with at least one frame in it.
    grid: Option<Arc<FairMutex<MirroredGrid>>>,
    attachment: Option<Arc<Attachment>>,
}

/// One host session, kept mirrored while this lives.
pub struct RemoteScreen {
    shared: Arc<Mutex<Shared>>,
    ticks: watch::Receiver<u64>,
    _task: AbortOnDrop,
}

impl RemoteScreen {
    /// Mirror `session_id` over whatever link `links` holds, now and after
    /// every reconnect. `links` is [`HostFeeds::screen_link`](super::follow::HostFeeds::screen_link).
    pub fn open(
        rt: &tokio::runtime::Handle,
        links: watch::Receiver<Option<Arc<ScreenLink>>>,
        session_id: &str,
    ) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            phase: ScreenPhase::Attaching,
            grid: None,
            attachment: None,
        }));
        let (tick_tx, ticks) = watch::channel(0u64);
        let task = rt.spawn(run(
            links,
            session_id.to_string(),
            Arc::clone(&shared),
            tick_tx,
        ));
        Self {
            shared,
            ticks,
            _task: AbortOnDrop(task),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn phase(&self) -> ScreenPhase {
        self.lock().phase.clone()
    }

    /// The grid to draw: the live one, or the last one seen. `None` until the
    /// first frame arrives.
    pub fn grid(&self) -> Option<Arc<FairMutex<MirroredGrid>>> {
        self.lock().grid.clone()
    }

    /// Ticks on every applied frame and every phase change: redraw.
    pub fn ticks(&self) -> watch::Receiver<u64> {
        self.ticks.clone()
    }

    /// Type into the host session (DEV-1166): exactly the bytes local typing
    /// would write. Snaps the view back to the bottom first, as local typing
    /// does. `Err` when nothing left this Mac (not live, link not keeping up).
    pub fn send_input(&self, bytes: &[u8]) -> crate::errors::Result<()> {
        let attachment = self.live_attachment()?;
        attachment.send_input(bytes)
    }

    /// Paste into the host session as one frame (≤ 64 KiB, never split).
    pub fn send_paste(&self, text: &str) -> crate::errors::Result<()> {
        let attachment = self.live_attachment()?;
        attachment.send_paste(text)
    }

    /// The host's last refusal of this tab's input, once.
    pub fn take_refusal(&self) -> Option<String> {
        let attachment = self.lock().attachment.clone();
        attachment.and_then(|a| a.take_refusal())
    }

    fn live_attachment(&self) -> crate::errors::Result<Arc<Attachment>> {
        let attachment = self.lock().attachment.clone().ok_or_else(|| {
            crate::errors::AlleleError::Remote("not connected to the other Mac".into())
        })?;
        let offset = {
            use crate::terminal::grid_source::GridView as _;
            attachment.grid().lock().display_offset()
        };
        if offset > 0 {
            attachment.scroll_display(-(offset as i32));
        }
        Ok(attachment)
    }

    /// Scroll into the host's scrollback; a no-op while not live.
    pub fn scroll_display(&self, delta: i32) {
        let attachment = self.lock().attachment.clone();
        if let Some(attachment) = attachment {
            attachment.scroll_display(delta);
        }
    }
}

fn tick(ticks: &watch::Sender<u64>) {
    ticks.send_modify(|n| *n = n.wrapping_add(1));
}

fn set_phase(shared: &Mutex<Shared>, ticks: &watch::Sender<u64>, phase: ScreenPhase) {
    let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
    if s.phase != phase {
        s.phase = phase;
        drop(s);
        tick(ticks);
    }
}

/// How one attach on one link ended.
enum Ended {
    /// A different link (or none) is current: start over on it.
    LinkChanged,
    /// The host refused, or ended the stream while the link stayed up.
    Host(String),
}

async fn run(
    mut links: watch::Receiver<Option<Arc<ScreenLink>>>,
    session_id: String,
    shared: Arc<Mutex<Shared>>,
    ticks: watch::Sender<u64>,
) {
    loop {
        let link = links.borrow_and_update().clone();
        let Some(link) = link else {
            set_phase(&shared, &ticks, ScreenPhase::Unreachable);
            if links.changed().await.is_err() {
                return;
            }
            continue;
        };
        match attach_once(&link, &session_id, &shared, &ticks, &mut links).await {
            Ended::LinkChanged => {}
            Ended::Host(message) => {
                set_phase(&shared, &ticks, ScreenPhase::Unavailable(message));
                // Try again later, or at once on a new link.
                tokio::select! {
                    _ = sleep(RETRY) => {}
                    changed = links.changed() => if changed.is_err() { return },
                }
            }
        }
    }
}

async fn attach_once(
    link: &Arc<ScreenLink>,
    session_id: &str,
    shared: &Mutex<Shared>,
    ticks: &watch::Sender<u64>,
    links: &mut watch::Receiver<Option<Arc<ScreenLink>>>,
) -> Ended {
    // Unavailable stays on screen while a retry is in flight; it only
    // clears once the retry works.
    if !matches!(shared_phase(shared), ScreenPhase::Unavailable(_)) {
        set_phase(shared, ticks, ScreenPhase::Attaching);
    }
    let Some(pending) = link.attach(session_id).await else {
        return wait_for_new_link(links).await;
    };
    let attachment = tokio::select! {
        answer = pending.wait() => match answer {
            Ok(attachment) => Arc::new(attachment),
            Err(e) => return Ended::Host(e.to_string()),
        },
        changed = links.changed() => {
            let _ = changed;
            return Ended::LinkChanged;
        }
    };

    let mut frames = attachment.changed();
    // Keep the old grid on screen until the new one has something in it.
    let first = tokio::select! {
        first = frames.changed() => first.is_ok(),
        _ = links.changed() => return Ended::LinkChanged,
    };
    if !first {
        return settle(links).await;
    }
    {
        let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
        s.grid = Some(attachment.grid());
        s.attachment = Some(Arc::clone(&attachment));
        s.phase = ScreenPhase::Live;
    }
    tick(ticks);

    let ended = loop {
        tokio::select! {
            frame = frames.changed() => match frame {
                Ok(()) => tick(ticks),
                Err(_) => break settle(links).await,
            },
            _ = links.changed() => break Ended::LinkChanged,
        }
    };
    shared
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .attachment = None;
    ended
}

fn shared_phase(shared: &Mutex<Shared>) -> ScreenPhase {
    shared
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .phase
        .clone()
}

/// The stream ended. If the link follows within [`SETTLE`], the connection
/// died; otherwise the host ended it (session stopped, `view` revoked).
async fn settle(links: &mut watch::Receiver<Option<Arc<ScreenLink>>>) -> Ended {
    tokio::select! {
        _ = links.changed() => Ended::LinkChanged,
        _ = sleep(SETTLE) => Ended::Host("The other Mac stopped showing this session".into()),
    }
}

async fn wait_for_new_link(links: &mut watch::Receiver<Option<Arc<ScreenLink>>>) -> Ended {
    let _ = links.changed().await;
    Ended::LinkChanged
}
