//! Planner (tasks and things to prepare for) and the awareness digest.

use dioxus::prelude::*;
use serde_json::{json, Value};

use crate::browser::Link;
use crate::{api_delete, api_send, Shell};

const SECTIONS: [(&str, &str); 6] =
    [("overdue", "Overdue"), ("today", "Next 24 hours"), ("week", "This week"), ("later", "Later"), ("someday", "No date"), ("done", "Done")];

/// What needs attention soon: the number next to "Plan".
pub fn pressing(counts: &Value) -> u64 {
    counts["overdue"].as_u64().unwrap_or(0) + counts["today"].as_u64().unwrap_or(0)
}

fn web_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

#[component]
pub fn PlannerView() -> Element {
    let shell = use_context::<Shell>();
    let mut data = use_signal(|| Value::Null);
    let mut error = use_signal(|| None::<String>);
    let mut show_done = use_signal(|| false);
    let mut reload = use_signal(|| 0u32);

    use_resource(move || async move {
        let _ = ((shell.inbox_tick)(), reload());
        let path = if show_done() { "/api/planner?done=1" } else { "/api/planner" };
        let server = shell.server.peek().clone();
        match api_send(&server, "GET", path, None).await {
            Ok(v) => {
                if *data.peek() != v {
                    data.set(v);
                }
                error.set(None);
            }
            Err(e) => error.set(Some(e)),
        }
    });

    let add = move |e: FormEvent| async move {
        e.prevent_default();
        let text = |name: &str| match e.get_first(name) {
            Some(FormValue::Text(t)) => t.trim().to_string(),
            _ => String::new(),
        };
        // The date field gives a calendar day; the server stores the end of that day in UTC.
        let due = crate::net::date_to_unix(&text("due")).map(|start| start + 24 * 3600 - 1);
        let server = shell.server.peek().clone();
        match api_send(&server, "POST", "/api/planner", Some(json!({ "title": text("title"), "due": due }))).await {
            Ok(_) => {
                crate::net::clear_inputs(".plan-form input");
                reload += 1;
            }
            Err(err) => error.set(Some(err)),
        }
    };

    let set_done = move |(id, done): (i64, bool)| async move {
        let server = shell.server.peek().clone();
        match api_send(&server, "POST", &format!("/api/planner/{id}"), Some(json!({ "done": done }))).await {
            Ok(_) => reload += 1,
            Err(e) => error.set(Some(e)),
        }
    };
    let remove = move |id: i64| async move {
        let server = shell.server.peek().clone();
        if api_delete(&server, &format!("/api/planner/{id}")).await {
            reload += 1;
        }
    };

    let entries = data()["entries"].as_array().cloned().unwrap_or_default();
    rsx! {
        section { class: "panel",
            div { class: "bar",
                span { class: "title", "Plan" }
                button { class: if show_done() { "toggle on" } else { "toggle" }, aria_pressed: show_done(), onclick: move |_| show_done.toggle(), "Show done" }
            }
            div { class: "panel-body",
                div { class: "p-stack",
                    form { class: "p-form plan-form", onsubmit: add,
                        label { class: "p-input plan-title",
                            span { "New task" }
                            input { r#type: "text", name: "title", placeholder: "What needs doing?", required: true }
                        }
                        label { class: "p-input",
                            span { "Due (optional)" }
                            input { r#type: "date", name: "due" }
                        }
                        button { class: "primary", r#type: "submit", "Add" }
                    }
                    if let Some(e) = error() {
                        p { class: "p-text error", role: "alert", "{e}" }
                    }
                    if entries.is_empty() && data() != Value::Null {
                        p { class: "p-empty", "Nothing planned. Tasks you add and events from your inbox appear here." }
                    }
                    for (id, label) in SECTIONS {
                        if entries.iter().any(|e| e["when"] == id) {
                            section { key: "{id}", class: "p-card",
                                h3 { class: if id == "overdue" { "plan-overdue" } else { "" }, "{label}" }
                                ul { class: "p-list",
                                    for entry in entries.iter().filter(|e| e["when"] == id).cloned() {
                                        li { key: "{entry[\"kind\"]}-{entry[\"task\"][\"id\"]}-{entry[\"item\"][\"id\"]}",
                                            PlanEntry { entry, on_done: set_done, on_remove: remove }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn PlanEntry(entry: Value, on_done: EventHandler<(i64, bool)>, on_remove: EventHandler<i64>) -> Element {
    let title = entry["title"].as_str().unwrap_or_default().to_string();
    let done = entry["done"] == true;
    let due = entry["due"].as_u64().map(crate::net::unix_to_date).unwrap_or_default();
    if entry["kind"] == "task" {
        let id = entry["task"]["id"].as_i64().unwrap_or(0);
        rsx! {
            div { class: "p-row",
                label { class: "plan-check",
                    input { r#type: "checkbox", checked: done, onchange: move |_| on_done.call((id, !done)) }
                    span { class: if done { "plan-done" } else { "" }, "{title}" }
                }
                if !due.is_empty() {
                    span { class: "p-text muted", "{due}" }
                }
                button { class: "icon-button small", aria_label: "Remove {title}", onclick: move |_| on_remove.call(id), crate::browser::Icon { name: "close" } }
            }
        }
    } else {
        // An inbox item that needs preparation: it is finished in the inbox.
        let url = entry["item"]["url"].as_str().unwrap_or_default().to_string();
        rsx! {
            div { class: "p-row",
                span { class: "p-badge accent", "from the inbox" }
                if web_url(&url) {
                    Link { url, label: title.clone() }
                } else {
                    span { class: "p-text", "{title}" }
                }
                if !due.is_empty() {
                    span { class: "p-text muted", "{due}" }
                }
            }
        }
    }
}

/// The digest: what arrived that needs nothing from the person.
#[component]
pub fn Digest() -> Element {
    let shell = use_context::<Shell>();
    let mut hours = use_signal(|| 24u64);
    let mut data = use_signal(|| Value::Null);
    let mut written = use_signal(|| None::<Result<String, String>>);
    let mut busy = use_signal(|| false);

    use_resource(move || async move {
        let _ = (shell.inbox_tick)();
        let server = shell.server.peek().clone();
        if let Ok(v) = api_send(&server, "GET", &format!("/api/digest?hours={}", hours()), None).await {
            if *data.peek() != v {
                data.set(v);
            }
        }
    });

    let write = move |_| async move {
        if *busy.peek() {
            return;
        }
        busy.set(true);
        let server = shell.server.peek().clone();
        let result = api_send(&server, "POST", &format!("/api/digest/summary?hours={}", *hours.peek()), Some(json!({}))).await;
        written.set(Some(result.map(|v| v["summary"].as_str().unwrap_or_default().to_string())));
        busy.set(false);
    };

    let groups = data()["groups"].as_array().cloned().unwrap_or_default();
    rsx! {
        div { class: "p-stack",
            div { class: "p-row",
                span { class: "p-text muted", "{data()[\"total\"].as_u64().unwrap_or(0)} things to know from the last" }
                select {
                    aria_label: "Period of the digest",
                    onchange: move |e| {
                        if let Ok(h) = e.value().parse() {
                            hours.set(h);
                            written.set(None);
                        }
                    },
                    option { value: "24", "24 hours" }
                    option { value: "72", "3 days" }
                    option { value: "168", "7 days" }
                }
                if !groups.is_empty() {
                    button { class: "toggle", disabled: busy(), title: "Sends these titles to the model configured on your server", onclick: write,
                        if busy() { "Writing…" } else { "Write it out" }
                    }
                }
            }
            match written() {
                Some(Ok(text)) => rsx! { div { class: "p-card p-markdown", "{text}" } },
                Some(Err(e)) => rsx! { p { class: "p-text error", role: "alert", "{e}" } },
                None => rsx! {},
            }
            if groups.is_empty() && data() != Value::Null {
                p { class: "p-empty", "Nothing new that is only for your information." }
            }
            for group in groups {
                section { key: "{group[\"source\"]}/{group[\"thread\"]}", class: "p-card",
                    h3 {
                        "{group[\"source\"].as_str().unwrap_or_default()} · {group[\"thread\"].as_str().unwrap_or_default()} "
                        span { class: "badge", "{group[\"count\"]}" }
                    }
                    ul { class: "p-list",
                        for item in group["items"].as_array().cloned().unwrap_or_default() {
                            li { key: "{item[\"id\"]}",
                                if web_url(item["url"].as_str().unwrap_or_default()) {
                                    Link { url: item["url"].as_str().unwrap_or_default().to_string(), label: item["title"].as_str().unwrap_or_default().to_string() }
                                } else {
                                    span { class: "p-text", "{item[\"title\"].as_str().unwrap_or_default()}" }
                                }
                            }
                        }
                    }
                    if group["count"].as_u64().unwrap_or(0) > 8 {
                        p { class: "p-text muted", "…and more in the FYI tab." }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressing_counts_overdue_and_today() {
        assert_eq!(pressing(&json!({ "overdue": 2, "today": 1, "week": 9 })), 3);
        assert_eq!(pressing(&Value::Null), 0);
    }
}
