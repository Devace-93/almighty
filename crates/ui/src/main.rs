//! almighty client (milestone-0 spike): session list with agent tree, a
//! terminal panel drawn from the server's grid, and a side browser panel.

// Windows release builds are GUI programs: no console window next to the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod account;
mod browser;
mod inbox;
mod model;
mod net;
mod panel;
mod planner;
mod review;
mod settings;
#[cfg(all(feature = "desktop", target_os = "linux"))]
mod sidebrowser;
#[cfg(all(feature = "desktop", target_os = "windows"))]
#[path = "sidebrowser_windows.rs"]
mod sidebrowser;

/// Whether this build can show a page next to the terminal. The browser
/// build uses an iframe; Linux and Windows desktop use native webviews.
/// Elsewhere links go to the system browser until a child webview exists.
pub(crate) const HAS_SIDE_BROWSER: bool = cfg!(target_arch = "wasm32")
    || cfg!(all(feature = "desktop", any(target_os = "linux", target_os = "windows")));

/// Opens a URL outside the app on native desktop builds.
pub(crate) fn open_external(_url: &str) {
    #[cfg(not(any(target_arch = "wasm32", target_os = "android", target_os = "ios")))]
    {
        let _ = webbrowser::open(_url);
    }
}

use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedReceiver;
use serde_json::{json, Value};

use account::{Accounts, ConnectScreen};
use browser::{Browser, BrowserPanel, Icon, LinkedText};
use model::{exit_label, session_tree, split_links, AgentNode, Grid, Piece, Run, SessionInfo, TranscriptLine};

const CSS: Asset = asset!("/assets/main.css");
const ICON: Asset = asset!("/assets/icon.svg");

/// Cell width used until the terminal font has been measured.
const CELL_W: f64 = 7.83;
const CELL_H: f64 = 17.0;

#[derive(Clone, PartialEq)]
pub(crate) enum View {
    Terminal(String),
    Agent(String, String),
    /// A plugin panel: plugin id, panel id, title.
    Panel(String, String, String),
    Inbox,
    /// Where phone notifications go.
    Push,
    Plan,
    /// Review of the change behind an item: item id, title.
    Review(String, String),
}

#[derive(Clone, Copy)]
pub(crate) struct Shell {
    pub(crate) server: Signal<String>,
    /// Token of the active workspace; sent with every request.
    pub(crate) token: Signal<String>,
    accounts: Signal<Accounts>,
    pub(crate) view: Signal<Option<View>>,
    pub(crate) browser: Signal<Browser>,
    /// Goes up when the server reports a change in plugins or items.
    pub(crate) plugins_tick: Signal<u32>,
    /// Goes up when the server reports a change in this person's inbox.
    pub(crate) inbox_tick: Signal<u32>,
    /// Goes up on every event from the server.
    pub(crate) events_tick: Signal<u32>,
}

fn main() {
    // Desktop windows get the app's name instead of the default "Dioxus App".
    #[cfg(all(feature = "desktop", not(any(target_os = "android", target_os = "ios"))))]
    {
        use dioxus::desktop::{Config, WindowBuilder};
        let icon = dioxus::desktop::icon_from_memory(include_bytes!("../assets/icon-128.png")).ok();
        let window = WindowBuilder::new().with_title("almighty").with_window_icon(icon);
        // No default "Window / Edit" menu bar: the app has no use for it yet.
        let config = Config::new().with_window(window).with_menu(None);
        dioxus::LaunchBuilder::new().with_cfg(config).launch(App);
        return;
    }
    #[allow(unreachable_code)]
    dioxus::launch(App);
}

/// Token of the active workspace, readable from any async task.
pub(crate) static TOKEN: GlobalSignal<String> = Signal::global(String::new);

pub(crate) async fn api_get(server: &str, path: &str) -> Option<Value> {
    let token = TOKEN.peek().clone();
    let res = reqwest::Client::new().get(format!("{server}{path}")).bearer_auth(token).send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.json().await.ok()
}

async fn api_post(server: &str, path: &str, body: Value) -> Option<Value> {
    let token = TOKEN.peek().clone();
    let res = reqwest::Client::new().post(format!("{server}{path}")).bearer_auth(token).json(&body).send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.json().await.ok()
}

/// Like `api_get`/`api_post`, but keeps the server's error message.
pub(crate) async fn api_send(server: &str, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let token = TOKEN.peek().clone();
    let client = reqwest::Client::new();
    let mut req = if method == "POST" { client.post(format!("{server}{path}")) } else { client.get(format!("{server}{path}")) };
    req = req.bearer_auth(token);
    if let Some(body) = body {
        req = req.json(&body);
    }
    let res = req.send().await.map_err(|_| "The server cannot be reached.".to_string())?;
    let ok = res.status().is_success();
    let value: Value = res.json().await.unwrap_or(Value::Null);
    if ok {
        Ok(value)
    } else {
        Err(value["error"].as_str().unwrap_or("The request failed.").to_string())
    }
}

