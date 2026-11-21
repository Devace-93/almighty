//! Side browser: tabs, link handling and bare-host detection.

use std::collections::HashMap;

use dioxus::prelude::*;

use crate::model::Piece;
use crate::{api_get, open_external, Shell, HAS_SIDE_BROWSER};

#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub id: u64,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Browser {
    pub tabs: Vec<Tab>,
    pub active: u64,
    pub open: bool,
    next_id: u64,
}

fn same_page(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

impl Browser {
    pub fn visible(&self) -> bool {
        self.open && !self.tabs.is_empty()
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == self.active)
    }

    /// Opens `url`, reusing the tab that already shows it.
    pub fn open_url(&mut self, url: &str) {
        self.open = true;
        if let Some(tab) = self.tabs.iter().find(|t| same_page(&t.url, url)) {
            self.active = tab.id;
            return;
        }
        self.next_id += 1;
        self.tabs.push(Tab { id: self.next_id, url: url.to_string() });
        self.active = self.next_id;
    }

    /// Address bar: the active tab goes somewhere else.
    pub fn navigate(&mut self, url: &str) {
        let active = self.active;
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == active) {
            tab.url = url.to_string();
        }
    }

    pub fn close_tab(&mut self, id: u64) {
        let Some(pos) = self.tabs.iter().position(|t| t.id == id) else { return };
        self.tabs.remove(pos);
        if self.active == id {
            // Prefer the neighbour on the right, then the one on the left.
            self.active = self.tabs.get(pos).or_else(|| self.tabs.last()).map(|t| t.id).unwrap_or(0);
        }
    }
}

fn tab_title(url: &str) -> String {
    let rest = url.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.");
    rest.trim_end_matches('/').to_string()
}

/// Plain click: side panel. Ctrl/Super/Alt+click: outside the app.
fn activate(url: &str, m: Modifiers, mut browser: Signal<Browser>) {
    let outside = m.contains(Modifiers::CONTROL) || m.contains(Modifiers::META) || m.contains(Modifiers::ALT);
    if HAS_SIDE_BROWSER && !outside {
        browser.write().open_url(url);
    } else {
        open_external(url);
    }
}

