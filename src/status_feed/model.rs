//! Serialisable status-feed types. Plain data: these are what crosses the
//! wire, so they mirror the session enums rather than reuse them — a change
//! to `SessionStatus` must not silently change the protocol.

use serde::{Deserialize, Serialize};

/// A session's status as the feed reports it. Mirrors
/// [`crate::session::SessionStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FeedStatus {
    Running,
    Idle,
    Done,
    Suspended,
    AwaitingInput,
    ResponseReady,
}

/// Why a session in `AwaitingInput` is waiting. Mirrors
/// [`crate::session::AttentionKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FeedAttention {
    Permission,
    Question,
    Idle,
}

/// One host session, as a client sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionEntry {
    /// The session's stable workspace id (`Session::id`).
    pub id: String,
    pub label: String,
    pub project_id: String,
    pub project_name: String,
    /// Sidebar group within the project, if any (DEV-853).
    #[serde(default)]
    pub group: Option<String>,
    pub status: FeedStatus,
    /// Set only while `status` is `AwaitingInput`.
    #[serde(default)]
    pub attention: Option<FeedAttention>,
    /// Last observed activity, Unix milliseconds.
    pub last_activity_ms: u64,
}

/// Every session on the host, in host sidebar order, as of delta `seq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StatusSnapshot {
    /// The sequence number of the last change already folded into
    /// `sessions`. The next delta on the same subscription is `seq + 1`.
    pub seq: u64,
    pub sessions: Vec<SessionEntry>,
}

/// One change, numbered. Sequence numbers are per host process and strictly
/// consecutive, so a client can tell a duplicate from a gap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StatusDelta {
    pub seq: u64,
    pub change: StatusChange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum StatusChange {
    /// A session appeared — or changed project or group, in which case this
    /// replaces the entry in place. Clients treat it as an upsert.
    Added {
        session: SessionEntry,
    },
    Removed {
        id: String,
    },
    Renamed {
        id: String,
        label: String,
    },
    StatusChanged {
        id: String,
        status: FeedStatus,
        attention: Option<FeedAttention>,
        last_activity_ms: u64,
    },
}

impl StatusChange {
    /// The session this change is about.
    pub fn session_id(&self) -> &str {
        match self {
            StatusChange::Added { session } => &session.id,
            StatusChange::Removed { id }
            | StatusChange::Renamed { id, .. }
            | StatusChange::StatusChanged { id, .. } => id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_tags_are_snake_case() {
        let delta = StatusDelta {
            seq: 7,
            change: StatusChange::StatusChanged {
                id: "s1".into(),
                status: FeedStatus::AwaitingInput,
                attention: Some(FeedAttention::Permission),
                last_activity_ms: 42,
            },
        };
        let json = serde_json::to_value(&delta).expect("serialise");
        assert_eq!(json["change"]["kind"], "status_changed");
        assert_eq!(json["change"]["status"], "awaiting_input");
        assert_eq!(json["change"]["attention"], "permission");
        let back: StatusDelta = serde_json::from_value(json).expect("round trip");
        assert_eq!(back, delta);
    }
}
