//! Gmail connector: recent inbox messages as items (metadata and snippet only).

wit_bindgen::generate!({ world: "plugin", path: "../../wit" });

use almighty::plugin::host;
use serde_json::{json, Value};

struct Gmail;

fn config() -> Value {
    serde_json::from_str(&host::config()).unwrap_or(Value::Null)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Percent-encodes a query or form value.
fn enc(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn missing() -> Option<&'static str> {
    ["GMAIL_CLIENT_ID", "GMAIL_CLIENT_SECRET", "GMAIL_REFRESH_TOKEN"].into_iter().find(|name| host::secret(name).is_none())
}

/// Trades the refresh token for a short-lived access token. Asked for on
/// every sync and never stored: plugin storage is not encrypted.
fn access_token(cfg: &Value) -> Result<String, String> {
    let secret = |name: &str| host::secret(name).ok_or_else(|| format!("{name} is not set"));
    let form = format!(
        "grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}",
        enc(&secret("GMAIL_CLIENT_ID")?),
        enc(&secret("GMAIL_CLIENT_SECRET")?),
        enc(&secret("GMAIL_REFRESH_TOKEN")?)
    );
    let response = host::http_fetch(&host::HttpRequest {
        method: "POST".into(),
        url: cfg["token_url"].as_str().unwrap_or("https://oauth2.googleapis.com/token").to_string(),
        headers: vec![("content-type".into(), "application/x-www-form-urlencoded".into())],
        body: Some(form.into_bytes()),
    })?;
    let body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    let token = body["access_token"].as_str().ok_or_else(|| {
        format!("Google refused the refresh token ({}): {}", response.status, body["error_description"].as_str().or(body["error"].as_str()).unwrap_or("no reason given"))
    })?;
    Ok(token.to_string())
}

fn get(url: &str, token: &str) -> Result<Value, String> {
    let response = host::http_fetch(&host::HttpRequest {
        method: "GET".into(),
        url: url.into(),
        headers: vec![("authorization".into(), format!("Bearer {token}"))],
        body: None,
    })?;
    let body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    if response.status != 200 {
        return Err(format!("Gmail answered {}: {}", response.status, body["error"]["message"].as_str().unwrap_or("no message")));
    }
    Ok(body)
}

/// `Ana <ana@example.com>` -> (`Ana`, `ana@example.com`).
fn sender(from: &str) -> String {
    match from.split_once('<') {
        Some((name, _)) if !name.trim().trim_matches('"').is_empty() => name.trim().trim_matches('"').to_string(),
        Some((_, address)) => address.trim_end_matches('>').to_string(),
        None => from.trim().to_string(),
    }
}

fn sync() -> Result<usize, String> {
    let cfg = config();
    let api = cfg["api"].as_str().unwrap_or("https://gmail.googleapis.com").trim_end_matches('/').to_string();
    let token = access_token(&cfg)?;
    let query = cfg["query"].as_str().unwrap_or("in:inbox newer_than:2d");
    let max = cfg["max"].as_u64().unwrap_or(30).clamp(1, 50);
    let listed = get(&format!("{api}/gmail/v1/users/me/messages?q={}&maxResults={max}", enc(query)), &token)?;
    let mut emitted = 0;
    for entry in listed["messages"].as_array().into_iter().flatten() {
        let Some(id) = entry["id"].as_str().filter(|id| id.chars().all(|c| c.is_ascii_alphanumeric())) else { continue };
        let fields = "metadataHeaders=From&metadataHeaders=Subject&metadataHeaders=List-Unsubscribe&metadataHeaders=List-Id";
        let message = get(&format!("{api}/gmail/v1/users/me/messages/{id}?format=metadata&{fields}"), &token)?;
        let header = |name: &str| -> Option<&str> {
            message["payload"]["headers"].as_array()?.iter().find(|h| h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))?["value"].as_str()
        };
        let labels: Vec<&str> = message["labelIds"].as_array().map(|l| l.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        let bulk = labels.iter().any(|l| matches!(*l, "CATEGORY_PROMOTIONS" | "CATEGORY_SOCIAL" | "CATEGORY_UPDATES" | "CATEGORY_FORUMS"))
            || header("List-Unsubscribe").is_some()
            || header("List-Id").is_some();
        let item = json!({
            "external_id": id,
            "source": "gmail",
            "kind": "email",
            "state": if labels.contains(&"UNREAD") { "open" } else { "read" },
            "title": header("Subject").filter(|s| !s.trim().is_empty()).unwrap_or("(no subject)"),
            "body": message["snippet"].as_str().unwrap_or(""),
            "url": format!("https://mail.google.com/mail/u/0/#inbox/{id}"),
            "thread": message["threadId"].as_str().unwrap_or(""),
            "actors": [sender(header("From").unwrap_or(""))],
            "received_at": message["internalDate"].as_str().and_then(|ms| ms.parse::<u64>().ok()).map(|ms| ms / 1000),
            "direct": !bulk,
        });
        host::emit_item(&item.to_string())?;
        emitted += 1;
    }
    host::kv_set("last", &json!({ "at": now(), "messages": emitted }).to_string())?;
    host::log("info", &format!("synced {emitted} messages"));
    Ok(emitted)
}

fn status(note: Option<String>) -> String {
    let mut children = vec![json!({ "type": "row", "children": [
        { "type": "text", "style": "title", "text": "Gmail" },
        { "type": "button", "label": "Sync now", "action": "sync" },
    ] })];
    if let Some(note) = note {
        children.push(json!({ "type": "text", "style": "error", "text": note }));
    }
    if let Some(name) = missing() {
        children.push(json!({ "type": "empty", "text":
            format!("{name} is not set. Create an OAuth client under your own Google account with the read-only Gmail scope and store the three secrets with `almighty plugin secret net.ema.gmail <NAME>`.") }));
    } else if let Some(last) = host::kv_get("last").and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        children.push(json!({ "type": "text", "style": "muted", "text": format!("The last sync read {} messages; they are in the inbox.", last["messages"]) }));
    } else {
        children.push(json!({ "type": "empty", "text": "Not synced yet." }));
    }
    json!({ "type": "stack", "children": children }).to_string()
}

impl Guest for Gmail {
    fn poll() -> Result<(), String> {
        if missing().is_some() {
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
            "gmail.sync" => Ok(json!({ "messages": sync()? }).to_string()),
            other => Err(format!("no command {other}")),
        }
    }
}

export!(Gmail);
