//! The pairing code: what the host shows and the client types or scans.
//!
//! ```text
//!   version (1) | host PeerId (32) | secret (10) | CRC-16 (2)   = 45 bytes
//!   → RFC 4648 base32, no padding = 72 characters, shown in groups of six
//! ```
//!
//! **Why it is not short.** The client dials the host *by its public key*, and
//! the transport proves the far end holds that key — that is what stops a
//! man-in-the-middle from collecting the secret. A shorter code would have to
//! drop key bits and find the host some other way (a rendezvous server, mDNS),
//! which this design does not have. So the QR is the everyday path; the text
//! is for copy-paste and for typing in a pinch, which the checksum makes safe.
//!
//! The alphabet is upper-case base32 so the QR can use its compact
//! alphanumeric mode, and the text never mixes up `0`/`O` or `1`/`l`.

use std::fmt;

use data_encoding::BASE32_NOPAD;
use sha2::{Digest, Sha256};

use crate::errors::{AlleleError, Result};
use crate::remote::transport::PeerId;

const VERSION: u8 = 1;
/// Secret length in bytes: 80 bits. Each guess costs the attacker a full
/// handshake inside a five-minute window, and a correct guess still only
/// reaches a prompt a human must approve, so this is far more than enough.
pub const SECRET_LEN: usize = 10;
const PAYLOAD_LEN: usize = 1 + 32 + SECRET_LEN;
const CODE_LEN: usize = PAYLOAD_LEN + 2;
const GROUP: usize = 6;

/// The single-use secret in a pairing code. `Debug` is redacted so a stray
/// `{:?}` cannot log it.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingSecret([u8; SECRET_LEN]);

impl PairingSecret {
    /// A fresh secret from the OS CSPRNG.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; SECRET_LEN];
        getrandom::fill(&mut bytes)
            .map_err(|e| AlleleError::Remote(format!("no randomness for a pairing code: {e}")))?;
        Ok(Self(bytes))
    }

    #[cfg(test)]
    pub fn from_bytes(bytes: [u8; SECRET_LEN]) -> Self {
        Self(bytes)
    }

    /// Compare in constant time, so response timing does not reveal how many
    /// leading bytes of a guess were right.
    pub fn matches(&self, presented: &[u8]) -> bool {
        if presented.len() != SECRET_LEN {
            return false;
        }
        let diff = self
            .0
            .iter()
            .zip(presented)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        // `black_box` keeps the optimiser from turning the fold back into an
        // early-exit comparison.
        std::hint::black_box(diff) == 0
    }

    /// The secret as text, for the wire.
    pub fn to_text(&self) -> String {
        BASE32_NOPAD.encode(&self.0)
    }

    /// Decode a secret sent over the wire. Any length is accepted here and
    /// rejected by [`PairingSecret::matches`], so a malformed secret counts as
    /// a wrong one rather than as a different kind of failure.
    pub fn decode_wire(text: &str) -> Vec<u8> {
        BASE32_NOPAD.decode(text.as_bytes()).unwrap_or_default()
    }

    fn bytes(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }
}

impl fmt::Debug for PairingSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingSecret(..)")
    }
}

/// A decoded pairing code: whom to dial, and what to prove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingCode {
    pub host: PeerId,
    pub secret: PairingSecret,
}

