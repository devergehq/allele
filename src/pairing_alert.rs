//! Tell the user a machine is waiting for pairing approval (DEV-1188).
//!
//! The host's Approve prompt lives in Settings → Machines. If that window is
//! closed, a request would wait unseen and time out after two minutes. So while
//! a request is open this module:
//!
//! - raises one macOS notification per request, naming the client machine;
//! - shows a banner in the main window whose *Review* button opens Settings at
//!   the approval card.
//!
//! It only reads [`crate::remote::RemoteService::status`]; nothing under
//! `src/remote/` knows it exists.
//!
//! The notification goes through `osascript`, which macOS attributes to Script
//! Editor, so clicking the notification cannot bring Allele forward. The banner
//! is the click target. Routing the notification click itself needs a real
//! `UNUserNotificationCenter` integration.

use std::time::Duration;

use gpui::*;

use crate::app_state::AppState;
use crate::hooks;
use crate::remote::pairing::{HostView, RequestId};
use crate::theme::theme;

actions!(allele, [OpenPairingApproval]);

/// How often the main window checks for a pairing request. One second is
/// quick next to the two-minute approval window and costs one in-memory read.
const POLL: Duration = Duration::from_secs(1);

/// The request currently waiting for the user's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingApproval {
    pub(crate) id: RequestId,
    /// The name the client claims, already sanitised by the pairing code.
    pub(crate) machine_name: String,
}

/// What changed between two looks at the host's pairing state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    None,
    /// A request the user has not been told about yet.
    Raised(PendingApproval),
    /// The open request was answered, timed out, or was cancelled.
    Cleared,
}

/// Remembers which request, if any, is waiting, so each one is announced once.
#[derive(Debug, Default)]
pub(crate) struct PairingAlert {
    pending: Option<PendingApproval>,
}

impl PairingAlert {
    /// The request the banner should show, if any.
    pub(crate) fn pending(&self) -> Option<&PendingApproval> {
        self.pending.as_ref()
    }

    /// Fold in the host's latest pairing state.
    ///
    /// A request counts as waiting only while it is `AwaitingApproval` with
    /// time left. Anything else (approved, denied, timed out, cancelled, a new
    /// code) clears it. The time check matters because the host's state only
    /// leaves `AwaitingApproval` when the serving task records the timeout, and
    /// the banner should not outlive the deadline if that task is late.
    pub(crate) fn observe(&mut self, host: &HostView) -> Change {
        let now = match host {
            HostView::AwaitingApproval {
                request,
                expires_in,
            } if !expires_in.is_zero() => Some(PendingApproval {
                id: request.id,
                machine_name: request.machine_name.clone(),
            }),
            _ => None,
        };
        let change = match (&self.pending, &now) {
            (_, Some(next)) if self.pending.as_ref().map(|p| p.id) != Some(next.id) => {
                Change::Raised(next.clone())
            }
            (Some(_), None) => Change::Cleared,
            _ => Change::None,
        };
        self.pending = now;
        change
    }
}

/// The notification's title and body for a new request.
pub(crate) fn notification_text(machine_name: &str) -> (String, String) {
    (
        "Pairing request".to_string(),
        format!(
            "\u{201c}{machine_name}\u{201d} wants to pair with this Mac. \
             Open Allele to compare the number and approve it."
        ),
    )
}

/// The banner's message.
pub(crate) fn banner_text(machine_name: &str) -> String {
    format!(
        "\u{201c}{machine_name}\u{201d} is asking to pair with this Mac. \
         Approve or deny it before the request times out."
    )
}

/// Poll the remote service for the life of the app. Started once at launch.
///
/// A timer, not the render tick: the render tick stops when the display
/// sleeps, and a request can arrive while it does.
pub(crate) fn spawn_watcher(cx: &mut Context<AppState>) {
    cx.spawn(async move |this, cx| loop {
        cx.background_executor().timer(POLL).await;
        let alive = this.update(cx, |state: &mut AppState, cx| {
            let host = state.remote.status().host;
            match state.pairing_alert.observe(&host) {
                Change::None => {}
                Change::Raised(request) => {
                    if state.user_settings.notifications_enabled {
                        let (title, body) = notification_text(&request.machine_name);
                        hooks::show_notification(&title, &body);
                    }
                    cx.notify();
                }
                Change::Cleared => cx.notify(),
            }
        });
        if alive.is_err() {
            break;
        }
    })
    .detach();
}

