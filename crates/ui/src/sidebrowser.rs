//! Side browser on native desktop.
//!
//! Dioxus desktop refuses every http(s) navigation inside its webview,
//! iframes included, and hands the URL to the system browser. A real page
//! therefore needs its own webview. On Linux each tab is a WebKitGTK view
//! laid over the bottom-right corner of the window, exactly on top of the
//! empty "slot" element the UI reserves for it.

use std::cell::RefCell;
use std::collections::HashMap;

use dioxus::desktop::tao::platform::unix::WindowExtUnix;
use dioxus::desktop::wry::{WebView, WebViewBuilder, WebViewBuilderExtUnix};
use gtk::prelude::*;

struct Pane {
    holder: gtk::Box,
    webview: WebView,
    url: String,
}

thread_local! {
    // GTK objects live on the main thread, where Dioxus desktop runs the UI.
    static PANES: RefCell<HashMap<u64, Pane>> = RefCell::new(HashMap::new());
}

/// Shows tab `id` at `url` in a pane of `width` x `height` logical pixels
/// anchored to the bottom-right corner of the window, hiding other tabs.
pub fn place(id: u64, url: &str, width: f64, height: f64) {
    let (width, height) = (width.round().max(1.0) as i32, height.round().max(1.0) as i32);
    PANES.with(|cell| {
        let mut panes = cell.borrow_mut();
        if !panes.contains_key(&id) {
            let Some(pane) = create(url) else { return };
            panes.insert(id, pane);
        }
        for (other, pane) in panes.iter_mut() {
            if *other == id {
                pane.holder.set_size_request(width, height);
                if pane.url != url {
                    let _ = pane.webview.load_url(url);
                    pane.url = url.to_string();
                }
                pane.holder.show_all();
            } else {
                pane.holder.hide();
            }
        }
    });
}

/// Destroys the webviews of tabs that no longer exist.
pub fn retain(ids: &[u64]) {
    PANES.with(|cell| {
        cell.borrow_mut().retain(|id, pane| {
            let keep = ids.contains(id);
            if !keep {
                if let Some(parent) = pane.holder.parent().and_then(|p| p.downcast::<gtk::Container>().ok()) {
                    parent.remove(&pane.holder);
                }
            }
            keep
        });
    });
}

pub fn hide_all() {
    PANES.with(|cell| {
        for pane in cell.borrow().values() {
            pane.holder.hide();
        }
    });
}

fn overlay() -> Option<gtk::Overlay> {
    let desktop = dioxus::desktop::window();
    let vbox = desktop.window.default_vbox()?;
    // The application webview is the last child of the window's box. Wrap it
    // once in an overlay so browser panes can float above it.
    let main = vbox.children().into_iter().last()?;
    if let Ok(existing) = main.clone().downcast::<gtk::Overlay>() {
        return Some(existing);
    }
    vbox.remove(&main);
    let overlay = gtk::Overlay::new();
    overlay.add(&main);
    vbox.pack_start(&overlay, true, true, 0);
    overlay.show_all();
    Some(overlay)
}

fn create(url: &str) -> Option<Pane> {
    let overlay = overlay()?;
    let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    holder.set_halign(gtk::Align::End);
    holder.set_valign(gtk::Align::End);
    overlay.add_overlay(&holder);
    let webview = WebViewBuilder::new().with_url(url).build_gtk(&holder).ok()?;
    Some(Pane { holder, webview, url: url.to_string() })
}
