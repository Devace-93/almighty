//! What almighty offers a service plugin (a separate process that speaks
//! HTTP): signed identity tokens it can verify offline, and events in both
//! directions signed with a secret the two share.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signer as _, SigningKey};
use hmac::{Hmac, Mac};
use rusqlite::params;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::plugins::Plugins;
use crate::session::now_secs;
use crate::store::Store;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS plugin_events (
    plugin TEXT NOT NULL,
    event_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (plugin, event_id)
);
";

/// Name under which a service plugin's event secret is kept (sealed) with its other secrets.
pub const EVENTS_SECRET: &str = "__events_key";
pub const TOKEN_TTL: u64 = 300;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The workspace's signing key (Ed25519), created on first use in
/// `signing.key`. Services verify tokens with the public half from
/// `/.well-known/jwks.json` and never need to call back.
pub struct Signer {
    path: PathBuf,
    key: OnceLock<SigningKey>,
}

impl Signer {
    pub fn new(data_dir: &Path) -> Signer {
        Signer { path: data_dir.join("signing.key"), key: OnceLock::new() }
    }

    fn key(&self) -> &SigningKey {
        self.key.get_or_init(|| {
            if let Some(seed) = std::fs::read(&self.path).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()) {
                return SigningKey::from_bytes(&seed);
            }
            let seed: [u8; 32] = rand::random();
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            use std::io::Write;
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            // If the file cannot be written the key still works until restart.
            let _ = options.open(&self.path).and_then(|mut f| f.write_all(&seed));
            SigningKey::from_bytes(&seed)
        })
    }

    fn kid(&self) -> String {
        Sha256::digest(self.key().verifying_key().as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    pub fn jwks(&self) -> Value {
        json!({ "keys": [{
            "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig",
            "kid": self.kid(), "x": b64(self.key().verifying_key().as_bytes()),
        }] })
    }

    /// A short-lived token saying who is looking, for one plugin only.
    pub fn token(&self, issuer: &str, plugin: &str, user: &str, role: &str, workspace: &str) -> String {
        let now = now_secs();
        let header = json!({ "alg": "EdDSA", "typ": "JWT", "kid": self.kid() });
        let claims = json!({
            "iss": issuer, "aud": plugin, "sub": user, "role": role, "workspace": workspace,
            "iat": now, "exp": now + TOKEN_TTL,
        });
        let signed = format!("{}.{}", b64(header.to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let signature = self.key().sign(signed.as_bytes());
        format!("{signed}.{}", b64(&signature.to_bytes()))
    }
}

pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length works");
    mac.update(body);
    format!("sha256={}", mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Constant-time check of a `sha256=<hex>` signature.
pub fn verify(secret: &str, body: &[u8], signature: &str) -> bool {
    let Some(hex) = signature.trim().strip_prefix("sha256=") else { return false };
    let Some(bytes) = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect::<Option<Vec<u8>>>() else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length works");
    mac.update(body);
    mac.verify_slice(&bytes).is_ok()
}

impl Store {
    /// The secret a service plugin shares with almighty for signing events.
    /// Created the first time; `true` when it is new and should be shown once.
    pub fn events_secret(&self, plugin: &str) -> (String, bool) {
        if let Some(secret) = self.secret_get(plugin, EVENTS_SECRET) {
            return (secret, false);
        }
        let bytes: [u8; 32] = rand::random();
        let secret = format!("alm_evt_{}", b64(&bytes));
        let _ = self.secret_set(plugin, EVENTS_SECRET, &secret);
        (secret, true)
    }

    /// `false` when this event id was already received from this plugin.
    pub fn event_is_new(&self, plugin: &str, event_id: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        // Ids older than a week are forgotten; senders do not retry for that long.
        let _ = conn.execute("DELETE FROM plugin_events WHERE at < ?1", params![now_secs() as i64 - 7 * 24 * 3600]);
        conn.execute("INSERT OR IGNORE INTO plugin_events(plugin, event_id, at) VALUES(?1, ?2, ?3)", params![plugin, event_id, now_secs() as i64])
            .unwrap_or(0)
            > 0
    }
}

/// Sends almighty's events to the service plugins that asked for them.
/// Best effort: three attempts, then the event is dropped and logged.
pub fn spawn_outbound(plugins: Arc<Plugins>, store: Arc<Store>, events: broadcast::Sender<String>, workspace: impl Fn() -> String + Send + 'static) {
    let mut rx = events.subscribe();
    tokio::spawn(async move {
        loop {
            let text = match rx.recv().await {
                Ok(text) => text,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
            if event["type"] != "item" {
                continue;
            }
            let kind = if event["created"] == true { "item.created" } else { "item.updated" };
            let Some(item) = event["id"].as_str().and_then(|id| store.item(id)) else { continue };
            let body = json!({
                "type": kind,
                "at": now_secs(),
                "workspace": workspace(),
                "actor": format!("plugin:{}", item.plugin),
                // The light version: enough to decide whether to fetch more.
                "data": { "id": item.id, "source": item.source, "kind": item.kind, "state": item.state, "title": item.title, "url": item.url },
            })
            .to_string();
            for plugin in plugins.list() {
                let m = &plugin.manifest;
                let Some(service) = m.service.as_ref().filter(|_| plugin.enabled && m.permissions.events.iter().any(|e| e == kind)) else { continue };
                // A plugin does not get its own items echoed back.
                if item.plugin == m.id {
                    continue;
                }
                let url = format!("{}{}", service.base_url, service.events_url.as_deref().unwrap_or("/api/v1/events"));
                let (secret, _) = store.events_secret(&m.id);
                let (body, id) = (body.clone(), m.id.clone());
                tokio::task::spawn_blocking(move || deliver(&url, &secret, &body, &id));
            }
        }
    });
}

fn deliver(url: &str, secret: &str, body: &str, plugin: &str) {
    let agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .new_agent();
    let event_id: [u8; 12] = rand::random();
    let event_id: String = event_id.iter().map(|b| format!("{b:02x}")).collect();
    for attempt in 0..3u64 {
        let result = agent
            .post(url)
            .header("content-type", "application/json")
            .header("x-almighty-signature", sign(secret, body.as_bytes()))
            // The same id on every attempt, so the receiver can drop repeats.
            .header("x-almighty-event-id", &event_id)
            .send(body);
        match result {
            Ok(response) if response.status().as_u16() < 300 => return,
            Ok(response) if response.status().as_u16() < 500 && response.status().as_u16() != 429 => {
                eprintln!("plugin {plugin} refused an event with {}", response.status());
                return;
            }
            _ => std::thread::sleep(Duration::from_millis(400 * (attempt + 1))),
        }
    }
    eprintln!("plugin {plugin} could not be reached to deliver an event");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let s = sign("secret", b"{\"a\":1}");
        assert!(s.starts_with("sha256=") && s.len() == 7 + 64);
        assert!(verify("secret", b"{\"a\":1}", &s));
        assert!(!verify("secret", b"{\"a\":2}", &s));
        assert!(!verify("other", b"{\"a\":1}", &s));
        assert!(!verify("secret", b"{\"a\":1}", "sha256=zz"));
        assert!(!verify("secret", b"{\"a\":1}", &s[7..]));
    }

    #[test]
    fn tokens_verify_with_the_published_key() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let dir = std::env::temp_dir().join(format!("almighty-signer-{}", rand::random::<u64>()));
        let signer = Signer::new(&dir);
        let token = signer.token("http://127.0.0.1:8092", "net.ema.flows", "ana", "member", "home");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let decode = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
        let claims: Value = serde_json::from_slice(&decode(parts[1])).unwrap();
        assert_eq!((claims["aud"].as_str(), claims["sub"].as_str(), claims["role"].as_str()), (Some("net.ema.flows"), Some("ana"), Some("member")));
        assert_eq!(claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(), TOKEN_TTL);

        // A service checks it with nothing but the JWKS document.
        let jwks = signer.jwks();
        let key = VerifyingKey::from_bytes(&<[u8; 32]>::try_from(decode(jwks["keys"][0]["x"].as_str().unwrap()).as_slice()).unwrap()).unwrap();
        let signature = Signature::from_slice(&decode(parts[2])).unwrap();
        assert!(key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature).is_ok());
        assert!(key.verify(format!("{}.{}x", parts[0], parts[1]).as_bytes(), &signature).is_err());

        // The key survives a restart.
        assert_eq!(Signer::new(&dir).jwks(), jwks);
        let _ = std::fs::remove_dir_all(dir);
    }
}
