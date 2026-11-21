//! Personal settings: where phone notifications go.

use dioxus::prelude::*;
use serde_json::{json, Value};

use crate::{api_delete, api_send, Shell};

#[component]
pub fn PushSettings() -> Element {
    let shell = use_context::<Shell>();
    let mut data = use_signal(|| Value::Null);
    let mut message = use_signal(|| None::<(bool, String)>);
    let mut reload = use_signal(|| 0u32);

    use_resource(move || async move {
        let _ = reload();
        let server = shell.server.peek().clone();
        match api_send(&server, "GET", "/api/push", None).await {
            Ok(v) => data.set(v),
            Err(e) => message.set(Some((false, e))),
        }
    });

    let add = move |e: FormEvent| async move {
        e.prevent_default();
        let value = |name: &str| match e.get_first(name) {
            Some(FormValue::Text(t)) => t.trim().to_string(),
            _ => String::new(),
        };
        let server = shell.server.peek().clone();
        match api_send(&server, "POST", "/api/push", Some(json!({ "url": value("url"), "label": value("label") }))).await {
            Ok(_) => {
                message.set(Some((true, "Endpoint added. Send a test to check it.".into())));
                crate::net::clear_inputs(".push-form input");
                reload += 1;
            }
            Err(err) => message.set(Some((false, err))),
        }
    };

    let test = move |_| async move {
        let server = shell.server.peek().clone();
        match api_send(&server, "POST", "/api/push/test", Some(json!({}))).await {
            Ok(v) => {
                message.set(Some((true, format!("Test sent; delivered to {} endpoint(s).", v["delivered"]))));
                reload += 1;
            }
            Err(err) => message.set(Some((false, err))),
        }
    };

    let hosts: Vec<String> = data()["allowed_hosts"].as_array().map(|a| a.iter().filter_map(|h| h.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let endpoints = data()["endpoints"].as_array().cloned().unwrap_or_default();
    let wake = data()["content"] == "wake";
    rsx! {
        section { class: "panel",
            div { class: "bar", span { class: "title", "Phone notifications" } }
            div { class: "panel-body",
                div { class: "p-stack",
                    p { class: "p-text",
                        "almighty can push to a topic on an ntfy server, or to any UnifiedPush endpoint. "
                        "Install the ntfy app on your phone, subscribe to a topic with a name nobody can guess, and add its address here."
                    }
                    if data() != Value::Null && hosts.is_empty() {
                        p { class: "p-text error", role: "alert",
                            "Push is off on this server. Its operator has to list the allowed hosts under [push] allowed_hosts in workspace.toml."
                        }
                    }
                    if !hosts.is_empty() {
                        p { class: "p-text muted",
                            "Allowed hosts: {hosts.join(\", \")}. "
                            if wake { "A push only says that something arrived; open the app to read it." } else { "A push carries the title and the short summary of the item." }
                        }
                    }
                    if let Some((ok, text)) = message() {
                        p { class: if ok { "p-text" } else { "p-text error" }, role: "status", "{text}" }
                    }
                    ul { class: "p-list",
                        for e in endpoints.clone() {
                            li { key: "{e[\"id\"]}",
                                div { class: "p-row",
                                    span { class: "p-text strong", "{e[\"label\"].as_str().filter(|l| !l.is_empty()).unwrap_or(\"Endpoint\")}" }
                                    span { class: "p-text muted", "{e[\"url\"].as_str().unwrap_or_default()}" }
                                    if let Some(err) = e["last_error"].as_str() {
                                        span { class: "p-badge prio-3", "{err}" }
                                    } else if e["last_ok_at"].as_u64().is_some() {
                                        span { class: "p-badge ok", "working" }
                                    }
                                    button {
                                        class: "toggle danger",
                                        onclick: {
                                            let id = e["id"].as_i64().unwrap_or(0);
                                            move |_| async move {
                                                let server = shell.server.peek().clone();
                                                if api_delete(&server, &format!("/api/push/{id}")).await {
                                                    reload += 1;
                                                }
                                            }
                                        },
                                        "Remove"
                                    }
                                }
                            }
                        }
                    }
                    form { class: "p-form push-form", onsubmit: add,
                        label { class: "p-input",
                            span { "Endpoint address" }
                            input { r#type: "url", name: "url", placeholder: "https://ntfy.example.org/my-topic", required: true }
                        }
                        label { class: "p-input",
                            span { "Name (optional)" }
                            input { r#type: "text", name: "label", placeholder: "My phone" }
                        }
                        button { class: "primary", r#type: "submit", "Add" }
                    }
                    if !endpoints.is_empty() {
                        div { class: "p-row", button { class: "toggle", onclick: test, "Send a test" } }
                    }
                }
            }
        }
    }
}
