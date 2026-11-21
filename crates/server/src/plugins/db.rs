//! Tables behind the plugin host: installed plugins, their private storage
//! and secrets, and the items connectors emit.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::session::now_secs;
use crate::store::Store;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS plugins (
    id TEXT PRIMARY KEY,
    version TEXT NOT NULL,
    checksum TEXT NOT NULL,        -- sha256 of the component
    manifest TEXT NOT NULL,        -- the manifest as installed
    granted TEXT NOT NULL,         -- JSON of the permissions an admin approved
    enabled INTEGER NOT NULL DEFAULT 1,
    installed_at INTEGER NOT NULL,
    installed_by TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS plugin_kv (
    plugin TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (plugin, key)
);
CREATE TABLE IF NOT EXISTS plugin_secrets (
    plugin TEXT NOT NULL,
    name TEXT NOT NULL,
    value TEXT NOT NULL,
    set_at INTEGER NOT NULL,
    PRIMARY KEY (plugin, name)
);
CREATE TABLE IF NOT EXISTS items (
    id TEXT PRIMARY KEY,
    plugin TEXT NOT NULL,
    external_id TEXT NOT NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    url TEXT NOT NULL,
    thread TEXT NOT NULL,
    actors TEXT NOT NULL,          -- JSON array of names
    links TEXT NOT NULL,           -- JSON array of URLs
    received_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (plugin, external_id)
);
CREATE INDEX IF NOT EXISTS items_recent ON items(updated_at DESC);
";

pub const MAX_KV_VALUE: usize = 256 * 1024;
pub const MAX_KV_KEYS: i64 = 2000;

#[derive(Debug, Clone)]
pub struct PluginRow {
    pub id: String,
    pub version: String,
    pub checksum: String,
    pub manifest: String,
    pub granted: String,
    pub enabled: bool,
    pub installed_at: u64,
}

/// What a connector hands over. Only `external_id` and `title` are required.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewItem {
    pub external_id: String,
    pub title: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub thread: String,
    #[serde(default)]
    pub actors: Vec<String>,
    #[serde(default)]
    pub links: Vec<String>,
    /// Unix seconds; defaults to now for a new item.
    #[serde(default)]
    pub received_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: String,
    pub plugin: String,
    pub external_id: String,
    pub source: String,
    pub kind: String,
    pub state: String,
    pub title: String,
    pub body: String,
    pub url: String,
    pub thread: String,
    pub actors: Vec<String>,
    pub links: Vec<String>,
    pub received_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemQuery {
    #[serde(default)]
    pub plugin: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

fn clip(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

impl Store {
    pub fn plugin_save(&self, id: &str, version: &str, checksum: &str, manifest: &str, granted: &str, by: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO plugins(id, version, checksum, manifest, granted, enabled, installed_at, installed_by)
             VALUES(?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET version = ?2, checksum = ?3, manifest = ?4, granted = ?5,
                 installed_at = ?6, installed_by = ?7",
            params![id, version, checksum, manifest, granted, now_secs() as i64, by],
        )?;
        Ok(())
    }

    pub fn plugin_rows(&self) -> Vec<PluginRow> {
        let conn = self.conn.lock().unwrap();
        let Ok(mut stmt) =
            conn.prepare("SELECT id, version, checksum, manifest, granted, enabled, installed_at FROM plugins ORDER BY id")
        else {
            return Vec::new();
        };
        let rows = stmt.query_map([], |r| {
            Ok(PluginRow {
                id: r.get(0)?,
                version: r.get(1)?,
                checksum: r.get(2)?,
                manifest: r.get(3)?,
                granted: r.get(4)?,
                enabled: r.get::<_, i64>(5)? != 0,
                installed_at: r.get::<_, i64>(6)? as u64,
            })
        });
        rows.map(|it| it.flatten().collect()).unwrap_or_default()
    }

    pub fn plugin_set_enabled(&self, id: &str, enabled: bool) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE plugins SET enabled = ?2 WHERE id = ?1", params![id, enabled as i64]).unwrap_or(0) > 0
    }

    /// Removes the plugin with its storage and secrets. Items it emitted stay.
    pub fn plugin_delete(&self, id: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM plugin_kv WHERE plugin = ?1", params![id]);
        let _ = conn.execute("DELETE FROM plugin_secrets WHERE plugin = ?1", params![id]);
        conn.execute("DELETE FROM plugins WHERE id = ?1", params![id]).unwrap_or(0) > 0
    }

    pub fn kv_get(&self, plugin: &str, key: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT value FROM plugin_kv WHERE plugin = ?1 AND key = ?2", params![plugin, key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn kv_set(&self, plugin: &str, key: &str, value: &str) -> Result<(), String> {
        if key.is_empty() || key.len() > 256 {
            return Err("key must have 1 to 256 bytes".into());
        }
        if value.len() > MAX_KV_VALUE {
            return Err(format!("value is larger than {MAX_KV_VALUE} bytes"));
        }
        let conn = self.conn.lock().unwrap();
        let exists: i64 = conn
            .query_row("SELECT COUNT(*) FROM plugin_kv WHERE plugin = ?1 AND key = ?2", params![plugin, key], |r| r.get(0))
            .unwrap_or(0);
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM plugin_kv WHERE plugin = ?1", params![plugin], |r| r.get(0)).unwrap_or(0);
        if exists == 0 && count >= MAX_KV_KEYS {
            return Err(format!("storage is full ({MAX_KV_KEYS} keys)"));
        }
        conn.execute(
            "INSERT INTO plugin_kv(plugin, key, value) VALUES(?1, ?2, ?3)
             ON CONFLICT(plugin, key) DO UPDATE SET value = ?3",
            params![plugin, key, value],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    pub fn kv_delete(&self, plugin: &str, key: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM plugin_kv WHERE plugin = ?1 AND key = ?2", params![plugin, key]);
    }

    pub fn secret_set(&self, plugin: &str, name: &str, value: &str) -> Result<()> {
        let value = seal(&self.secret_key, value)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO plugin_secrets(plugin, name, value, set_at) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(plugin, name) DO UPDATE SET value = ?3, set_at = ?4",
            params![plugin, name, value, now_secs() as i64],
        )?;
        Ok(())
    }

    pub fn secret_delete(&self, plugin: &str, name: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM plugin_secrets WHERE plugin = ?1 AND name = ?2", params![plugin, name]).unwrap_or(0) > 0
    }

    pub fn secret_get(&self, plugin: &str, name: &str) -> Option<String> {
        let stored: String = {
            let conn = self.conn.lock().unwrap();
            conn.query_row("SELECT value FROM plugin_secrets WHERE plugin = ?1 AND name = ?2", params![plugin, name], |r| r.get(0))
                .optional()
                .ok()
                .flatten()?
        };
        open(&self.secret_key, &stored)
    }

    /// Names only; values never leave the server.
    pub fn secret_names(&self, plugin: &str) -> Vec<String> {
        let conn = self.conn.lock().unwrap();
        let Ok(mut stmt) = conn.prepare("SELECT name FROM plugin_secrets WHERE plugin = ?1 ORDER BY name") else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![plugin], |r| r.get(0));
        rows.map(|it| it.flatten().collect()).unwrap_or_default()
    }

    /// Creates the item or updates the one with the same `external_id`.
    /// Returns its id and whether it is new.
    pub fn item_upsert(&self, plugin: &str, item: &NewItem) -> Result<(String, bool), String> {
        if item.external_id.is_empty() || item.external_id.len() > 512 {
            return Err("external_id must have 1 to 512 bytes".into());
        }
        if item.title.trim().is_empty() {
            return Err("title is required".into());
        }
        let now = now_secs() as i64;
        let actors = json!(item.actors.iter().take(50).collect::<Vec<_>>()).to_string();
        let links = json!(item.links.iter().take(50).collect::<Vec<_>>()).to_string();
        let title = clip(item.title.trim(), 500);
        let body = clip(&item.body, 20_000);
        let conn = self.conn.lock().unwrap();
        let existing: Option<String> = conn
            .query_row("SELECT id FROM items WHERE plugin = ?1 AND external_id = ?2", params![plugin, item.external_id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|e| e.to_string())?;
        let source = item.source.clone().unwrap_or_else(|| plugin.to_string());
        let kind = item.kind.as_deref().unwrap_or("item");
        let state = item.state.as_deref().unwrap_or("open");
        match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE items SET source = ?2, kind = ?3, state = ?4, title = ?5, body = ?6, url = ?7, thread = ?8,
                         actors = ?9, links = ?10, updated_at = ?11 WHERE id = ?1",
                    params![id, source, kind, state, title, body, item.url, item.thread, actors, links, now],
                )
                .map_err(|e| e.to_string())?;
                Ok((id, false))
            }
            None => {
                let bytes: [u8; 9] = rand::random();
                let id = format!("it_{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
                conn.execute(
                    "INSERT INTO items(id, plugin, external_id, source, kind, state, title, body, url, thread, actors, links,
                         received_at, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        id,
                        plugin,
                        item.external_id,
                        source,
                        kind,
                        state,
                        title,
                        body,
                        item.url,
                        item.thread,
                        actors,
                        links,
                        item.received_at.map(|t| t as i64).unwrap_or(now),
                        now
                    ],
                )
                .map_err(|e| e.to_string())?;
                Ok((id, true))
            }
        }
    }

    pub fn items(&self, q: &ItemQuery) -> Vec<Item> {
        let conn = self.conn.lock().unwrap();
        let sql = "SELECT id, plugin, external_id, source, kind, state, title, body, url, thread, actors, links,
                          received_at, updated_at FROM items
                   WHERE (?1 IS NULL OR plugin = ?1) AND (?2 IS NULL OR source = ?2)
                     AND (?3 IS NULL OR kind = ?3) AND (?4 IS NULL OR state = ?4)
                   ORDER BY updated_at DESC, id LIMIT ?5";
        let Ok(mut stmt) = conn.prepare(sql) else { return Vec::new() };
        let list = |text: String| serde_json::from_str::<Vec<String>>(&text).unwrap_or_default();
        let rows = stmt.query_map(params![q.plugin, q.source, q.kind, q.state, q.limit.unwrap_or(100).min(500)], |r| {
            Ok(Item {
                id: r.get(0)?,
                plugin: r.get(1)?,
                external_id: r.get(2)?,
                source: r.get(3)?,
                kind: r.get(4)?,
                state: r.get(5)?,
                title: r.get(6)?,
                body: r.get(7)?,
                url: r.get(8)?,
                thread: r.get(9)?,
                actors: list(r.get(10)?),
                links: list(r.get(11)?),
                received_at: r.get::<_, i64>(12)? as u64,
                updated_at: r.get::<_, i64>(13)? as u64,
            })
        });
        rows.map(|it| it.flatten().collect()).unwrap_or_default()
    }
}