/// The main-window banner, while a request is waiting.
pub(crate) fn render_banner(state: &AppState, cx: &mut Context<AppState>) -> Option<Div> {
    let pending = state.pairing_alert.pending()?;
    Some(
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .px(px(16.0))
            .py(px(10.0))
            .bg(theme().tint_warning_soft)
            .border_b_1()
            .border_color(theme().warning)
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(13.0))
                    .text_color(theme().text_primary)
                    .child(banner_text(&pending.machine_name)),
            )
            .child(
                div()
                    .id("pairing-review-btn")
                    .flex_shrink_0()
                    .cursor_pointer()
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(6.0))
                    .bg(theme().accent)
                    .text_size(px(11.0))
                    .text_color(theme().text_on_accent)
                    .child("Review\u{2026}")
                    // Opening a window must not happen inside render, so the
                    // click goes through an app-level action (see
                    // `startup::actions`).
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_this: &mut AppState, _event, window, cx| {
                            window.dispatch_action(OpenPairingApproval.boxed_clone(), cx);
                        }),
                    ),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::{banner_text, notification_text, Change, PairingAlert, PendingApproval};
    use crate::remote::pairing::{HostView, Outcome, PairingRequest, RequestId};
    use crate::remote::LocalIdentity;
    use std::time::Duration;

    fn request(id: u64, name: &str) -> PairingRequest {
        PairingRequest {
            id: RequestId(id),
            client: LocalIdentity::ephemeral().id(),
            machine_name: name.to_string(),
            confirmation: "123456".to_string(),
        }
    }

    fn awaiting(id: u64, name: &str) -> HostView {
        HostView::AwaitingApproval {
            request: request(id, name),
            expires_in: Duration::from_secs(90),
        }
    }

    fn pending(id: u64, name: &str) -> PendingApproval {
        PendingApproval {
            id: RequestId(id),
            machine_name: name.to_string(),
        }
    }

    #[test]
    fn a_new_request_is_raised_once() {
        let mut alert = PairingAlert::default();
        assert_eq!(
            alert.observe(&awaiting(1, "Laptop")),
            Change::Raised(pending(1, "Laptop"))
        );
        assert_eq!(alert.pending(), Some(&pending(1, "Laptop")));
        // The same request on later polls stays quiet.
        assert_eq!(alert.observe(&awaiting(1, "Laptop")), Change::None);
        assert_eq!(alert.observe(&awaiting(1, "Laptop")), Change::None);
    }

    #[test]
    fn idle_and_a_showing_code_raise_nothing() {
        let mut alert = PairingAlert::default();
        assert_eq!(alert.observe(&HostView::Idle), Change::None);
        let showing = HostView::Showing {
            code: "ABCD".into(),
            expires_in: Duration::from_secs(60),
            failed_attempts: 0,
        };
        assert_eq!(alert.observe(&showing), Change::None);
        assert_eq!(alert.pending(), None);
    }

    #[test]
    fn approving_clears_the_banner() {
        let mut alert = PairingAlert::default();
        alert.observe(&awaiting(1, "Laptop"));
        let finishing = HostView::Finishing {
            machine_name: "Laptop".into(),
            confirmation: "123456".into(),
        };
        assert_eq!(alert.observe(&finishing), Change::Cleared);
        assert_eq!(alert.pending(), None);
    }

    #[test]
    fn declining_or_a_timeout_clears_the_banner() {
        for outcome in [Outcome::Declined, Outcome::TimedOut] {
            let mut alert = PairingAlert::default();
            alert.observe(&awaiting(1, "Laptop"));
            assert_eq!(alert.observe(&HostView::Ended(outcome)), Change::Cleared);
            assert_eq!(alert.pending(), None);
        }
    }

    #[test]
    fn a_passed_deadline_clears_even_before_the_host_records_it() {
        let mut alert = PairingAlert::default();
        alert.observe(&awaiting(1, "Laptop"));
        let overdue = HostView::AwaitingApproval {
            request: request(1, "Laptop"),
            expires_in: Duration::ZERO,
        };
        assert_eq!(alert.observe(&overdue), Change::Cleared);
        assert_eq!(alert.pending(), None);
    }

    #[test]
    fn a_later_request_is_raised_again_even_from_the_same_name() {
        let mut alert = PairingAlert::default();
        alert.observe(&awaiting(1, "Laptop"));
        alert.observe(&HostView::Ended(Outcome::Declined));
        assert_eq!(
            alert.observe(&awaiting(2, "Laptop")),
            Change::Raised(pending(2, "Laptop"))
        );
        // A different id replacing the open one without a gap also counts.
        assert_eq!(
            alert.observe(&awaiting(3, "Desktop")),
            Change::Raised(pending(3, "Desktop"))
        );
    }

    #[test]
    fn the_notification_and_banner_name_the_client() {
        let (title, body) = notification_text("Studio");
        assert_eq!(title, "Pairing request");
        assert!(body.contains("\u{201c}Studio\u{201d}"), "{body}");
        assert!(banner_text("Studio").contains("\u{201c}Studio\u{201d}"));
    }
}