#[component]
fn Link(url: String, label: String) -> Element {
    let browser = use_context::<Shell>().browser;
    if cfg!(target_arch = "wasm32") {
        // A real anchor, so Ctrl/Super+click and Alt+click keep their browser meaning.
        return rsx! {
            a {
                class: "link",
                href: "{url}",
                target: "_blank",
                rel: "noreferrer",
                onclick: move |e: MouseEvent| {
                    let m = e.modifiers();
                    if !(m.contains(Modifiers::CONTROL) || m.contains(Modifiers::META) || m.contains(Modifiers::ALT)) {
                        e.prevent_default();
                        activate(&url, m, browser);
                    }
                },
                "{label}"
            }
        };
    }
    // Native webviews hand any <a href> navigation to the system browser, so
    // links are handled entirely here.
    rsx! {
        span {
            class: "link",
            role: "link",
            onclick: move |e: MouseEvent| activate(&url, e.modifiers(), browser),
            "{label}"
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Probe {
    Pending,
    Up,
    Down,
}

/// Answers to "does this bare host respond?", shared by every terminal line.
static PROBES: GlobalSignal<HashMap<String, Probe>> = Signal::global(HashMap::new);

/// `google.com` style text: becomes a link once the server confirms the host
/// answers; stays plain text otherwise.
#[component]
fn HostText(text: String, host: String) -> Element {
    let shell = use_context::<Shell>();
    let state = PROBES.read().get(&host).copied();
    let probe_host = host.clone();
    use_hook(move || {
        spawn(async move {
            if PROBES.peek().contains_key(&probe_host) {
                return;
            }
            PROBES.write().insert(probe_host.clone(), Probe::Pending);
            let server = shell.server.peek().clone();
            let up = api_get(&server, &format!("/api/probe?host={probe_host}"))
                .await
                .and_then(|v| v["ok"].as_bool())
                .unwrap_or(false);
            PROBES.write().insert(probe_host, if up { Probe::Up } else { Probe::Down });
        });
    });
    match state {
        Some(Probe::Up) => rsx! { Link { url: "https://{text}", label: text } },
        _ => rsx! { "{text}" },
    }
}

/// Terminal text with its links made clickable.
#[component]
pub fn LinkedText(pieces: Vec<Piece>) -> Element {
    rsx! {
        for (i, piece) in pieces.into_iter().enumerate() {
            match piece {
                Piece::Text(t) => rsx! { "{t}" },
                Piece::Url(url) => rsx! { Link { key: "{i}", label: url.clone(), url } },
                Piece::Host { text, host } => rsx! { HostText { key: "{i}-{host}", text, host } },
            }
        }
    }
}

#[component]
pub(crate) fn Icon(name: &'static str) -> Element {
    let paths: &[&str] = match name {
        "external" => &["M15 3h6v6", "M10 14 21 3", "M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"],
        "close" => &["M18 6 6 18", "M6 6l12 12"],
        _ => &[],
    };
    rsx! {
        svg {
            class: "icon",
            view_box: "0 0 24 24",
            width: "16",
            height: "16",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            for d in paths.iter() {
                path { d: "{d}" }
            }
        }
    }
}

#[component]
pub fn BrowserPanel() -> Element {
    let mut browser = use_context::<Shell>().browser;
    let state = browser();
    let Some(active) = state.active_tab().cloned() else { return rsx! {} };
    let active_url = active.url.clone();

    rsx! {
        aside { class: "browser",
            div { class: "tabs", role: "tablist",
                for tab in state.tabs.iter().cloned() {
                    div {
                        key: "{tab.id}",
                        class: if tab.id == state.active { "tab active" } else { "tab" },
                        button {
                            class: "tab-label",
                            role: "tab",
                            aria_selected: tab.id == state.active,
                            title: "{tab.url}",
                            onclick: move |_| browser.write().active = tab.id,
                            {tab_title(&tab.url)}
                        }
                        button {
                            class: "icon-button small",
                            aria_label: "Close tab",
                            title: "Close tab",
                            onclick: move |_| browser.write().close_tab(tab.id),
                            Icon { name: "close" }
                        }
                    }
                }
            }
            // A keyed list of one: a new key gives a fresh, correctly filled field.
            for address in [active.clone()] {
                AddressBar { key: "{address.id}|{address.url}", url: address.url }
            }
            if cfg!(target_arch = "wasm32") {
                // One frame per tab, so switching tabs keeps each page's state.
                for tab in state.tabs.iter().cloned() {
                    iframe {
                        key: "{tab.id}",
                        src: "{tab.url}",
                        title: "{tab.url}",
                        hidden: tab.id != state.active,
                    }
                }
            } else {
                PageSlot { id: active.id, url: active_url, ids: state.tabs.iter().map(|t| t.id).collect::<Vec<_>>() }
            }
        }
    }
}

#[component]
fn AddressBar(url: String) -> Element {
    let mut browser = use_context::<Shell>().browser;
    // Uncontrolled field (see ConnectScreen); the parent re-creates this
    // component when the active tab or its address changes.
    let mut draft = use_signal(|| url.clone());
    let initial = url.clone();
    rsx! {
        form {
            class: "bar",
            onsubmit: move |e| {
                e.prevent_default();
                let mut next = draft.peek().trim().to_string();
                if next.is_empty() {
                    return;
                }
                if !next.starts_with("http://") && !next.starts_with("https://") {
                    next = format!("https://{next}");
                }
                browser.write().navigate(&next);
            },
            input {
                r#type: "text",
                initial_value: "{initial}",
                oninput: move |e| draft.set(e.value()),
                aria_label: "Address",
            }
            if cfg!(target_arch = "wasm32") {
                a {
                    class: "icon-button",
                    href: "{url}",
                    target: "_blank",
                    rel: "noreferrer",
                    aria_label: "Open in the system browser",
                    title: "Open in the system browser",
                    Icon { name: "external" }
                }
            } else {
                button {
                    r#type: "button",
                    class: "icon-button",
                    aria_label: "Open in the system browser",
                    title: "Open in the system browser",
                    onclick: move |_| open_external(&url),
                    Icon { name: "external" }
                }
            }
            button {
                r#type: "button",
                class: "icon-button",
                aria_label: "Close side browser",
                title: "Close side browser",
                onclick: move |_| browser.write().open = false,
                Icon { name: "close" }
            }
        }
    }
}

/// Where the page is drawn on native builds: a native webview laid over this
/// empty element (Linux and Windows desktop), or a notice where that does not
/// exist yet.
#[component]
fn PageSlot(id: u64, url: String, ids: Vec<u64>) -> Element {
    #[cfg(all(feature = "desktop", any(target_os = "linux", target_os = "windows")))]
    {
        use crate::sidebrowser;
        let mut size = use_signal(|| (0.0f64, 0.0f64));
        use_effect(use_reactive!(|id, url, ids| {
            let (w, h) = size();
            sidebrowser::retain(&ids);
            if w > 0.0 && h > 0.0 {
                sidebrowser::place(id, &url, w, h);
            }
        }));
        use_drop(sidebrowser::hide_all);
        return rsx! {
            div {
                class: "page-slot",
                onresize: move |e: ResizeEvent| {
                    if let Ok(s) = e.get_content_box_size() {
                        size.set((s.width, s.height));
                    }
                },
            }
        };
    }
    #[allow(unreachable_code)]
    {
        let _ = (id, ids);
        rsx! { p { class: "hint", "The side browser is not available on this platform yet: {url}" } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_are_reused_and_closed() {
        let mut b = Browser::default();
        b.open_url("https://a.com");
        b.open_url("https://b.com/");
        b.open_url("https://a.com/"); // same page as the first tab
        assert_eq!(b.tabs.len(), 2);
        assert_eq!(b.active_tab().unwrap().url, "https://a.com");
        b.open_url("https://c.com");
        b.close_tab(b.active); // closing the last tab activates its left neighbour
        assert_eq!(b.active_tab().unwrap().url, "https://b.com/");
        let first = b.tabs[0].id;
        b.active = first;
        b.close_tab(first); // closing the first activates the one that takes its place
        assert_eq!(b.active_tab().unwrap().url, "https://b.com/");
        b.close_tab(b.active);
        assert!(!b.visible());
        b.open_url("https://d.com");
        assert!(b.visible());
    }
}
