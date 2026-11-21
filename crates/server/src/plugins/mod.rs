//! Plugin host: light plugins are WebAssembly components that the server
//! runs with the permissions an admin approved (`wit/plugin.wit`).
//!
//! A plugin keeps no memory between calls: every call gets a fresh instance,
//! so a crash or a timeout cannot poison later calls. State goes through the
//! host's key-value storage.

pub mod db;
mod host;
pub mod manifest;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use wasmtime::component::{Component, Linker};
use wasmtime::Engine;

use crate::session::now_secs;
use crate::store::Store;
use manifest::{Manifest, Permissions, MANIFEST_FILE};

const COMPONENT_FILE: &str = "plugin.wasm";
pub const MAX_COMPONENT_BYTES: usize = 24 * 1024 * 1024;
const MAX_TREE_BYTES: usize = 1024 * 1024;
const LOG_LINES: usize = 200;
/// Epoch ticks are 100 ms; a call may compute for this many.
const CALL_TICKS: u64 = 100;

#[derive(Debug, Default, Clone)]
pub struct Status {
    pub last_poll: Option<u64>,
    pub last_error: Option<String>,
}

pub struct Loaded {
    pub manifest: Manifest,
    pub checksum: String,
    pub installed_at: u64,
    pub enabled: bool,
    component: Component,
    /// One call at a time per plugin.
    busy: Mutex<()>,
    pub status: Mutex<Status>,
    logs: Mutex<VecDeque<String>>,
}

impl Loaded {
    fn log(&self, level: &str, message: &str) {
        let mut logs = self.logs.lock().unwrap();
        if logs.len() == LOG_LINES {
            logs.pop_front();
        }
        let message: String = message.chars().take(2000).collect();
        logs.push_back(format!("{} {level} {message}", now_secs()));
    }

    pub fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap().iter().cloned().collect()
    }

    pub fn describe(&self, secrets_set: &[String]) -> Value {
        let status = self.status.lock().unwrap().clone();
        json!({
            "id": self.manifest.id,
            "name": self.manifest.name,
            "version": self.manifest.version,
            "license": self.manifest.license,
            "checksum": self.checksum,
            "enabled": self.enabled,
            "installed_at": self.installed_at,
            "permissions": self.manifest.permissions,
            "poll_seconds": self.manifest.poll_seconds(),
            "panels": self.manifest.panels,
            "commands": self.manifest.commands,
            "review_kinds": self.manifest.review.as_ref().map(|r| r.kinds.clone()).unwrap_or_default(),
            "secrets_set": secrets_set,
            "last_poll": status.last_poll,
            "last_error": status.last_error,
        })
    }
}

pub enum Call {
    Poll,
    Render { panel: String },
    Action { panel: String, action: String, payload: Value },
    Command { id: String, args: Value },
}

pub struct Plugins {
    engine: Engine,
    linker: Linker<host::Ctx>,
    data_dir: PathBuf,
    store: Arc<Store>,
    events: broadcast::Sender<String>,
    http: ureq::Agent,
    loaded: RwLock<HashMap<String, Arc<Loaded>>>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

impl Plugins {
    pub fn new(data_dir: &Path, store: Arc<Store>, events: broadcast::Sender<String>) -> anyhow::Result<Arc<Plugins>> {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|e| anyhow::anyhow!("plugin engine: {e:#}"))?;
        let mut linker = Linker::new(&engine);
        host::add_to_linker(&mut linker).map_err(|e| anyhow::anyhow!("plugin host: {e:#}"))?;

        // Advances the clock that bounds how long a call may compute.
        let weak = engine.weak();
        std::thread::Builder::new().name("plugin-epoch".into()).spawn(move || {
            while let Some(engine) = weak.upgrade() {
                engine.increment_epoch();
                drop(engine);
                std::thread::sleep(Duration::from_millis(100));
            }
        })?;

