//! Pairing messages on the wire: length-prefixed JSON, one frame at a time.
//!
//! ```text
//!   u32 big-endian length | JSON body (at most MAX_FRAME bytes)
//! ```
//!
//! The length is checked **before** the body is read, so an unpaired peer can
//! make the host buffer at most [`MAX_FRAME`] bytes. DEV-1161 defines the full
//! remote protocol; these frames are the pairing subset and should become part
//! of it, not a second framing.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::errors::{AlleleError, Result};

/// Largest frame accepted. A hello is a ~16-byte secret and a ≤64-character
/// name; 4 KiB is generous and still bounds what a stranger can make us hold.
pub const MAX_FRAME: usize = 4 * 1024;

/// The pairing protocol version carried in the hello.
pub const PAIRING_VERSION: u32 = 1;

/// Why the host refused, as the client is told. Deliberately coarse: a wrong
/// secret and a contested code look alike from outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// The host user clicked Decline.
    Declined,
    /// The code is more than five minutes old.
    Expired,
    /// The code was already used, or used by two machines at once.
    Used,
    /// The code is wrong.
    Invalid,
    /// Nobody answered on the host in time.
    TimedOut,
}

impl RefusalCode {
    /// What the client shows.
    pub fn message(self) -> &'static str {
        match self {
            Self::Declined => "The other Mac declined the pairing.",
            Self::Expired => "That code has expired. Make a new one on the other Mac.",
            Self::Used => {
                "That code was already used. If you did not use it, someone else may \
                 have seen it — make a new one on the other Mac."
            }
            Self::Invalid => "That code is not valid. Make a new one on the other Mac.",
            Self::TimedOut => "Nobody approved the pairing on the other Mac in time.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// The prefix is the wire tag (`pair_hello`); DEV-1161 adds non-pairing frames.
#[allow(clippy::enum_variant_names)]
pub enum Frame {
    /// Client → host: "I have your code."
    PairHello {
        v: u32,
        /// The code's secret, base32.
        secret: String,
        machine_name: String,
    },
    /// Host → client: the host user approved.
    PairApproved { machine_name: String },
    /// Host → client: no.
    PairRefused { reason: RefusalCode },
    /// Client → host: "I have saved you." The host saves the client only on
    /// this, so a pairing that breaks half-way leaves the host trusting
    /// nobody new.
    PairAck,
}

pub async fn write_frame<W: AsyncWrite + Unpin + ?Sized>(w: &mut W, frame: &Frame) -> Result<()> {
    let body = serde_json::to_vec(frame)
        .map_err(|e| AlleleError::Remote(format!("could not encode a frame: {e}")))?;
    if body.len() > MAX_FRAME {
        return Err(AlleleError::Remote("frame too large".into()));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> Result<Frame> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(AlleleError::Remote(format!(
            "frame of {len} bytes exceeds the {MAX_FRAME}-byte limit"
        )));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map_err(|e| AlleleError::Remote(format!("could not decode a frame: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let hello = Frame::PairHello {
            v: PAIRING_VERSION,
            secret: "ABC".into(),
            machine_name: "Laptop".into(),
        };
        write_frame(&mut a, &hello).await.unwrap();
        write_frame(&mut a, &Frame::PairAck).await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap(), hello);
        assert_eq!(read_frame(&mut b).await.unwrap(), Frame::PairAck);
    }

    #[tokio::test]
    async fn an_oversized_length_is_refused_before_the_body_is_read() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // Claim 1 GiB and send nothing more: the reader must fail on the
        // length alone rather than wait for, or allocate, the body.
        a.write_all(&(1u32 << 30).to_be_bytes()).await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err().to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn refusals_serialise_as_snake_case() {
        let json = serde_json::to_string(&Frame::PairRefused {
            reason: RefusalCode::TimedOut,
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"pair_refused","reason":"timed_out"}"#);
    }
}
