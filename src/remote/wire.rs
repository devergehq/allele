//! The remote protocol on the wire: length-prefixed JSON, one frame at a time.
//!
//! ```text
//!   u32 big-endian length | JSON body
//! ```
//!
//! **One framing for everything** — pairing (DEV-1160) and the control stream
//! (DEV-1161) share [`Frame`]. Later streams (status, DEV-1162; screens,
//! DEV-1165) add variants here rather than a second encoding.
//!
//! The length is checked **before** the body is read. How much a peer may
//! make the host buffer depends on who it is:
//!
//! | reader | limit | why |
//! | -- | -- | -- |
//! | [`read_frame`] | [`MAX_FRAME`] (4 KiB) | an unpaired stranger, or the first frame on any stream |
//! | [`read_frame_within`] | [`MAX_PEER_FRAME`] (1 MiB) | an admitted peer's control stream, after its hello |
//!
//! ## Frame set (protocol 1)
//!
//! ```text
//!   pairing (any peer, while a code is live)
//!     client → host  pair_hello { v, secret, machine_name }
//!     host → client  pair_approved { machine_name } | pair_refused { reason }
//!     client → host  pair_ack
//!
//!   control stream (trusted peers; one per connection, client-opened)
//!     client → host  hello    { protocol: 1, machine_name }
//!     host → client  hello    { protocol: 1, machine_name, permissions }
//!                  | refused  { code, message }        then the host waits for
//!                                                      the client to hang up
//!     client → host  request  { id, op: <dispatch Request> }
//!     host → client  response { id, result: <dispatch Response> }
//!     client → host  attach   { session_id }   needs view (DEV-1165)
//!                  | detach   { session_id }
//!     host → client  attach_failed { session_id, code, message }
//!
//!   screen stream (one per attached session; HOST-opened, DEV-1165)
//!     host → client  screen_open { session_id }   first frame
//!     host → client  screen { frame: snapshot | diff | scrollback }
//!     client → host  scrollback_request { start, count } | resync
//! ```
//!
//! `op` and `result` are `crate::dispatch::protocol`'s own types, nested
//! unchanged — so `control.sock` and the remote listener cannot drift apart on
//! what an operation means. Responses may arrive out of order; `id` pairs them.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::dispatch::protocol::{ErrorCode, Request, Response};
use crate::errors::{AlleleError, Result};
use crate::terminal::mirror::ScreenFrame;

use super::peers::PeerPermissions;

/// Largest frame read from a peer that has not yet said who it is. A pairing
/// hello is a ~16-byte secret and a ≤64-character name; 4 KiB is generous and
/// still bounds what a stranger can make us hold.
pub const MAX_FRAME: usize = 4 * 1024;

/// Largest frame on an admitted peer's control stream, in either direction.
/// A `sessions_list` for a busy host runs to tens of KiB; a `sessions_create`
/// prompt is a few. 1 MiB is far above both and still a bound.
pub const MAX_PEER_FRAME: usize = 1024 * 1024;

/// The pairing protocol version carried in the pairing hello.
pub const PAIRING_VERSION: u32 = 1;

/// The control-stream protocol version carried in [`Frame::Hello`].
pub const PROTOCOL_VERSION: u32 = 1;

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

/// Why the host refused a control stream. `message` in [`Frame::Refused`] is
/// what the client shows; the code is for a client that wants to branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamRefusal {
    /// The client's `protocol` is not one this host speaks.
    UnsupportedProtocol,
    /// The first frame was not a hello, or the stream broke the protocol.
    BadHello,
    /// This connection already has a control stream.
    AlreadyOpen,
    /// The peer was removed from this host's trusted list.
    NotPaired,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    // ── pairing (DEV-1160) ────────────────────────────────────────────────
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

    // ── control stream (DEV-1161) ─────────────────────────────────────────
    /// Both ways, once, to open a control stream. The client sends it without
    /// `permissions`; the host answers with what the client may do *right
    /// now*. Permissions can change mid-connection and every request is
    /// checked again, so a client should treat this as a hint for its UI.
    Hello {
        protocol: u32,
        machine_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permissions: Option<PeerPermissions>,
    },
    /// Host → client: the control stream is not going ahead.
    Refused {
        code: StreamRefusal,
        message: String,
    },
    /// Client → host: one dispatch call.
    Request { id: u64, op: Request },
    /// Host → client: the answer to request `id`. Exactly one per request.
    Response { id: u64, result: Response },

    // ── screens (DEV-1165) ────────────────────────────────────────────────
    /// Client → host, on the control stream: mirror this session's screen.
    /// Needs `view`. The host answers by opening a screen stream, or with
    /// [`Frame::AttachFailed`].
    Attach { session_id: String },
    /// Client → host, on the control stream: stop mirroring this session.
    /// Closing the screen stream does the same.
    Detach { session_id: String },
    /// Host → client, on the control stream: the attach is not going ahead,
    /// or an attached stream stopped (session gone, `view` revoked).
    AttachFailed {
        session_id: String,
        code: ErrorCode,
        message: String,
    },
    /// Host → client: the first frame on a host-opened screen stream.
    ScreenOpen { session_id: String },
    /// Host → client, on a screen stream: a snapshot, diff or scrollback page.
    Screen { frame: ScreenFrame },
    /// Client → host, on a screen stream: send scrollback rows
    /// `start..start + count` (0 = the line just above the screen).
    ScrollbackRequest { start: u32, count: u32 },
    /// Client → host, on a screen stream: the mirror lost track (a frame it
    /// could not apply); send a full snapshot.
    Resync,
}

/// Write one frame. Refuses a frame larger than [`MAX_PEER_FRAME`]; the
/// reader may hold the writer to less.
pub async fn write_frame<W: AsyncWrite + Unpin + ?Sized>(w: &mut W, frame: &Frame) -> Result<()> {
    write_body(w, &encode(frame)?).await
}

