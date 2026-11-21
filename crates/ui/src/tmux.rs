//! Opens windows of the tmux server that runs next to almighty.

use dioxus::prelude::*;
use serde_json::{json, Value};

use crate::{api_send, Shell, View};

/// One line per window: what a person recognises it by.
pub fn label(window: &Value) -> String {
    format!(
        "{}:{} {}",
        window["session"].as_str().unwrap_or(""),
        window["index"],
        window["name"].as_str().unwrap_or("")
    )
}

#[component]
pub fn TmuxWindows(on_done: EventHandler<()>) -> Element {
    let shell = use_context::<Shell>();
    let mut view = shell.view;
    let mut data = use_signal(|| None::<Result<Value, String>>);

    use_resource(move || async move {
        let server = shell.server.peek().clone();
        data.set(Some(api_send(&server, "GET", "/api/tmux", None).await));
    });

    let body = match data() {
        None => rsx! { p { class: "hint", "Looking for tmux windows…" } },
        Some(Err(e)) => rsx! { p { class: "hint", role: "alert", "{e}" } },
        Some(Ok(v)) if v["available"] == false => rsx! {
            p { class: "hint", {v["reason"].as_str().unwrap_or("tmux is not available").to_string()} }
        },
        Some(Ok(v)) if v["windows"].as_array().is_none_or(|w| w.is_empty()) => rsx! {
            p { class: "hint", "No tmux windows are open on the server's machine." }
        },
        Some(Ok(v)) => rsx! {
            ul { class: "sessions",
                for w in v["windows"].as_array().cloned().unwrap_or_default() {
                    li { key: "{w[\"session_id\"]}:{w[\"index\"]}",
                        button {
                            class: "row",
                            title: "Opens this window here. Nothing in it is restarted.",
                            onclick: {
                                let w = w.clone();
                                move |_| {
                                    let w = w.clone();
                                    async move {
                                        let server = shell.server.peek().clone();
                                        let body = json!({ "session": w["session_id"], "window": w["index"] });
                                        match api_send(&server, "POST", "/api/tmux/adopt", Some(body)).await {
                                            Ok(s) => {
                                                if let Some(id) = s["id"].as_str() {
                                                    view.set(Some(View::Terminal(id.to_string())));
                                                }
                                                on_done.call(());
                                            }
                                            Err(e) => data.set(Some(Err(e))),
                                        }
                                    }
                                }
                            },
                            span { class: "title", {label(&w)} }
                            span { class: "kind", {w["command"].as_str().unwrap_or("").to_string()} }
                        }
                    }
                }
            }
        },
    };

    rsx! {
        div { class: "tmux-list", role: "group", aria_label: "tmux windows on the server's machine", {body} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_a_window() {
        let w = json!({ "session": "work", "index": 2, "name": "logs", "command": "bash" });
        assert_eq!(label(&w), "work:2 logs");
    }
}