        let http = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .user_agent(concat!("almighty/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();

        let plugins = Arc::new(Plugins {
            engine,
            linker,
            data_dir: data_dir.to_path_buf(),
            store,
            events,
            http,
            loaded: RwLock::new(HashMap::new()),
        });
        plugins.load_installed();
        Ok(plugins)
    }

    fn dir_of(&self, id: &str) -> PathBuf {
        self.data_dir.join("plugins").join(id)
    }

    /// Loads what the database lists. A plugin that fails to load is skipped
    /// with a message; it must not stop the server.
    fn load_installed(&self) {
        for row in self.store.plugin_rows() {
            let loaded = (|| -> Result<Loaded, String> {
                let manifest = Manifest::parse(&row.manifest)?;
                let granted: Permissions = serde_json::from_str(&row.granted).map_err(|e| e.to_string())?;
                if granted != manifest.permissions {
                    return Err("stored approval does not match the manifest".into());
                }
                let bytes = std::fs::read(self.dir_of(&row.id).join(COMPONENT_FILE)).map_err(|e| e.to_string())?;
                if sha256_hex(&bytes) != row.checksum {
                    return Err("component file does not match the installed checksum".into());
                }
                let component = Component::new(&self.engine, &bytes).map_err(|e| format!("{e:#}"))?;
                Ok(self.loaded_from(manifest, row.checksum.clone(), row.installed_at, row.enabled, component))
            })();
            match loaded {
                Ok(l) => {
                    self.loaded.write().unwrap().insert(row.id.clone(), Arc::new(l));
                }
                Err(e) => eprintln!("plugin {} not loaded: {e}", row.id),
            }
        }
    }

    fn loaded_from(&self, manifest: Manifest, checksum: String, installed_at: u64, enabled: bool, component: Component) -> Loaded {
        Loaded {
            manifest,
            checksum,
            installed_at,
            enabled,
            component,
            busy: Mutex::new(()),
            status: Mutex::new(Status::default()),
            logs: Mutex::new(VecDeque::new()),
        }
    }

    /// Installs or updates a plugin. `granted` is what the admin approved; it
    /// has to be exactly what the manifest asks for, so a new version that
    /// wants more cannot slip in under an old approval.
    pub fn install(&self, manifest_text: &str, component: &[u8], granted: &Permissions, by: &str) -> Result<Arc<Loaded>, String> {
        let manifest = Manifest::parse(manifest_text)?;
        if *granted != manifest.permissions {
            return Err("the approved permissions do not match the manifest".into());
        }
        if component.len() > MAX_COMPONENT_BYTES {
            return Err("component is too large".into());
        }
        let compiled =
            Component::new(&self.engine, component).map_err(|e| format!("not a valid WebAssembly component: {e:#}"))?;
        let checksum = sha256_hex(component);
        let dir = self.dir_of(&manifest.id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(COMPONENT_FILE), component).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(MANIFEST_FILE), manifest_text).map_err(|e| e.to_string())?;
        let granted_json = serde_json::to_string(granted).map_err(|e| e.to_string())?;
        self.store
            .plugin_save(&manifest.id, &manifest.version, &checksum, manifest_text, &granted_json, by)
            .map_err(|e| e.to_string())?;
        let enabled = self.get(&manifest.id).map(|p| p.enabled).unwrap_or(true);
        let loaded = Arc::new(self.loaded_from(manifest, checksum, now_secs(), enabled, compiled));
        self.loaded.write().unwrap().insert(loaded.manifest.id.clone(), loaded.clone());
        self.changed();
        Ok(loaded)
    }

