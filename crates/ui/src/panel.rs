//! Plugin panels. A plugin describes its panel as a tree of nodes (JSON);
//! this module maps each node to the client's own widgets. Nothing from the
//! plugin runs here: unknown node types are skipped, and interaction is an
//! action id sent back to the server.

use dioxus::prelude::*;
use serde_json::{json, Value};

use crate::browser::Link;
use crate::{api_send, Shell, View};

/// One entry of the sidebar: a panel of an enabled plugin.
#[derive(Clone, PartialEq, Debug)]
pub struct PanelRef {
    pub plugin: String,
    pub panel: String,
    pub title: String,
    pub plugin_name: String,
    /// Web panels of service plugins: the page to open. `None` for declarative panels.
    pub web_url: Option<String>,
}

/// Panels of the enabled plugins, ordered by the user's `layout.toml`:
/// `panels = ["plugin/panel", …]` come first in that order, `hidden` are left out.
pub fn panel_list(plugins: &Value, layout: &Value) -> Vec<PanelRef> {
    let names = |key: &str| -> Vec<String> {
        layout[key].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
    };
    let (order, hidden) = (names("panels"), names("hidden"));
    let mut all: Vec<PanelRef> = Vec::new();
    for p in plugins.as_array().into_iter().flatten().filter(|p| p["enabled"] == true) {
        for panel in p["panels"].as_array().into_iter().flatten() {
            all.push(PanelRef {
                plugin: p["id"].as_str().unwrap_or_default().to_string(),
                panel: panel["id"].as_str().unwrap_or_default().to_string(),
                title: panel["title"].as_str().unwrap_or_default().to_string(),
                plugin_name: p["name"].as_str().unwrap_or_default().to_string(),
                web_url: match (panel["type"].as_str(), p["base_url"].as_str(), panel["url"].as_str()) {
                    (Some("web"), Some(base), Some(path)) => Some(format!("{base}{path}")),
                    _ => None,
                },
            });
        }
    }
    let key = |r: &PanelRef| format!("{}/{}", r.plugin, r.panel);
    all.retain(|r| !hidden.contains(&key(r)));
    // Stable: panels the layout does not mention keep their order after the listed ones.
    all.sort_by_key(|r| order.iter().position(|o| *o == key(r)).unwrap_or(usize::MAX));
    all
}

/// Only web links: a plugin must not be able to hand over `javascript:` or `file:` URLs.
fn web_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

