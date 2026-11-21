//! Workspaces the client is signed in to, and the screen that adds one.
//!
//! A workspace is a server URL plus the token that server issued. The client
//! can hold several and switch between them.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::net;

const STORAGE_KEY: &str = "workspaces";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub url: String,
    pub token: String,
    pub user: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Accounts {
    pub list: Vec<Workspace>,
    pub active: Option<usize>,
}

impl Accounts {
    pub fn load() -> Accounts {
        let mut accounts: Accounts =
            net::storage_get(STORAGE_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        if accounts.active.is_some_and(|i| i >= accounts.list.len()) {
            accounts.active = None;
        }
        accounts
    }

    pub fn save(&self) {
        if let Ok(text) = serde_json::to_string(self) {
            net::storage_set(STORAGE_KEY, &text);
        }
    }

    pub fn current(&self) -> Option<&Workspace> {
        self.list.get(self.active?)
    }

    /// Adds a workspace, replacing an earlier sign-in to the same server as
    /// the same user, and makes it the active one.
    pub fn add(&mut self, ws: Workspace) {
        match self.list.iter().position(|w| w.url == ws.url && w.user.eq_ignore_ascii_case(&ws.user)) {
            Some(i) => {
                self.list[i] = ws;
                self.active = Some(i);
            }
            None => {
                self.list.push(ws);
                self.active = Some(self.list.len() - 1);
            }
        }
    }

    pub fn remove_current(&mut self) {
        if let Some(i) = self.active {
            self.list.remove(i);
            self.active = if self.list.is_empty() { None } else { Some(0) };
        }
    }
}

pub fn normalize_url(input: &str) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

async fn post(url: &str, path: &str, body: Value) -> Result<Value, String> {
    let res = reqwest::Client::new()
        .post(format!("{url}{path}"))
        .json(&body)
        .send()
        .await
        .map_err(|_| format!("Cannot reach {url}. Check the address and that the server is running."))?;
    let ok = res.status().is_success();
    let value: Value = res.json().await.unwrap_or(Value::Null);
    if ok {
        Ok(value)
    } else {
        Err(value["error"].as_str().unwrap_or("The server refused the request.").to_string())
    }
}

/// Sign in to a server, or create its first owner when it is brand new.
///
/// The fields are uncontrolled (`initial_value`): on native webviews events
/// cross a process boundary, and writing state back into a field while the
/// user types drops characters.
#[component]
pub fn ConnectScreen(accounts: Signal<Accounts>, on_cancel: Option<EventHandler<()>>) -> Element {
    let mut url = use_signal(net::default_server);
    let mut name = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut code = use_signal(String::new);
    let mut workspace = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    // None: not asked yet. Some(true): the server has no users.
    let mut setup_needed = use_signal(|| None::<bool>);

    // Ask the server what it needs whenever the address changes.
    use_effect(move || {
        let target = normalize_url(&url());
        spawn(async move {
            let info: Option<Value> = match reqwest::get(format!("{target}/api/workspace")).await {
                Ok(res) => res.json().await.ok(),
                Err(_) => None,
            };
            if normalize_url(&url.peek()) == target {
                setup_needed.set(info.map(|v| v["setup_needed"].as_bool().unwrap_or(false)));
            }
        });
    });

    let submit = move |e: FormEvent| {
        e.prevent_default();
        if busy() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn(async move {
            let target = normalize_url(&url.peek());
            let user = name.peek().trim().to_string();
            let result = if *setup_needed.peek() == Some(true) {
                let body = json!({
                    "code": code.peek().trim(), "name": user, "password": *password.peek(),
                    "workspace": workspace.peek().trim(),
                });
                post(&target, "/api/setup", body).await
            } else {
                post(&target, "/api/auth/login", json!({ "name": user, "password": *password.peek(), "device": "app" })).await
            };
            match result {
                Ok(v) => {
                    let token = v["token"].as_str().unwrap_or_default().to_string();
                    let ws_name = match reqwest::get(format!("{target}/api/workspace")).await {
                        Ok(res) => res.json::<Value>().await.ok().and_then(|w| w["name"].as_str().map(str::to_string)),
                        Err(_) => None,
                    };
                    let mut all = accounts.write();
                    all.add(Workspace {
                        name: ws_name.unwrap_or_else(|| target.clone()),
                        url: target,
                        token,
                        user,
                        role: v["role"].as_str().unwrap_or("member").to_string(),
                    });
                    all.save();
                }
                Err(message) => error.set(Some(message)),
            }
            busy.set(false);
        });
    };

    let first_run = setup_needed() == Some(true);
    rsx! {
        div { class: "connect",
            form { class: "connect-card", onsubmit: submit,
                h1 { crate::Wordmark {} }
                p { class: "hint",
                    if first_run {
                        "This server is new. Create its owner account; the setup code is printed where the server runs."
                    } else {
                        "Sign in to a workspace. Each workspace is a server you or your team runs."
                    }
                }
                label { "Server"
                    input {
                        r#type: "text",
                        initial_value: "{url}",
                        autocapitalize: "off",
                        spellcheck: false,
                        oninput: move |e| url.set(e.value()),
                    }
                }
                if setup_needed().is_none() {
                    p { class: "hint", "No answer from that address yet." }
                }
                if first_run {
                    label { "Setup code"
                        input { r#type: "text", initial_value: "{code}", autocapitalize: "off", spellcheck: false, oninput: move |e| code.set(e.value()) }
                    }
                    label { "Workspace name"
                        input { r#type: "text", initial_value: "{workspace}", placeholder: "personal", oninput: move |e| workspace.set(e.value()) }
                    }
                }
                label { "User name"
                    input { r#type: "text", initial_value: "{name}", autocapitalize: "off", autocomplete: "username", spellcheck: false, oninput: move |e| name.set(e.value()) }
                }
                label { if first_run { "Password (10 characters or more)" } else { "Password" }
                    input {
                        r#type: "password",
                        initial_value: "{password}",
                        autocomplete: if first_run { "new-password" } else { "current-password" },
                        oninput: move |e| password.set(e.value()),
                    }
                }
                if let Some(message) = error() {
                    p { class: "error", role: "alert", "{message}" }
                }
                div { class: "actions",
                    if let Some(cancel) = on_cancel {
                        button { r#type: "button", class: "toggle", onclick: move |_| cancel.call(()), "Cancel" }
                    }
                    button { r#type: "submit", class: "primary", disabled: busy(),
                        if busy() { "Working…" } else if first_run { "Create owner" } else { "Sign in" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(url: &str, user: &str, token: &str) -> Workspace {
        Workspace { name: url.into(), url: url.into(), token: token.into(), user: user.into(), role: "member".into() }
    }

    #[test]
    fn workspaces() {
        let mut a = Accounts::default();
        a.add(ws("http://home:8092", "ana", "t1"));
        a.add(ws("http://work:8092", "ana", "t2"));
        assert_eq!(a.current().unwrap().token, "t2");
        // Signing in again to the same server as the same user replaces the token.
        a.add(ws("http://home:8092", "Ana", "t3"));
        assert_eq!(a.list.len(), 2);
        assert_eq!(a.current().unwrap().token, "t3");
        a.remove_current();
        assert_eq!(a.current().unwrap().url, "http://work:8092");
        a.remove_current();
        assert!(a.current().is_none());
        assert_eq!(normalize_url(" example.org:8092/ "), "http://example.org:8092");
        assert_eq!(normalize_url("https://x.io"), "https://x.io");
    }
}
