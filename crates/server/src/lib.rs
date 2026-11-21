//! almighty server: terminal sessions, agent tree, control API.
pub mod api;
pub mod hooks;
pub mod keys;
pub mod llm;
pub mod oidc;
pub mod planner;
pub mod plugins;
pub mod push;
pub mod review;
pub mod service;
pub mod session;
pub mod store;
pub mod triage;

pub use api::{build, serve, App, AppState, Config};

/// The user's home directory on Unix (`HOME`) and Windows (`USERPROFILE`).
pub fn home_dir() -> Option<String> {
    std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok()).filter(|h| !h.is_empty())
}