    pub fn remove(&self, id: &str) -> bool {
        let known = self.loaded.write().unwrap().remove(id).is_some();
        let stored = self.store.plugin_delete(id);
        if known || stored {
            let _ = std::fs::remove_dir_all(self.dir_of(id));
            self.changed();
        }
        known || stored
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> bool {
        let mut loaded = self.loaded.write().unwrap();
        let Some(old) = loaded.get(id).cloned() else { return false };
        self.store.plugin_set_enabled(id, enabled);
        let mut next = self.loaded_from(old.manifest.clone(), old.checksum.clone(), old.installed_at, enabled, old.component.clone());
        next.status = Mutex::new(old.status.lock().unwrap().clone());
        next.logs = Mutex::new(old.logs.lock().unwrap().clone());
        loaded.insert(id.to_string(), Arc::new(next));
        drop(loaded);
        self.changed();
        true
    }

    pub fn get(&self, id: &str) -> Option<Arc<Loaded>> {
        self.loaded.read().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<Loaded>> {
        let mut all: Vec<_> = self.loaded.read().unwrap().values().cloned().collect();
        all.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        all
    }

    fn changed(&self) {
        let _ = self.events.send(json!({ "type": "plugins" }).to_string());
    }

    /// The plugin's table in `workspace.toml`, as JSON text. Read on every
    /// call so edits apply without a restart.
    fn config_of(&self, id: &str) -> String {
        let table = std::fs::read_to_string(self.data_dir.join("workspace.toml"))
            .ok()
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
            .and_then(|root| root.get("plugins")?.get(id).cloned());
        match table {
            Some(value @ toml::Value::Table(_)) => serde_json::to_string(&value).unwrap_or_else(|_| "{}".into()),
            _ => "{}".into(),
        }
    }

    /// Runs one call to completion. Blocking: use `spawn_blocking` from async code.
    pub fn call(&self, id: &str, call: Call) -> Result<Value, String> {
        let plugin = self.get(id).ok_or_else(|| format!("no plugin {id}"))?;
        if !plugin.enabled {
            return Err(format!("plugin {id} is disabled"));
        }
        let _busy = plugin.busy.lock().unwrap_or_else(|e| e.into_inner());
        let is_poll = matches!(call, Call::Poll);
        let notify = !matches!(call, Call::Render { .. });
        let result = self.run(&plugin, call);
        {
            let mut status = plugin.status.lock().unwrap();
            if is_poll {
                status.last_poll = Some(now_secs());
            }
            match &result {
                Err(e) => {
                    status.last_error = Some(e.clone());
                    plugin.log("error", e);
                }
                Ok(_) if is_poll => status.last_error = None,
                Ok(_) => {}
            }
        }
        if notify {
            let _ = self.events.send(json!({ "type": "plugin", "id": id }).to_string());
        }
        result
    }

    fn run(&self, plugin: &Arc<Loaded>, call: Call) -> Result<Value, String> {
        let ctx = host::Ctx::new(plugin.clone(), self.store.clone(), self.events.clone(), self.http.clone(), self.config_of(&plugin.manifest.id));
        let mut store = wasmtime::Store::new(&self.engine, ctx);
        store.limiter(|ctx| &mut ctx.limits);
        store.set_epoch_deadline(CALL_TICKS);
        let trap = |e: wasmtime::Error| {
            let text = format!("{e:#}");
            if text.contains("interrupt") {
                "the plugin ran out of time".to_string()
            } else {
                format!("the plugin crashed: {}", text.lines().next().unwrap_or(""))
            }
        };
        let instance = host::Plugin::instantiate(&mut store, &plugin.component, &self.linker).map_err(trap)?;
        let text = |v: &Value| v.to_string();
        match call {
            Call::Poll => instance.call_poll(&mut store).map_err(trap)?.map(|()| Value::Null),
            Call::Render { panel } => {
                self.panel_known(plugin, &panel)?;
                instance.call_render(&mut store, &panel).map_err(trap)?.and_then(tree)
            }
            Call::Action { panel, action, payload } => {
                self.panel_known(plugin, &panel)?;
                instance.call_action(&mut store, &panel, &action, &text(&payload)).map_err(trap)?.and_then(tree)
            }
            Call::Command { id, args } => {
                if !plugin.manifest.commands.iter().any(|c| c.id == id) {
                    return Err(format!("no command {id}"));
                }
                let out = instance.call_command(&mut store, &id, &text(&args)).map_err(trap)??;
                Ok(serde_json::from_str(&out).unwrap_or(Value::String(out)))
            }
        }
    }

    fn panel_known(&self, plugin: &Loaded, panel: &str) -> Result<(), String> {
        match plugin.manifest.panels.iter().any(|p| p.id == panel) {
            true => Ok(()),
            false => Err(format!("no panel {panel}")),
        }
    }

    /// Plugins whose connector is due, for the scheduler.
    pub fn due(&self) -> Vec<String> {
        let now = now_secs();
        self.list()
            .into_iter()
            .filter(|p| p.enabled && p.manifest.poll_seconds() > 0)
            .filter(|p| {
                let every = p.manifest.poll_seconds().max(30);
                p.status.lock().unwrap().last_poll.is_none_or(|last| now >= last + every)
            })
            .map(|p| p.manifest.id.clone())
            .collect()
    }
}

/// A panel tree is a JSON object with a `type`; clients ignore what they do
/// not know, so only the envelope is checked here.
fn tree(text: String) -> Result<Value, String> {
    if text.len() > MAX_TREE_BYTES {
        return Err("panel tree is too large".into());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.get("type").and_then(Value::as_str).is_some() => Ok(v),
        Ok(_) => Err("panel tree must be an object with a type".into()),
        Err(e) => Err(format!("panel tree is not JSON: {e}")),
    }
}

/// Polls connectors that are due, one at a time, for as long as the server lives.
pub fn spawn_scheduler(plugins: Arc<Plugins>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            for id in plugins.due() {
                let plugins = plugins.clone();
                let _ = tokio::task::spawn_blocking(move || plugins.call(&id, Call::Poll)).await;
            }
        }
    });
}
