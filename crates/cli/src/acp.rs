//! `almighty acp -- <agent command>`: a small client for the Agent Client
//! Protocol (JSON-RPC 2.0, one message per line over the agent's stdio).
//!
//! It makes any ACP agent usable from an almighty terminal: prompts are read
//! from this terminal, the agent's text, plans and tool calls are printed,
//! and permission requests are put to the person at the keyboard. Inside an
//! almighty session the tool calls are also reported to the server.
//!
//! The adapter offers the agent no file or terminal access of its own.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};

use crate::Client;

const PROTOCOL_VERSION: u64 = 1;

/// What to answer when the agent asks for permission and nobody can be asked.
#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum Policy {
    /// Ask at the keyboard (refuse when there is no keyboard).
    Ask,
    /// Refuse every request.
    Deny,
    /// Allow every request, once each. Only for agents you trust in a place you can lose.
    Allow,
}

struct Agent {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    policy: Policy,
    /// Reports tool calls to the server when running inside an almighty session.
    report: Option<(Client, String)>,
    /// Tool calls by id, to print a title when only an update arrives.
    titles: HashMap<String, String>,
    in_message: bool,
}

impl Agent {
    async fn send(&mut self, message: Value) -> Result<()> {
        // One message per line: serde_json never emits raw newlines.
        let line = format!("{message}\n");
        let written = async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.flush().await
        };
        // An agent that already ended can fail this write before any read sees
        // the end of its output (seen on Windows): report it the same way.
        written.await.ok().context("the agent closed its output")
    }

    /// Sends a request and handles everything the agent says until its answer arrives.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await?;
        loop {
            // Output that is not text (a program printing in a legacy code page) is not an agent.
            let line = self.lines.next_line().await.ok().context("the program's output is not text: it is not an ACP agent")?;
            let line = line.context("the agent closed its output")?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            if message.get("method").is_some() {
                self.incoming(&message).await?;
            } else if message["id"] == json!(id) {
                if let Some(error) = message.get("error") {
                    bail!("the agent answered {method} with an error: {}", error["message"].as_str().unwrap_or("no message"));
                }
                return Ok(message["result"].clone());
            }
        }
    }

    /// A notification or a request from the agent.
    async fn incoming(&mut self, message: &Value) -> Result<()> {
        let method = message["method"].as_str().unwrap_or("");
        let params = &message["params"];
        match method {
            "session/update" => self.update(&params["update"]).await,
            "session/request_permission" => {
                let outcome = self.permission(params);
                self.send(json!({ "jsonrpc": "2.0", "id": message["id"], "result": { "outcome": outcome } })).await?;
            }
            // Anything else it asks of the client (files, terminals) is not offered.
            _ if message.get("id").is_some_and(|id| !id.is_null()) => {
                let error = json!({ "code": -32601, "message": format!("{method} is not available in this client") });
                self.send(json!({ "jsonrpc": "2.0", "id": message["id"], "error": error })).await?;
            }
            _ => {}
        }
        Ok(())
    }

    fn end_message(&mut self) {
        if self.in_message {
            println!();
            self.in_message = false;
        }
    }

    async fn update(&mut self, update: &Value) {
        let text = |v: &Value| v["content"]["text"].as_str().unwrap_or_default().to_string();
        match update["sessionUpdate"].as_str().unwrap_or("") {
            "agent_message_chunk" => {
                print!("{}", text(update));
                let _ = std::io::stdout().flush();
                self.in_message = true;
            }
            "agent_thought_chunk" => {}
            "plan" => {
                self.end_message();
                println!("plan:");
                for entry in update["entries"].as_array().into_iter().flatten() {
                    println!("  [{}] {}", entry["status"].as_str().unwrap_or("pending"), entry["content"].as_str().unwrap_or(""));
                }
            }
            kind @ ("tool_call" | "tool_call_update") => {
                self.end_message();
                let id = update["toolCallId"].as_str().unwrap_or("").to_string();
                if let Some(title) = update["title"].as_str() {
                    self.titles.insert(id.clone(), title.to_string());
                }
                let title = self.titles.get(&id).cloned().unwrap_or_else(|| id.clone());
                let status = update["status"].as_str().unwrap_or(if kind == "tool_call" { "pending" } else { "" });
                if !status.is_empty() {
                    println!("tool: {title} ({status})");
                }
                if let Some((client, session)) = &self.report {
                    let body = json!({ "hook_event_name": "AcpToolCall", "tool_call_id": id, "title": title, "status": status, "kind": update["kind"] });
                    // Reporting must never get in the agent's way.
                    let _ = client.post(&format!("/api/hooks/claude?session={session}"), body).await;
                }
            }
            _ => {}
        }
    }

    /// Chooses one of the options the agent offers. Returns the `outcome` object.
    fn permission(&mut self, params: &Value) -> Value {
        self.end_message();
        let options: Vec<&Value> = params["options"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        let find = |kinds: &[&str]| options.iter().find(|o| kinds.contains(&o["kind"].as_str().unwrap_or(""))).copied();
        let title = params["toolCall"]["title"].as_str().unwrap_or("an action");
        let allow = match self.policy {
            Policy::Allow => true,
            Policy::Deny => false,
            Policy::Ask if !std::io::stdin().is_terminal() => {
                eprintln!("The agent asked for permission ({title}) and nobody can be asked: refused.");
                false
            }
            Policy::Ask => {
                eprint!("The agent wants to: {title}. Allow once? [y/N] ");
                let _ = std::io::stderr().flush();
                let mut answer = String::new();
                let _ = std::io::stdin().read_line(&mut answer);
                matches!(answer.trim(), "y" | "Y" | "yes")
            }
        };
        // "Always" options are never chosen for the person.
        let chosen = if allow { find(&["allow_once"]) } else { find(&["reject_once"]) };
        match chosen.and_then(|o| o["optionId"].as_str()) {
            Some(id) => {
                println!("permission: {title} — {}", if allow { "allowed once" } else { "refused" });
                json!({ "outcome": "selected", "optionId": id })
            }
            None => {
                println!("permission: {title} — refused");
                json!({ "outcome": "cancelled" })
            }
        }
    }
}

