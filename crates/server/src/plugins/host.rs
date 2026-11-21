//! The functions a plugin can call, each checked against its permissions.

use std::io::Read;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;
use wasmtime::component::{HasSelf, Linker, ResourceTable};
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::db::{ItemQuery, NewItem};
use super::Loaded;
use crate::store::Store;

wasmtime::component::bindgen!({ world: "plugin", path: "../../wit" });

use almighty::plugin::host::{Host, HttpRequest, HttpResponse};

const MAX_MEMORY: usize = 128 * 1024 * 1024;
const MAX_REQUEST_BODY: usize = 1024 * 1024;
const MAX_RESPONSE_BODY: u64 = 8 * 1024 * 1024;
const MAX_CALLS: u32 = 200;

pub struct Ctx {
    wasi: WasiCtx,
    table: ResourceTable,
    pub limits: StoreLimits,
    plugin: Arc<Loaded>,
    store: Arc<Store>,
    events: broadcast::Sender<String>,
    http: ureq::Agent,
    config: String,
    /// Outbound requests made during this call.
    requests: u32,
}

impl Ctx {
    pub fn new(plugin: Arc<Loaded>, store: Arc<Store>, events: broadcast::Sender<String>, http: ureq::Agent, config: String) -> Ctx {
        Ctx {
            // No files, no environment, no sockets, no inherited output.
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(MAX_MEMORY).instances(16).build(),
            plugin,
            store,
            events,
            http,
            config,
            requests: 0,
        }
    }

    fn id(&self) -> &str {
        &self.plugin.manifest.id
    }

    fn may(&self, what: &str) -> bool {
        self.plugin.manifest.permissions.items.iter().any(|p| p == what)
    }
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

pub fn add_to_linker(linker: &mut Linker<Ctx>) -> wasmtime::Result<()> {
    wasmtime_wasi::p2::add_to_linker_sync(linker)?;
    Plugin::add_to_linker::<_, HasSelf<_>>(linker, |ctx| ctx)
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

impl Host for Ctx {
    fn log(&mut self, level: String, message: String) {
        let level = match level.as_str() {
            "debug" | "warn" | "error" => level.as_str(),
            _ => "info",
        };
        self.plugin.log(level, &message);
    }

    fn config(&mut self) -> String {
        self.config.clone()
    }

    fn secret(&mut self, name: String) -> Option<String> {
        if !self.plugin.manifest.permissions.secrets.contains(&name) {
            return None;
        }
        self.store.secret_get(self.id(), &name)
    }

    fn kv_get(&mut self, key: String) -> Option<String> {
        self.store.kv_get(self.id(), &key)
    }

    fn kv_set(&mut self, key: String, value: String) -> Result<(), String> {
        self.store.kv_set(self.id(), &key, &value)
    }

    fn kv_delete(&mut self, key: String) {
        self.store.kv_delete(self.id(), &key)
    }

    fn http_fetch(&mut self, request: HttpRequest) -> Result<HttpResponse, String> {
        use ureq::http;
        self.requests += 1;
        if self.requests > MAX_CALLS {
            return Err("too many requests in one call".into());
        }
        let uri: http::Uri = request.url.parse().map_err(|_| "not a URL".to_string())?;
        let host = uri.host().unwrap_or_default().trim_matches(['[', ']']).to_string();
        if !self.plugin.manifest.permissions.allows_host(&host) {
            return Err(format!("host {host:?} is not in the plugin's network permissions"));
        }
        match uri.scheme_str() {
            Some("https") => {}
            // Plain HTTP only reaches this machine, for local services and tests.
            Some("http") if is_loopback(&host) => {}
            _ => return Err("only https URLs are allowed".into()),
        }
        let method = request.method.to_ascii_uppercase();
        if !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"].contains(&method.as_str()) {
            return Err(format!("method {method} is not allowed"));
        }
        let body = request.body.unwrap_or_default();
        if body.len() > MAX_REQUEST_BODY {
            return Err("request body is too large".into());
        }
        let mut builder = http::Request::builder().method(method.as_str()).uri(uri);
        for (name, value) in &request.headers {
            if !name.eq_ignore_ascii_case("host") && !name.eq_ignore_ascii_case("content-length") {
                builder = builder.header(name.as_str(), value.as_str());
            }
        }
        let has_body = !body.is_empty() || matches!(method.as_str(), "POST" | "PUT" | "PATCH");
        let response = if has_body {
            self.http.run(builder.body(body).map_err(|e| e.to_string())?)
        } else {
            self.http.run(builder.body(()).map_err(|e| e.to_string())?)
        }
        .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_RESPONSE_BODY + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_RESPONSE_BODY {
            return Err("response body is too large".into());
        }
        Ok(HttpResponse { status, headers, body: bytes })
    }

    fn emit_item(&mut self, item: String) -> Result<String, String> {
        if !self.may("write") {
            return Err("the plugin has no permission to write items".into());
        }
        let item: NewItem = serde_json::from_str(&item).map_err(|e| format!("item: {e}"))?;
        let (id, created) = self.store.item_upsert(self.id(), &item)?;
        let _ = self.events.send(json!({ "type": "item", "id": id, "created": created, "plugin": self.id() }).to_string());
        Ok(json!({ "id": id, "created": created }).to_string())
    }

    fn query_items(&mut self, query: String) -> Result<String, String> {
        let mut query: ItemQuery = if query.trim().is_empty() {
            ItemQuery::default()
        } else {
            serde_json::from_str(&query).map_err(|e| format!("query: {e}"))?
        };
        if !self.may("read") {
            query.plugin = Some(self.id().to_string());
        }
        serde_json::to_string(&self.store.items(&query)).map_err(|e| e.to_string())
    }
}
