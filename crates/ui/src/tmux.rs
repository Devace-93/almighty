//! Opens panes of the tmux server that runs next to almighty.

use dioxus::prelude::*;
use serde_json::{json, Value};

use crate::{api_send, Shell, View};

/// One line per pane: what a person recognises it by.
pub fn label(pane: &Value) -> String {
    let place = format!("{}:{}", pane["session"].as_str().unwrap_or(""), pane["window"]);
    let name = pane["window_name"].as_str().unwrap_or("");
    if pane["panes"].as_u64().unwrap_or(1) > 1 {
        format!("{place}.{} {name}", pane["index"])
    } else {
        format!("{place} {name}")
    }
}

#[component]
pub fn TmuxPanes(on_done: EventHandler<()>) -> Element {
    let shell = use_context::<Shell>();
    let mut view = shell.view;
    let mut data = use_signal(|| None::<Result<Value, String>>);

    use_resource(move || async move {
        let server = shell.server.peek().clone();
        data.set(Some(api_send(&server, "GET", "/api/tmux", None).await));
    });

    let body = match data() {
        None => rsx! { p { class: "hint", "Looking for tmux panes…" } },
        Some(Err(e)) => rsx! { p { class: "hint", role: "alert", "{e}" } },
        Some(Ok(v)) if v["available"] == false => rsx! {
            p { class: "hint", {v["reason"].as_str().unwrap_or("tmux is not available").to_string()} }
        },
        Some(Ok(v)) if v["panes"].as_array().is_none_or(|w| w.is_empty()) => rsx! {
            p { class: "hint", "No tmux panes are open on the server's machine." }
        },
        Some(Ok(v)) => rsx! {
            ul { class: "sessions",
                for w in v["panes"].as_array().cloned().unwrap_or_default() {
                    li { key: "{w[\"id\"]}",
                        button {
                            class: "row",
                            title: "Opens this pane here. Nothing in it is restarted.",
                            onclick: {
                                let w = w.clone();
                                move |_| {
                                    let w = w.clone();
                                    async move {
                                        let server = shell.server.peek().clone();
                                        let body = json!({ "pane": w["id"] });
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
        div { class: "tmux-list", role: "group", aria_label: "tmux panes on the server's machine", {body} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_a_pane() {
        let alone = json!({ "session": "work", "window": 2, "index": 1, "window_name": "logs", "panes": 1 });
        assert_eq!(label(&alone), "work:2 logs");
        let split = json!({ "session": "work", "window": 1, "index": 2, "window_name": "editor", "panes": 2 });
        assert_eq!(label(&split), "work:1.2 editor");
    }
}
