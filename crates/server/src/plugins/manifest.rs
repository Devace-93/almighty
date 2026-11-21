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
    pub service: Option<Service>,
    /// Resource types this plugin opens (`3m4://<type>/<id>`).
    #[serde(default)]
    pub resources: Vec<Resource>,
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

/// A separate process that speaks HTTP: its own UI, API and MCP endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Service {
    /// Where the service listens, e.g. `http://localhost:8087`.
    pub base_url: String,
    #[serde(default)]
    pub well_known: Option<String>,
    #[serde(default)]
    pub mcp: Option<String>,
    /// Path that receives almighty's events; default `/api/v1/events`.
    #[serde(default)]
    pub events_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resource {
    #[serde(rename = "type")]
    pub kind: String,
    /// Path of the embeddable view, with `{id}` where the resource id goes.
    pub embed: String,
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
    /// Commands that agents may call as tools. Part of what the admin
    /// approves, so an update cannot open a command to agents silently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_tools: Vec<String>,
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
    /// `type = "web"` only: path of the page on the service.
    #[serde(default)]
    pub url: Option<String>,
}

fn declarative() -> String {
    "declarative".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Command {
    pub id: String,
    pub title: String,
    /// What the command does, for an agent choosing a tool.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// JSON Schema of the arguments; any object when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
}

impl Manifest {
    /// The commands agents may call, in manifest order.
    pub fn agent_tools(&self) -> impl Iterator<Item = &Command> {
        self.commands.iter().filter(|c| self.permissions.agent_tools.contains(&c.id))
    }
}

/// The MCP name of a plugin command: letters, digits, `_` and `-` only, at
/// most 64 characters, and the same for every caller.
pub fn tool_name(plugin: &str, command: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>();
    let mut name = format!("{}__{}", clean(plugin), clean(command));
    name.truncate(64);
    name
}

fn valid_id(id: &str) -> bool {
    (3..=100).contains(&id.len())
        && id.contains('.')
        && id.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        })
}

/// A path on the service itself: no scheme, no host, no way up.
fn valid_path(path: &str) -> bool {
    path.starts_with('/') && !path.starts_with("//") && !path.contains("..") && !path.contains(['\\', ' ', '\n', '#']) && path.len() <= 300
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
            "service" => {
                let service = m.service.as_ref().ok_or("manifest: kind \"service\" needs a [service] table")?;
                let url = &service.base_url;
                if !(url.starts_with("https://") || url.starts_with("http://")) || url.ends_with('/') || url.contains(['?', '#', ' ']) {
                    return Err("manifest: service.base_url must be an http(s) URL without a trailing slash".into());
                }
                let paths = [&service.well_known, &service.mcp, &service.events_url];
                if paths.into_iter().flatten().any(|p| !valid_path(p)) {
                    return Err("manifest: service paths must start with / and stay on the service".into());
                }
                if m.connector.is_some() || m.review.is_some() || !m.commands.is_empty() {
                    return Err("manifest: connectors, commands and review are for wasm plugins".into());
                }
            }
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
        if let Some(bad) = p.secrets.iter().find(|s| !valid_name(s) || s.starts_with("__")) {
            return Err(format!("manifest: {bad:?} is not a valid secret name"));
        }
        let web_ok = |p: &Panel| m.kind == "service" && p.kind == "web" && p.url.as_deref().is_some_and(valid_path);
        let declarative_ok = |p: &Panel| m.kind == "wasm" && p.kind == "declarative" && p.url.is_none();
        if let Some(bad) = m.panels.iter().find(|p| !valid_name(&p.id) || !(web_ok(p) || declarative_ok(p))) {
            return Err(format!(
                "manifest: panel {:?} must have a simple id and be \"declarative\" (wasm) or \"web\" with a url (service)",
                bad.id
            ));
        }
        if let Some(bad) = m.resources.iter().find(|r| m.kind != "service" || !valid_name(&r.kind) || !valid_path(&r.embed) || !r.embed.contains("{id}")) {
            return Err(format!("manifest: resource {:?} needs a service plugin and an embed path with {{id}}", bad.kind));
        }
        if !known(&p.events, &["item.created", "item.updated"]) {
            return Err("manifest: permissions.events accepts \"item.created\" and \"item.updated\"".into());
        }
        if let Some(bad) = m.commands.iter().find(|c| !valid_name(&c.id)) {
            return Err(format!("manifest: command id {:?} is not valid", bad.id));
        }
        for id in &p.agent_tools {
            let Some(command) = m.commands.iter().find(|c| c.id == *id) else {
                return Err(format!("manifest: permissions.agent_tools lists {id:?}, which is not a declared command"));
            };
            if command.description.trim().is_empty() || command.description.len() > 1000 {
                return Err(format!("manifest: command {id:?} needs a description (up to 1000 characters) to be an agent tool"));
            }
            if command.input.as_ref().is_some_and(|schema| schema["type"] != "object") {
                return Err(format!("manifest: the input of command {id:?} must be a JSON Schema of type \"object\""));
            }
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
        assert!(Manifest::parse(&BASE.replace("\"wasm\"", "\"service\"")).is_err(), "a service needs [service]");
        let service = "id = \"net.example.s\"\nname = \"s\"\nversion = \"1\"\nkind = \"service\"\n[service]\nbase_url = \"http://localhost:8087\"\n";
        let m = Manifest::parse(&format!("{service}[[panels]]\nid = \"runs\"\ntitle = \"Runs\"\ntype = \"web\"\nurl = \"/embed/runs\"\n[[resources]]\ntype = \"workflows\"\nembed = \"/embed/workflows/{{id}}\"\n")).unwrap();
        assert_eq!(m.panels[0].url.as_deref(), Some("/embed/runs"));
        assert!(Manifest::parse(&service.replace("8087", "8087/")).is_err());
        assert!(Manifest::parse(&format!("{service}[[panels]]\nid = \"x\"\ntitle = \"X\"\ntype = \"web\"\nurl = \"//evil.example/x\"\n")).is_err());
        assert!(Manifest::parse(&format!("{service}[[panels]]\nid = \"x\"\ntitle = \"X\"\ntype = \"web\"\nurl = \"https://evil.example/x\"\n")).is_err());
        assert!(Manifest::parse(&format!("{service}[[panels]]\nid = \"x\"\ntitle = \"X\"\n")).is_err(), "declarative panels need a wasm plugin");
        assert!(Manifest::parse(&format!("{service}[connector]\npoll_seconds = 60\n")).is_err());
        assert!(Manifest::parse(&format!("{service}[permissions]\nevents = [\"everything\"]\n")).is_err());
        assert!(Manifest::parse(&format!("{BASE}[permissions]\nitems = [\"delete\"]\n")).is_err());
        assert!(Manifest::parse(&format!("{BASE}[permissions]\nnetwork = [\"https://x\"]\n")).is_err());
        assert!(Manifest::parse(&format!("{BASE}[[panels]]\nid = \"p\"\ntitle = \"P\"\ntype = \"web\"\n")).is_err());
    }
}
