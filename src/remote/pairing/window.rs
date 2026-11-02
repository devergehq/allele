//! The host's pairing window: one code, its lifetime, and what has been
//! presented against it. Pure state — the clock is passed in — so the
//! security properties are tested directly:
//!
//! - a code is **single-use**: the first correct presentation consumes it, and
//!   every later presentation is refused;
//! - a code **expires** five minutes after it is shown;
//! - a correct code only ever produces a *request* — nothing is trusted until
//!   [`PairingWindow::decide`] is called with the host user's answer;
//! - a second correct presentation while a request awaits approval means the
//!   code leaked, so the pairing is **aborted** rather than raced;
//! - a wrong secret is refused and counted, but does not burn the code — that
//!   would hand anyone who knows the host's id a way to kill every pairing.

use std::time::Duration;

use tokio::time::Instant;

use super::code::{confirmation_number, PairingCode};
use crate::remote::peers::sanitize_machine_name;
use crate::remote::transport::PeerId;

/// How long a code can be presented after it is shown.
pub const CODE_TTL: Duration = Duration::from_secs(5 * 60);

/// How long the host user has to answer an approval prompt. No answer is a
/// decline.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Identifies one approval request, so a click on a stale prompt cannot answer
/// a newer one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// A correct presentation waiting for the host user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingRequest {
    pub id: RequestId,
    pub client: PeerId,
    /// The name the client claims, sanitised. Not evidence of anything.
    pub machine_name: String,
    /// What the client's screen should be showing. The real check.
    pub confirmation: String,
}

/// Why a presentation was refused. Sent to the client in coarse form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The code is older than [`CODE_TTL`].
    Expired,
    /// The code was already presented, approved, declined or cancelled.
    Used,
    /// The secret did not match.
    WrongSecret,
    /// A second correct presentation arrived while one awaited approval.
    Contested,
}

/// How a window ended, for the host UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Paired {
        machine_name: String,
    },
    Declined,
    /// Nobody answered the prompt in time.
    TimedOut,
    Expired,
    Contested,
    Cancelled,
    /// The exchange broke after approval (the client vanished, a write failed).
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Showing the code, waiting for a client.
    Open,
    /// A client presented the code; the host user has not answered.
    AwaitingApproval(PairingRequest),
    /// Approved; finishing the exchange with the client.
    Approved(PairingRequest),
    Closed(Outcome),
}

pub struct PairingWindow {
    code: PairingCode,
    opened_at: Instant,
    phase: Phase,
    failed_attempts: u32,
    next_request: u64,
}

impl PairingWindow {
    pub fn open(code: PairingCode, now: Instant) -> Self {
        Self {
            code,
            opened_at: now,
            phase: Phase::Open,
            failed_attempts: 0,
            next_request: 1,
        }
    }

    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn expires_at(&self) -> Instant {
        self.opened_at + CODE_TTL
    }

    /// Wrong secrets presented so far. Shown on the host: a climbing number
    /// means someone is guessing.
    pub fn failed_attempts(&self) -> u32 {
        self.failed_attempts
    }

    /// Close an open window whose time is up. Returns whether it did.
    pub fn expire_if_due(&mut self, now: Instant) -> bool {
        if self.phase == Phase::Open && now >= self.expires_at() {
            self.phase = Phase::Closed(Outcome::Expired);
            return true;
        }
        false
    }

    /// Whether the listener should let an unpaired peer send a hello.
    pub fn accepts_presentations(&mut self, now: Instant) -> bool {
        self.expire_if_due(now);
        matches!(self.phase, Phase::Open | Phase::AwaitingApproval(_))
    }

    /// A client presents `secret` for this window's code.
    pub fn present(
        &mut self,
        client: PeerId,
        secret: &[u8],
        claimed_name: &str,
        now: Instant,
    ) -> Result<PairingRequest, Refusal> {
        self.expire_if_due(now);
        let correct = self.code.secret.matches(secret);
        match &self.phase {
            Phase::Closed(Outcome::Expired) => Err(Refusal::Expired),
            Phase::Closed(_) | Phase::Approved(_) => Err(Refusal::Used),
            _ if !correct => {
                self.failed_attempts += 1;
                Err(Refusal::WrongSecret)
            }
            Phase::AwaitingApproval(_) => {
                tracing::warn!(
                    "pairing code presented twice (second by {}); aborting the pairing",
                    client.short()
                );
                self.phase = Phase::Closed(Outcome::Contested);
                Err(Refusal::Contested)
            }
            Phase::Open => {
                let request = PairingRequest {
                    id: RequestId(self.next_request),
                    client,
                    machine_name: sanitize_machine_name(claimed_name),
                    confirmation: confirmation_number(self.code.host, client, &self.code.secret),
                };
                self.next_request += 1;
                self.phase = Phase::AwaitingApproval(request.clone());
                Ok(request)
            }
        }
    }

    /// The host user's answer. Returns the request if `id` is the one awaiting
    /// approval; any other id (a stale prompt) changes nothing.
    pub fn decide(&mut self, id: RequestId, approve: bool) -> Option<PairingRequest> {
        let Phase::AwaitingApproval(request) = &self.phase else {
            return None;
        };
        if request.id != id {
            return None;
        }
        let request = request.clone();
        self.phase = if approve {
            Phase::Approved(request.clone())
        } else {
            Phase::Closed(Outcome::Declined)
        };
        Some(request)
    }

