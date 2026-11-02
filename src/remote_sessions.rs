//! Starting, interrupting and discarding sessions on a paired Mac (DEV-1168,
//! proposal §3.8).
//!
//! ```text
//!   New Session picker ── RemoteCreate ──► sessions_create ─┐
//!   remote row menu ──── Interrupt ──────► sessions_interrupt ├─► RemoteService::call
//!                   └─── Discard ────────► sessions_discard ─┘   (the follower's
//!                                                                 control stream)
//!   answer ─► Created: select the new row (attach is DEV-1167)
//!          └► error:   the warning banner, as a local create's error is
//! ```
//!
//! **Human-initiated only.** Everything here starts from a click in this
//! Mac's window. Neither `control.sock` nor the MCP server can reach
//! [`crate::remote::RemoteService::call`] — `tests/architecture.rs` keeps
//! `src/dispatch/` from naming it — so an agent cannot start sessions on
//! another Mac, and a remote create's depth-0 on the host stays honest
//! (DEV-1187).
//!
//! The host decides everything that matters: its dispatch cap, its project
//! list, and this Mac's grant, re-read on every request. What this Mac knows
//! of the grant (from the host's hello) only decides what the UI offers.

use gpui::{Context, Pixels, Point};

use crate::app_state::AppState;
use crate::dispatch::protocol::{CreateRequest, ErrorCode, ProjectSummary, Request, Response};
use crate::remote::follow::CallError;
use crate::remote::peers::{PeerPermissions, TrustedPeer};
use crate::remote::PeerId;
use crate::status_feed::client::Reachability;

/// One session on one host.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RemoteSessionKey {
    pub peer: PeerId,
    pub session_id: String,
}

/// A create the New Session picker asked for.
#[derive(Clone, Debug)]
pub(crate) struct RemoteCreate {
    pub peer: PeerId,
    pub host_name: String,
    pub request: CreateRequest,
}

/// What a click on a remote session asks for. Queued as a
/// [`crate::actions::SessionAction::Remote`].
#[derive(Debug)]
pub(crate) enum RemoteSessionAction {
    Create(RemoteCreate),
    Interrupt {
        key: RemoteSessionKey,
        label: String,
        host_name: String,
    },
    Discard {
        key: RemoteSessionKey,
        label: String,
        host_name: String,
    },
    /// Stand-in for opening the session attached (DEV-1167): highlight it.
    Select(RemoteSessionKey),
}

/// A remote session's right-click menu.
#[derive(Clone, Debug)]
pub(crate) struct RemoteMenu {
    pub key: RemoteSessionKey,
    pub label: String,
    pub host_name: String,
    pub position: Point<Pixels>,
    /// Discard was clicked once; the menu now asks to confirm.
    pub confirming_discard: bool,
}

/// A create sent and not yet answered, drawn under its host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartingRemote {
    pub peer: PeerId,
    pub label: String,
    token: u64,
}

/// The remote-session UI state on `AppState`.
#[derive(Default)]
pub(crate) struct RemoteUi {
    /// The remote row last opened. Attach (DEV-1167) will replace this.
    pub selected: Option<RemoteSessionKey>,
    pub starting: Vec<StartingRemote>,
    pub menu: Option<RemoteMenu>,
    next_token: u64,
}

impl RemoteUi {
    fn start(&mut self, peer: PeerId, label: String) -> u64 {
        self.next_token += 1;
        let token = self.next_token;
        self.starting.push(StartingRemote { peer, label, token });
        token
    }

    fn finish(&mut self, token: u64) {
        self.starting.retain(|s| s.token != token);
    }

    /// Creates still waiting on `peer`, for its sidebar rows.
    pub(crate) fn starting_on(&self, peer: PeerId) -> impl Iterator<Item = &StartingRemote> {
        self.starting.iter().filter(move |s| s.peer == peer)
    }
}

/// A host the New Session picker offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PickerHost {
    pub id: PeerId,
    pub name: String,
    /// The host granted this Mac `control`. Without it the picker still lists
    /// the host — it is connected, and hiding it would look like a bug — but
    /// explains why Create is unavailable.
    pub can_create: bool,
}

/// The hosts the picker lists after "This Mac": every paired host whose feed
/// is live, in pairing order. An unreachable or still-connecting host is left
/// out — creating there is out of scope, and nothing could answer.
pub(crate) fn picker_hosts(
    peers: &[TrustedPeer],
    reachability: impl Fn(PeerId) -> Option<Reachability>,
    grant: impl Fn(PeerId) -> Option<PeerPermissions>,
) -> Vec<PickerHost> {
    peers
        .iter()
        .filter(|p| reachability(p.id) == Some(Reachability::Connected))
        .map(|p| PickerHost {
            id: p.id,
            name: p.machine_name.clone(),
            can_create: grant(p.id).is_some_and(|g| g.control),
        })
        .collect()
}