async fn api_delete(server: &str, path: &str) -> bool {
    let token = TOKEN.peek().clone();
    match reqwest::Client::new().delete(format!("{server}{path}")).bearer_auth(token).send().await {
        Ok(res) => res.status().is_success(),
        Err(_) => false,
    }
}

/// `Some(false)` when the server says the token is no longer valid, `None`
/// when the server cannot be reached at all.
async fn token_valid(server: &str) -> Option<bool> {
    let token = TOKEN.peek().clone();
    let res = reqwest::Client::new().get(format!("{server}/api/auth/me")).bearer_auth(token).send().await.ok()?;
    Some(res.status() != reqwest::StatusCode::UNAUTHORIZED)
}

/// WebSocket URL for `path` on `server`, carrying the token (browsers
/// cannot set headers on WebSocket requests).
pub(crate) fn ws_url(server: &str, path: &str) -> String {
    format!("{}{path}?token={}", server.replacen("http", "ws", 1), TOKEN.peek())
}

#[component]
fn App() -> Element {
    let accounts = use_signal(Accounts::load);
    let mut adding = use_signal(|| false);
    let shell = Shell {
        server: use_signal(net::default_server),
        token: use_signal(String::new),
        accounts,
        view: use_signal(|| None),
        browser: use_signal(Browser::default),
        plugins_tick: use_signal(|| 0u32),
        inbox_tick: use_signal(|| 0u32),
        events_tick: use_signal(|| 0u32),
    };
    use_context_provider(|| shell);

    let current = accounts().current().cloned();
    // Point every request at the active workspace before its panels render.
    if let Some(ws) = &current {
        if *shell.server.peek() != ws.url || *TOKEN.peek() != ws.token {
            let (mut server, mut token) = (shell.server, shell.token);
            server.set(ws.url.clone());
            token.set(ws.token.clone());
            *TOKEN.write() = ws.token.clone();
        }
    }

    rsx! {
        document::Stylesheet { href: CSS }
        document::Link { rel: "icon", r#type: "image/svg+xml", href: ICON }
        match current {
            Some(ws) if !adding() => rsx! {
                // Keyed: switching workspace rebuilds every panel from scratch.
                Workbench { key: "{ws.url}|{ws.user}|{ws.token}", on_add: move |_| adding.set(true) }
            },
            Some(_) => rsx! {
                ConnectScreen { accounts, on_cancel: move |_| adding.set(false) }
            },
            None => rsx! { ConnectScreen { accounts } },
        }
    }
}

#[component]
fn Workbench(on_add: EventHandler<()>) -> Element {
    let shell = use_context::<Shell>();
    let mut view = shell.view;
    let mut browser = shell.browser;
    // A new workspace starts with nothing selected and no pages open.
    use_hook(move || {
        view.set(None);
        browser.set(Browser::default());
    });
    // Phones show either the menu or the chosen view; picking something closes the menu.
    let mut menu = use_signal(|| true);
    use_effect(move || {
        let chosen = view().is_some();
        menu.set(!chosen);
    });
    let layout = match (browser().visible(), menu()) {
        (true, true) => "app with-browser menu-open",
        (true, false) => "app with-browser menu-closed",
        (false, true) => "app menu-open",
        (false, false) => "app menu-closed",
    };
    rsx! {
        div { class: layout,
            button { class: "menu-button toggle", aria_expanded: menu(), onclick: move |_| menu.toggle(),
                if menu() { "Close menu" } else { "Menu" }
            }
            Sidebar { on_add }
            main { class: "center",
                match view() {
                    Some(View::Terminal(id)) => rsx! { TerminalView { key: "{id}", id } },
                    Some(View::Agent(sid, aid)) => rsx! { AgentView { key: "{sid}-{aid}", session: sid, agent: aid } },
                    Some(View::Panel(plugin, panel, title)) => rsx! { panel::PanelView { key: "{plugin}/{panel}", plugin, panel, title } },
                    Some(View::Inbox) => rsx! { inbox::InboxView {} },
                    Some(View::Push) => rsx! { settings::PushSettings {} },
                    Some(View::Plan) => rsx! { planner::PlannerView {} },
                    Some(View::Review(item, title)) => rsx! { review::ReviewView { key: "{item}", item, title } },
                    None => rsx! { div { class: "empty", "Pick a session, or start a new terminal." } },
                }
            }
            if browser().visible() {
                BrowserPanel {}
            }
            review::Explainer {}
        }
    }
}

#[component]
fn Sidebar(on_add: EventHandler<()>) -> Element {
    let shell = use_context::<Shell>();
    let mut accounts = shell.accounts;
    let mut refresh = use_signal(|| 0u32);
    // Session whose close button was pressed and is waiting for an answer.
    let mut closing = use_signal(|| None::<String>);
    let mut view = shell.view;
    let mut sessions = use_signal(Vec::<SessionInfo>::new);
    let mut agents = use_signal(Vec::<AgentNode>::new);
    let mut online = use_signal(|| true);
    let mut plugins_tick = shell.plugins_tick;
    let mut inbox_tick = shell.inbox_tick;
    let mut events_tick = shell.events_tick;
    let mut inbox_counts = use_signal(|| Value::Null);
    let mut plan_counts = use_signal(|| Value::Null);

    use_resource(move || async move {
        let _ = inbox_tick();
        let server = shell.server.peek().clone();
        // Guests have no inbox: the row stays hidden for them.
        let counts = api_get(&server, "/api/inbox/counts").await.unwrap_or(Value::Null);
        if *inbox_counts.peek() != counts {
            inbox_counts.set(counts);
        }
        let plan = api_get(&server, "/api/planner").await.map(|v| v["counts"].clone()).unwrap_or(Value::Null);
        if *plan_counts.peek() != plan {
            plan_counts.set(plan);
        }
    });
    let mut panels = use_signal(Vec::<panel::PanelRef>::new);

    // Panels of installed plugins, in the order of the user's layout file.
    use_resource(move || async move {
        let _ = plugins_tick();
        let server = shell.server.peek().clone();
        let plugins = api_get(&server, "/api/plugins").await.unwrap_or(Value::Null);
        let layout = api_get(&server, "/api/layout").await.unwrap_or(Value::Null);
        let list = panel::panel_list(&plugins, &layout);
        if *panels.peek() != list {
            panels.set(list);
        }
    });

    // The server pushes "something changed"; the lists are then refetched.
    use_future(move || async move {
        loop {
            let (_tx, mut rx) = futures_channel::mpsc::unbounded::<String>();
            let url = ws_url(&shell.server.peek(), "/api/events");
            net::run_ws(url, &mut rx, move |text| {
                refresh += 1;
                events_tick += 1;
                if text.contains("\"type\":\"plugin") || text.contains("\"type\":\"item\"") {
                    plugins_tick += 1;
                }
                if text.contains("\"type\":\"inbox\"") || text.contains("\"type\":\"item\"") || text.contains("\"type\":\"planner\"") {
                    inbox_tick += 1;
                }
                if text.contains("\"type\":\"notify\"") {
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        net::notify(v["title"].as_str().unwrap_or("almighty"), v["body"].as_str().unwrap_or(""));
                    }
                }
            })
            .await;
            net::sleep_ms(2000).await; // connection lost: retry
        }
    });

    use_future(move || async move {
        loop {
            let server = shell.server.peek().clone();
            match api_get(&server, "/api/sessions").await {
                Some(v) => {
                    online.set(true);
                    let list: Vec<SessionInfo> = serde_json::from_value(v["sessions"].clone()).unwrap_or_default();
                    if *sessions.peek() != list {
                        sessions.set(list);
                    }
                }
                None => {
                    // Signed out elsewhere or token expired: back to sign-in.
                    match token_valid(&server).await {
                        Some(false) => {
                            let mut all = accounts.write();
                            all.remove_current();
                            all.save();
                            return;
                        }
                        Some(true) => online.set(true),
                        None => online.set(false),
                    }
                }
            }
            let current = match &*view.peek() {
                Some(View::Terminal(id)) | Some(View::Agent(id, _)) => Some(id.clone()),
                _ => None,
            };
            let list: Vec<AgentNode> = match current {
                Some(id) => api_get(&server, &format!("/api/sessions/{id}/agents"))
                    .await
                    .and_then(|v| serde_json::from_value(v["agents"].clone()).ok())
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            if *agents.peek() != list {
                agents.set(list);
            }
            // Wait for a push, or refetch every 10 s as a safety net.
            let seen = *refresh.peek();
            for _ in 0..40 {
                net::sleep_ms(250).await;
                if *refresh.peek() != seen {
                    break;
                }
            }
        }
    });

    let sign_out = move |_| async move {
        let server = shell.server.peek().clone();
        let _ = api_post(&server, "/api/auth/logout", json!({})).await;
        let mut all = accounts.write();
        all.remove_current();
        all.save();
    };
    let all = accounts();
    let active = all.active.unwrap_or(0);

    let new_terminal = move |_| async move {
        let server = shell.server.peek().clone();
        let body = json!({ "command": ["bash"], "cols": 100, "rows": 30 });
        if let Some(v) = api_post(&server, "/api/sessions", body).await {
            if let Some(id) = v["id"].as_str() {
                view.set(Some(View::Terminal(id.to_string())));
            }
        }
    };

    let selected = match view() {
        Some(View::Terminal(id)) | Some(View::Agent(id, _)) => Some(id),
        _ => None,
    };
    let selected_agent = match view() {
        Some(View::Agent(_, a)) => Some(a),
        _ => None,
    };

    rsx! {
        nav { class: "sidebar",
            header {
                Wordmark {}
                span { class: if online() { "dot on" } else { "dot off" }, title: "{shell.server}" }
            }
            div { class: "workspace",
                select {
                    aria_label: "Workspace",
                    onchange: move |e| {
                        if let Ok(i) = e.value().parse::<usize>() {
                            let mut all = accounts.write();
                            if i < all.list.len() {
                                all.active = Some(i);
                                all.save();
                            }
                        }
                    },
                    for (i, ws) in all.list.iter().enumerate() {
                        option { value: "{i}", selected: i == active, "{ws.name} · {ws.user}" }
                    }
                }
                button { class: "toggle", title: "Sign in to another workspace", onclick: move |_| on_add.call(()), "Add" }
                button { class: "toggle", title: "Sign out of this workspace", onclick: sign_out, "Sign out" }
                if inbox_counts() != Value::Null {
                    button { class: "toggle", title: "Where phone notifications go", onclick: move |_| view.set(Some(View::Push)), "Phone alerts" }
                }
            }
            button { class: "new", onclick: new_terminal, "+ New terminal" }
            if !online() {
                p { class: "hint", "Server not reachable at {shell.server}" }
            }
            if inbox_counts() != Value::Null {
                button {
                    class: if matches!(view(), Some(View::Inbox)) { "row active" } else { "row" },
                    onclick: move |_| view.set(Some(View::Inbox)),
                    span { class: "title", "Inbox" }
                    if inbox::pending(&inbox_counts()) > 0 {
                        span { class: "badge", title: "Items that need a reply, an action or preparation", "{inbox::pending(&inbox_counts())}" }
                    }
                }
            }
            if inbox_counts() != Value::Null {
                button {
                    class: if matches!(view(), Some(View::Plan)) { "row active" } else { "row" },
                    onclick: move |_| view.set(Some(View::Plan)),
                    span { class: "title", "Plan" }
                    if planner::pressing(&plan_counts()) > 0 {
                        span { class: "badge", title: "Overdue or due in the next 24 hours", "{planner::pressing(&plan_counts())}" }
                    }
                }
            }
            if !panels().is_empty() {
                h2 { class: "section", "Panels" }
                ul { class: "sessions",
                    for p in panels() {
                        li { key: "{p.plugin}/{p.panel}",
                            button {
                                class: if matches!(view(), Some(View::Panel(a, b, _)) if a == p.plugin && b == p.panel) { "row active" } else { "row" },
                                title: if p.web_url.is_some() { "Opens in the side panel" } else { "" },
                                onclick: {
                                    let p = p.clone();
                                    move |_| {
                                        let p = p.clone();
                                        async move {
                                            let Some(url) = p.web_url.clone() else {
                                                view.set(Some(View::Panel(p.plugin, p.panel, p.title)));
                                                return;
                                            };
                                            // A service's page runs on its own origin: it opens next to the
                                            // work, with a short-lived token that says who is looking. The
                                            // token travels in the fragment, which is not sent to servers.
                                            let server = shell.server.peek().clone();
                                            let token = api_post(&server, &format!("/api/plugins/{}/token", p.plugin), json!({}))
                                                .await
                                                .and_then(|v| v["token"].as_str().map(str::to_string));
                                            let url = match token {
                                                Some(token) => format!("{url}#almighty_token={token}"),
                                                None => url,
                                            };
                                            if HAS_SIDE_BROWSER {
                                                let mut browser = shell.browser;
                                                browser.write().open_url(&url);
                                            } else {
                                                open_external(&url);
                                            }
                                        }
                                    }
                                },
                                span { class: "title", "{p.title}" }
                                span { class: "kind", "{p.plugin_name}" }
                            }
                        }
                    }
                }
                h2 { class: "section", "Terminals" }
            }
            ul { class: "sessions",
                for (s, depth) in session_tree(&sessions()) {
                    li { key: "{s.id}",
                        div { class: "session-row", style: "padding-left: {depth * 14}px",
                            button {
                                class: if selected.as_deref() == Some(s.id.as_str()) && selected_agent.is_none() { "row active" } else { "row" },
                                title: if s.exited { exit_label(s.exit_code) } else { s.created_by.clone() },
                                onclick: {
                                    let id = s.id.clone();
                                    move |_| view.set(Some(View::Terminal(id.clone())))
                                },
                                span { class: if s.exited { "state exited" } else { "state running" } }
                                span { class: "title", "{s.title}" }
                                if s.agents > 0 {
                                    span { class: "badge", "{s.agents}" }
                                }
                            }
                            if s.access != "read" {
                                button {
                                    class: "icon-button small",
                                    aria_label: "Close session {s.title}",
                                    title: if s.exited { "Remove from the list" } else { "Close…" },
                                    onclick: {
                                        let id = s.id.clone();
                                        let exited = s.exited;
                                        move |_| {
                                            let id = id.clone();
                                            async move {
                                                // Nothing left to keep running: just remove it.
                                                if exited {
                                                    let server = shell.server.peek().clone();
                                                    if api_delete(&server, &format!("/api/sessions/{id}")).await {
                                                        if matches!(&*view.peek(), Some(View::Terminal(v)) | Some(View::Agent(v, _)) if *v == id) {
                                                            view.set(None);
                                                        }
                                                        refresh += 1;
                                                    }
                                                } else {
                                                    closing.set(Some(id));
                                                }
                                            }
                                        }
                                    },
                                    Icon { name: "close" }
                                }
                            }
                        }
                        if closing().as_deref() == Some(s.id.as_str()) {
                            div { class: "close-ask", role: "group", aria_label: "Close {s.title}",
                                span { "Still running. What should happen to it?" }
                                div { class: "close-actions",
                                    button {
                                        class: "toggle",
                                        title: "The process keeps running; the session leaves this list",
                                        onclick: {
                                            let id = s.id.clone();
                                            move |_| {
                                                let id = id.clone();
                                                async move {
                                                    let server = shell.server.peek().clone();
                                                    let done = api_post(&server, &format!("/api/sessions/{id}/background"), json!({ "background": true })).await.is_some();
                                                    if done && matches!(&*view.peek(), Some(View::Terminal(v)) | Some(View::Agent(v, _)) if *v == id) {
                                                        view.set(None);
                                                    }
                                                    closing.set(None);
                                                    refresh += 1;
                                                }
                                            }
                                        },
                                        "Keep in background"
                                    }
                                    button {
                                        class: "toggle danger",
                                        title: "Ends the process",
                                        onclick: {
                                            let id = s.id.clone();
                                            move |_| {
                                                let id = id.clone();
                                                async move {
                                                    let server = shell.server.peek().clone();
                                                    if api_delete(&server, &format!("/api/sessions/{id}")).await
                                                        && matches!(&*view.peek(), Some(View::Terminal(v)) | Some(View::Agent(v, _)) if *v == id)
                                                    {
                                                        view.set(None);
                                                    }
                                                    closing.set(None);
                                                    refresh += 1;
                                                }
                                            }
                                        },
                                        "End session"
                                    }
                                    button { class: "toggle", onclick: move |_| closing.set(None), "Cancel" }
                                }
                            }
                        }
                        if selected.as_deref() == Some(s.id.as_str()) {
                            ul { class: "agents",
                                for a in agents() {
                                    li { key: "{a.agent_id}",
                                        button {
                                            class: if selected_agent.as_deref() == Some(a.agent_id.as_str()) { "row agent active" } else { "row agent" },
                                            style: if a.parent.is_some() { "padding-left: 40px" } else { "" },
                                            onclick: {
                                                let sid = s.id.clone();
                                                let aid = a.agent_id.clone();
                                                move |_| view.set(Some(View::Agent(sid.clone(), aid.clone())))
                                            },
                                            span { class: if a.status == "running" { "state running" } else { "state done" } }
                                            span { class: "title",
                                                {a.description.clone().unwrap_or_else(|| a.agent_type.clone())}
                                            }
                                            span { class: "kind", "{a.agent_type}" }
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

/// Browser key event -> key name understood by the server's key encoder.
fn key_name(e: &KeyboardEvent) -> Option<String> {
    let m = e.modifiers();
    let (ctrl, alt, shift) = (m.contains(Modifiers::CONTROL), m.contains(Modifiers::ALT), m.contains(Modifiers::SHIFT));
    let (base, printable) = match e.key() {
        Key::Character(c) => (c, true),
        Key::Enter => ("Enter".to_string(), false),
        Key::Tab => ("Tab".to_string(), false),
        Key::Escape => ("Esc".to_string(), false),
        Key::Backspace => ("Backspace".to_string(), false),
        Key::Delete => ("Delete".to_string(), false),
        Key::Insert => ("Insert".to_string(), false),
        Key::ArrowUp => ("Up".to_string(), false),
        Key::ArrowDown => ("Down".to_string(), false),
        Key::ArrowLeft => ("Left".to_string(), false),
        Key::ArrowRight => ("Right".to_string(), false),
        Key::Home => ("Home".to_string(), false),
        Key::End => ("End".to_string(), false),
        Key::PageUp => ("PageUp".to_string(), false),
        Key::PageDown => ("PageDown".to_string(), false),
        Key::F1 => ("F1".to_string(), false),
        Key::F2 => ("F2".to_string(), false),
        Key::F3 => ("F3".to_string(), false),
        Key::F4 => ("F4".to_string(), false),
        Key::F5 => ("F5".to_string(), false),
        Key::F6 => ("F6".to_string(), false),
        Key::F7 => ("F7".to_string(), false),
        Key::F8 => ("F8".to_string(), false),
        Key::F9 => ("F9".to_string(), false),
        Key::F10 => ("F10".to_string(), false),
        Key::F11 => ("F11".to_string(), false),
        Key::F12 => ("F12".to_string(), false),
        _ => return None,
    };
    let base = if base == " " { "Space".to_string() } else { base };
    let mut name = String::new();
    if ctrl {
        name.push_str("C-");
    }
    if alt {
        name.push_str("M-");
    }
    // Shift is already folded into printable characters ("A", "!").
    if shift && !printable {
        name.push_str("S-");
    }
    name.push_str(&base);
    Some(name)
}

#[component]
fn TerminalView(id: String) -> Element {
    let shell = use_context::<Shell>();
    let mut grid = use_signal(Grid::default);
    let mut connected = use_signal(|| false);
    let mut can_write = use_signal(|| true);
    // Measured width of one character of the terminal font, in CSS pixels.
    let mut cell_w = use_signal(|| CELL_W);
    // Wheel movement not yet turned into whole lines.
    let mut wheel_rest = use_signal(|| 0.0f64);
    // Rich mode: images the application printed inline.
    let mut media = use_signal(Vec::<Value>::new);
    let mut zoomed = use_signal(|| None::<u64>);
    let media_id = id.clone();
    use_resource(move || {
        let path = format!("/api/sessions/{media_id}/media");
        let _ = (shell.events_tick)();
        async move {
            let server = shell.server.peek().clone();
            let list = api_get(&server, &path).await.and_then(|v| v["media"].as_array().cloned()).unwrap_or_default();
            if *media.peek() != list {
                media.set(list);
            }
        }
    });
    let media_base = format!("{}/api/sessions/{id}/media", shell.server.peek());

    let ws_id = id.clone();
    let tx = use_coroutine(move |mut rx: UnboundedReceiver<String>| {
        let id = ws_id.clone();
        async move {
            let url = ws_url(&shell.server.peek(), &format!("/api/sessions/{id}/grid"));
            connected.set(true);
            net::run_ws(url, &mut rx, move |text| {
                let Ok(v) = serde_json::from_str::<Value>(&text) else { return };
                if v["type"] == "full" {
                    if let Ok(g) = serde_json::from_value::<Grid>(v["grid"].clone()) {
                        grid.set(g);
                    }
                    can_write.set(v["can_write"].as_bool().unwrap_or(true));
                } else if v["type"] == "patch" {
                    let mut g = grid.write();
                    for line in v["lines"].as_array().into_iter().flatten() {
                        let i = line["i"].as_u64().unwrap_or(u64::MAX) as usize;
                        if let (Some(slot), Ok(runs)) =
                            (g.lines.get_mut(i), serde_json::from_value::<Vec<Run>>(line["runs"].clone()))
                        {
                            *slot = runs;
                        }
                    }
                    g.cursor_row = v["cursor_row"].as_u64().unwrap_or(0) as u16;
                    g.cursor_col = v["cursor_col"].as_u64().unwrap_or(0) as u16;
                    g.cursor_visible = v["cursor_visible"].as_bool().unwrap_or(true);
                    g.title = v["title"].as_str().unwrap_or("").to_string();
                    g.exited = v["exited"].as_bool().unwrap_or(false);
                    g.exit_code = v["exit_code"].as_u64().map(|c| c as u32);
                    g.locked = v["locked"].as_bool().unwrap_or(false);
                    g.offset = v["offset"].as_u64().unwrap_or(0) as usize;
                    g.scrollback_len = v["scrollback_len"].as_u64().unwrap_or(0) as usize;
                }
            })
            .await;
            connected.set(false);
        }
    });

    let onkeydown = move |e: KeyboardEvent| {
        let m = e.modifiers();
        // Terminal convention: Ctrl+Shift+C copies the selection, Ctrl+Shift+V pastes.
        if m.contains(Modifiers::CONTROL) && m.contains(Modifiers::SHIFT) {
            match e.key() {
                Key::Character(c) if c.eq_ignore_ascii_case("c") => {
                    e.prevent_default();
                    spawn(net::copy_selection());
                }
                Key::Character(c) if c.eq_ignore_ascii_case("v") => {
                    e.prevent_default();
                    spawn(async move {
                        if let Some(text) = net::read_clipboard().await {
                            tx.send(json!({ "type": "input", "text": text }).to_string());
                        }
                    });
                }
                // Other Ctrl+Shift shortcuts (developer tools…) stay with the browser.
                _ => {}
            }
            return;
        }
        if m.contains(Modifiers::META) {
            return;
        }
        if let Some(name) = key_name(&e) {
            e.prevent_default();
            tx.send(json!({ "type": "input", "keys": [name] }).to_string());
        }
    };

    let g = grid();
    let cursor_style = format!(
        "left: calc({} * var(--cell-w)); top: calc({} * var(--cell-h));",
        g.cursor_col, g.cursor_row
    );
    let screen_style = format!("--cell-w: {}px;", cell_w());
    let offset = g.offset;
    let locked = g.locked;

    rsx! {
        section { class: "terminal-panel",
            header { class: "bar",
                span { class: "title", if g.title.is_empty() { "{id}" } else { "{g.title}" } }
                span { class: "meta", "{g.cols}×{g.rows}" }
                if g.exited {
                    span { class: "tag", {exit_label(g.exit_code)} }
                }
                if offset > 0 {
                    button {
                        class: "toggle on",
                        title: "You are reading history. Jump back to the live output.",
                        onclick: move |_| tx.send(json!({ "type": "scroll", "offset": 0 }).to_string()),
                        "↑ {offset} lines · Latest"
                    }
                }
                if !connected() {
                    span { class: "tag", "disconnected" }
                }
                if !can_write() {
                    span { class: "tag", "read-only" }
                }
                button {
                    class: if locked { "toggle on" } else { "toggle" },
                    title: "When locked, agents cannot type into this session",
                    onclick: move |_| tx.send(json!({ "type": "lock", "locked": !locked }).to_string()),
                    if locked { "Agents locked out" } else { "Agents allowed" }
                }
            }
            if !media().is_empty() {
                div { class: if zoomed().is_some() { "media-strip zoomed" } else { "media-strip" }, role: "group", aria_label: "Images shown in this terminal",
                    for m in media() {
                        button {
                            key: "{m[\"id\"]}",
                            class: if zoomed() == m["id"].as_u64() { "media-thumb active" } else { "media-thumb" },
                            title: "{m[\"name\"].as_str().unwrap_or(\"image\")}",
                            onclick: {
                                let this = m["id"].as_u64();
                                move |_| zoomed.set(if zoomed() == this { None } else { this })
                            },
                            img {
                                src: "{media_base}/{m[\"id\"]}?token={TOKEN.peek()}",
                                alt: "{m[\"name\"].as_str().unwrap_or(\"image\")}",
                            }
                        }
                    }
                }
            }
            div {
                class: "terminal",
                tabindex: "0",
                autofocus: true,
                onkeydown,
                onresize: move |e: ResizeEvent| {
                    if let Ok(size) = e.get_content_box_size() {
                        let cols = ((size.width - 16.0) / *cell_w.peek()).floor().clamp(20.0, 400.0) as u16;
                        let rows = ((size.height - 16.0) / CELL_H).floor().clamp(5.0, 200.0) as u16;
                        let current = grid.peek();
                        if cols != current.cols || rows != current.rows {
                            tx.send(json!({ "type": "resize", "cols": cols, "rows": rows }).to_string());
                        }
                    }
                },
                onwheel: move |e: WheelEvent| {
                    use dioxus::html::geometry::WheelDelta;
                    // Up (negative) goes back in history: a positive offset change.
                    let lines = match e.delta() {
                        WheelDelta::Pixels(v) => -v.y / CELL_H,
                        WheelDelta::Lines(v) => -v.y,
                        WheelDelta::Pages(v) => -v.y * 20.0,
                    };
                    let total = *wheel_rest.peek() + lines;
                    let whole = total.trunc();
                    wheel_rest.set(total - whole);
                    if whole != 0.0 {
                        tx.send(json!({ "type": "scroll", "delta": whole as i64 }).to_string());
                    }
                },
                div { class: "screen", style: "{screen_style}",
                    // Invisible ruler: 50 characters of the terminal font.
                    span {
                        class: "ruler",
                        "aria-hidden": "true",
                        onmounted: move |e| async move {
                            if let Ok(rect) = e.get_client_rect().await {
                                let width = rect.size.width / 50.0;
                                if width > 2.0 && (width - *cell_w.peek()).abs() > 0.01 {
                                    cell_w.set(width);
                                }
                            }
                        },
                        {"M".repeat(50)}
                    }
                    for (i, line) in g.lines.iter().enumerate() {
                        div { key: "{i}", class: "line",
                            for run in line.iter() {
                                RunSpan { run: run.clone() }
                            }
                        }
                    }
                    if g.cursor_visible && !g.exited {
                        span { class: "cursor", style: "{cursor_style}" }
                    }
                }
            }
            TouchKeys { send: move |msg: String| tx.send(msg) }
        }
    }
}

/// What to type so that a field that held `prev` ends up holding `next`:
/// how many characters to erase, then the text to add. On-screen keyboards
/// rewrite the word being composed, so plain "append" is not enough.
fn typing_delta(prev: &str, next: &str) -> (usize, String) {
    let common = prev.chars().zip(next.chars()).take_while(|(a, b)| a == b).count();
    (prev.chars().count() - common, next.chars().skip(common).collect())
}

/// Input row for touch devices (shown only for coarse pointers): a real text
/// field, because a focusable `div` does not raise the on-screen keyboard,
/// plus the keys such keyboards lack.
#[component]
fn TouchKeys(send: EventHandler<String>) -> Element {
    // What the field held at the last input event. Never rendered: the field
    // is uncontrolled, because on native webviews events cross a process
    // boundary and writing a stale value back would drop characters typed in
    // between.
    let mut prev = use_signal(String::new);
    let key = move |name: &'static str| {
        move |_| send.call(json!({ "type": "input", "keys": [name] }).to_string())
    };
    rsx! {
        div { class: "touch-keys",
            button { r#type: "button", onclick: key("Esc"), "Esc" }
            button { r#type: "button", onclick: key("Tab"), "Tab" }
            button { r#type: "button", onclick: key("C-c"), "Ctrl-C" }
            button { r#type: "button", aria_label: "Up", onclick: key("Up"), "↑" }
            button { r#type: "button", aria_label: "Down", onclick: key("Down"), "↓" }
            input {
                r#type: "text",
                id: "touch-input",
                class: "touch-input",
                placeholder: "Type here",
                aria_label: "Terminal input",
                autocapitalize: "off",
                autocomplete: "off",
                spellcheck: false,
                oninput: move |e| {
                    let next = e.value();
                    let (erase, add) = typing_delta(&prev.peek(), &next);
                    if erase > 0 {
                        send.call(json!({ "type": "input", "keys": vec!["Backspace"; erase] }).to_string());
                    }
                    if !add.is_empty() {
                        send.call(json!({ "type": "input", "text": add }).to_string());
                    }
                    prev.set(next);
                },
                onkeydown: move |e: KeyboardEvent| match e.key() {
                    Key::Enter => {
                        e.prevent_default();
                        send.call(json!({ "type": "input", "keys": ["Enter"] }).to_string());
                        // Empty the field without touching its focus. `prev` is
                        // reset only once the field is really empty, so keys
                        // typed in between are still diffed against the old text.
                        spawn(async move {
                            let _ = net::clear_field("touch-input").await;
                            prev.set(String::new());
                        });
                    }
                    Key::Backspace if prev.peek().is_empty() => {
                        send.call(json!({ "type": "input", "keys": ["Backspace"] }).to_string());
                    }
                    _ => {}
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::typing_delta;

    #[test]
    fn deltas() {
        assert_eq!(typing_delta("", "h"), (0, "h".into()));
        assert_eq!(typing_delta("hel", "hell"), (0, "l".into()));
        assert_eq!(typing_delta("hell", "hel"), (1, String::new()));
        // Autocorrect replaces the word being composed.
        assert_eq!(typing_delta("teh", "the "), (2, "he ".into()));
        assert_eq!(typing_delta("ñu", "ñus"), (0, "s".into()));
        assert_eq!(typing_delta("abc", ""), (3, String::new()));
    }
}

#[component]
fn RunSpan(run: Run) -> Element {
    let css = run.css();
    // Most runs have no dot at all: skip the link scan for them.
    if !run.t.contains('.') {
        return rsx! { span { style: "{css}", "{run.t}" } };
    }
    let pieces = split_links(&run.t);
    if matches!(pieces.as_slice(), [] | [Piece::Text(_)]) {
        return rsx! { span { style: "{css}", "{run.t}" } };
    }
    rsx! { span { style: "{css}", LinkedText { pieces } } }
}

#[component]
fn AgentView(session: String, agent: String) -> Element {
    let shell = use_context::<Shell>();
    let mut lines = use_signal(Vec::<TranscriptLine>::new);
    let path = format!("/api/sessions/{session}/agents/{agent}/transcript");

    use_future(move || {
        let path = path.clone();
        async move {
            loop {
                let server = shell.server.peek().clone();
                if let Some(v) = api_get(&server, &path).await {
                    let list: Vec<TranscriptLine> = serde_json::from_value(v["lines"].clone()).unwrap_or_default();
                    if *lines.peek() != list {
                        lines.set(list);
                    }
                }
                net::sleep_ms(1500).await;
            }
        }
    });

    rsx! {
        section { class: "terminal-panel",
            header { class: "bar",
                span { class: "title", "Subagent {agent}" }
                span { class: "tag", "read-only" }
            }
            div { class: "transcript",
                if lines().is_empty() {
                    p { class: "hint", "No transcript yet." }
                }
                for (i, l) in lines().into_iter().enumerate() {
                    div { key: "{i}", class: "msg {l.role} {l.kind}",
                        span { class: "who", "{l.role} · {l.kind}" }
                        pre { "{l.text}" }
                    }
                }
            }
        }
    }
}

/// The wordmark: the app mark (a ring and a block cursor) stands in for the
/// first two letters, followed by "mighty". Reads as "almighty"; the mark
/// alone can also be read as "ai".
#[component]
pub(crate) fn Wordmark() -> Element {
    rsx! {
        span { class: "brand", role: "img", aria_label: "almighty",
            svg {
                class: "brand-mark",
                // Cropped to the mark itself: no tile, so it sits in the text line.
                view_box: "192 232 616 560",
                "aria-hidden": "true",
                defs {
                    linearGradient { id: "brand-bowl", x1: "0", y1: "0", x2: "1", y2: "1",
                        stop { offset: "0", stop_color: "var(--mark-from)" }
                        stop { offset: "1", stop_color: "var(--mark-to)" }
                    }
                }
                circle { cx: "472", cy: "512", r: "212", fill: "none", stroke: "url(#brand-bowl)", stroke_width: "136" }
                rect { x: "664", y: "232", width: "144", height: "560", rx: "36", fill: "var(--mark-cursor)" }
            }
            span { "aria-hidden": "true", "mighty" }
        }
    }
}
