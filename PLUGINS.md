# almighty — plugin contract (draft v0)

**Status.** Light (WebAssembly) plugins are implemented: manifest,
permissions, storage, secrets, outbound HTTP, items, connectors, declarative
panels and commands. The interface is `wit/plugin.wit`; `plugins/example` is
the smallest plugin; `plugins/github`, `plugins/slack` and `plugins/gmail` are connectors. Service plugins are implemented on almighty's side (see
"Service plugins" below). Still a draft, not built yet: triage rules, item actions, agent tools, agent
adapters, terminal access from plugins, events, `llm`, `notify`, web panels,
and installing from a git URL or a registry. Sections below mark what exists.

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

[connector]                   # optional: the plugin brings items in
poll_seconds = 300            # 0 = only on demand; the server never polls faster than 30 s

[permissions]                 # everything is denied unless listed
items = ["read", "write"]     # write: emit items; read: see items of other plugins
terminals = ["read"]          # "read" | "write" | "create"
network = ["api.github.com"]  # outbound hosts, exact or "*.example.com" (wasm only)
secrets = ["GITHUB_TOKEN"]    # names an admin may provide; nothing else is readable
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

[review]                      # optional: items of these kinds open in the review view
command = "github.review"     # a declared command; gets {external_id, url, thread}
kinds = ["pull_request"]      # and returns {title, body, author, url, base, head, truncated,
                              #   files: [{path, status, additions, deletions, patch}]}
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

## Declarative panels (implemented)

A panel render returns a tree of nodes as JSON; clients map each node to
their own widgets and skip types they do not know. No script from a plugin
runs in the client, and links are limited to `http(s)`.

| Node | Fields |
|---|---|
| `stack`, `row` | `children` |
| `list` | `items` (nodes) |
| `card` | `title`, `children` |
| `table` | `columns` (strings), `rows` (arrays of strings, numbers or nodes) |
| `text` | `text`, `style`: `title` · `muted` · `strong` · `error` |
| `badge` | `text`, `tone`: `accent` · `muted` · `ok` · `warn` |
| `link` | `text`, `url` (opens in the side browser) |
| `button` | `label`, `action`, `payload` |
| `form` | `action`, `submit` (label), `children`; the payload is `{input name: text}` |
| `input` | `name`, `label`, `value`, `placeholder`, `kind`: `text` · `number` · `password` · `email` · `url` · `date` |
| `code`, `markdown` | `text` (Markdown is shown as written for now) |
| `diff` | `text` (lines starting with `+` / `-` are coloured) |
| `diagram` | `source` (shown as text for now) |
| `media` | `url`, `alt`, `kind`: `image` or anything else for a link |
| `terminal` | `session` (button that opens that session) |
| `empty` | `text` |

Interaction: a `button` or `form` carries an `action` id; the client sends
`{action, payload}` to the server, the plugin answers with the new tree.
Guests can look at panels; acting needs the `member` role.

## Items (implemented, minimal)

A connector emits items; the inbox and triage of milestone 4 will read them.
`emit-item` takes JSON: `external_id` and `title` are required; `source`
(defaults to the plugin id), `kind`, `state` (default `open`), `body`, `url`,
`thread`, `actors`, `links`, `received_at` (Unix seconds) are optional.
Emitting the same `external_id` again updates the item.

Hints that triage reads: `direct` (addressed to one person: a direct message,
a review request), `mentions` (names the item calls out; compared with the
`me` list of each person's preferences) and `due` (a deadline or start time,
Unix seconds). A `state` of `closed`, `merged`, `done`, `resolved`, `read`
or `archived` marks the item as done in every inbox.

## Limits of a call (implemented)

Every call (`poll`, `render`, `action`, `command`) runs in a fresh instance
with no files, environment or sockets. It may compute for 10 s, use 128 MB,
make 200 outbound requests (20 s each, 1 MB out, 8 MB in, redirects are
returned and not followed, `https` only except towards the local machine).
Storage holds 2000 keys of up to 256 KB per plugin.

## Configuration files (implemented)

In the server's data directory:

