# almighty — plugin contract (draft v0)

Two kinds of plugin, one manifest. A plugin is installed per workspace by an
admin, or per user when it only needs that user's permissions.

## Manifest (`almighty-plugin.toml`)

```toml
id = "net.3m4.flows"          # reverse-DNS, unique
name = "flows"
version = "0.1.0"
kind = "service"              # "wasm" | "service"
license = "MIT"

[wasm]                        # kind = "wasm"
component = "plugin.wasm"     # WebAssembly component, WIT world `almighty:plugin`

[service]                     # kind = "service"
base_url = "http://localhost:8087"
well_known = "/.well-known/m4.json"   # 3m4 apps already expose this
mcp = "/mcp"

[permissions]                 # everything is denied unless listed
items = ["read", "write"]     # inbox items
terminals = ["read"]          # "read" | "write" | "create"
network = ["api.github.com"]  # outbound hosts (wasm only; services manage their own)
secrets = ["GITHUB_TOKEN"]    # names the user is asked to provide
events = ["item.created", "terminal.exited"]

[[panels]]
id = "runs"
title = "Runs"
type = "declarative"          # "declarative" | "web"
# type = "web": url = "/embed/runs"  (service plugins; sandboxed frame + postMessage,
#                                      same protocol as flows/CONTRACT.md §7)

[[commands]]
id = "flows.run"
title = "Run workflow…"
```

## Extension points

| Point | What a plugin provides |
|---|---|
| Connector | pulls or receives external events and emits `Item`s |
| Triage rule | adjusts bucket/priority/summary before or after the triage agent |
| Item action | a button on an item ("Approve PR", "Snooze", "Create task") |
| Panel | a view: declarative tree, or a web embed |
| Command | palette entry, bindable to a key |
| Agent tools | MCP tools merged into the workspace MCP endpoint, namespaced by plugin id |
| Agent adapter | a runtime that reports sessions, subagents and normalized events |

## Declarative panels

A panel render returns a tree of nodes; clients map each node to their own
widgets. v0 node set: `stack`, `row`, `list`, `table`, `card`, `text`,
`markdown`, `code`, `diff`, `diagram` (Mermaid source), `media`, `form`,
`input`, `button`, `badge`, `terminal` (session id), `empty`.

Interaction: a node carries an `action` id; the client sends
`{action, payload}` back to the plugin, which returns a patch or a new tree.
No arbitrary script runs in the client.

## Host API (WIT world `almighty:plugin`, sketch)

- `items`: `emit`, `get`, `query`, `update`
- `terminals`: `list`, `read`, `send-text`, `send-keys`, `wait`, `create`
- `http`: `fetch` (allow-listed hosts only)
- `kv`: per-plugin storage
- `llm`: `complete` through the operator's provider, metered per plugin
- `notify`: request a notification (the server still applies user thresholds)
- `log`

## Service plugins and the 3m4 platform

Service plugins authenticate calls from almighty with a workspace-issued
token (JWT, audience = plugin id) verified against almighty's JWKS; 3m4 apps
keep accepting `accounts.3m4.net` sessions when they run standalone. Events
flow both ways as HMAC-signed webhooks (flows/CONTRACT.md §5). Resource URIs
(`3m4://<type>/<id>@v`) are opened by the plugin that registered the type.

## Distribution

`almighty plugin add <path | git url | registry entry>`; a registry is a
plain JSON/TOML index anyone can host. Plugins pin a version and a checksum.
Updating a plugin that asks for new permissions requires approval again.
