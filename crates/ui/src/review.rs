//! Review view: what a change touches, where the risk is, the diff per file,
//! and an optional summary from the operator's model.

use dioxus::prelude::*;
use serde_json::Value;

use crate::browser::Link;
use crate::{api_send, Shell};

#[component]
pub fn ReviewView(item: String, title: String) -> Element {
    let shell = use_context::<Shell>();
    let mut report = use_signal(|| None::<Value>);
    let mut error = use_signal(|| None::<String>);
    let mut summary = use_signal(|| None::<Value>);
    let mut summary_error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);

    let load_item = item.clone();
    use_resource(move || {
        let path = format!("/api/review/{load_item}");
        async move {
            let server = shell.server.peek().clone();
            match api_send(&server, "GET", &path, None).await {
                Ok(v) => report.set(Some(v)),
                Err(e) => error.set(Some(e)),
            }
        }
    });

    let summarize = move |_| {
        let path = format!("/api/review/{item}/summary");
        async move {
            if *busy.peek() {
                return;
            }
            busy.set(true);
            summary_error.set(None);
            let server = shell.server.peek().clone();
            match api_send(&server, "POST", &path, None).await {
                Ok(v) => summary.set(Some(v)),
                Err(e) => summary_error.set(Some(e)),
            }
            busy.set(false);
        }
    };

    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let strings = |v: &Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default() };
    rsx! {
        section { class: "panel",
            div { class: "bar",
                span { class: "title", "Review · {title}" }
            }
            div { class: "panel-body",
                if let Some(e) = error() {
                    p { class: "p-text error", role: "alert", "{e}" }
                }
                if report().is_none() && error().is_none() {
                    p { class: "p-empty", "Loading the change…" }
                }
                if let Some(r) = report() {
                    div { class: "p-stack",
                        div { class: "p-row",
                            if text(&r["change"]["url"]).starts_with("https://") {
                                Link { url: text(&r["change"]["url"]), label: text(&r["change"]["title"]) }
                            } else {
                                span { class: "p-text strong", "{text(&r[\"change\"][\"title\"])}" }
                            }
                        }
                        div { class: "p-row",
                            span { class: "p-text muted", "{text(&r[\"change\"][\"author\"])} · {text(&r[\"change\"][\"head\"])} → {text(&r[\"change\"][\"base\"])}" }
                            span { class: "p-badge", "{r[\"totals\"][\"files\"]} files" }
                            span { class: "p-badge ok", "+{r[\"totals\"][\"additions\"]}" }
                            span { class: "p-badge warn", "−{r[\"totals\"][\"deletions\"]}" }
                            if r["totals"]["high"].as_u64().unwrap_or(0) > 0 {
                                span { class: "p-badge prio-3", "{r[\"totals\"][\"high\"]} high risk" }
                            }
                        }
                        for note in strings(&r["notes"]) {
                            p { class: "p-text muted", "{note}" }
                        }

                        section { class: "p-card",
                            h3 { "Summary" }
                            match summary() {
                                Some(s) => rsx! {
                                    p { class: "inbox-summary", "{text(&s[\"summary\"])}" }
                                    if !strings(&s["risks"]).is_empty() {
                                        h3 { "Look closely at" }
                                        ul { for risk in strings(&s["risks"]) { li { "{risk}" } } }
                                    }
                                    if !strings(&s["questions"]).is_empty() {
                                        h3 { "Ask the author" }
                                        ul { for q in strings(&s["questions"]) { li { "{q}" } } }
                                    }
                                    if !strings(&s["files_left_out"]).is_empty() {
                                        p { class: "p-text muted", "Not read by the model: {strings(&s[\"files_left_out\"]).join(\", \")}" }
                                    }
                                    p { class: "p-text muted", "Written by the model your server is configured with. Check it against the diff." }
                                },
                                None => rsx! {
                                    div { class: "p-row",
                                        button { class: "toggle", disabled: busy(), onclick: summarize,
                                            if busy() { "Summarizing…" } else { "Summarize with the model" }
                                        }
                                        span { class: "p-text muted", "Sends the diff to the model configured on your server." }
                                    }
                                    if let Some(e) = summary_error() {
                                        p { class: "p-text error", role: "alert", "{e}" }
                                    }
                                },
                            }
                        }

                        section { class: "p-card",
                            h3 { "Risk map" }
                            ul { class: "p-list",
                                for risk in r["risks"].as_array().cloned().unwrap_or_default() {
                                    li { key: "{risk[\"path\"]}",
                                        div { class: "p-row",
                                            span { class: "p-badge risk-{text(&risk[\"level\"])}", "{text(&risk[\"level\"])}" }
                                            span { class: "p-text strong", "{text(&risk[\"path\"])}" }
                                            span { class: "p-text muted", "{strings(&risk[\"reasons\"]).join(\" · \")}" }
                                        }
                                    }
                                }
                            }
                        }

                        for file in r["change"]["files"].as_array().cloned().unwrap_or_default() {
                            details { key: "{file[\"path\"]}", class: "review-file",
                                summary {
                                    span { class: "p-text strong", "{text(&file[\"path\"])}" }
                                    span { class: "p-text muted", " {text(&file[\"status\"])} +{file[\"additions\"]} −{file[\"deletions\"]}" }
                                }
                                if text(&file["patch"]).is_empty() {
                                    p { class: "p-empty", "No diff to show (binary or too large)." }
                                } else {
                                    pre { class: "p-code",
                                        for (i, line) in text(&file["patch"]).lines().enumerate() {
                                            span {
                                                key: "{i}",
                                                class: if line.starts_with('+') { "add" } else if line.starts_with('-') { "del" } else if line.starts_with("@@") { "hunk" } else { "" },
                                                "{line}\n"
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
}

/// "Explain this": sends the current text selection to the server's model
/// and shows the answer in a card over the view.
#[component]
pub fn Explainer() -> Element {
    let shell = use_context::<Shell>();
    let mut answer = use_signal(|| None::<Result<String, String>>);
    let mut busy = use_signal(|| false);

    let explain = move |_| async move {
        if *busy.peek() {
            return;
        }
        let selection = crate::net::selected_text().await.unwrap_or_default();
        if selection.trim().is_empty() {
            answer.set(Some(Err("Select some text first, then press Explain.".into())));
            return;
        }
        busy.set(true);
        let context = match &*shell.view.peek() {
            Some(crate::View::Terminal(_)) => "terminal output",
            Some(crate::View::Agent(..)) => "an agent's transcript",
            Some(crate::View::Review(..)) => "a code change under review",
            Some(crate::View::Inbox) => "an inbox item",
            Some(crate::View::Push) => "the settings",
            Some(crate::View::Plan) => "the planner",
            _ => "the app",
        };
        let server = shell.server.peek().clone();
        let body = serde_json::json!({ "text": selection, "context": context });
        let result = api_send(&server, "POST", "/api/explain", Some(body)).await;
        answer.set(Some(result.map(|v| v["explanation"].as_str().unwrap_or_default().to_string())));
        busy.set(false);
    };

    rsx! {
        div { class: "explainer",
            if let Some(result) = answer() {
                div { class: "explain-card", role: "status",
                    div { class: "p-row",
                        span { class: "p-text strong", "Explanation" }
                        button { class: "icon-button small", aria_label: "Close explanation", onclick: move |_| answer.set(None), crate::browser::Icon { name: "close" } }
                    }
                    match result {
                        Ok(text) => rsx! { div { class: "p-markdown", "{text}" } },
                        Err(e) => rsx! { p { class: "p-text error", "{e}" } },
                    }
                }
            }
            button {
                class: "toggle explain-button",
                title: "Explain the selected text with the model configured on your server",
                disabled: busy(),
                // Keep the selection: a button that takes focus on mouse down would clear it in some webviews.
                onmousedown: move |e| e.prevent_default(),
                onclick: explain,
                if busy() { "Explaining…" } else { "Explain selection" }
            }
        }
    }
}