#[component]
pub fn PanelView(plugin: String, panel: String, title: String) -> Element {
    let shell = use_context::<Shell>();
    let mut tree = use_signal(|| None::<Value>);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let path = format!("/api/plugins/{plugin}/panels/{panel}");

    // Loads on entry and again whenever the server says the plugin changed.
    let load_path = path.clone();
    use_resource(move || {
        let path = load_path.clone();
        let _ = (shell.plugins_tick)();
        async move {
            let server = shell.server.peek().clone();
            match api_send(&server, "GET", &path, None).await {
                Ok(v) => {
                    if tree.peek().as_ref() != Some(&v["tree"]) {
                        tree.set(Some(v["tree"].clone()));
                    }
                    error.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
        }
    });

    let on_action = move |(action, payload): (String, Value)| {
        let path = format!("{path}/action");
        async move {
            if *busy.peek() {
                return;
            }
            busy.set(true);
            let server = shell.server.peek().clone();
            match api_send(&server, "POST", &path, Some(json!({ "action": action, "payload": payload }))).await {
                Ok(v) => {
                    tree.set(Some(v["tree"].clone()));
                    error.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        }
    };

    rsx! {
        section { class: "panel", aria_busy: busy(),
            div { class: "bar",
                span { class: "title", "{title}" }
                if busy() {
                    span { class: "meta", "working…" }
                }
                span { class: "tag", "{plugin}" }
            }
            div { class: "panel-body",
                if let Some(e) = error() {
                    p { class: "p-text error", role: "alert", "{e}" }
                }
                match tree() {
                    Some(node) => rsx! { Node { node, on_action } },
                    None if error().is_none() => rsx! { p { class: "p-empty", "Loading…" } },
                    None => rsx! {},
                }
            }
        }
    }
}

#[component]
fn Node(node: Value, on_action: EventHandler<(String, Value)>) -> Element {
    let shell = use_context::<Shell>();
    let mut view = shell.view;
    let text = |key: &str| node[key].as_str().unwrap_or_default().to_string();
    let list = |key: &str| node[key].as_array().cloned().unwrap_or_default();
    // Class names come from a fixed set, never from the plugin's text.
    let pick = |key: &str, allowed: &[&'static str]| -> &'static str {
        allowed.iter().find(|a| node[key].as_str() == Some(**a)).copied().unwrap_or("")
    };
    match node["type"].as_str().unwrap_or_default() {
        "stack" | "row" => {
            let class = if node["type"] == "row" { "p-row" } else { "p-stack" };
            rsx! {
                div { class,
                    for (i, child) in list("children").into_iter().enumerate() {
                        Node { key: "{i}", node: child, on_action }
                    }
                }
            }
        }
        "list" => rsx! {
            ul { class: "p-list",
                for (i, child) in list("items").into_iter().enumerate() {
                    li { key: "{i}", Node { node: child, on_action } }
                }
            }
        },
        "card" => rsx! {
            section { class: "p-card",
                if !text("title").is_empty() {
                    h3 { "{text(\"title\")}" }
                }
                for (i, child) in list("children").into_iter().enumerate() {
                    Node { key: "{i}", node: child, on_action }
                }
            }
        },
        "text" => {
            let style = pick("style", &["title", "muted", "strong", "error"]);
            rsx! { span { class: "p-text {style}", "{text(\"text\")}" } }
        }
        // Shown as written for now; rendering Markdown and diagrams comes later.
        "markdown" => rsx! { div { class: "p-markdown", "{text(\"text\")}" } },
        "code" => rsx! { pre { class: "p-code", "{text(\"text\")}" } },
        "diagram" => rsx! { pre { class: "p-code", "{text(\"source\")}" } },
        "diff" => rsx! {
            pre { class: "p-code",
                for (i, line) in text("text").lines().enumerate() {
                    span {
                        key: "{i}",
                        class: if line.starts_with('+') { "add" } else if line.starts_with('-') { "del" } else { "" },
                        "{line}\n"
                    }
                }
            }
        },
        "badge" => {
            let tone = pick("tone", &["accent", "muted", "ok", "warn"]);
            rsx! { span { class: "p-badge {tone}", "{text(\"text\")}" } }
        }
        "link" => {
            let (url, label) = (text("url"), text("text"));
            if web_url(&url) {
                rsx! { Link { url, label } }
            } else {
                rsx! { span { class: "p-text", "{label}" } }
            }
        }
        "media" => {
            let (url, alt) = (text("url"), text("alt"));
            if web_url(&url) && node["kind"].as_str().unwrap_or("image") == "image" {
                rsx! { img { class: "p-media", src: "{url}", alt: "{alt}" } }
            } else if web_url(&url) {
                rsx! { Link { label: if alt.is_empty() { url.clone() } else { alt }, url } }
            } else {
                rsx! {}
            }
        }
        "button" => {
            let (action, payload) = (text("action"), node["payload"].clone());
            rsx! {
                button {
                    class: "toggle",
                    onclick: move |_| on_action.call((action.clone(), payload.clone())),
                    "{text(\"label\")}"
                }
            }
        }
        "form" => {
            let action = text("action");
            let submit = if text("submit").is_empty() { "Send".to_string() } else { text("submit") };
            rsx! {
                form {
                    class: "p-form",
                    onsubmit: move |e: FormEvent| {
                        e.prevent_default();
                        let mut values = serde_json::Map::new();
                        for (name, value) in e.values() {
                            if let FormValue::Text(t) = value {
                                values.insert(name, json!(t));
                            }
                        }
                        on_action.call((action.clone(), Value::Object(values)));
                    },
                    for (i, child) in list("children").into_iter().enumerate() {
                        Node { key: "{i}", node: child, on_action }
                    }
                    button { class: "primary", r#type: "submit", "{submit}" }
                }
            }
        }
        "input" => {
            let kind = match pick("kind", &["number", "password", "email", "url", "date"]) {
                "" => "text",
                other => other,
            };
            rsx! {
                label { class: "p-input",
                    span { "{text(\"label\")}" }
                    input {
                        r#type: kind,
                        name: "{text(\"name\")}",
                        placeholder: "{text(\"placeholder\")}",
                        initial_value: "{text(\"value\")}",
                    }
                }
            }
        }
        "table" => rsx! {
            table { class: "p-table",
                thead {
                    tr {
                        for (i, column) in list("columns").into_iter().enumerate() {
                            th { key: "{i}", scope: "col", {column.as_str().unwrap_or_default().to_string()} }
                        }
                    }
                }
                tbody {
                    for (r, row) in list("rows").into_iter().enumerate() {
                        tr { key: "{r}",
                            for (c, cell) in row.as_array().cloned().unwrap_or_default().into_iter().enumerate() {
                                td { key: "{c}",
                                    match cell {
                                        Value::String(s) => rsx! { "{s}" },
                                        Value::Number(n) => rsx! { "{n}" },
                                        other => rsx! { Node { node: other, on_action } },
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
        "terminal" => {
            let session = text("session");
            rsx! {
                button {
                    class: "toggle",
                    onclick: move |_| view.set(Some(View::Terminal(session.clone()))),
                    "Open terminal {text(\"session\")}"
                }
            }
        }
        "empty" => rsx! { p { class: "p-empty", "{text(\"text\")}" } },
        _ => rsx! {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_orders_and_hides() {
        let plugins = json!([
            { "id": "a.b", "name": "ab", "enabled": true, "panels": [{ "id": "one", "title": "One" }, { "id": "two", "title": "Two" }] },
            { "id": "c.d", "name": "cd", "enabled": true, "panels": [{ "id": "x", "title": "X" }] },
            { "id": "off.p", "name": "off", "enabled": false, "panels": [{ "id": "y", "title": "Y" }] },
        ]);
        let titles = |layout: Value| panel_list(&plugins, &layout).into_iter().map(|p| p.title).collect::<Vec<_>>();
        assert_eq!(titles(json!({})), ["One", "Two", "X"]);
        assert_eq!(titles(json!({ "panels": ["c.d/x"], "hidden": ["a.b/one"] })), ["X", "Two"]);

        let service = json!([{ "id": "s.v", "name": "sv", "enabled": true, "base_url": "http://localhost:8087",
            "panels": [{ "id": "runs", "title": "Runs", "type": "web", "url": "/embed/runs" }] }]);
        assert_eq!(panel_list(&service, &json!({}))[0].web_url.as_deref(), Some("http://localhost:8087/embed/runs"));
        assert_eq!(panel_list(&plugins, &json!({}))[0].web_url, None);
    }

    #[test]
    fn only_web_urls() {
        assert!(web_url("https://example.com/a"));
        assert!(!web_url("javascript:alert(1)"));
        assert!(!web_url("file:///etc/passwd"));
    }
}
