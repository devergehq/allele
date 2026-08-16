//! `Allele --mcp-serve` — the stdio MCP server (DEV-415).
//!
//! Runs the *same binary* as a JSON-RPC 2.0 server on stdin/stdout, proxying
//! every tool call to the running app over the control socket. Registered in
//! `~/.claude.json` as:
//!
//! ```json
//! { "command": "/Applications/Allele.app/Contents/MacOS/Allele",
//!   "args": ["--mcp-serve"] }
//! ```
//!
//! Same binary rather than a second one so there is no extra artifact to ship,
//! no PATH to get wrong, and no way for the two halves to drift out of version
//! with each other.
//!
//! This process opens **no window**. It is a client of the running Allele, and
//! if none is running it says so: `connect()` fails immediately and the caller
//! gets `allele_not_running` rather than a timeout. That is the whole reason
//! the transport is a socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use serde_json::{json, Value};

use super::protocol::{ErrorCode, Response};
use super::server::socket_path;

/// MCP protocol revision this server implements.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Take over the process as an MCP server when `--mcp-serve` was passed, and
/// never return; otherwise do nothing and let the app launch normally.
///
/// Checked before any GUI setup: this mode must open no window. It is a client
/// of a *running* Allele, not a second one.
pub fn exit_if_serving() {
    if !std::env::args().any(|arg| arg == "--mcp-serve") {
        return;
    }
    // Before anything can write to stdout. Stdout is the JSON-RPC frame
    // stream here, and one stray log line desynchronises the client's parser
    // — a failure that looks like a malformed server rather than a log.
    crate::errors::init_tracing_stderr();
    std::process::exit(serve());
}

/// Run the server until stdin closes. Returns the process exit code.
fn serve() -> i32 {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue; // unparseable frame; MCP has no way to attribute it
        };

        // Notifications carry no id and must not be answered at all.
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));

        let body = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "allele", "version": env!("CARGO_PKG_VERSION") },
            })),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => call_tool(&params),
            _ => Err(format!("unknown method: {method}")),
        };

        let response = match body {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(message) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32603, "message": message },
            }),
        };

        if writeln!(stdout, "{response}").is_err() || stdout.flush().is_err() {
            break;
        }
    }
    0
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "allele_projects_list",
            "description": "List allele projects a session can be created in.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        },
        {
            "name": "allele_sessions_list",
            "description":
                "List allele sessions with their state. Use this rather than ListAgents to \
                 decide whether a worker is finished: ListAgents collapses every state into \
                 idle/busy, so a session blocked on a permission prompt is indistinguishable \
                 there from one that has finished.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        },
        {
            "name": "allele_sessions_status",
            "description":
                "State of one allele session. `awaiting_input` means it is blocked on a \
                 permission prompt and nobody is coming unless a human acts; `state_age_secs` \
                 says how long it has been that way.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The allele session id — the only durable identity. \
                                        Names are stable in practice; SendMessage refs are not.",
                    },
                },
                "required": ["session_id"],
                "additionalProperties": false,
            },
        },
        {
            "name": "allele_sessions_create",
            "description":
                "Create an allele session and send it an initial prompt. Returns the session \
                 id and the name allele minted, which may differ from the one requested. \
                 The result is NOT a SendMessage address: resolve the name to `name [ref]` \
                 via ListAgents, fresh at every send, because refs rotate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project name or id." },
                    "name": { "type": "string", "description": "Requested session name." },
                    "prompt": {
                        "type": "string",
                        "description": "Sent to the agent once it is up. Prefer a short \
                                        orientation and an artifact URL over a long brief.",
                    },
                    "orchestration": {
                        "type": "string",
                        "enum": ["full", "startup_only", "nothing"],
                        "description": "How much of the project's setup to run. Defaults to \
                                        startup_only: run the startup command so tests have \
                                        what they need, without opening drawer terminals.",
                    },
                },
                "required": ["project", "name", "prompt"],
                "additionalProperties": false,
            },
        },
        {
            "name": "allele_sessions_discard",
            "description":
                "Discard an allele session that was created by an agent, freeing a slot \
                 against the dispatch cap. Non-destructive: uncommitted work is committed \
                 and the session branch is archived before its workspace is removed, and \
                 the project's shutdown command runs if its startup did. Refuses sessions \
                 a human started — those are theirs to remove.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The allele session id, as returned by \
                                        sessions_create or sessions_list.",
                    },
                },
                "required": ["session_id"],
                "additionalProperties": false,
            },
        },
    ])
}