const SEALED: &str = "enc1:";

/// XChaCha20-Poly1305 with a random nonce: `enc1:<base64 nonce + ciphertext>`.
fn seal(key: &[u8; 32], plain: &str) -> Result<String> {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, KeyInit};
    let nonce: [u8; 24] = rand::random();
    let cipher = chacha20poly1305::XChaCha20Poly1305::new(key.into());
    let mut out = nonce.to_vec();
    out.extend(cipher.encrypt((&nonce).into(), plain.as_bytes()).map_err(|_| anyhow::anyhow!("cannot encrypt the secret"))?);
    Ok(format!("{SEALED}{}", base64::engine::general_purpose::STANDARD.encode(out)))
}

/// `None` when the value was sealed with another key or is damaged.
fn open(key: &[u8; 32], stored: &str) -> Option<String> {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, KeyInit};
    let bytes = base64::engine::general_purpose::STANDARD.decode(stored.strip_prefix(SEALED)?).ok()?;
    if bytes.len() < 24 {
        return None;
    }
    let (nonce, sealed) = bytes.split_at(24);
    let cipher = chacha20poly1305::XChaCha20Poly1305::new(key.into());
    String::from_utf8(cipher.decrypt(nonce.into(), sealed).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_sealed_at_rest() {
        let store = Store::in_memory().unwrap();
        store.secret_set("a.b", "TOKEN", "s3cret").unwrap();
        assert_eq!(store.secret_get("a.b", "TOKEN").as_deref(), Some("s3cret"));
        let raw: String = store.conn.lock().unwrap().query_row("SELECT value FROM plugin_secrets", [], |r| r.get(0)).unwrap();
        assert!(raw.starts_with(SEALED) && !raw.contains("s3cret"));
        // Another key (a restored database without its key file) reads nothing.
        assert_eq!(open(&[7; 32], &raw), None);
        assert_eq!(open(&store.secret_key, "enc1:AAAA"), None);
    }
}

pub fn items_json(items: &[Item]) -> Value {
    json!(items)
}
