//! Slack connector: recent messages of the conversations the token's owner
//! is in, as items. Direct messages and mentions carry triage hints.

wit_bindgen::generate!({ world: "plugin", path: "../../wit" });

use almighty::plugin::host;
use serde_json::{json, Value};

struct Slack;

const MAX_CONVERSATIONS: usize = 40;

fn config() -> Value {
    serde_json::from_str(&host::config()).unwrap_or(Value::Null)
}

fn api_base(cfg: &Value) -> String {
    cfg["api"].as_str().unwrap_or("https://slack.com/api").trim_end_matches('/').to_string()
}

/// Calls a Web API method. Slack answers 200 with `ok: false` on failure.
fn call(base: &str, token: &str, method: &str, query: &str) -> Result<Value, String> {
    let response = host::http_fetch(&host::HttpRequest {
        method: "GET".into(),
        url: format!("{base}/{method}?{query}"),
        headers: vec![("authorization".into(), format!("Bearer {token}"))],
        body: None,
    })?;
    if response.status == 429 {
        return Err("Slack asked to slow down; the next sync will continue".into());
    }
    let body: Value = serde_json::from_slice(&response.body).map_err(|_| format!("Slack answered {} without JSON", response.status))?;
    if body["ok"] != true {
        return Err(format!("Slack refused {method}: {}", body["error"].as_str().unwrap_or("unknown error")));
    }
    Ok(body)
}

/// Display name of a user id, remembered in the plugin's storage.
fn user_name(base: &str, token: &str, id: &str) -> String {
    let key = format!("user:{id}");
    if let Some(name) = host::kv_get(&key) {
        return name;
    }
    let name = call(base, token, "users.info", &format!("user={id}"))
        .ok()
        .and_then(|v| {
            let u = &v["user"];
            u["profile"]["display_name"].as_str().filter(|n| !n.is_empty()).or(u["name"].as_str()).map(str::to_string)
        })
        .unwrap_or_else(|| id.to_string());
    let _ = host::kv_set(&key, &name);
    name
}

/// Slack timestamps look like "1712345678.000200".
fn seconds(ts: &str) -> Option<u64> {
    ts.split('.').next()?.parse().ok()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn sync() -> Result<usize, String> {
    let cfg = config();
    let base = api_base(&cfg);
    let token = host::secret("SLACK_TOKEN").ok_or("SLACK_TOKEN is not set")?;
    let me = call(&base, &token, "auth.test", "")?;
    let (my_id, my_name) = (me["user_id"].as_str().unwrap_or(""), me["user"].as_str().unwrap_or(""));
    let team_url = me["url"].as_str().unwrap_or("https://slack.com/").to_string();

    let listed = call(&base, &token, "users.conversations", "types=public_channel,private_channel,im,mpim&exclude_archived=true&limit=200")?;
    let wanted: Vec<&str> = cfg["channels"].as_array().map(|a| a.iter().filter_map(|c| c.as_str()).collect()).unwrap_or_default();
    let conversations: Vec<&Value> = listed["channels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| wanted.is_empty() || wanted.contains(&c["id"].as_str().unwrap_or("")))
        .take(MAX_CONVERSATIONS)
        .collect();

    let first_window = cfg["hours"].as_u64().unwrap_or(24).clamp(1, 24 * 14) * 3600;
    let mut emitted = 0;
    for conversation in conversations {
        let Some(channel) = conversation["id"].as_str() else { continue };
        let is_dm = conversation["is_im"] == true;
        let cursor_key = format!("oldest:{channel}");
        let oldest = host::kv_get(&cursor_key).unwrap_or_else(|| format!("{}.000000", now().saturating_sub(first_window)));
        let history = call(&base, &token, "conversations.history", &format!("channel={channel}&oldest={oldest}&limit=50"))?;
        let place = if is_dm {
            "direct message".to_string()
        } else {
            format!("#{}", conversation["name"].as_str().unwrap_or(channel))
        };
        let mut newest = oldest.clone();
        for message in history["messages"].as_array().into_iter().flatten() {
            let (Some(ts), Some(text)) = (message["ts"].as_str(), message["text"].as_str()) else { continue };
            if ts > newest.as_str() {
                newest = ts.to_string();
            }
            // Joins, bots' housekeeping and the person's own messages are not inbox material.
            let author = message["user"].as_str().unwrap_or("");
            if message.get("subtype").is_some_and(|s| !s.is_null()) || author.is_empty() || author == my_id || text.trim().is_empty() {
                continue;
            }
            let mentioned = !my_id.is_empty() && text.contains(&format!("<@{my_id}>"));
            let title: String = text.lines().next().unwrap_or("").chars().take(140).collect();
            let item = json!({
                "external_id": format!("{channel}:{ts}"),
                "source": "slack",
                "kind": "message",
                "title": title,
                "body": text,
                "url": format!("{team_url}archives/{channel}/p{}", ts.replace('.', "")),
                "thread": place,
                "actors": [user_name(&base, &token, author)],
                "received_at": seconds(ts),
                "direct": is_dm,
                "mentions": if mentioned { json!([my_name, my_id]) } else { json!([]) },
            });
            host::emit_item(&item.to_string())?;
            emitted += 1;
        }
        host::kv_set(&cursor_key, &newest)?;
    }
    host::kv_set("last", &json!({ "at": now(), "messages": emitted, "user": my_name }).to_string())?;
    host::log("info", &format!("synced {emitted} new messages"));
    Ok(emitted)
}

fn status(note: Option<String>) -> String {
    let mut children = vec![json!({ "type": "row", "children": [
        { "type": "text", "style": "title", "text": "Slack" },
        { "type": "button", "label": "Sync now", "action": "sync" },
    ] })];
    if let Some(note) = note {
        children.push(json!({ "type": "text", "style": "error", "text": note }));
    }
    if host::secret("SLACK_TOKEN").is_none() {
        children.push(json!({ "type": "empty", "text":
            "No token yet. Create a Slack app with user scopes for reading conversations and store its user token with `almighty plugin secret net.ema.slack SLACK_TOKEN`." }));
    } else if let Some(last) = host::kv_get("last").and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        children.push(json!({ "type": "text", "style": "muted", "text":
            format!("Signed in as {}. The last sync brought {} new messages; they are in the inbox.", last["user"].as_str().unwrap_or("?"), last["messages"]) }));
    } else {
        children.push(json!({ "type": "empty", "text": "Not synced yet." }));
    }
    json!({ "type": "stack", "children": children }).to_string()
}

impl Guest for Slack {
    fn poll() -> Result<(), String> {
        // Without a token there is nothing to do, and that is not an error worth repeating every two minutes.
        if host::secret("SLACK_TOKEN").is_none() {
            return Ok(());
        }
        sync().map(|_| ())
    }

    fn render(panel: String) -> Result<String, String> {
        match panel.as_str() {
            "status" => Ok(status(None)),
            other => Err(format!("no panel {other}")),
        }
    }

    fn action(panel: String, action: String, _payload: String) -> Result<String, String> {
        match (panel.as_str(), action.as_str()) {
            ("status", "sync") => Ok(status(sync().err())),
            _ => Err(format!("no action {action} in panel {panel}")),
        }
    }

    fn command(id: String, _args: String) -> Result<String, String> {
        match id.as_str() {
            "slack.sync" => Ok(json!({ "messages": sync()? }).to_string()),
            other => Err(format!("no command {other}")),
        }
    }
}

export!(Slack);
