//! The client's side of pairing: dial the host named in the code, prove the
//! secret, wait for the host user **and** this Mac's user, then record the
//! host.
//!
//! The client user's Confirm is what stops a substituted code (swapped in a
//! clipboard or a chat) from making this Mac trust a stranger's host: the
//! stranger's host would show a different confirmation number.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::{sleep_until, timeout, Instant};

use super::code::{confirmation_number, PairingCode};
use super::host::HELLO_TIMEOUT;
use super::window::APPROVAL_TIMEOUT;
use crate::errors::{AlleleError, Result};
use crate::remote::peers::TrustedPeer;
use crate::remote::registry::PeerRegistry;
use crate::remote::transport::{PeerConnection, PeerId, PeerTransport};
use crate::remote::wire::{read_frame, write_frame, Frame, PAIRING_VERSION};

/// How long to try to reach the host.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long this Mac's user has, once the confirmation number is shown, to
/// confirm it. No answer is a cancel.
pub const CONFIRM_TIMEOUT: Duration = APPROVAL_TIMEOUT;

/// Slack on top of the host's approval timeout, so the host's own "timed out"
/// reply arrives before the client gives up on its own.
const REPLY_SLACK: Duration = Duration::from_secs(15);

/// Progress the client UI shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientProgress {
    Connecting,
    /// Connected and the code was sent. This Mac's user is asked whether the
    /// confirmation number matches the one on the host's screen.
    AwaitingConfirmation {
        confirmation: String,
    },
    /// This Mac's user confirmed; the host user is still deciding.
    Confirmed {
        confirmation: String,
    },
}

/// Pair with the host in `code`. On success the host is in `registry` and is
/// returned.
///
/// `confirm` carries this Mac's user's answer to "does the number match?".
/// `false`, a dropped sender, or no answer within [`CONFIRM_TIMEOUT`] cancels:
/// nothing is saved here, and no `PairAck` is sent, so the host saves nothing
/// either.
pub(crate) async fn pair_with(
    transport: &dyn PeerTransport,
    code: &PairingCode,
    machine_name: &str,
    registry: &PeerRegistry,
    confirm: oneshot::Receiver<bool>,
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
    let result = exchange(&conn, me, code, machine_name, registry, confirm, &progress).await;
    conn.close("pairing finished");
    result
}

async fn exchange(
    conn: &Arc<dyn PeerConnection>,
    me: PeerId,
    code: &PairingCode,
    machine_name: &str,
    registry: &PeerRegistry,
    mut confirm: oneshot::Receiver<bool>,
    progress: &impl Fn(ClientProgress),
) -> Result<TrustedPeer> {
    let mut stream = conn.open_stream().await?;
    let hello = Frame::PairHello {
        v: PAIRING_VERSION,
        secret: code.secret.to_text(),
        machine_name: machine_name.to_string(),
    };
    write_frame(&mut stream.send, &hello).await?;
    let confirmation = confirmation_number(code.host, me, &code.secret);
    progress(ClientProgress::AwaitingConfirmation {
        confirmation: confirmation.clone(),
    });

    // Wait for both answers, in either order: the host's reply and this
    // Mac's Confirm. `read_frame` is not cancel-safe, so the one read lives
    // across loop turns rather than being restarted.
    let now = Instant::now();
    let (confirm_by, reply_by) = (now + CONFIRM_TIMEOUT, now + APPROVAL_TIMEOUT + REPLY_SLACK);
    let read = read_frame(&mut stream.recv);
    tokio::pin!(read);
    let (mut reply, mut confirmed) = (None, false);
    while reply.is_none() || !confirmed {
        tokio::select! {
            // A reply that arrived just before the close still counts.
            biased;
            r = &mut read, if reply.is_none() => reply = Some(r?),
            answer = &mut confirm, if !confirmed => {
                if answer != Ok(true) {
                    return Err(AlleleError::Remote("pairing cancelled on this Mac".into()));
                }
                confirmed = true;
                progress(ClientProgress::Confirmed { confirmation: confirmation.clone() });
            }
            _ = sleep_until(confirm_by), if !confirmed => {
                return Err(AlleleError::Remote(
                    "the number was not confirmed on this Mac in time".into(),
                ));
            }
            _ = sleep_until(reply_by), if reply.is_none() => {
                return Err(AlleleError::Remote("the other Mac did not answer".into()));
            }
            reason = conn.closed() => {
                return Err(AlleleError::Remote(format!("the other Mac disconnected ({reason})")));
            }
        }
        // A refusal ends the pairing whether or not this Mac confirmed.
        if let Some(Frame::PairRefused { reason }) = &reply {
            return Err(AlleleError::Remote(reason.message().into()));
        }
    }
    let reply = reply.expect("loop exits only with a reply");
    let host_name = match reply {
        Frame::PairApproved { machine_name } => machine_name,
        other => {
            return Err(AlleleError::Remote(format!(
                "unexpected reply from the other Mac: {other:?}"
            )))
        }
    };

    // Both users said yes. Save the host first, then acknowledge: the host
    // saves us only on the acknowledgement, so if anything breaks from here
    // the host is the side left trusting nobody new.
    let host = TrustedPeer::newly_paired(code.host, &host_name);
    registry.add_paired(host.clone())?;
    write_frame(&mut stream.send, &Frame::PairAck).await?;
    // Give the host a moment to read the ack before the connection closes.
    let _ = timeout(HELLO_TIMEOUT, conn.closed()).await;
    Ok(host)
}
