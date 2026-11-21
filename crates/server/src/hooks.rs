//! Claude Code hook payloads -> agent tree of a session.
//!
//! Lifecycle comes from SubagentStart / SubagentStop. Those payloads do not
//! name the parent or the task, so the preceding `Agent` tool call
//! (PreToolUse) is queued and matched by `subagent_type`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::session::{now_secs, AgentNode, PendingSpawn, Session};

const MAX_EVENTS: usize = 500;

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|x| !x.is_empty())
}

pub fn apply(session: &Session, payload: Value) {
    let event = s(&payload, "hook_event_name").unwrap_or("").to_string();
    match event.as_str() {
        "PreToolUse" => {
            let tool = s(&payload, "tool_name").unwrap_or("");
            if tool == "Agent" || tool == "Task" {
                let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
                session.pending_spawns.lock().unwrap().push(PendingSpawn {
                    subagent_type: s(&input, "subagent_type").unwrap_or("general-purpose").to_string(),
                    description: s(&input, "description").map(str::to_string),
                    // Present when the call was made from inside a subagent.
                    parent: s(&payload, "agent_id").map(str::to_string),
                });
            }
        }
        "SubagentStart" => {
            if let Some(agent_id) = s(&payload, "agent_id") {
                let agent_type = s(&payload, "agent_type").unwrap_or("agent").to_string();
                let mut agents = session.agents.lock().unwrap();
                if let Some(existing) = agents.iter_mut().find(|a| a.agent_id == agent_id) {
                    existing.status = "running".into(); // resumed
                    existing.ended_at = None;
                } else {
                    let mut pending = session.pending_spawns.lock().unwrap();
                    let idx = pending
                        .iter()
                        .position(|p| p.subagent_type == agent_type)
                        .or(if pending.is_empty() { None } else { Some(0) });
                    let spawn = idx.map(|i| pending.remove(i));
                    agents.push(AgentNode {
                        agent_id: agent_id.to_string(),
                        agent_type,
                        parent: spawn.as_ref().and_then(|p| p.parent.clone()),
                        description: spawn.and_then(|p| p.description),
                        status: "running".into(),
                        transcript_path: guess_transcript(&payload, agent_id),
                        started_at: now_secs(),
                        ended_at: None,
                    });
                }
            }
        }
        "SubagentStop" => {
            if let Some(agent_id) = s(&payload, "agent_id") {
                let mut agents = session.agents.lock().unwrap();
                // Internal agents (prompt suggestions etc.) stop without a start: ignore.
                if let Some(a) = agents.iter_mut().find(|a| a.agent_id == agent_id) {
                    a.status = "done".into();
                    a.ended_at = Some(now_secs());
                    if let Some(p) = s(&payload, "agent_transcript_path") {
                        a.transcript_path = Some(p.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    let mut events = session.hook_events.lock().unwrap();
    events.push(payload);
    if events.len() > MAX_EVENTS {
        let extra = events.len() - MAX_EVENTS;
        events.drain(..extra);
    }
}

/// `<project>/<session>.jsonl` -> `<project>/<session>/subagents/agent-<id>.jsonl`
fn guess_transcript(payload: &Value, agent_id: &str) -> Option<String> {
    let parent = Path::new(s(payload, "transcript_path")?);
    let dir = parent.with_extension("");
    let id = agent_id.strip_prefix("agent-").unwrap_or(agent_id);
    Some(dir.join("subagents").join(format!("agent-{id}.jsonl")).to_string_lossy().into_owned())
}

#[derive(Debug, serde::Serialize)]
pub struct TranscriptLine {
    pub role: String,
    pub kind: String, // text | thinking | tool_use | tool_result
    pub text: String,
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Hook payloads are untrusted input: only read transcripts that live under
/// `~/.claude/projects` and are `.jsonl` files.
fn safe_transcript_path(raw: &str) -> Option<PathBuf> {
    let home = crate::home_dir()?;
    let expanded = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("~\\"))
        .map(|r| format!("{home}/{r}"))
        .unwrap_or_else(|| raw.to_string());
    let real = std::fs::canonicalize(expanded).ok()?;
    let root = std::fs::canonicalize(Path::new(&home).join(".claude/projects")).ok()?;
    (real.starts_with(&root) && real.extension().is_some_and(|e| e == "jsonl")).then_some(real)
}

pub fn read_transcript(raw_path: &str, max_lines: usize) -> Option<Vec<TranscriptLine>> {
    let path = safe_transcript_path(raw_path)?;
    let data = std::fs::read_to_string(path).ok()?;
    let mut out = Vec::new();
    for line in data.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let Some(msg) = v.get("message") else { continue };
        let role = s(msg, "role").unwrap_or("?").to_string();
        match msg.get("content") {
            Some(Value::String(t)) => out.push(TranscriptLine { role, kind: "text".into(), text: clip(t, 4000) }),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    let (kind, text) = match s(b, "type").unwrap_or("") {
                        "text" => ("text", s(b, "text").unwrap_or("").to_string()),
                        "thinking" => ("thinking", s(b, "thinking").unwrap_or("").to_string()),
                        "tool_use" => (
                            "tool_use",
                            format!(
                                "{} {}",
                                s(b, "name").unwrap_or("tool"),
                                clip(&b.get("input").map(Value::to_string).unwrap_or_default(), 600)
                            ),
                        ),
                        "tool_result" => {
                            let c = match b.get("content") {
                                Some(Value::String(t)) => t.clone(),
                                Some(Value::Array(parts)) => parts
                                    .iter()
                                    .filter_map(|p| s(p, "text"))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                _ => String::new(),
                            };
                            ("tool_result", clip(&c, 1500))
                        }
                        _ => continue,
                    };
                    if !text.is_empty() {
                        out.push(TranscriptLine { role: role.clone(), kind: kind.into(), text: clip(&text, 4000) });
                    }
                }
            }
            _ => {}
        }
    }
    let skip = out.len().saturating_sub(max_lines);
    Some(out.into_iter().skip(skip).collect())
}