- `workspace.toml`: `[plugins."<id>"]` is handed to that plugin as its
  configuration. Read on every call; no restart needed.
- `users/<name>/layout.toml`: `panels = ["<plugin id>/<panel id>", …]` sets
  the order in that person's sidebar, `hidden = […]` removes entries.

## Host API (WIT world `almighty:plugin`)

Implemented in `wit/plugin.wit`: `log`, `config`, `secret`, `kv-get/set/delete`,
`http-fetch`, `emit-item`, `query-items`. Planned:

- `items`: `get`, `update`
- `terminals`: `list`, `read`, `send-text`, `send-keys`, `wait`, `create`
- `http`: `fetch` (allow-listed hosts only)
- `kv`: per-plugin storage
- `llm`: `complete` through the operator's provider, metered per plugin
- `notify`: request a notification (the server still applies user thresholds)
- `log`

## Service plugins (implemented on almighty's side)

A service plugin is a separate process with its own UI and API; its manifest
has `kind = "service"` and no component. `plugins/3m4/` holds manifests for
flows, crew, sketch, blueprint and trace.

```toml
kind = "service"

[service]
base_url = "http://localhost:8087"   # where the service listens
mcp = "/mcp"                         # shown to the person; not proxied yet
events_url = "/api/v1/events"        # where almighty posts events (default)

[permissions]
items = ["write"]                    # may create inbox items through events
events = ["item.created"]            # events almighty sends it: item.created, item.updated

[[panels]]
id = "runs"
title = "Runs"
type = "web"
url = "/embed/runs"                  # a path on the service

[[resources]]                        # 3m4://workflows/<id>[@version] opens here
type = "workflows"
embed = "/embed/workflows/{id}"
```

- **Pages.** A web panel opens the service's page in the side browser, on
  the service's own origin. Nothing from it runs inside almighty's page.
- **Who is looking.** The URL carries `#almighty_token=<JWT>` (a fragment:
  not sent to servers or written to their logs). The token is signed with
  the workspace's Ed25519 key (`alg: EdDSA`), lives five minutes and has
  `iss` (the server URL), `aud` (the plugin id), `sub` (user name), `role`
  and `workspace`. The public key is at `GET /.well-known/jwks.json`, so the
  service verifies it without calling back. A service must check `aud`.
- **Events, both ways**, signed with a secret shown once when the plugin is
  installed (`almighty plugin rotate-events-secret <id>` replaces it):
  `X-Almighty-Signature: sha256=<hex HMAC-SHA256 of the body>` and
  `X-Almighty-Event-Id` (the same id on every retry). The `X-3m4-*` header
  names of the 3m4 platform contract are accepted on the way in.
  - Service → almighty: `POST /hooks/<plugin id>` with
    `{"type": "...", "uri": "...", "item": {…optional, see Items…}}`.
    Answers `202`; a repeated id is acknowledged and ignored.
  - almighty → service: `POST <base_url><events_url>` with
    `{"type": "item.created", "at", "workspace", "actor", "data": {id,
    source, kind, state, title, url}}` — the light version, never the body.
    Three attempts, then dropped. A service does not get its own items back.
- **Resource links.** `GET /api/resolve?uri=3m4://<type>/<id>[@version]`
  returns the page of the plugin that registered the type.

Not built yet: merging a service's MCP tools into the workspace endpoint,
the postMessage protocol of flows/CONTRACT.md §7 (`ready`, `resize`,
`saved`, `theme`…), fetching `well_known`, opening `3m4://` links from the
client, and a delivery queue that survives restarts. 3m4 apps keep accepting
`accounts.3m4.net` sessions when they run standalone.

## Distribution

Today: `almighty plugin add <directory>` reads the manifest, shows the
permissions and uploads the component after an admin approves them. The
server stores the component's SHA-256 and refuses to load a file that no
longer matches. Every install or update needs the approval to equal what the
manifest asks for, so a new version cannot gain permissions silently.

Planned: `almighty plugin add <git url | registry entry>`; a registry is a
plain JSON/TOML index anyone can host.
