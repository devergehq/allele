//! The client's side of pairing: dial the host named in the code, prove the
//! secret, wait for the host user, then record the host.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use super::code::{confirmation_number, PairingCode};
use super::host::HELLO_TIMEOUT;
use super::window::APPROVAL_TIMEOUT;
use super::wire::{read_frame, write_frame, Frame, PAIRING_VERSION};
use crate::errors::{AlleleError, Result};
use crate::remote::peers::TrustedPeer;
use crate::remote::registry::PeerRegistry;
use crate::remote::transport::{PeerConnection, PeerId, PeerTransport};

/// How long to try to reach the host.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Slack on top of the host's approval timeout, so the host's own "timed out"
/// reply arrives before the client gives up on its own.
const REPLY_SLACK: Duration = Duration::from_secs(15);

/// Progress the client UI shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientProgress {
    Connecting,
    /// Connected and the code was sent; the host user is deciding. The
    /// confirmation number must match the one on the host's screen.
    AwaitingApproval {
        confirmation: String,
    },
}

/// Pair with the host in `code`. On success the host is in `registry` and is
/// returned.
pub(crate) async fn pair_with(
    transport: &dyn PeerTransport,
    code: &PairingCode,
    machine_name: &str,
    registry: &PeerRegistry,
    progress: impl Fn(ClientProgress),
) -> Result<TrustedPeer> {
    let me = transport.local_id();
    if code.host == me {
        return Err(AlleleError::Remote(
            "that is this Mac's own pairing code — enter it on the other Mac".into(),
        ));
    }
    progress(ClientProgress::Connecting);
    let conn = timeout(DIAL_TIMEOUT, transport.dial(code.host))
        .await
        .map_err(|_| AlleleError::Remote("could not reach the other Mac".into()))??;
    let result = exchange(&conn, me, code, machine_name, registry, &progress).await;
    conn.close("pairing finished");
    result
}

async fn exchange(
    conn: &Arc<dyn PeerConnection>,
    me: PeerId,
    code: &PairingCode,
    machine_name: &str,
    registry: &PeerRegistry,
    progress: &impl Fn(ClientProgress),
) -> Result<TrustedPeer> {
    let mut stream = conn.open_stream().await?;
    let hello = Frame::PairHello {
        v: PAIRING_VERSION,
        secret: code.secret.to_text(),
        machine_name: machine_name.to_string(),
    };
    write_frame(&mut stream.send, &hello).await?;
    progress(ClientProgress::AwaitingApproval {
        confirmation: confirmation_number(code.host, me, &code.secret),
    });

    let reply = tokio::select! {
        // A reply that arrived just before the close still counts.
        biased;
        r = timeout(APPROVAL_TIMEOUT + REPLY_SLACK, read_frame(&mut stream.recv)) => r
            .map_err(|_| AlleleError::Remote("the other Mac did not answer".into()))??,
        reason = conn.closed() => {
            return Err(AlleleError::Remote(format!("the other Mac disconnected ({reason})")));
        }
    };
    let host_name = match reply {
        Frame::PairApproved { machine_name } => machine_name,
        Frame::PairRefused { reason } => return Err(AlleleError::Remote(reason.message().into())),
        other => {
            return Err(AlleleError::Remote(format!(
                "unexpected reply from the other Mac: {other:?}"
            )))
        }
    };

    // Save the host first, then acknowledge: the host saves us only on the
    // acknowledgement, so if anything breaks from here the host is the side
    // left trusting nobody new.
    let host = TrustedPeer::newly_paired(code.host, &host_name);
    registry.add_paired(host.clone())?;
    write_frame(&mut stream.send, &Frame::PairAck).await?;
    // Give the host a moment to read the ack before the connection closes.
    let _ = timeout(HELLO_TIMEOUT, conn.closed()).await;
    Ok(host)
}