/// Which of a host's projects to preselect: the one named like the local
/// project the modal was opened from (ignoring case), else the first.
pub(crate) fn default_project(projects: &[ProjectSummary], local_name: &str) -> usize {
    projects
        .iter()
        .position(|p| p.name.eq_ignore_ascii_case(local_name))
        .unwrap_or(0)
}

/// Whether a remote row offers Interrupt and Discard: the host is live and
/// granted this Mac `control`. A `view`-only Mac sees no menu at all.
pub(crate) fn can_manage(connected: bool, grant: Option<PeerPermissions>) -> bool {
    connected && grant.is_some_and(|g| g.control)
}

/// The banner text for a remote op that did not succeed, or `None` when it
/// did. Host refusals keep the host's own words, prefixed with its name so
/// the banner says which Mac refused — except `not_dispatched`, whose host
/// wording speaks to an agent, not a person at another Mac.
pub(crate) fn failure_text(
    host: &str,
    session_label: Option<&str>,
    result: &Result<Response, CallError>,
) -> Option<String> {
    match result {
        Err(e) => Some(format!("{host} {e}.")),
        Ok(Response::Error {
            code: ErrorCode::NotDispatched,
            ..
        }) => Some(format!(
            "{} was started by hand on {host}, so it can only be interrupted or \
             discarded there.",
            session_label.map_or("That session".to_string(), |l| format!(
                "\u{201c}{l}\u{201d}"
            ))
        )),
        Ok(Response::Error {
            code: ErrorCode::PermissionDenied,
            ..
        }) => Some(format!(
            "{host} doesn't let this Mac change its sessions. Turn on control for this Mac \
             in {host}'s Settings → Machines."
        )),
        Ok(Response::Error { message, .. }) => Some(format!("{host}: {message}")),
        Ok(_) => None,
    }
}

impl AppState {
    /// What the New Session picker offers for the project at `project_idx`.
    pub(crate) fn machine_choices(
        &self,
        project_idx: usize,
    ) -> crate::new_session_modal::MachineChoices {
        let feeds = self.remote.hosts();
        crate::new_session_modal::MachineChoices {
            hosts: picker_hosts(
                &self.remote.paired(),
                |id| feeds.get(id).map(|h| h.reachability()),
                |id| feeds.grant(id),
            ),
            remote: self.remote.clone(),
            project_name: self
                .projects
                .get(project_idx)
                .map(|p| p.name.clone())
                .unwrap_or_default(),
        }
    }

