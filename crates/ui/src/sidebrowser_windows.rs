//! Side browser on Windows desktop.
//!
//! Same contract as the Linux module: each tab is a native webview (WebView2)
//! created as a child of the application window and laid over the
//! bottom-right corner, exactly on top of the empty "slot" element the UI
//! reserves for it.

use std::cell::RefCell;
use std::collections::HashMap;

use dioxus::desktop::window;
use dioxus::desktop::wry::dpi::{LogicalPosition, LogicalSize};
use dioxus::desktop::wry::{Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows};

struct Pane {
    webview: WebView,
    url: String,
}

thread_local! {
    // Webviews belong to the UI thread, where Dioxus desktop runs the app.
    static PANES: RefCell<HashMap<u64, Pane>> = RefCell::new(HashMap::new());
}

/// Bottom-right rectangle of the window's client area, in logical pixels.
fn bounds(width: f64, height: f64) -> Rect {
    let desktop = window();
    let inner = desktop.window.inner_size().to_logical::<f64>(desktop.window.scale_factor());
    let width = width.clamp(1.0, inner.width.max(1.0));
    let height = height.clamp(1.0, inner.height.max(1.0));
    Rect {
        position: LogicalPosition::new(inner.width - width, inner.height - height).into(),
        size: LogicalSize::new(width, height).into(),
    }
}

/// Shows tab `id` at `url` in a pane of `width` x `height` logical pixels
/// anchored to the bottom-right corner of the window, hiding other tabs.
pub fn place(id: u64, url: &str, width: f64, height: f64) {
    let bounds = bounds(width, height);
    PANES.with(|cell| {
        let mut panes = cell.borrow_mut();
        if !panes.contains_key(&id) {
            let Some(pane) = create(url, bounds) else { return };
            panes.insert(id, pane);
        }
        for (other, pane) in panes.iter_mut() {
            if *other == id {
                if pane.url != url {
                    let _ = pane.webview.load_url(url);
                    pane.url = url.to_string();
                }
                let _ = pane.webview.set_bounds(bounds);
                let _ = pane.webview.set_visible(true);
            } else {
                let _ = pane.webview.set_visible(false);
            }
        }
    });
}

/// Destroys the webviews of tabs that no longer exist.
pub fn retain(ids: &[u64]) {
    // Dropping a pane closes its WebView2 controller and child window.
    PANES.with(|cell| cell.borrow_mut().retain(|id, _| ids.contains(id)));
}

pub fn hide_all() {
    PANES.with(|cell| {
        for pane in cell.borrow().values() {
            let _ = pane.webview.set_visible(false);
        }
    });
}

fn create(url: &str, bounds: Rect) -> Option<Pane> {
    let desktop = window();
    let webview = WebViewBuilder::new()
        .with_url(url)
        .with_bounds(bounds)
        // Reuse the browser processes of the application webview; a second
        // WebView2 environment starts a whole extra process group.
        .with_environment(desktop.webview.environment())
        .build_as_child(&desktop.window)
        .ok()?;
    Some(Pane { webview, url: url.to_string() })
}
