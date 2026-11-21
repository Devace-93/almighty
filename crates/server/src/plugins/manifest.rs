//! `almighty-plugin.toml`: what a plugin is and what it asks for.

use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE: &str = "almighty-plugin.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub wasm: Option<Wasm>,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub connector: Option<Connector>,
    /// The command that returns the files of a change, for the review view.
    #[serde(default)]
    pub review: Option<Review>,
    #[serde(default)]
    pub panels: Vec<Panel>,
    #[serde(default)]
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Wasm {
    pub component: String,
}

/// Everything is denied unless listed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Permissions {
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub terminals: Vec<String>,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub events: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Connector {
    /// 0: only when someone asks.
    #[serde(default)]
    pub poll_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Review {
    pub command: String,
    /// Item kinds it can open, e.g. ["pull_request"].
    #[serde(default)]
    pub kinds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Panel {
    pub id: String,
    pub title: String,
    #[serde(rename = "type", default = "declarative")]
    pub kind: String,
}

fn declarative() -> String {
    "declarative".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Command {
    pub id: String,
    pub title: String,
}

fn valid_id(id: &str) -> bool {
    (3..=100).contains(&id.len())
        && id.contains('.')
        && id.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        })
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A network entry is a host name, optionally `*.` for every subdomain.
fn valid_host_pattern(host: &str) -> bool {
    let bare = host.strip_prefix("*.").unwrap_or(host);
    !bare.is_empty()
        && bare.len() <= 253
        && bare.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && !bare.starts_with('.')
        && !bare.ends_with('.')
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let m: Manifest = toml::from_str(text).map_err(|e| format!("manifest: {e}"))?;
        if !valid_id(&m.id) {
            return Err("manifest: id must be reverse-DNS in lower case, e.g. net.example.thing".into());
        }
        match m.kind.as_str() {
            "wasm" if m.wasm.is_some() => {}
            "wasm" => return Err("manifest: kind \"wasm\" needs a [wasm] table".into()),
            "service" => return Err("service plugins are not supported yet".into()),
            other => return Err(format!("manifest: unknown kind {other:?}")),
        }
        let p = &m.permissions;
        let known = |list: &[String], allowed: &[&str]| list.iter().all(|v| allowed.contains(&v.as_str()));
        if !known(&p.items, &["read", "write"]) {
            return Err("manifest: permissions.items accepts \"read\" and \"write\"".into());
        }
        if !p.terminals.is_empty() {
            return Err("manifest: terminal access from plugins is not available yet".into());
        }
        if let Some(bad) = p.network.iter().find(|h| !valid_host_pattern(h)) {
            return Err(format!("manifest: {bad:?} is not a host name"));
        }
        if let Some(bad) = p.secrets.iter().find(|s| !valid_name(s)) {
            return Err(format!("manifest: {bad:?} is not a valid secret name"));
        }
        if let Some(bad) = m.panels.iter().find(|p| !valid_name(&p.id) || p.kind != "declarative") {
            return Err(format!("manifest: panel {:?} must have a simple id and type \"declarative\"", bad.id));
        }
        if let Some(bad) = m.commands.iter().find(|c| !valid_name(&c.id)) {
            return Err(format!("manifest: command id {:?} is not valid", bad.id));
        }
        if let Some(review) = &m.review {
            if !m.commands.iter().any(|c| c.id == review.command) {
                return Err("manifest: [review] command must be one of the declared commands".into());
            }
        }
        Ok(m)
    }

    pub fn poll_seconds(&self) -> u64 {
        self.connector.as_ref().map(|c| c.poll_seconds).unwrap_or(0)
    }
}

impl Permissions {
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.network.iter().any(|pattern| {
            let pattern = pattern.to_ascii_lowercase();
            match pattern.strip_prefix("*.") {
                Some(domain) => host.ends_with(&format!(".{domain}")),
                None => host == pattern,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "id = \"net.example.a\"\nname = \"a\"\nversion = \"1\"\nkind = \"wasm\"\n[wasm]\ncomponent = \"a.wasm\"\n";

    #[test]
    fn parses_and_rejects() {
        let m = Manifest::parse(&format!("{BASE}[permissions]\nnetwork = [\"api.example.com\", \"*.example.org\"]\n")).unwrap();
        assert!(m.permissions.allows_host("API.example.com"));
        assert!(m.permissions.allows_host("a.b.example.org"));
        assert!(!m.permissions.allows_host("example.org"));
        assert!(!m.permissions.allows_host("evil-example.org"));
        assert!(!m.permissions.allows_host("api.example.com.evil.net"));

        assert!(Manifest::parse(&BASE.replace("net.example.a", "Bad Id")).is_err());
        assert!(Manifest::parse(&BASE.replace("\"wasm\"", "\"service\"")).is_err());
        assert!(Manifest::parse(&format!("{BASE}[permissions]\nitems = [\"delete\"]\n")).is_err());
        assert!(Manifest::parse(&format!("{BASE}[permissions]\nnetwork = [\"https://x\"]\n")).is_err());
        assert!(Manifest::parse(&format!("{BASE}[[panels]]\nid = \"p\"\ntitle = \"P\"\ntype = \"web\"\n")).is_err());
    }
}