fn call_tool(params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call is missing `name`")?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let request = match name {
        "allele_projects_list" => json!({ "op": "projects_list" }),
        "allele_sessions_list" => json!({ "op": "sessions_list" }),
        "allele_sessions_status" => json!({
            "op": "sessions_status",
            "session_id": args.get("session_id").and_then(Value::as_str).unwrap_or_default(),
        }),
        "allele_sessions_discard" => json!({
            "op": "sessions_discard",
            "session_id": args.get("session_id").and_then(Value::as_str).unwrap_or_default(),
        }),
        "allele_sessions_create" => {
            let mut v = json!({ "op": "sessions_create" });
            // This process is a child of the calling Claude session, so its
            // environment names that session. Allele resolves the id against
            // its own records and reads the depth from there — the claim
            // cannot grant a depth the named session does not have.
            if let Ok(id) = std::env::var("CLAUDE_CODE_SESSION_ID") {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("caller_session_id".into(), json!(id));
                }
            }
            if let (Some(obj), Some(a)) = (v.as_object_mut(), args.as_object()) {
                for (k, val) in a {
                    obj.insert(k.clone(), val.clone());
                }
            }
            v
        }
        other => return Err(format!("unknown tool: {other}")),
    };

    let response = round_trip(&request)?;
    let is_error = matches!(response, Response::Error { .. });
    let text = serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    }))
}

/// One request over the control socket, one response back.
///
/// A fresh connection per call: calls are infrequent, and a dead connection to
/// an Allele that has since quit is a worse failure than the cost of
/// reconnecting.
fn round_trip(request: &Value) -> Result<Response, String> {
    let path = socket_path().ok_or("no home directory")?;
    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => {
            // The failure the socket exists to make legible. A spool write
            // would have "succeeded" here and left the caller to time out.
            return Ok(Response::Error {
                code: ErrorCode::AlleleNotRunning,
                message: format!(
                    "Allele is not running, or is not serving {}",
                    path.display()
                ),
            });
        }
    };

    let mut out = stream.try_clone().map_err(|e| e.to_string())?;
    let mut line = serde_json::to_string(request).map_err(|e| e.to_string())?;
    line.push('\n');
    out.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;

    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| e.to_string())?;
    if reply.trim().is_empty() {
        return Err("Allele closed the connection without replying".to_string());
    }
    serde_json::from_str(&reply).map_err(|e| format!("could not parse reply: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every advertised tool must map to a control-socket op — an MCP client
    /// that calls a listed tool and gets "unknown tool" is worse than one that
    /// never saw it.
    #[test]
    fn every_advertised_tool_is_dispatchable() {
        let tools = tool_definitions();
        for tool in tools.as_array().expect("array") {
            let name = tool["name"].as_str().expect("name");
            let args = match name {
                "allele_sessions_status" | "allele_sessions_discard" => {
                    json!({ "session_id": "x" })
                }
                "allele_sessions_create" => {
                    json!({ "project": "p", "name": "n", "prompt": "go" })
                }
                _ => json!({}),
            };
            let params = json!({ "name": name, "arguments": args });
            // Reaches the socket and fails there (no app in a test), which is
            // past the routing this asserts.
            let err = call_tool(&params).err();
            assert!(
                err.is_none() || !err.as_deref().unwrap_or("").starts_with("unknown tool"),
                "{name} is advertised but not routed"
            );
        }
    }

    /// The destructive tool must be advertised with a session id and nothing
    /// else — no "all", no project-wide sweep. One session per call is what
    /// keeps a confused caller's blast radius to one session.
    #[test]
    fn discard_takes_exactly_one_session_id() {
        let tools = tool_definitions();
        let discard = tools
            .as_array()
            .expect("array")
            .iter()
            .find(|t| t["name"] == "allele_sessions_discard")
            .expect("discard is advertised");
        let props = discard["inputSchema"]["properties"]
            .as_object()
            .expect("properties");
        assert_eq!(props.len(), 1, "discard takes one argument");
        assert!(props.contains_key("session_id"));
        assert_eq!(discard["inputSchema"]["required"][0], "session_id");
        assert_eq!(discard["inputSchema"]["additionalProperties"], false);
    }

    #[test]
    fn unknown_tools_are_rejected() {
        let params = json!({ "name": "allele_delete_everything", "arguments": {} });
        assert!(call_tool(&params).unwrap_err().starts_with("unknown tool"));
    }

    /// The create tool must not advertise a permission-mode parameter: it was
    /// descoped (DEV-413) and a schema promising it would be a lie an
    /// orchestrator plans around.
    #[test]
    fn create_advertises_no_permission_mode() {
        let tools = serde_json::to_string(&tool_definitions()).expect("serialises");
        assert!(!tools.contains("permission_mode"));
        assert!(!tools.contains("depth"), "depth is derived, never supplied");
    }
}
