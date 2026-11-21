//! Which agents may read or type into a terminal session.
//!
//! An agent only reaches the session it runs in and the ones it opened. A
//! person can let the agent of another session read or drive this one.

use dioxus::prelude::*;
use serde_json::{json, Value};

use crate::model::SessionInfo;
use crate::{api_get, api_send, Shell};

/// The sessions whose agent could be given access to `target`, with the
/// access each one has now (`""`, `read` or `write`).
pub fn candidates(target: &str, sessions: &[SessionInfo], grants: &Value) -> Vec<(SessionInfo, String)> {
    sessions
        .iter()
        .filter(|s| s.id != target && !s.exited)
        .map(|s| (s.clone(), grants[&s.id].as_str().unwrap_or("").to_string()))
        .collect()
}

#[component]
pub fn Grants(id: String) -> Element {
    let shell = use_context::<Shell>();
    let mut sessions = use_signal(Vec::<SessionInfo>::new);
    let mut grants = use_signal(|| Value::Null);
    let mut error = use_signal(|| None::<String>);
    let mut reload = use_signal(|| 0u32);

    let load_id = id.clone();
    use_resource(move || {
        let id = load_id.clone();
        let _ = reload();
        let _ = (shell.events_tick)();
        async move {
            let server = shell.server.peek().clone();
            match api_send(&server, "GET", &format!("/api/sessions/{id}/grants"), None).await {
                Ok(v) => grants.set(v["grants"].clone()),
                Err(e) => {
                    error.set(Some(e));
                    return;
                }
            }
            let list = api_get(&server, "/api/sessions?all=1")
                .await
                .and_then(|v| serde_json::from_value(v["sessions"].clone()).ok())
                .unwrap_or_default();
            sessions.set(list);
        }
    });

    let rows = candidates(&id, &sessions(), &grants());

    rsx! {
        div { class: "grants", role: "group", aria_label: "Agent access to this terminal",
            p { class: "grants-help",
                "An agent reaches its own terminal and the ones it opened. Here you let the agent of another terminal read or type into this one."
            }
            if let Some(e) = error() {
                p { class: "grants-error", role: "alert", "{e}" }
            }
            if rows.is_empty() && error().is_none() {
                p { class: "grants-help", "No other terminal is running." }
            }
            for (s, scope) in rows {
                label { key: "{s.id}", class: "grants-row",
                    span { class: "title", "Agent in “{s.title}”" }
                    select {
                        aria_label: "Access for the agent in {s.title}",
                        onchange: {
                            let target = id.clone();
                            let agent = s.id.clone();
                            move |e: FormEvent| {
                                let target = target.clone();
                                let agent = agent.clone();
                                async move {
                                    let scope = match e.value().as_str() {
                                        "read" => json!("read"),
                                        "write" => json!("write"),
                                        _ => Value::Null,
                                    };
                                    let server = shell.server.peek().clone();
                                    let body = json!({ "agent": agent, "scope": scope });
                                    match api_send(&server, "POST", &format!("/api/sessions/{target}/grants"), Some(body)).await {
                                        Ok(_) => error.set(None),
                                        Err(e) => error.set(Some(e)),
                                    }
                                    reload += 1;
                                }
                            }
                        },
                        option { value: "", selected: scope.is_empty(), "No access" }
                        option { value: "read", selected: scope == "read", "Can read" }
                        option { value: "write", selected: scope == "write", "Can read and type" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, exited: bool) -> SessionInfo {
        serde_json::from_value(json!({
            "id": id, "title": id, "cols": 80, "rows": 24, "exited": exited, "locked": false, "agents": 0
        }))
        .unwrap()
    }

    #[test]
    fn lists_other_running_sessions_with_their_access() {
        let all = [session("s1", false), session("s2", false), session("s3", true), session("s4", false)];
        let rows = candidates("s1", &all, &json!({ "s2": "write" }));
        let got: Vec<(&str, &str)> = rows.iter().map(|(s, scope)| (s.id.as_str(), scope.as_str())).collect();
        assert_eq!(got, [("s2", "write"), ("s4", "")]);
        // No grants yet: the server sends an empty object or nothing.
        assert!(candidates("s1", &all, &Value::Null).iter().all(|(_, scope)| scope.is_empty()));
    }
}