    pub(crate) fn handle_remote_session_action(
        &mut self,
        action: RemoteSessionAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            RemoteSessionAction::Create(create) => self.create_remote_session(create, cx),
            RemoteSessionAction::Interrupt {
                key,
                label,
                host_name,
            } => {
                let op = Request::SessionsInterrupt {
                    session_id: key.session_id.clone(),
                };
                self.manage_remote_session(key, label, host_name, op, cx);
            }
            RemoteSessionAction::Discard {
                key,
                label,
                host_name,
            } => {
                let op = Request::SessionsDiscard {
                    session_id: key.session_id.clone(),
                };
                self.manage_remote_session(key, label, host_name, op, cx);
            }
            RemoteSessionAction::Select(key) => {
                self.remote_ui.selected = Some(key);
                cx.notify();
            }
        }
    }

    fn create_remote_session(&mut self, create: RemoteCreate, cx: &mut Context<Self>) {
        let RemoteCreate {
            peer,
            host_name,
            request,
        } = create;
        let token = self.remote_ui.start(peer, request.name.clone());
        let answer = self.remote.call(peer, Request::SessionsCreate(request));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = answer.await;
            let _ = this.update(cx, |this, cx| {
                this.remote_ui.finish(token);
                if let Ok(Response::Created { session }) = &result {
                    // "Opens attached" is DEV-1167; until then the new row is
                    // selected, which is where attach will start from. The
                    // row itself arrives on the status feed.
                    this.remote_ui.selected = Some(RemoteSessionKey {
                        peer,
                        session_id: session.session_id.clone(),
                    });
                }
                if let Some(text) = failure_text(&host_name, None, &result) {
                    this.pull_warning = Some(text);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn manage_remote_session(
        &mut self,
        key: RemoteSessionKey,
        label: String,
        host_name: String,
        op: Request,
        cx: &mut Context<Self>,
    ) {
        let discarding = matches!(op, Request::SessionsDiscard { .. });
        let answer = self.remote.call(key.peer, op);
        cx.spawn(async move |this, cx| {
            let result = answer.await;
            let _ = this.update(cx, |this, cx| {
                if discarding
                    && matches!(result, Ok(Response::Discarded { .. }))
                    && this.remote_ui.selected.as_ref() == Some(&key)
                {
                    this.remote_ui.selected = None;
                }
                // Success needs no message: the row's status (or its
                // disappearing) arrives on the feed.
                if let Some(text) = failure_text(&host_name, Some(&label), &result) {
                    this.pull_warning = Some(text);
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::protocol::{CreatedSession, SessionState};

    fn peer() -> PeerId {
        crate::remote::LocalIdentity::ephemeral().id()
    }

    const CONTROL: PeerPermissions = PeerPermissions {
        view: true,
        control: true,
    };

    #[test]
    fn the_picker_lists_connected_hosts_in_pairing_order() {
        let (desk, studio, attic) = (peer(), peer(), peer());
        let peers = [
            TrustedPeer::newly_paired(desk, "Desk"),
            TrustedPeer::newly_paired(studio, "Studio"),
            TrustedPeer::newly_paired(attic, "Attic"),
        ];
        let hosts = picker_hosts(
            &peers,
            |id| {
                Some(if id == studio {
                    Reachability::Unreachable {
                        since: std::time::SystemTime::now(),
                    }
                } else {
                    Reachability::Connected
                })
            },
            |id| (id == desk).then_some(CONTROL),
        );
        let names: Vec<_> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["Desk", "Attic"], "an unreachable host is left out");
        assert!(hosts[0].can_create);
        assert!(!hosts[1].can_create, "view only: listed, but cannot create");
    }

    #[test]
    fn a_host_not_yet_heard_from_is_not_offered() {
        let peers = [TrustedPeer::newly_paired(peer(), "Desk")];
        assert!(picker_hosts(&peers, |_| Some(Reachability::Connecting), |_| None).is_empty());
        assert!(picker_hosts(&peers, |_| None, |_| None).is_empty());
    }

    #[test]
    fn the_host_project_named_like_the_local_one_is_preselected() {
        let project = |name: &str| ProjectSummary {
            id: name.into(),
            name: name.into(),
            source_path: String::new(),
            session_count: 0,
        };
        let list = [project("web"), project("Allele"), project("api")];
        assert_eq!(default_project(&list, "allele"), 1);
        assert_eq!(default_project(&list, "elsewhere"), 0);
    }

    #[test]
    fn interrupt_and_discard_need_control_on_a_live_host() {
        assert!(can_manage(true, Some(CONTROL)));
        assert!(
            !can_manage(true, Some(PeerPermissions::NEW_PEER)),
            "view only"
        );
        assert!(!can_manage(true, None), "never greeted");
        assert!(!can_manage(false, Some(CONTROL)), "unreachable");
    }

    #[test]
    fn a_hosts_cap_error_is_shown_in_its_own_words() {
        let capped = Ok(Response::Error {
            code: ErrorCode::CapacityExceeded,
            message: "6 dispatched sessions already running (limit 6)".into(),
        });
        assert_eq!(
            failure_text("Desk", None, &capped).as_deref(),
            Some("Desk: 6 dispatched sessions already running (limit 6)")
        );
        let unknown = Ok(Response::Error {
            code: ErrorCode::ProjectNotRegistered,
            message: "no project named web".into(),
        });
        assert_eq!(
            failure_text("Desk", None, &unknown).as_deref(),
            Some("Desk: no project named web")
        );
    }

    #[test]
    fn permission_and_human_session_refusals_speak_to_a_person() {
        let denied = Ok(Response::Error {
            code: ErrorCode::PermissionDenied,
            message: "needs control".into(),
        });
        assert!(failure_text("Desk", None, &denied)
            .unwrap()
            .contains("Turn on control for this Mac in Desk's Settings"));
        let by_hand = Ok(Response::Error {
            code: ErrorCode::NotDispatched,
            message: "not an agent's to do".into(),
        });
        let text = failure_text("Desk", Some("fix tests"), &by_hand).unwrap();
        assert!(text.contains("\u{201c}fix tests\u{201d} was started by hand on Desk"));
        assert!(!text.contains("agent"));
    }

    #[test]
    fn transport_failures_name_the_host() {
        assert_eq!(
            failure_text("Desk", None, &Err(CallError::Unreachable)).as_deref(),
            Some("Desk isn't connected right now.")
        );
    }

    #[test]
    fn success_shows_nothing() {
        let created = Ok(Response::Created {
            session: CreatedSession {
                session_id: "s".into(),
                name: "n".into(),
                project: "p".into(),
                state: SessionState::Running,
                reply_to: None,
            },
        });
        assert_eq!(failure_text("Desk", None, &created), None);
    }

    #[test]
    fn a_starting_row_lasts_until_its_answer() {
        let (desk, studio) = (peer(), peer());
        let mut ui = RemoteUi::default();
        let a = ui.start(desk, "one".into());
        let _b = ui.start(desk, "two".into());
        ui.start(studio, "elsewhere".into());
        assert_eq!(ui.starting_on(desk).count(), 2);
        ui.finish(a);
        let left: Vec<_> = ui.starting_on(desk).map(|s| s.label.as_str()).collect();
        assert_eq!(left, ["two"]);
    }
}