    /// End the window, unless it already ended — the first outcome stands.
    pub fn close(&mut self, outcome: Outcome) {
        if !matches!(self.phase, Phase::Closed(_)) {
            self.phase = Phase::Closed(outcome);
        }
    }

    /// The window's request is still the live one with this id.
    pub fn is_current(&self, id: RequestId) -> bool {
        matches!(&self.phase,
            Phase::AwaitingApproval(r) | Phase::Approved(r) if r.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::code::PairingSecret;
    use super::*;
    use crate::remote::LocalIdentity;

    fn window() -> (PairingWindow, PairingSecret, Instant) {
        let secret = PairingSecret::generate().unwrap();
        let code = PairingCode {
            host: LocalIdentity::ephemeral().id(),
            secret: secret.clone(),
        };
        let now = Instant::now();
        (PairingWindow::open(code, now), secret, now)
    }

    fn id() -> PeerId {
        LocalIdentity::ephemeral().id()
    }

    fn bytes(s: &PairingSecret) -> Vec<u8> {
        PairingSecret::decode_wire(&s.to_text())
    }

    #[test]
    fn a_correct_code_only_raises_a_request() {
        let (mut w, s, now) = window();
        let req = w.present(id(), &bytes(&s), "Laptop", now).unwrap();
        assert_eq!(w.phase(), &Phase::AwaitingApproval(req));
    }

    #[test]
    fn a_code_is_rejected_after_five_minutes() {
        let (mut w, s, now) = window();
        let late = now + CODE_TTL;
        assert_eq!(
            w.present(id(), &bytes(&s), "Laptop", late),
            Err(Refusal::Expired)
        );
        assert!(!w.accepts_presentations(late));
    }

    #[test]
    fn a_code_is_accepted_just_inside_five_minutes() {
        let (mut w, s, now) = window();
        let just_in = now + CODE_TTL - Duration::from_millis(1);
        assert!(w.present(id(), &bytes(&s), "Laptop", just_in).is_ok());
    }

    #[test]
    fn a_code_is_rejected_on_its_second_use_after_approval() {
        let (mut w, s, now) = window();
        let req = w.present(id(), &bytes(&s), "Laptop", now).unwrap();
        w.decide(req.id, true).unwrap();
        w.close(Outcome::Paired {
            machine_name: "Laptop".into(),
        });
        assert_eq!(
            w.present(req.client, &bytes(&s), "Laptop", now),
            Err(Refusal::Used)
        );
    }

    #[test]
    fn a_code_is_rejected_on_its_second_use_after_a_decline() {
        let (mut w, s, now) = window();
        let req = w.present(id(), &bytes(&s), "Laptop", now).unwrap();
        w.decide(req.id, false).unwrap();
        assert_eq!(w.phase(), &Phase::Closed(Outcome::Declined));
        assert_eq!(
            w.present(id(), &bytes(&s), "Laptop", now),
            Err(Refusal::Used)
        );
    }

    #[test]
    fn a_second_presentation_while_pending_aborts_the_pairing() {
        let (mut w, s, now) = window();
        let attacker = w.present(id(), &bytes(&s), "Laptop", now).unwrap();
        // The real laptop now presents the same code.
        assert_eq!(
            w.present(id(), &bytes(&s), "Laptop", now),
            Err(Refusal::Contested)
        );
        // The first request can no longer be approved.
        assert!(w.decide(attacker.id, true).is_none());
        assert_eq!(w.phase(), &Phase::Closed(Outcome::Contested));
    }

    #[test]
    fn wrong_secrets_are_refused_and_counted_but_do_not_burn_the_code() {
        let (mut w, s, now) = window();
        for _ in 0..50 {
            assert_eq!(
                w.present(id(), &[0; 10], "Guesser", now),
                Err(Refusal::WrongSecret)
            );
        }
        assert_eq!(w.failed_attempts(), 50);
        assert_eq!(w.phase(), &Phase::Open, "no request was raised");
        // The rightful client still gets through.
        assert!(w.present(id(), &bytes(&s), "Laptop", now).is_ok());
    }

    #[test]
    fn a_wrong_secret_never_reaches_the_approval_prompt() {
        let (mut w, _s, now) = window();
        let _ = w.present(id(), b"guess", "Laptop", now);
        assert!(!matches!(w.phase(), Phase::AwaitingApproval(_)));
    }

    #[test]
    fn a_stale_prompt_cannot_answer_a_different_request() {
        let (mut w, s, now) = window();
        let req = w.present(id(), &bytes(&s), "Laptop", now).unwrap();
        assert!(w.decide(RequestId(req.id.0 + 1), true).is_none());
        assert!(matches!(w.phase(), Phase::AwaitingApproval(_)));
    }

    #[test]
    fn the_request_carries_a_sanitised_name_and_the_confirmation_number() {
        let (mut w, s, now) = window();
        let client = id();
        let req = w
            .present(client, &bytes(&s), "\u{202E}Lap\ntop", now)
            .unwrap();
        assert_eq!(req.machine_name, "Lap top");
        assert_eq!(
            req.confirmation,
            confirmation_number(w.code().host, client, &s)
        );
    }

    #[test]
    fn the_first_outcome_stands() {
        let (mut w, _s, _now) = window();
        w.close(Outcome::Cancelled);
        w.close(Outcome::Expired);
        assert_eq!(w.phase(), &Phase::Closed(Outcome::Cancelled));
    }
}
