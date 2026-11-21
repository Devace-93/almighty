//! Push notifications for phones. The server posts to an endpoint the
//! person registered: a topic on an ntfy server (the ntfy app shows it), or
//! any UnifiedPush endpoint. almighty runs no relay of its own.
//!
//! The operator decides where pushes may go and how much they say, in
//! `workspace.toml`:
//!
//! ```toml
//! [push]
//! allowed_hosts = ["ntfy.example.org"]   # empty or missing: push is off
//! content = "wake"                       # "wake" (no content, default) or "summary"
//! ```

use std::path::Path;
use std::time::Duration;

use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

use crate::session::now_secs;
use crate::store::Store;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS push_endpoints (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    label TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_ok_at INTEGER,
    last_error TEXT,
    UNIQUE (user_id, url)
);
";

const MAX_ENDPOINTS: i64 = 10;

#[derive(Debug, Clone, Serialize)]
pub struct Endpoint {
    pub id: i64,
    pub url: String,
    pub label: String,
    pub created_at: u64,
    pub last_ok_at: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Policy {
    pub allowed_hosts: Vec<String>,
    /// When false, a push only says that something arrived.
    pub summary: bool,
}

impl Policy {
    pub fn load(data_dir: &Path) -> Policy {
        let table = std::fs::read_to_string(data_dir.join("workspace.toml"))
            .ok()
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
            .and_then(|root| root.get("push").and_then(|p| p.as_table()).cloned());
        let Some(t) = table else { return Policy::default() };
        Policy {
            allowed_hosts: t
                .get("allowed_hosts")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|h| h.as_str().map(|h| h.to_ascii_lowercase())).collect())
                .unwrap_or_default(),
            summary: t.get("content").and_then(|v| v.as_str()) == Some("summary"),
        }
    }

    /// An endpoint must be `https://<allowed host>/<path>`. Plain HTTP is
    /// accepted only towards this machine, for a local ntfy and for tests.
    pub fn check(&self, url: &str) -> Result<(), String> {
        if self.allowed_hosts.is_empty() {
            return Err("push is off: the operator has not listed any host under [push] allowed_hosts in workspace.toml".into());
        }
        let (scheme, rest) = url.split_once("://").ok_or("the endpoint must be a URL")?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if authority.contains('@') || url.len() > 500 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("the endpoint is not a plain URL".into());
        }
        let host = authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority).to_ascii_lowercase();
        let local = host == "localhost" || host == "127.0.0.1";
        if !(scheme == "https" || (scheme == "http" && local)) {
            return Err("the endpoint must use https".into());
        }
        // The port is part of what the operator allows: `host` or `host:port`.
        if !self.allowed_hosts.iter().any(|a| *a == host || *a == authority.to_ascii_lowercase()) {
            return Err(format!("{host} is not in the operator's list of push hosts"));
        }
        if rest.trim_end_matches('/') == authority {
            return Err("the endpoint needs a path (for ntfy: the topic)".into());
        }
        Ok(())
    }
}

impl Store {
    pub fn push_add(&self, user: i64, url: &str, label: &str) -> Result<i64, String> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM push_endpoints WHERE user_id = ?1", params![user], |r| r.get(0)).unwrap_or(0);
        if count >= MAX_ENDPOINTS {
            return Err(format!("at most {MAX_ENDPOINTS} endpoints per person"));
        }
        let label: String = label.chars().filter(|c| !c.is_control()).take(80).collect();
        conn.execute(
            "INSERT INTO push_endpoints(user_id, url, label, created_at) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(user_id, url) DO UPDATE SET label = ?3",
            params![user, url, label, now_secs() as i64],
        )
        .map_err(|e| e.to_string())?;
        conn.query_row("SELECT id FROM push_endpoints WHERE user_id = ?1 AND url = ?2", params![user, url], |r| r.get(0)).map_err(|e| e.to_string())
    }

    pub fn push_list(&self, user: i64) -> Vec<Endpoint> {
        let conn = self.conn.lock().unwrap();
        let Ok(mut stmt) =
            conn.prepare("SELECT id, url, label, created_at, last_ok_at, last_error FROM push_endpoints WHERE user_id = ?1 ORDER BY id")
        else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![user], |r| {
            Ok(Endpoint {
                id: r.get(0)?,
                url: r.get(1)?,
                label: r.get(2)?,
                created_at: r.get::<_, i64>(3)? as u64,
                last_ok_at: r.get::<_, Option<i64>>(4)?.map(|t| t as u64),
                last_error: r.get(5)?,
            })
        });
        rows.map(|it| it.flatten().collect()).unwrap_or_default()
    }

    pub fn push_remove(&self, user: i64, id: i64) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM push_endpoints WHERE id = ?1 AND user_id = ?2", params![id, user]).unwrap_or(0) > 0
    }

    fn push_result(&self, id: i64, result: &Result<(), String>) {
        let conn = self.conn.lock().unwrap();
        let _ = match result {
            Ok(()) => conn.execute("UPDATE push_endpoints SET last_ok_at = ?2, last_error = NULL WHERE id = ?1", params![id, now_secs() as i64]),
            Err(e) => conn.execute("UPDATE push_endpoints SET last_error = ?2 WHERE id = ?1", params![id, e]),
        };
    }
}

