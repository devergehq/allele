//! The sidebar's MACHINES section, as plain data (DEV-1163).
//!
//! Each paired host becomes a [`MachineView`]: its name, how reachable it is,
//! how many of its sessions need the user, and its sessions grouped by
//! project in the host's own sidebar order. `render.rs` draws these; keeping
//! the shaping here means the rules are tested without a window.
//!
//! The paired list (the trusted registry) decides which hosts appear, not the
//! feeds: a host we have paired with but not yet heard from still gets a row,
//! shown as connecting.

use std::time::{Duration, SystemTime};

use crate::remote::peers::TrustedPeer;
use crate::remote::PeerId;
use crate::session::{AttentionKind, SessionStatus};
use crate::status_feed::client::Reachability;
use crate::status_feed::model::{FeedAttention, FeedStatus};
use crate::status_feed::{RemoteHostState, SessionEntry};

/// What the host row says about the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MachineLink {
    Connected,
    /// Not heard from yet since launch (or since pairing).
    Connecting,
    /// Dropped. `last_seen` is when it was last reachable.
    Unreachable {
        last_seen: SystemTime,
    },
}

impl MachineLink {
    fn from_reachability(r: Reachability) -> Self {
        match r {
            Reachability::Connected => Self::Connected,
            Reachability::Connecting => Self::Connecting,
            Reachability::Unreachable { since } => Self::Unreachable { last_seen: since },
        }
    }

    /// The host row's status text, e.g. "last seen 4m ago".
    pub(crate) fn describe(self, now: SystemTime) -> String {
        match self {
            Self::Connected => "connected".to_string(),
            Self::Connecting => "connecting…".to_string(),
            Self::Unreachable { last_seen } => {
                let ago = now.duration_since(last_seen).unwrap_or(Duration::ZERO);
                format!("unreachable · last seen {}", ago_text(ago))
            }
        }
    }
}

/// "just now", "4m ago", "3h ago", "2d ago". Coarse on purpose: the row only
/// has to say whether the Mac dropped a moment ago or went to sleep last night.
pub(crate) fn ago_text(ago: Duration) -> String {
    let secs = ago.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// The fold key for one project on one host (DEV-1235). The same project
/// name on two hosts is two headings, so the peer is part of the key.
pub(crate) fn remote_project_key(peer: PeerId, project: &str) -> String {
    format!("{peer}/{project}")
}

/// One remote session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteSessionView {
    pub id: String,
    pub label: String,
    pub group: Option<String>,
    /// Mapped back onto the local enums so the row reuses their icon and
    /// colour: a remote session is styled exactly as a local one would be.
    pub status: SessionStatus,
    pub attention: Option<AttentionKind>,
}

/// A host's sessions in one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteProjectView {
    pub name: String,
    pub sessions: Vec<RemoteSessionView>,
}

/// One paired host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineView {
    pub id: PeerId,
    pub name: String,
    pub link: MachineLink,
    pub awaiting_input: usize,
    pub projects: Vec<RemoteProjectView>,
}

impl MachineView {
    /// Last-known sessions of a host we cannot reach: listed, but greyed.
    pub(crate) fn is_stale(&self) -> bool {
        matches!(self.link, MachineLink::Unreachable { .. })
    }
}

/// Build the section from the paired hosts (in pairing order) and a lookup
/// of each host's feed. Empty when nothing is paired, which hides the section.
pub(crate) fn machine_views(
    peers: &[TrustedPeer],
    feed: impl Fn(PeerId) -> Option<RemoteHostState>,
) -> Vec<MachineView> {
    peers
        .iter()
        .map(|peer| {
            let state = feed(peer.id).unwrap_or_default();
            MachineView {
                id: peer.id,
                name: peer.machine_name.clone(),
                link: MachineLink::from_reachability(state.reachability()),
                awaiting_input: state.awaiting_input().count(),
                projects: group_by_project(state.sessions()),
            }
        })
        .collect()
}

/// Group sessions by project, keeping the host's order: projects in the order
/// their first session appears, sessions in feed order within each.
fn group_by_project(sessions: &[SessionEntry]) -> Vec<RemoteProjectView> {
    let mut projects: Vec<(String, RemoteProjectView)> = Vec::new();
    for s in sessions {
        let view = RemoteSessionView {
            id: s.id.clone(),
            label: s.label.clone(),
            group: s.group.clone(),
            status: local_status(s.status),
            attention: s.attention.map(local_attention),
        };
        match projects.iter_mut().find(|(id, _)| *id == s.project_id) {
            Some((_, p)) => p.sessions.push(view),
            None => projects.push((
                s.project_id.clone(),
                RemoteProjectView {
                    name: s.project_name.clone(),
                    sessions: vec![view],
                },
            )),
        }
    }
    projects.into_iter().map(|(_, p)| p).collect()
}

