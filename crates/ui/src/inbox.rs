//! The triaged inbox: items from every connector, sorted per person into
//! reply / act / prepare / fyi / noise.

use dioxus::prelude::*;
use serde_json::{json, Value};

use crate::browser::Link;
use crate::{api_send, Shell, View};

pub const BUCKETS: [(&str, &str); 5] =
    [("reply", "Reply"), ("act", "Act"), ("prepare", "Prepare"), ("fyi", "FYI"), ("noise", "Noise")];

/// Items that want something from the person: the number next to "Inbox".
pub fn pending(counts: &Value) -> u64 {
    ["reply", "act", "prepare"].iter().map(|b| counts[*b].as_u64().unwrap_or(0)).sum()
}

fn web_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

#[component]
pub fn InboxView() -> Element {
    let shell = use_context::<Shell>();
    // "reply" … "noise", or "done".
    let mut tab = use_signal(|| "reply".to_string());
    let mut data = use_signal(|| Value::Null);
    let mut error = use_signal(|| None::<String>);
    let mut reload = use_signal(|| 0u32);

    use_resource(move || async move {
        let _ = ((shell.inbox_tick)(), reload());
        let tab = tab();
        // The digest tab has its own data; the counts still come from the inbox.
        let tab = if tab == "digest" { "fyi".to_string() } else { tab };
        let path = if tab == "done" { "/api/inbox?show=done".to_string() } else { format!("/api/inbox?bucket={tab}") };
        let server = shell.server.peek().clone();
        match api_send(&server, "GET", &path, None).await {
            Ok(v) => {
                if *data.peek() != v {
                    data.set(v);
                }
                error.set(None);
            }
            Err(e) => error.set(Some(e)),
        }
    });

    let change = move |(item, body): (String, Value)| async move {
        let server = shell.server.peek().clone();
        match api_send(&server, "POST", &format!("/api/inbox/{item}"), Some(body)).await {
            Ok(_) => reload += 1,
            Err(e) => error.set(Some(e)),
        }
    };

    let to_plan = move |item: String| async move {
        let server = shell.server.peek().clone();
        match api_send(&server, "POST", "/api/planner", Some(json!({ "title": "", "item_id": item }))).await {
            Ok(_) => error.set(None),
            Err(e) => error.set(Some(e)),
        }
    };

    let counts = data()["counts"].clone();
    let entries = data()["items"].as_array().cloned().unwrap_or_default();
    rsx! {
        section { class: "panel",
            div { class: "bar",
                span { class: "title", "Inbox" }
            }
            div { class: "inbox-tabs", role: "tablist",
                for (id, label) in BUCKETS {
                    button {
                        key: "{id}",
                        role: "tab",
                        aria_selected: tab() == id,
                        class: if tab() == id { "inbox-tab active" } else { "inbox-tab" },
                        onclick: move |_| tab.set(id.to_string()),
                        "{label}"
                        span { class: "badge", "{counts[id].as_u64().unwrap_or(0)}" }
                    }
                }
                button {
                    role: "tab",
                    aria_selected: tab() == "digest",
                    class: if tab() == "digest" { "inbox-tab active" } else { "inbox-tab" },
                    title: "What arrived that needs nothing from you, grouped",
                    onclick: move |_| tab.set("digest".to_string()),
                    "Digest"
                }
                button {
                    role: "tab",
                    aria_selected: tab() == "done",
                    class: if tab() == "done" { "inbox-tab active" } else { "inbox-tab" },
                    onclick: move |_| tab.set("done".to_string()),
                    "Done"
                }
            }
            div { class: "panel-body",
                if let Some(e) = error() {
                    p { class: "p-text error", role: "alert", "{e}" }
                }
                if tab() == "digest" {
                    crate::planner::Digest {}
                } else if entries.is_empty() && data() != Value::Null {
                    p { class: "p-empty", "Nothing here." }
                }
                ul { class: "p-list", hidden: tab() == "digest",
                    for entry in entries {
                        li { key: "{entry[\"item\"][\"id\"]}",
                            Entry { entry, on_change: change, on_plan: to_plan }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Entry(entry: Value, on_change: EventHandler<(String, Value)>, on_plan: EventHandler<String>) -> Element {
    let (item, triage) = (&entry["item"], &entry["triage"]);
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let id = text(&item["id"]);
    let (title, url) = (text(&item["title"]), text(&item["url"]));
    let from = item["actors"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    let place = [text(&item["source"]), text(&item["thread"])].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
    let (summary, draft, reason) = (text(&triage["summary"]), text(&triage["draft"]), text(&triage["reason"]));
    let bucket = text(&triage["bucket"]);
    let priority = triage["priority"].as_u64().unwrap_or(0);
    let done = triage["done"] == true;
    let mut view = use_context::<Shell>().view;
    let reviewable = matches!(item["kind"].as_str(), Some("pull_request") | Some("review_request"));
    let (id_review, title_review) = (id.clone(), title.clone());
    let id_plan = id.clone();
    let (id_move, id_rank, id_done, id_snooze) = (id.clone(), id.clone(), id.clone(), id.clone());
    rsx! {
        article { class: "inbox-entry",
            div { class: "p-row",
                span { class: "p-badge prio-{priority}", title: "Priority {priority} of 3", "P{priority}" }
                if web_url(&url) {
                    Link { url, label: title.clone() }
                } else {
                    span { class: "p-text strong", "{title}" }
                }
            }
            div { class: "p-text muted",
                "{place}"
                if !from.is_empty() {
                    " · {from}"
                }
            }
            if !summary.is_empty() && summary != title {
                p { class: "inbox-summary", "{summary}" }
            }
            if !draft.is_empty() {
                details { class: "inbox-draft",
                    summary { "Draft reply" }
                    pre { class: "p-markdown", "{draft}" }
                }
            }
            div { class: "p-row inbox-actions",
                label { class: "inbox-control",
                    span { "Move to" }
                    select {
                        aria_label: "Move {title} to another bucket",
                        onchange: move |e| on_change.call((id_move.clone(), json!({ "bucket": e.value() }))),
                        for (b, label) in BUCKETS {
                            option { value: "{b}", selected: b == bucket, "{label}" }
                        }
                    }
                }
                label { class: "inbox-control",
                    span { "Priority" }
                    select {
                        aria_label: "Priority of {title}",
                        onchange: move |e| {
                            if let Ok(p) = e.value().parse::<u8>() {
                                on_change.call((id_rank.clone(), json!({ "priority": p })));
                            }
                        },
                        for p in 0..=3u64 {
                            option { value: "{p}", selected: p == priority, "{p}" }
                        }
                    }
                }
                if reviewable {
                    button {
                        class: "toggle",
                        title: "Open the change: files, risk map and diff",
                        onclick: move |_| view.set(Some(View::Review(id_review.clone(), title_review.clone()))),
                        "Review"
                    }
                }
                if !done {
                    button {
                        class: "toggle",
                        title: "Add to your plan as a task",
                        onclick: move |_| on_plan.call(id_plan.clone()),
                        "Plan"
                    }
                }
                if !done {
                    button {
                        class: "toggle",
                        title: "Hide until tomorrow",
                        onclick: move |_| {
                            let until = (js_now_secs() + 24 * 3600) as u64;
                            on_change.call((id_snooze.clone(), json!({ "snooze_until": until })));
                        },
                        "Snooze 1 day"
                    }
                }
                button {
                    class: "toggle",
                    onclick: move |_| on_change.call((id_done.clone(), json!({ "done": !done }))),
                    if done { "Reopen" } else { "Done" }
                }
                span { class: "p-text muted inbox-reason", title: "Why it is here", "{reason}" }
            }
        }
    }
}

/// Unix seconds on every platform (the browser build has no system clock in std).
fn js_now_secs() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        (js_sys::Date::now() / 1000.0) as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_counts_only_what_asks_for_something() {
        assert_eq!(pending(&json!({ "reply": 2, "act": 1, "prepare": 3, "fyi": 9, "noise": 40 })), 6);
        assert_eq!(pending(&Value::Null), 0);
    }
}
