//! opencode event-integration plugin.
//!
//! opencode has no `--settings`-style hook flag like Claude Code. Instead it
//! auto-loads JS/TS plugins from its global plugin directory
//! (`$XDG_CONFIG_HOME/opencode/plugins/`, default `~/.config/opencode/plugins/`).
//! We install a tiny plugin there that subscribes to opencode's event bus and
//! normalises the events we care about into allele's shared per-session JSONL
//! transport (`~/.allele/events/<session_id>.jsonl`) — exactly the same file
//! format Claude's shell receiver writes.
//!
//! The plugin can't know allele's session id on its own (opencode mints its
//! own ids), so [`super::OpencodeAdapter::event_integration`] passes
//! `ALLELE_SESSION_ID` (and `ALLELE_EVENTS_DIR`) as PTY env vars; the plugin
//! reads them from `process.env`. Each session runs in its own clone/process,
//! so the mapping is unambiguous.
//!
//! Native → canonical `kind` mapping performed by the plugin:
//! - `session.created`            → `session_start`  (Idle)
//! - user message / `tool.execute.before` / `permission.replied` → `busy` (Running)
//! - `permission.asked`           → `awaiting_input` (AwaitingInput, carries tool/message)
//! - `session.idle` / `session.error` → `turn_complete` (ResponseReady)

use std::path::PathBuf;

/// Plugin source. Written verbatim to disk; regenerated on every startup so
/// the on-disk copy always matches the shipped version. Do not edit the
/// installed copy by hand.
const PLUGIN_JS: &str = r#"// allele opencode events plugin — managed by the Allele app.
// Forwards opencode lifecycle events to allele's per-session JSONL files
// under $ALLELE_EVENTS_DIR (default ~/.allele/events). Regenerated on every
// Allele launch — do not edit by hand.
import { appendFileSync, mkdirSync, existsSync, writeFileSync } from "node:fs"
import { join } from "node:path"
import { homedir } from "node:os"

export const AlleleEvents = async () => {
  const sessionId = process.env.ALLELE_SESSION_ID
  // No allele session id => this opencode wasn't launched by allele; do nothing.
  if (!sessionId) return {}

  const eventsDir =
    process.env.ALLELE_EVENTS_DIR || join(homedir(), ".allele", "events")
  try { mkdirSync(eventsDir, { recursive: true }) } catch (_) {}

  const outFile = join(eventsDir, sessionId + ".jsonl")
  const promptFile = join(eventsDir, sessionId + ".prompt")

  const emit = (kind, extra) => {
    const line = Object.assign(
      { ts: Math.floor(Date.now() / 1000), kind },
      extra || {}
    )
    try { appendFileSync(outFile, JSON.stringify(line) + "\n") } catch (_) {}
  }

  // Capture the first user prompt so allele's auto-naming can pick it up,
  // mirroring what Claude's receiver writes on user_prompt_submit.
  const capturePrompt = (text) => {
    if (!text || typeof text !== "string") return
    try { if (!existsSync(promptFile)) writeFileSync(promptFile, text) } catch (_) {}
  }

  // opencode fires session.idle (and others) for sub-agent sessions too.
  // Latch the first session id we see and ignore events tagged with a
  // different one, so a sub-agent finishing doesn't flip the parent's status.
  let rootSessionId = null
  const sidOf = (props) =>
    (props && (props.sessionID || props.sessionId ||
      (props.session && props.session.id) ||
      (props.info && props.info.sessionID))) || null
  const isForeign = (props) => {
    const sid = sidOf(props)
    return rootSessionId && sid && sid !== rootSessionId
  }

  const textOfMessage = (info) => {
    if (!info) return null
    if (typeof info.text === "string") return info.text
    if (Array.isArray(info.parts)) {
      const t = info.parts
        .map((p) => (p && typeof p.text === "string" ? p.text : ""))
        .join("")
      if (t) return t
    }
    return null
  }

  return {
    event: async ({ event }) => {
      const type = event && event.type
      const props = (event && event.properties) || {}
      if (!type) return

      switch (type) {
        case "session.created": {
          if (!rootSessionId) rootSessionId = sidOf(props)
          emit("session_start")
          break
        }
        case "message.updated":
        case "message.part.updated": {
          if (isForeign(props)) break
          // Only a *user* message marks a new turn starting; assistant
          // streaming (and post-turn summary/title generation) must not
          // flip a completed session back to Running.
          const info = props.info || props.message || {}
          const role = info.role || (props.part && props.part.role)
          if (role === "user") {
            emit("busy")
            capturePrompt(textOfMessage(info))
          }
          break
        }
        case "tool.execute.before": {
          if (isForeign(props)) break
          emit("busy")
          break
        }
        case "permission.asked": {
          if (isForeign(props)) break
          const info = props.permission || props
          const toolName =
            info.tool || info.pattern || info.type || undefined
          const message =
            info.title || info.message || "Permission required"
          emit("awaiting_input", { tool_name: toolName, message })
          break
        }
        case "permission.replied": {
          if (isForeign(props)) break
          emit("busy")
          break
        }
        case "session.idle":
        case "session.error": {
          if (isForeign(props)) break
          emit("turn_complete")
          break
        }
        default:
          break
      }
    },
  }
}
"#;

/// Absolute path to opencode's global plugin directory. Honours
/// `XDG_CONFIG_HOME`, falling back to `~/.config`. opencode autoloads every
/// `.js`/`.ts` file in `<config>/opencode/plugins/` when a session is created
/// (verified empirically against opencode 1.17.18).
fn plugin_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("opencode").join("plugins"))
}

/// Write the allele events plugin into opencode's global plugin directory.
/// Idempotent — always rewrites so the on-disk copy tracks the shipped
/// source. Safe to call on every startup.
pub fn install() -> std::io::Result<()> {
    let dir = plugin_dir().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no config directory")
    })?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("allele-events.js"), PLUGIN_JS)?;
    Ok(())
}