/// Write a body [`encode`] produced.
pub async fn write_body<W: AsyncWrite + Unpin + ?Sized>(w: &mut W, body: &[u8]) -> Result<()> {
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(body).await?;
    w.flush().await?;
    Ok(())
}

/// Encode a frame's body, refusing one over [`MAX_PEER_FRAME`]. Split out so
/// a caller can find out a reply is too large *before* it is half-written.
pub fn encode(frame: &Frame) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(frame)
        .map_err(|e| AlleleError::Remote(format!("could not encode a frame: {e}")))?;
    if body.len() > MAX_PEER_FRAME {
        return Err(AlleleError::Remote(format!(
            "frame of {} bytes exceeds the {MAX_PEER_FRAME}-byte limit",
            body.len()
        )));
    }
    Ok(body)
}

/// Read one frame of at most [`MAX_FRAME`] bytes — the limit for anyone who
/// has not yet been admitted.
pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> Result<Frame> {
    read_frame_within(r, MAX_FRAME).await
}

/// Read one frame of at most `limit` bytes. The length is checked before the
/// body is read or allocated.
pub async fn read_frame_within<R: AsyncRead + Unpin + ?Sized>(
    r: &mut R,
    limit: usize,
) -> Result<Frame> {
    decode(&read_body_within(r, limit).await?)
}

/// Read one frame's body, undecoded, so a caller can still answer a request
/// it cannot decode (see [`request_id`]).
pub async fn read_body_within<R: AsyncRead + Unpin + ?Sized>(
    r: &mut R,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > limit {
        return Err(AlleleError::Remote(format!(
            "frame of {len} bytes exceeds the {limit}-byte limit"
        )));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(body)
}

pub fn decode(body: &[u8]) -> Result<Frame> {
    serde_json::from_slice(body)
        .map_err(|e| AlleleError::Remote(format!("could not decode a frame: {e}")))
}

/// The `id` of a body that is shaped like a request but did not decode — an
/// op this Allele does not know, say — so it can still be answered.
pub fn request_id(body: &[u8]) -> Option<u64> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(rename = "type")]
        kind: String,
        id: u64,
    }
    let env: Envelope = serde_json::from_slice(body).ok()?;
    (env.kind == "request").then_some(env.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::protocol::ErrorCode;

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

    /// A stranger is held to 4 KiB even though an admitted peer may send 1 MiB.
    #[tokio::test]
    async fn the_stranger_limit_is_the_default() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let big = Frame::PairHello {
            v: 1,
            secret: "A".repeat(MAX_FRAME),
            machine_name: String::new(),
        };
        write_frame(&mut a, &big).await.unwrap();
        write_frame(&mut a, &big).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());

        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        write_frame(&mut a, &big).await.unwrap();
        assert_eq!(
            read_frame_within(&mut b, MAX_PEER_FRAME).await.unwrap(),
            big
        );
    }

    #[test]
    fn a_frame_over_the_peer_limit_is_not_encoded() {
        let huge = Frame::Refused {
            code: StreamRefusal::BadHello,
            message: "x".repeat(MAX_PEER_FRAME),
        };
        assert!(encode(&huge).is_err());
    }

    #[test]
    fn an_unknown_op_still_yields_its_request_id() {
        let body = br#"{"type":"request","id":42,"op":{"op":"sessions_teleport"}}"#;
        assert!(decode(body).is_err());
        assert_eq!(request_id(body), Some(42));
        assert_eq!(request_id(br#"{"type":"hello","id":1}"#), None);
        assert_eq!(request_id(b"not json"), None);
    }

    #[test]
    fn refusals_serialise_as_snake_case() {
        let json = serde_json::to_string(&Frame::PairRefused {
            reason: RefusalCode::TimedOut,
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"pair_refused","reason":"timed_out"}"#);
    }

    /// The shapes in the ticket and the module docs, pinned: a client written
    /// against them must keep working.
    #[test]
    fn control_frames_have_the_documented_shape() {
        let client_hello = Frame::Hello {
            protocol: 1,
            machine_name: "Laptop".into(),
            permissions: None,
        };
        assert_eq!(
            serde_json::to_string(&client_hello).unwrap(),
            r#"{"type":"hello","protocol":1,"machine_name":"Laptop"}"#
        );
        let host_hello = Frame::Hello {
            protocol: 1,
            machine_name: "Studio".into(),
            permissions: Some(PeerPermissions::NEW_PEER),
        };
        assert_eq!(
            serde_json::to_string(&host_hello).unwrap(),
            r#"{"type":"hello","protocol":1,"machine_name":"Studio","permissions":{"view":true,"control":false}}"#
        );
        let request = Frame::Request {
            id: 7,
            op: Request::SessionsList,
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"type":"request","id":7,"op":{"op":"sessions_list"}}"#
        );
        let response = Frame::Response {
            id: 7,
            result: Response::Error {
                code: ErrorCode::PermissionDenied,
                message: "no".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"type":"response","id":7,"result":{"result":"error","code":"permission_denied","message":"no"}}"#
        );
    }

    #[test]
    fn a_create_request_round_trips_nested() {
        let frame = Frame::Request {
            id: 1,
            op: Request::SessionsCreate(crate::dispatch::protocol::CreateRequest {
                project: "allele".into(),
                name: "x".into(),
                prompt: "do it".into(),
                orchestration: crate::session::Orchestration::Nothing,
                caller_session_id: None,
                caller_reply_to: None,
                group: None,
            }),
        };
        let json = serde_json::to_vec(&frame).unwrap();
        assert_eq!(serde_json::from_slice::<Frame>(&json).unwrap(), frame);
    }
}
