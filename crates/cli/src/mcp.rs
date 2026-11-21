//! Minimal MCP server over stdio (JSON-RPC 2.0, one message per line) that
//! exposes the terminal control tools to any agent runtime.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::Client;

fn tools() -> Value {
    let session = json!({ "type": "string", "description": "Session id, e.g. s1" });
    json!([
        {
            "name": "terminal_list",
            "description": "List terminal sessions with id, title, size and state.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "terminal_create",
            "description": "Start a command in a new interactive terminal session. Returns the session id.",
            "inputSchema": { "type": "object", "required": ["command"], "properties": {
                "command": { "type": "array", "items": { "type": "string" }, "description": "Program and arguments, e.g. [\"bash\"]" },
                "cwd": { "type": "string" },
                "cols": { "type": "integer" }, "rows": { "type": "integer" },
                "title": { "type": "string" }
            } }
        },
        {
            "name": "terminal_read",
            "description": "Read the rendered screen of a session as text (what a person sees), with cursor position. Set scrollback_lines to also get output that already scrolled off the top.",
            "inputSchema": { "type": "object", "required": ["session"], "properties": {
                "session": session,
                "scrollback_lines": { "type": "integer", "description": "Lines of history to include above the screen (default 0, max 10000)" }
            } }
        },
        {
            "name": "terminal_send_text",
            "description": "Type text into a session. Does not press Enter; use terminal_send_keys for that.",
            "inputSchema": { "type": "object", "required": ["session", "text"], "properties": {
                "session": session, "text": { "type": "string" }
            } }
        },
        {
            "name": "terminal_send_keys",
            "description": "Press keys in a session. Names: Enter, Tab, Esc, Space, Backspace, Up, Down, Left, Right, Home, End, PageUp, PageDown, Insert, Delete, F1-F12; modifiers C- (Ctrl), M- (Alt), S- (Shift), e.g. C-c, M-x, S-Tab; or one literal character.",
            "inputSchema": { "type": "object", "required": ["session", "keys"], "properties": {
                "session": session, "keys": { "type": "array", "items": { "type": "string" } }
            } }
        },
        {
            "name": "terminal_wait",
            "description": "Block until a regex appears on screen, the screen stays unchanged for idle_ms, the process exits, or the timeout passes. Returns the reason and the screen.",
            "inputSchema": { "type": "object", "required": ["session"], "properties": {
                "session": session,
                "pattern": { "type": "string", "description": "Regex searched in the screen text" },
                "idle_ms": { "type": "integer" },
                "timeout_ms": { "type": "integer", "description": "Default 30000" }
            } }
        },
        {
            "name": "terminal_resize",
            "description": "Resize a session.",
            "inputSchema": { "type": "object", "required": ["session", "cols", "rows"], "properties": {
                "session": session, "cols": { "type": "integer" }, "rows": { "type": "integer" }
            } }
        },
        {
            "name": "terminal_close",
            "description": "Kill the process of a session and remove it.",
            "inputSchema": { "type": "object", "required": ["session"], "properties": { "session": session } }
        }
    ])
}

async fn call_tool(client: &Client, name: &str, args: &Value) -> Result<Value> {
    let session = args["session"].as_str().unwrap_or("");
    let path = |suffix: &str| format!("/api/sessions/{session}{suffix}");
    match name {
        "terminal_list" => client.get("/api/sessions").await,
        "terminal_create" => client.post("/api/sessions", args.clone()).await,
        "terminal_read" => {
            let lines = args["scrollback_lines"].as_u64().unwrap_or(0);
            client.get(&path(&format!("/screen?scrollback={lines}"))).await
        }
        "terminal_send_text" => client.post(&path("/input"), json!({ "text": args["text"] })).await,
        "terminal_send_keys" => client.post(&path("/input"), json!({ "keys": args["keys"] })).await,
        "terminal_wait" => {
            let mut body = json!({ "pattern": args["pattern"], "idle_ms": args["idle_ms"] });
            if let Some(t) = args["timeout_ms"].as_u64() {
                body["timeout_ms"] = json!(t);
            }
            client.post(&path("/wait"), body).await
        }
        "terminal_resize" => client.post(&path("/resize"), json!({ "cols": args["cols"], "rows": args["rows"] })).await,
        "terminal_close" => client.call(reqwest::Method::DELETE, &path(""), None).await,
        other => anyhow::bail!("unknown tool {other}"),
    }
}

pub async fn serve(client: Client) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "almighty", "version": env!("CARGO_PKG_VERSION") }
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let name = req["params"]["name"].as_str().unwrap_or("");
                let args = req["params"].get("arguments").cloned().unwrap_or(json!({}));
                // Tool failures are results the model can read, not protocol errors.
                Ok(match call_tool(&client, name, &args).await {
                    Ok(v) => json!({ "content": [{ "type": "text", "text": render(name, &v) }] }),
                    Err(e) => json!({ "content": [{ "type": "text", "text": format!("error: {e:#}") }], "isError": true }),
                })
            }
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        // Notifications carry no id and get no reply.
        let Some(id) = id else { continue };
        let msg = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        };
        stdout.write_all(format!("{msg}\n").as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

/// Screens read better as plain text than as an escaped JSON string.
fn render(tool: &str, v: &Value) -> String {
    let screen = match tool {
        "terminal_read" => Some(v),
        "terminal_wait" => v.get("screen"),
        _ => None,
    };
    match screen {
        Some(s) => {
            let mut head = format!(
                "cursor: row {} col {} | size {}x{} | exited: {}",
                s["cursor_row"], s["cursor_col"], s["cols"], s["rows"], s["exited"]
            );
            if tool == "terminal_wait" {
                head = format!("reason: {} | {head}", v["reason"].as_str().unwrap_or(""));
            }
            let history = match s["scrollback"].as_str().filter(|h| !h.is_empty()) {
                Some(h) => format!("{h}\n--- screen ---\n"),
                None => String::new(),
            };
            format!(
                "{head} | history lines available: {}\n---\n{history}{}",
                s["scrollback_len"],
                s["text"].as_str().unwrap_or("")
            )
        }
        None => v.to_string(),
    }
}