impl PairingCode {
    /// The code as shown: base32 in dash-separated groups of six.
    pub fn to_text(&self) -> String {
        let mut bytes = Vec::with_capacity(CODE_LEN);
        bytes.push(VERSION);
        bytes.extend_from_slice(self.host.as_bytes());
        bytes.extend_from_slice(self.secret.bytes());
        let crc = crc16(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        let flat = BASE32_NOPAD.encode(&bytes);
        flat.as_bytes()
            .chunks(GROUP)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("-")
    }

    /// Parse a code as typed or pasted: case, spaces and dashes are ignored.
    /// A typo fails the checksum here, before anything is dialled, so it never
    /// reaches the host as a wrong guess.
    pub fn parse(text: &str) -> Result<Self> {
        let cleaned: String = text
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let invalid = || AlleleError::Remote("that is not a pairing code".into());
        let bytes = BASE32_NOPAD
            .decode(cleaned.as_bytes())
            .map_err(|_| invalid())?;
        if bytes.len() != CODE_LEN {
            return Err(invalid());
        }
        let (payload, crc) = bytes.split_at(PAYLOAD_LEN);
        if crc16(payload).to_be_bytes() != crc {
            return Err(AlleleError::Remote(
                "that pairing code has a typo — check it against the other Mac".into(),
            ));
        }
        if payload[0] != VERSION {
            return Err(AlleleError::Remote(
                "that pairing code is from a different version of Allele".into(),
            ));
        }
        let hex: String = payload[1..33].iter().map(|b| format!("{b:02x}")).collect();
        let host: PeerId = hex.parse().map_err(|_| invalid())?;
        let mut secret = [0u8; SECRET_LEN];
        secret.copy_from_slice(&payload[33..]);
        Ok(Self {
            host,
            secret: PairingSecret(secret),
        })
    }
}

/// The confirmation number both Macs show while the host decides.
///
/// Derived from the host's key, the client's key and the secret, so it is the
/// same on both screens only if the host is talking to *this* client. If a
/// leaked code was used by someone else, the host shows a number the user's
/// own Mac does not — and the user's Mac, presenting the same code second, is
/// refused outright. Six digits: a match by chance is one in a million, and an
/// attacker cannot steer it without grinding keys online within the window.
pub fn confirmation_number(host: PeerId, client: PeerId, secret: &PairingSecret) -> String {
    let mut h = Sha256::new();
    h.update(b"allele/pair/confirm/1");
    h.update(host.as_bytes());
    h.update(client.as_bytes());
    h.update(secret.bytes());
    let d = h.finalize();
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// CRC-16/CCITT-FALSE. A typo detector, not an integrity check: the code's
/// integrity comes from the host key it carries.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::LocalIdentity;

    fn code() -> PairingCode {
        PairingCode {
            host: LocalIdentity::ephemeral().id(),
            secret: PairingSecret::generate().unwrap(),
        }
    }

    #[test]
    fn crc_matches_the_reference_check_value() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn a_code_carries_the_host_id_and_secret_and_round_trips() {
        let c = code();
        let text = c.to_text();
        assert_eq!(text.replace('-', "").len(), 72);
        assert_eq!(PairingCode::parse(&text).unwrap(), c);
    }

    #[test]
    fn parsing_ignores_case_spaces_and_dashes() {
        let c = code();
        let messy = c.to_text().to_lowercase().replace('-', "  ");
        assert_eq!(PairingCode::parse(&messy).unwrap(), c);
    }

    #[test]
    fn a_mistyped_code_is_caught_by_the_checksum() {
        let text = code().to_text();
        // Change one character in the secret region.
        let mut chars: Vec<char> = text.chars().collect();
        let i = chars.len() - 8;
        chars[i] = if chars[i] == 'A' { 'B' } else { 'A' };
        let typo: String = chars.into_iter().collect();
        let err = PairingCode::parse(&typo).unwrap_err().to_string();
        assert!(err.contains("typo"), "{err}");
    }

    #[test]
    fn garbage_and_truncated_codes_are_rejected() {
        assert!(PairingCode::parse("hello").is_err());
        let text = code().to_text();
        assert!(PairingCode::parse(&text[..text.len() - 6]).is_err());
    }

    #[test]
    fn two_codes_never_share_a_secret() {
        assert_ne!(code().secret, code().secret);
    }

    #[test]
    fn secret_comparison_rejects_near_misses_and_wrong_lengths() {
        let s = PairingSecret::from_bytes([7; SECRET_LEN]);
        assert!(s.matches(&[7; SECRET_LEN]));
        let mut near = [7; SECRET_LEN];
        near[SECRET_LEN - 1] = 8;
        assert!(!s.matches(&near));
        assert!(!s.matches(&[7; SECRET_LEN - 1]));
        assert!(!s.matches(&[]));
    }

    #[test]
    fn the_secret_survives_the_wire_encoding() {
        let s = PairingSecret::generate().unwrap();
        assert!(s.matches(&PairingSecret::decode_wire(&s.to_text())));
        assert!(PairingSecret::decode_wire("!!").is_empty());
    }

    #[test]
    fn the_secret_is_not_debug_printed() {
        let s = format!("{:?}", code());
        assert!(s.contains("PairingSecret(..)"), "{s}");
    }

    #[test]
    fn the_confirmation_number_depends_on_both_keys() {
        let c = code();
        let me = LocalIdentity::ephemeral().id();
        let attacker = LocalIdentity::ephemeral().id();
        let mine = confirmation_number(c.host, me, &c.secret);
        assert_eq!(mine, confirmation_number(c.host, me, &c.secret));
        assert_eq!(mine.len(), 7);
        // A different client (an attacker using the leaked code) sees a
        // different number on the host. 1-in-a-million collisions aside.
        assert_ne!(mine, confirmation_number(c.host, attacker, &c.secret));
    }
}
