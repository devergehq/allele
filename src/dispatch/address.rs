//! Durable session addressing (DEV-440).
//!
//! ## The problem this exists to remove
//!
//! A session has two names and they are not the same value. Allele's label is
//! mutable — the sidebar renames it, and auto-naming rewrites it a few seconds
//! after the first prompt lands, by design. The Claude Code process name is
//! whatever `--name` said at spawn and is immutable for the life of that
//! process. `SendMessage` and `ListAgents` resolve against the *process* name.
//!
//! So an orchestrator that hands a worker "reply to `<my allele name>`" hands
//! out a string that resolves to nothing, and the worker discovers this only
//! when it has a finished report to send. It then stalls in `awaiting_input`
//! holding work nobody is watching.
//!
//! Allele cannot fix that by keeping the two names in sync. It cannot rename a
//! live Claude process, and it does not own the resolver. **Every design that
//! requires the two names to agree is unbuildable from here.**
//!
//! ## What is used instead
//!
//! Claude Code binds a per-process Unix socket for messaging and names it in
//! the session's own environment as `CLAUDE_CODE_MESSAGING_SOCKET`. Prefixed
//! with `uds:`, that path is an address `SendMessage` accepts directly.
//!
//! It is derived from process identity, never from a display string. That is
//! the whole invariant:
//!
//! > **Nothing on the addressing path reads a label.** Renaming a session
//! > touches `Session::label` and only `Session::label`, so a rename cannot
//! > break inbound or outbound messaging — not because the two are kept in
//! > sync, but because the address never depended on the name in the first
//! > place.
//!
//! Collisions go the same way. Two sessions can carry the same *name*; they
//! cannot carry the same pid, so they cannot carry the same socket. A wrong
//! address here is not a wrong recipient, it is no recipient.
//!
//! ## Verified or null, never guessed
//!
//! The socket directory is a Claude Code implementation detail, and a future
//! version may move it. So a derived path is only ever returned when the
//! socket is actually there. A changed convention degrades this module to
//! "I don't know", which a caller can act on; it never degrades it to a
//! confident wrong answer, which a caller cannot.

use std::path::{Path, PathBuf};

/// Where Claude Code binds its per-process messaging sockets.
///
/// A convention, not a contract — which is why every path built from it is
/// existence-checked before being handed out. Overridable so the fallback can
/// be tested, and so a Claude Code that moves the directory can be pointed at
/// without a new allele build.
const SOCKET_DIR_ENV: &str = "ALLELE_CC_SOCKET_DIR";
const DEFAULT_SOCKET_DIR: &str = "/tmp/cc-socks";

/// The env var Claude Code sets in each session naming that session's own
/// messaging socket. Read from the *MCP server's* environment, where it is
/// inherited from the calling session — the same mechanism that already
/// carries `CLAUDE_CODE_SESSION_ID` (see [`crate::dispatch::mcp`]).
const MESSAGING_SOCKET_ENV: &str = "CLAUDE_CODE_MESSAGING_SOCKET";

fn socket_dir() -> PathBuf {
    std::env::var_os(SOCKET_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_DIR))
}

/// The address of the session whose environment this process inherited.
///
/// Authoritative: the path is read from the environment rather than derived,
/// so it holds whatever the socket directory happens to be. This is the one
/// that matters at dispatch time — it is how allele learns where to tell a
/// worker to send its report.
pub(crate) fn from_env() -> Option<String> {
    let path = std::env::var_os(MESSAGING_SOCKET_ENV)?;
    address_if_bound(Path::new(&path))
}

/// The address of the agent process running under `pid`, if it has one.
///
/// Derived rather than read, because allele's own process never inherits a
/// session's environment — it *is* the parent. `pid` is the PTY child, which
/// for every supported agent is the agent binary itself (alacritty execs it
/// directly; there is no wrapping shell), so it is the pid that names the
/// socket.
///
/// Not cached anywhere. A `/clear` or a cold resume replaces the process and
/// therefore the socket, so this must be answered from the pid a session holds
/// *now*.
pub(crate) fn for_pid(pid: u32) -> Option<String> {
    address_if_bound(&socket_dir().join(format!("{pid}.sock")))
}

/// `uds:<path>` when something is actually bound there, `None` otherwise.
fn address_if_bound(path: &Path) -> Option<String> {
    // `exists()` rather than a connect: this runs on the foreground thread
    // inside `sessions_list`, and a connect to a wedged peer would block the
    // UI. A stale socket file is possible but bounded — Claude Code unlinks on
    // exit, and the worst case is an address that errors at send time, which
    // is the loud failure this module is trying to produce anyway.
    path.exists().then(|| format!("uds:{}", path.display()))
}

/// The address of a live session, resolved from the agent process it is
/// currently running.
///
/// `None` for a suspended session (no process), for a session whose PTY failed
/// to spawn, and for one whose socket allele cannot find. All three are the
/// same answer to a caller — "not reachable right now" — and all three are
/// better than a path that looks right and is not.
pub(crate) fn for_session(session: &crate::session::Session, cx: &gpui::App) -> Option<String> {
    let pid = session.terminal_view.as_ref()?.read(cx).agent_pid()?;
    for_pid(pid)
}