pub async fn run(client: Client, command: Vec<String>, prompt: Option<String>, policy: Policy) -> Result<()> {
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot start {}", command[0]))?;
    let stdin = child.stdin.take().context("no stdin")?;
    let lines = BufReader::new(child.stdout.take().context("no stdout")?).lines();
    let report = std::env::var("ALMIGHTY_SESSION").ok().filter(|s| !s.is_empty() && client.token.is_some()).map(|s| (client, s));
    let mut agent = Agent { stdin, lines, next_id: 0, policy, report, titles: HashMap::new(), in_message: false };

    let init = agent
        .request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                // No file or terminal access is offered: the agent works with its own tools.
                "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
                "clientInfo": { "name": "almighty", "title": "almighty", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await?;
    if init["protocolVersion"].as_u64() != Some(PROTOCOL_VERSION) {
        bail!("the agent speaks protocol version {}, this client speaks {PROTOCOL_VERSION}", init["protocolVersion"]);
    }
    if init["authMethods"].as_array().is_some_and(|m| !m.is_empty()) {
        eprintln!("Note: the agent lists sign-in methods; if it refuses to start a session, sign in with its own tool first.");
    }
    let name = init["agentInfo"]["title"].as_str().or(init["agentInfo"]["name"].as_str()).unwrap_or(&command[0]).to_string();
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let session = agent.request("session/new", json!({ "cwd": cwd, "mcpServers": [] })).await?;
    let session_id = session["sessionId"].as_str().context("the agent did not open a session")?.to_string();

    let interactive = prompt.is_none();
    if interactive {
        eprintln!("{name} is ready. Type a prompt and press Enter; an empty line or Ctrl-D ends.");
    }
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut next = prompt;
    loop {
        let text = match next.take() {
            Some(text) => text,
            None if interactive => {
                eprint!("> ");
                let _ = std::io::stderr().flush();
                match input.next_line().await? {
                    Some(line) if !line.trim().is_empty() => line,
                    _ => break,
                }
            }
            None => break,
        };
        let result = agent.request("session/prompt", json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": text }] })).await?;
        agent.end_message();
        match result["stopReason"].as_str().unwrap_or("") {
            "end_turn" => {}
            other => println!("(the agent stopped: {other})"),
        }
    }
    Ok(())
}

/// A stand-in ACP agent used by the tests: answers every prompt with a plan,
/// a tool call that needs permission, and a short message.
pub async fn echo_agent() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    let mut say = async |message: Value| -> Result<()> {
        out.write_all(format!("{message}\n").as_bytes()).await?;
        out.flush().await?;
        Ok(())
    };
    while let Some(line) = lines.next_line().await? {
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let id = req["id"].clone();
        match req["method"].as_str().unwrap_or("") {
            "initialize" => {
                let result = json!({ "protocolVersion": 1, "agentCapabilities": {}, "agentInfo": { "name": "echo", "title": "Echo agent", "version": "0" }, "authMethods": [] });
                say(json!({ "jsonrpc": "2.0", "id": id, "result": result })).await?;
            }
            "session/new" => say(json!({ "jsonrpc": "2.0", "id": id, "result": { "sessionId": "sess_1" } })).await?,
            "session/prompt" => {
                let text = req["params"]["prompt"][0]["text"].as_str().unwrap_or("").to_string();
                let update = |update: Value| json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "sess_1", "update": update } });
                say(update(json!({ "sessionUpdate": "plan", "entries": [{ "content": "Read the prompt", "priority": "high", "status": "completed" }] }))).await?;
                say(update(json!({ "sessionUpdate": "tool_call", "toolCallId": "call_1", "title": "Write notes.txt", "kind": "edit", "status": "pending" }))).await?;
                // Ask before acting, and wait for the client's choice.
                let options = json!([{ "optionId": "yes", "name": "Allow", "kind": "allow_once" }, { "optionId": "no", "name": "Reject", "kind": "reject_once" }]);
                say(json!({ "jsonrpc": "2.0", "id": "perm-1", "method": "session/request_permission",
                    "params": { "sessionId": "sess_1", "toolCall": { "toolCallId": "call_1", "title": "Write notes.txt" }, "options": options } }))
                .await?;
                let mut allowed = false;
                while let Some(line) = lines.next_line().await? {
                    let Ok(answer) = serde_json::from_str::<Value>(&line) else { continue };
                    if answer["id"] == "perm-1" {
                        allowed = answer["result"]["outcome"]["optionId"] == "yes";
                        break;
                    }
                }
                // Something this client does not offer: the agent must get an error, not silence.
                say(json!({ "jsonrpc": "2.0", "id": "fs-1", "method": "fs/read_text_file", "params": { "sessionId": "sess_1", "path": "/etc/hostname" } })).await?;
                let mut refused = false;
                while let Some(line) = lines.next_line().await? {
                    let Ok(answer) = serde_json::from_str::<Value>(&line) else { continue };
                    if answer["id"] == "fs-1" {
                        refused = answer.get("error").is_some();
                        break;
                    }
                }
                let status = if allowed { "completed" } else { "failed" };
                say(update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "call_1", "status": status }))).await?;
                for chunk in ["You said: ", &text, if refused { " (no file access)" } else { " (file access?!)" }] {
                    say(update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": chunk } }))).await?;
                }
                say(json!({ "jsonrpc": "2.0", "id": id, "result": { "stopReason": "end_turn" } })).await?;
            }
            _ if !id.is_null() => say(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "method not found" } })).await?,
            _ => {}
        }
    }
    Ok(())
}