/// Header values must stay on one line and in ASCII; other text goes in the body.
fn header_safe(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii() && !c.is_ascii_control() { c } else { '?' }).take(120).collect()
}

/// Sends one push to every endpoint of a person. Blocking.
/// Returns how many were delivered.
pub fn send(store: &Store, data_dir: &Path, user: i64, title: &str, body: &str) -> usize {
    let policy = Policy::load(data_dir);
    let (title, body) = if policy.summary { (title, body) } else { ("almighty", "Something needs your attention.") };
    let agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .new_agent();
    let mut delivered = 0;
    for endpoint in store.push_list(user) {
        // The operator's list may have changed since the endpoint was added.
        let result = policy.check(&endpoint.url).and_then(|()| {
            let response = agent
                .post(&endpoint.url)
                // ntfy reads these; a UnifiedPush distributor just passes the body on.
                .header("title", header_safe(title))
                .header("tags", "bell")
                .header("content-type", "text/plain; charset=utf-8")
                .send(format!("{title}\n{body}"))
                .map_err(|e| format!("cannot reach the endpoint: {e}"))?;
            match response.status().as_u16() {
                200..=299 => Ok(()),
                status => Err(format!("the endpoint answered {status}")),
            }
        });
        if result.is_ok() {
            delivered += 1;
        }
        store.push_result(endpoint.id, &result);
    }
    delivered
}

/// Turns a `notify` event of the server into pushes for that person.
pub fn spawn_worker(store: std::sync::Arc<Store>, data_dir: std::path::PathBuf, events: tokio::sync::broadcast::Sender<String>) {
    let mut rx = events.subscribe();
    tokio::spawn(async move {
        loop {
            let text = match rx.recv().await {
                Ok(text) => text,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
            if event["type"] != "notify" {
                continue;
            }
            let Some(name) = event["user"].as_str() else { continue };
            let Some(user) = store.list_users().into_iter().find(|u| u.name.eq_ignore_ascii_case(name)) else { continue };
            let (store, data_dir) = (store.clone(), data_dir.clone());
            let title = event["title"].as_str().unwrap_or("almighty").to_string();
            let body = event["body"].as_str().unwrap_or("").to_string();
            tokio::task::spawn_blocking(move || send(&store, &data_dir, user.id, &title, &body));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_must_be_on_an_allowed_host() {
        let off = Policy::default();
        assert!(off.check("https://ntfy.example.org/topic").unwrap_err().contains("push is off"));
        let p = Policy { allowed_hosts: vec!["ntfy.example.org".into(), "127.0.0.1:2586".into()], summary: false };
        assert!(p.check("https://ntfy.example.org/my-topic").is_ok());
        assert!(p.check("https://NTFY.example.org:443/my-topic").is_ok());
        assert!(p.check("http://127.0.0.1:2586/topic").is_ok());
        for bad in [
            "http://ntfy.example.org/topic",          // not https
            "https://ntfy.example.org",               // no topic
            "https://ntfy.example.org/",              // no topic
            "https://evil.example/topic",             // not listed
            "https://ntfy.example.org.evil.net/t",    // looks alike
            "https://ntfy.example.org@evil.net/t",    // credentials trick
            "https://evil.net/ntfy.example.org",      // host in the path
            "http://127.0.0.1:9999/topic",            // another local port
            "ftp://ntfy.example.org/topic",
            "ntfy.example.org/topic",
            "https://ntfy.example.org/a b",
        ] {
            assert!(p.check(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn headers_stay_plain() {
        assert_eq!(header_safe("Revisión\r\nX-Evil: 1"), "Revisi?n??X-Evil: 1");
    }
}
