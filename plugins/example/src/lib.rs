//! Smallest plugin that touches every host function. The server's tests load it.

wit_bindgen::generate!({ world: "plugin", path: "../../wit" });

use almighty::plugin::host;
use serde_json::{json, Value};

struct Example;

fn count() -> i64 {
    host::kv_get("count").and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn tree() -> String {
    let greeting = serde_json::from_str::<Value>(&host::config())
        .ok()
        .and_then(|c| c["greeting"].as_str().map(str::to_string))
        .unwrap_or_else(|| "hello".into());
    json!({
        "type": "stack",
        "children": [
            { "type": "text", "style": "title", "text": greeting },
            { "type": "row", "children": [
                { "type": "badge", "text": format!("count {}", count()) },
                { "type": "button", "label": "Add one", "action": "add", "payload": { "by": 1 } },
            ] },
            { "type": "form", "action": "set", "submit": "Set", "children": [
                { "type": "input", "name": "value", "label": "Value", "kind": "number" },
            ] },
        ]
    })
    .to_string()
}

impl Guest for Example {
    fn poll() -> Result<(), String> {
        host::log("info", "poll");
        let item = json!({
            "external_id": "tick",
            "kind": "note",
            "title": format!("tick {}", count()),
            "state": if host::secret("EXAMPLE_TOKEN").is_some() { "with-secret" } else { "open" },
        });
        host::emit_item(&item.to_string()).map(|_| ())
    }

    fn render(panel: String) -> Result<String, String> {
        match panel.as_str() {
            "counter" => Ok(tree()),
            other => Err(format!("no panel {other}")),
        }
    }

    fn action(_panel: String, action: String, payload: String) -> Result<String, String> {
        let payload: Value = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
        let next = match action.as_str() {
            "add" => count() + payload["by"].as_i64().unwrap_or(1),
            "set" => payload["value"].as_str().and_then(|v| v.parse().ok()).ok_or("value must be a number")?,
            other => return Err(format!("no action {other}")),
        };
        host::kv_set("count", &next.to_string())?;
        Ok(tree())
    }

    fn command(id: String, args: String) -> Result<String, String> {
        match id.as_str() {
            "example.fetch" => {
                let args: Value = serde_json::from_str(&args).map_err(|e| e.to_string())?;
                let response = host::http_fetch(&host::HttpRequest {
                    method: "GET".into(),
                    url: args["url"].as_str().unwrap_or_default().to_string(),
                    headers: vec![],
                    body: None,
                })?;
                Ok(json!({ "status": response.status, "body": String::from_utf8_lossy(&response.body) }).to_string())
            }
            "example.spin" => loop {
                std::hint::spin_loop();
            },
            "example.items" => host::query_items(&args),
            other => Err(format!("no command {other}")),
        }
    }
}

export!(Example);