fn local_status(s: FeedStatus) -> SessionStatus {
    match s {
        FeedStatus::Running => SessionStatus::Running,
        FeedStatus::Idle => SessionStatus::Idle,
        FeedStatus::Done => SessionStatus::Done,
        FeedStatus::Suspended => SessionStatus::Suspended,
        FeedStatus::AwaitingInput => SessionStatus::AwaitingInput,
        FeedStatus::ResponseReady => SessionStatus::ResponseReady,
    }
}

fn local_attention(a: FeedAttention) -> AttentionKind {
    match a {
        FeedAttention::Permission => AttentionKind::Permission,
        FeedAttention::Question => AttentionKind::Question,
        FeedAttention::Idle => AttentionKind::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_feed::StatusSnapshot;

    fn peer(name: &str) -> TrustedPeer {
        TrustedPeer::newly_paired(crate::remote::LocalIdentity::ephemeral().id(), name)
    }

    fn entry(id: &str, project: &str, status: FeedStatus) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            label: format!("label {id}"),
            project_id: format!("p-{project}"),
            project_name: project.into(),
            group: None,
            status,
            attention: (status == FeedStatus::AwaitingInput).then_some(FeedAttention::Question),
            last_activity_ms: 0,
        }
    }

    fn connected(sessions: Vec<SessionEntry>) -> RemoteHostState {
        let mut h = RemoteHostState::new();
        h.apply_snapshot(StatusSnapshot { seq: 0, sessions }, SystemTime::now());
        h
    }

    #[test]
    fn nothing_paired_means_no_section() {
        assert!(machine_views(&[], |_| None).is_empty());
    }

    #[test]
    fn every_paired_host_gets_a_row_in_pairing_order() {
        let peers = [peer("Studio"), peer("Laptop")];
        let views = machine_views(&peers, |_| None);
        let names: Vec<_> = views.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["Studio", "Laptop"]);
    }

    #[test]
    fn a_host_not_yet_heard_from_is_connecting_and_empty() {
        let views = machine_views(&[peer("Studio")], |_| None);
        assert_eq!(views[0].link, MachineLink::Connecting);
        assert!(views[0].projects.is_empty());
        assert!(!views[0].is_stale());
    }

    #[test]
    fn a_connected_host_reads_connected() {
        let views = machine_views(&[peer("Studio")], |_| Some(connected(vec![])));
        assert_eq!(views[0].link, MachineLink::Connected);
        assert_eq!(views[0].link.describe(SystemTime::now()), "connected");
    }

    #[test]
    fn sessions_group_by_project_in_host_order() {
        let host = connected(vec![
            entry("a", "web", FeedStatus::Running),
            entry("b", "api", FeedStatus::Idle),
            entry("c", "web", FeedStatus::Done),
        ]);
        let views = machine_views(&[peer("Studio")], |_| Some(host.clone()));
        let projects = &views[0].projects;
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "web");
        let web: Vec<_> = projects[0].sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(web, ["a", "c"]);
        assert_eq!(projects[1].name, "api");
    }

    #[test]
    fn statuses_map_onto_the_local_styling() {
        let host = connected(vec![entry("a", "web", FeedStatus::AwaitingInput)]);
        let views = machine_views(&[peer("Studio")], |_| Some(host.clone()));
        let s = &views[0].projects[0].sessions[0];
        assert_eq!(s.status, SessionStatus::AwaitingInput);
        assert_eq!(s.attention, Some(AttentionKind::Question));
    }

    #[test]
    fn the_host_row_counts_sessions_awaiting_input() {
        let host = connected(vec![
            entry("a", "web", FeedStatus::AwaitingInput),
            entry("b", "web", FeedStatus::Running),
            entry("c", "api", FeedStatus::AwaitingInput),
        ]);
        let views = machine_views(&[peer("Studio")], |_| Some(host.clone()));
        assert_eq!(views[0].awaiting_input, 2);
    }

    #[test]
    fn an_unreachable_host_keeps_its_sessions_and_says_when_it_was_seen() {
        let mut host = connected(vec![entry("a", "web", FeedStatus::Running)]);
        let dropped = SystemTime::now();
        host.connection_lost(dropped);
        let views = machine_views(&[peer("Studio")], |_| Some(host.clone()));
        let v = &views[0];
        assert!(v.is_stale());
        assert_eq!(v.projects[0].sessions.len(), 1);
        let later = dropped + Duration::from_secs(5 * 60);
        assert_eq!(v.link.describe(later), "unreachable · last seen 5m ago");
    }

    #[test]
    fn ago_text_is_coarse() {
        assert_eq!(ago_text(Duration::from_secs(10)), "just now");
        assert_eq!(ago_text(Duration::from_secs(150)), "2m ago");
        assert_eq!(ago_text(Duration::from_secs(3 * 3600 + 5)), "3h ago");
        assert_eq!(ago_text(Duration::from_secs(2 * 86_400)), "2d ago");
    }

    #[test]
    fn a_clock_that_ran_backwards_reads_just_now() {
        let now = SystemTime::now();
        let link = MachineLink::Unreachable {
            last_seen: now + Duration::from_secs(30),
        };
        assert_eq!(link.describe(now), "unreachable · last seen just now");
    }
}