/// The block prepended to a dispatched session's first prompt so it knows how
/// to reach whoever dispatched it (DEV-440).
///
/// Injected rather than left to the dispatcher to remember, because the
/// dispatcher cannot look up its own address: `ListAgents` does not list self,
/// and the name in its sidebar is not the name its process answers to. Every
/// dispatcher that tried to state an address by hand got it wrong.
///
/// The `None` arm is the important one. Failing to supply an address is not
/// silence — the worker is told, in the first thing it reads, that it has no
/// way back and must not invent one. Today that discovery happens minutes
/// later, with a finished report in hand and nowhere to put it.
pub(crate) fn dispatch_preamble(reply_to: Option<&str>) -> String {
    match reply_to {
        Some(address) => format!(
            "[allele] Reply to the session that dispatched you with:\n\
             \x20   SendMessage(to: {address:?}, message: \"…\")\n\
             That address is its process socket. It is durable: renaming a session \
             does not change it. Do NOT address the dispatcher by name — an allele \
             session name and a Claude Code process name are different values, and \
             the name you would guess resolves to nothing or to the wrong session.\n\n"
        ),
        None => "[allele] Your dispatcher supplied no reply address, so you cannot \
             message it. Do NOT guess a name — an allele session name and a Claude \
             Code process name are different values, and a guessed name resolves to \
             nothing or to the wrong session. Leave your report in this session and \
             say plainly that you could not deliver it.\n\n"
            .to_string(),
    }
}

/// The prompt a dispatched session actually receives: the reply-address block,
/// then the caller's brief verbatim.
///
/// A prefix rather than a wrapper. The brief is the last thing the agent reads
/// and is not reworded, indented or truncated on the way through — a dispatch
/// surface that quietly edits the instructions it delivers is unusable.
pub(crate) fn compose_dispatch_prompt(reply_to: Option<&str>, brief: &str) -> String {
    format!("{}{}", dispatch_preamble(reply_to), brief)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole module rests on: a path that is not bound is
    /// never handed out. Everything else degrades gracefully only because of
    /// this — a moved socket directory becomes "unknown", not a wrong address.
    #[test]
    fn an_unbound_path_yields_no_address() {
        assert_eq!(
            address_if_bound(Path::new("/tmp/cc-socks/definitely-not-bound-4408.sock")),
            None
        );
    }

    #[test]
    fn a_bound_path_yields_a_uds_address() {
        let dir = std::env::temp_dir().join(format!("allele-addr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("7.sock");
        std::fs::write(&path, b"").expect("write");

        assert_eq!(
            address_if_bound(&path),
            Some(format!("uds:{}", path.display()))
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A pid with nothing bound is answered honestly rather than optimistically.
    /// `sessions_list` reports this field for every session, including suspended
    /// ones with no process at all.
    #[test]
    fn a_pid_with_no_socket_yields_no_address() {
        assert_eq!(for_pid(0), None);
    }

    /// The no-address arm has to be actionable on its own, because it is the
    /// only thing the worker will ever be told about how to report back.
    #[test]
    fn the_no_address_preamble_forbids_guessing() {
        let preamble = dispatch_preamble(None);
        assert!(preamble.contains("Do NOT guess a name"));
        assert!(!preamble.contains("SendMessage(to:"));
    }

    /// The address must appear in a form the worker can copy verbatim.
    #[test]
    fn the_preamble_quotes_the_address_for_copying() {
        let preamble = dispatch_preamble(Some("uds:/tmp/cc-socks/65452.sock"));
        assert!(preamble.contains(r#"SendMessage(to: "uds:/tmp/cc-socks/65452.sock""#));
    }

    /// The preamble is a prefix, not a wrapper: the agent's actual brief has to
    /// survive it intact and be the last thing read.
    #[test]
    fn the_preamble_ends_in_a_blank_line_so_the_brief_stands_alone() {
        assert!(dispatch_preamble(Some("uds:/x.sock")).ends_with("\n\n"));
        assert!(dispatch_preamble(None).ends_with("\n\n"));
    }

    /// The brief must arrive byte-for-byte. A dispatch surface that reworded
    /// the instructions it delivered would be worse than no dispatch surface.
    #[test]
    fn the_brief_survives_composition_verbatim() {
        let brief = "Read https://example.test/brief\nThen do the thing.";
        let composed = compose_dispatch_prompt(Some("uds:/tmp/cc-socks/1.sock"), brief);
        assert!(composed.ends_with(brief));
        assert!(composed.starts_with("[allele] "));
    }

    /// Composition is total: a dispatch with no reachable caller still gets a
    /// prompt, and still gets told why it cannot report back.
    #[test]
    fn a_brief_is_still_delivered_without_a_reply_address() {
        let composed = compose_dispatch_prompt(None, "do the thing");
        assert!(composed.ends_with("do the thing"));
        assert!(composed.contains("no reply address"));
    }
}
