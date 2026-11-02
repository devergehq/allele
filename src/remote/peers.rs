//! The trusted-peers list: which machines may connect, and what each may do
//! (DEV-1160, proposal §3.2–3.3).
//!
//! Stored at `~/.allele/peers.json`, owner-only (0600). The file is the whole
//! trust decision: a connection whose authenticated [`PeerId`] is not listed
//! here is refused before anything it sends is read (see `remote::listener`).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::transport::PeerId;

/// What a paired machine may do on this one.
///
/// `view` is status and screen; `control` is input, create, interrupt and
/// discard. A newly paired machine gets [`PeerPermissions::NEW_PEER`]: view
/// only. `control` is granted by a separate, explicit toggle on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerPermissions {
    #[serde(default)]
    pub view: bool,
    /// Defaults to `false` when absent, so a hand-edited entry that forgets
    /// the field fails closed.
    #[serde(default)]
    pub control: bool,
}

impl PeerPermissions {
    /// What pairing grants: view, never control.
    pub const NEW_PEER: Self = Self {
        view: true,
        control: false,
    };
}

/// One machine this one trusts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    /// The peer's public key — what the transport authenticates.
    pub id: PeerId,
    /// The name the peer gave when it paired, sanitised. Display only: it is
    /// never used to decide anything.
    pub machine_name: String,
    pub permissions: PeerPermissions,
    /// When the pairing was approved, in seconds since the Unix epoch.
    pub paired_at: u64,
}

impl TrustedPeer {
    /// A peer as pairing records it: view only, paired now.
    pub fn newly_paired(id: PeerId, machine_name: &str) -> Self {
        Self {
            id,
            machine_name: sanitize_machine_name(machine_name),
            permissions: PeerPermissions::NEW_PEER,
            paired_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

/// The on-disk shape of `peers.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerList {
    #[serde(default = "PeerList::current_version")]
    pub version: u32,
    #[serde(default)]
    pub peers: Vec<TrustedPeer>,
}

impl PeerList {
    fn current_version() -> u32 {
        1
    }

    pub fn new(peers: Vec<TrustedPeer>) -> Self {
        Self {
            version: Self::current_version(),
            peers,
        }
    }
}

/// Longest machine name shown, in characters.
pub const MAX_MACHINE_NAME_CHARS: usize = 64;

/// Make a name another machine *claims* safe to show.
///
/// The name arrives from an unauthorised peer during pairing and is shown in
/// the host's approval prompt, so it is attacker-controlled text. Control
/// characters and Unicode format characters are removed — the latter include
/// the bidi overrides (U+202E) that can make "Laptop" render from bytes that
/// spell something else, and zero-width characters that make two names look
/// identical. Whitespace runs collapse to one space, and the result is capped.
/// An empty result becomes the fallback name.
///
/// This stops the name *rendering* deceptively; it cannot stop an attacker
/// from simply claiming "Laptop". The confirmation number does that.
pub fn sanitize_machine_name(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if c.is_control() || is_format_char(c) {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
        if out.chars().count() >= MAX_MACHINE_NAME_CHARS {
            break;
        }
    }
    if out.is_empty() {
        super::identity::FALLBACK_MACHINE_NAME.to_string()
    } else {
        out
    }
}

/// Unicode general category Cf (format) characters that matter for spoofing:
/// bidi embeddings, overrides and isolates, zero-width joiners and spaces, the
/// word joiner and invisible operators, the BOM, and the interlinear and tag
/// blocks. std has no category lookup, so the ranges are listed.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::LocalIdentity;

    #[test]
    fn a_new_peer_has_view_and_not_control() {
        let peer = TrustedPeer::newly_paired(LocalIdentity::ephemeral().id(), "Laptop");
        assert!(peer.permissions.view);
        assert!(!peer.permissions.control);
    }

    #[test]
    fn a_missing_control_field_fails_closed() {
        let p: PeerPermissions = serde_json::from_str(r#"{"view": true}"#).unwrap();
        assert!(!p.control);
    }

    #[test]
    fn peer_list_round_trips_through_json() {
        let list = PeerList::new(vec![TrustedPeer::newly_paired(
            LocalIdentity::ephemeral().id(),
            "Desk",
        )]);
        let json = serde_json::to_string(&list).unwrap();
        assert_eq!(serde_json::from_str::<PeerList>(&json).unwrap(), list);
    }

    #[test]
    fn sanitising_strips_bidi_overrides_and_control_characters() {
        // "Laptop" spelled with a right-to-left override and a newline that
        // would split the approval prompt.
        assert_eq!(sanitize_machine_name("\u{202E}potpaL\nevil"), "potpaL evil");
        assert_eq!(sanitize_machine_name("Lap\u{200B}top"), "Laptop");
        assert_eq!(sanitize_machine_name("a\u{0007}b"), "ab");
    }

    #[test]
    fn sanitising_collapses_whitespace_and_trims() {
        assert_eq!(sanitize_machine_name("  My \t  Mac  "), "My Mac");
    }

    #[test]
    fn sanitising_caps_length_and_falls_back_when_empty() {
        let long = "x".repeat(500);
        assert_eq!(
            sanitize_machine_name(&long).chars().count(),
            MAX_MACHINE_NAME_CHARS
        );
        assert_eq!(
            sanitize_machine_name("\u{202E}\n "),
            crate::remote::identity::FALLBACK_MACHINE_NAME
        );
    }
}
