# almighty

**One place for everything that asks for your attention while agents write the code.**
`almighty` is a self-hosted, plugin-based workbench: a triaged inbox (pull
requests, Slack, email, Telegram, calendars), visual review of changes, a
planner, and a multiplexer of agent terminals that both you and your agents
can drive. Desktop, mobile and web clients talk to a server **you** run. MIT.

> Built by Enrique Magallon Alvarez. Named after *Bruce Almighty*: too many
> simultaneous requests for one person. There is **no public server**: every
> person, team or company runs its own. A personal instance lives at
> `almighty.3m4.net`.
> (`aimighty.3m4.net` only redirects there.)

This repo is **empty on purpose**: this README is the brief for the agent that
builds it. The plugin contract is [`PLUGINS.md`](PLUGINS.md); read it first.
The five 3m4 agent-platform apps (flows, crew, sketch, blueprint, trace) ship
as default plugins and keep running standalone on the web; their shared
contract is `flows/CONTRACT.md`.

---

## What almighty owns

- **Server (`almightyd`)**: one Rust binary. Owns terminal sessions (PTYs),
  the event store, connectors, the triage agent, the plugin host, workspaces
  and auth. Nothing leaves the machine or network it runs on except calls to
  the LLM provider the operator configured.
- **Clients**: desktop (Linux, macOS, Windows), mobile (iOS, Android; a light
  layout of the same panels) and web. A client can be signed in to several
  **workspaces** at once (Slack-style): each workspace is a server URL plus
  credentials, with isolated data and notifications.
- **Inbox**: every incoming item (PR review request, mention, DM, email,
  calendar invite, CI failure, agent permission request) is normalized into an
  `Item` and classified by the triage agent.
- **Terminals**: sessions with a tree of agents and subagents, a rich mode
  (inline images, video, diagrams, link previews) and a control interface for
  agents (§ Terminal sessions).
- **Review**: high-level, visual reading of changes (PRs, docs): summary,
  risk map, diagrams from blueprint, sketches from sketch, audio notes; and
  the reverse path, proposing a change from a drawing, a diagram or a voice note.
- **Planner**: todo lists and upcoming events the user must prepare for,
  fed by the inbox ("this needs prep by Thursday") and by hand.
- **Explainer**: any selection anywhere (a diff hunk, a message, a terminal
  region, a diagram node) can be sent to "explain this".
- **Side browser**: links open in an integrated browser panel by default;
  `Alt`+click opens a new app window; `Ctrl`/`Super`+click opens the system
  browser.

What it does **not** own: workflows (flows), agent roles and skills (crew),
canvases (sketch), diagrams (blueprint), traces (trace). Those are plugins.

## Architecture

```
clients (desktop / mobile / web / your own terminal)
        │  WebSocket + HTTP, per-workspace token
        ▼
almightyd ── sessions (PTY + headless terminal emulator, one per session)
          ├─ agent adapters (claude first; ACP for codex/cursor/others)
          ├─ connectors (github, gmail/imap, slack, telegram, calendar, webhook)
          ├─ event store (SQLite by default, Postgres optional)
          ├─ triage agent (classify, summarize, translate, prioritize)
          ├─ plugin host (WASM components + service plugins)
          ├─ control API (MCP at /mcp, same tools over HTTP and the CLI)
          └─ notifier (push relay adapters, desktop notifications)
```

- **Language**: Rust everywhere. Client UI is **Dioxus** (decided
  2026-11-20): one Rust codebase for web (WASM), desktop and mobile; system
  webview today, the native Blitz renderer when it is stable. Keep UI code
  free of webview-only assumptions so that switch stays cheap; JS interop is
  allowed only behind a small, replaceable module.
- **Storage**: SQLite file by default so a personal server is one binary and
  one file; Postgres for teams.
- **Install**: `skills/` holds agent skills (`SKILL.md`) that set up a server
  on a laptop, a VPS or inside a company network, including connector apps
  (GitHub App, Slack app) created under the operator's own accounts.

## Workspaces and auth

- Personal: local accounts with password (argon2id) + optional TOTP.
- Teams/companies: generic **OIDC** client (Okta, Google Workspace, Entra,
  Keycloak, `accounts.3m4.net` once it is an OIDC provider). No dependency on
  any 3m4 service.
- Clients hold one refresh token per workspace in the OS keychain; revoking a
  device is server-side.
- Roles: `owner`, `admin`, `member`, `guest`. Connector credentials belong to
  a user unless shared with the workspace on purpose.

## Items and triage

`Item { id, source, kind, actors, title, body, links, received_at, thread, raw_ref }`

Triage output per item and per user:

| Field | Values |
|---|---|
| `bucket` | `reply` (needs an answer from me) · `act` (needs an action) · `prepare` (event/deadline ahead) · `fyi` (stay informed, nothing to do) · `noise` |
| `priority` | 0–3, from user preferences and learned corrections |
| `summary` | short text in the user's **input language** |
| `draft` | optional reply, in the **output language** (`match` the incoming message, or a fixed language) |
| `due` | optional date, creates a planner entry when `prepare` |

Rules:
- Preferences live in a per-user file (`triage.toml`): people and channels
  that always matter, quiet hours, languages, notification thresholds.
- Every correction by the user (moved bucket, changed priority) is stored and
  fed back as examples.
- The triage agent runs on the server with the operator's LLM provider; raw
  content never goes to a client-side third party.
- Awareness feeds (team, engineering org, company, tech news) are items with
  `bucket = fyi` grouped into digests, never notifications.

## Terminal sessions

A session is a PTY owned by `almightyd` plus a **headless terminal emulator**
that keeps the screen grid, cursor, and input modes. The process survives
client disconnects (tmux-like). Any number of clients attach to one session.

**Use your own terminal.** Three ways, all the same session underneath:
- `almighty attach <session>` from Ghostty, kitty, Windows Terminal, etc.
  (raw byte stream; the app's panel is just another attached client).
- `almighty run -- claude` started in your own terminal registers the session
  with the server, so it shows up in the app and on mobile.
- tmux adapter: existing tmux panes are discovered through control mode and
  exposed as sessions without restarting them.

Rich mode (inline media, link handling, explainer) exists only in the app
clients; a plain terminal gets the plain stream.

**Agent tree.** The agent adapter reports subagents as children of a session.
Each child appears in the side menu under its parent, opens as its own
read-only terminal view, and becomes interactive when the runtime supports
it. Claude Code first (hooks + transcript stream); other runtimes through the
Agent Client Protocol or a generic "CLI + JSONL" adapter shared with flows.

**Agent control interface.** Agents drive interactive terminals through MCP
tools (also HTTP and `almighty term …`):

| Tool | Purpose |
|---|---|
| `terminal_list` | sessions the caller may see, with title, cwd, state |
| `terminal_create` | spawn a command in a new session (cwd, env, size) |
| `terminal_read` | rendered screen text (grid, not raw bytes), cursor, optional scrollback range |
| `terminal_send_text` | type text; uses bracketed paste when the app enabled it |
| `terminal_send_keys` | named keys and chords: `Enter`, `Tab`, `Esc`, `Up`, `C-c`, `M-x`, `F5`, `PageDown`… |
| `terminal_wait` | block until a regex appears, the screen is idle for N ms, or the process exits |
| `terminal_resize` / `terminal_close` | housekeeping |

- Named keys are encoded by the server from the emulator's current modes
  (application cursor keys, keypad mode, kitty keyboard protocol), so `Up`
  is correct in a shell, in vim and in a TUI alike.
- Permissions: an agent controls only sessions it created or that the user
  granted (`read` / `write` scopes per session). Every agent keystroke is
  logged and sent to trace.
- Human priority: when a person types in a session, agent input to it is
  paused and the agent is told; the user can take over or lock a session.

## Plugins

Summary; the contract is [`PLUGINS.md`](PLUGINS.md).

- **Light plugins** (from day one): WASM components run by the server with
  declared permissions. They add connectors, triage rules, item actions,
  commands and panels. Panels are **declarative UI trees** (list, table,
  form, card, markdown, diagram, media, terminal) rendered by each client,
  so one plugin works on desktop, mobile and web.
- **Service plugins**: separate processes in any language that expose a
  manifest, an MCP endpoint, events and optional web panels (`/embed`). The
  five 3m4 apps are service plugins; each still runs alone on the web.
- Install from a path, a git URL or a registry file; no PR to this repo.
- Layout and enabled panels come from config files (`workspace.toml`,
  per-user `layout.toml`), shareable inside a team.

## Notifications

- Server decides what deserves a notification from the triage result and the
  user's thresholds; the payload is the light summary, not the raw content.
- Desktop: native notifications from the connected client.
- Android: UnifiedPush / self-hosted ntfy. iOS: APNs needs the app signer's
  key, so the operator chooses between a self-built app, or a minimal relay
  that only forwards an encrypted wake-up (no content). Documented, not hidden.
- Inbound webhooks (`POST /hooks/<connector>`), HMAC-verified, are the entry
  point for sources that push.

## Rules

- Everything in this repo (code, comments, commits, docs) is in English.
- No telemetry, no phone-home, no default external endpoint.
- Secrets and tokens stored hashed or in the OS keychain; never logged.
- Agent input into terminals is always attributable, logged and revocable.
- Unofficial protocol clients (e.g. personal WhatsApp) are never in core;
  they can exist as community plugins that state their own risk.
- No code, data, names or fixtures from any employer or client.

## Open decisions

1. **Terminal rendering in the client**: xterm.js through JS interop
   (works today inside the webview) vs a Rust renderer drawing the server's
   grid. Decide in the milestone-0 spike; measure speed and memory on Linux.
2. **iOS push**: self-built app only, or optional content-free relay.
3. **Reuse**: which MIT pieces to adopt instead of writing (terminal
   emulation crate, agent-terminal projects, a messaging gateway connector).

## Milestones

0. **Spikes** (done): Dioxus shell on Linux + Android + web with a terminal panel
   and a side browser panel; PTY + headless emulator + attach from an external
   terminal; Claude Code subagent tree from hooks.
1. **Server skeleton** (done, see docs/m1.md): workspaces, local auth, tokens, SQLite, WebSocket
   protocol, `almighty` CLI (`serve`, `attach`, `run`, `term`).
2. **Terminals** (done except the tmux adapter, see docs/m2.md): sessions, multi-client attach, agent tree, control MCP
   tools with key encoding, human-priority lock, desktop client panel.
3. **Plugin host**: WASM runtime, permissions, declarative panels, config
   files; first light plugin (GitHub connector).
4. **Inbox + triage**: items, buckets, preferences, languages, corrections,
   notifications on desktop; Gmail/IMAP and Slack connectors.
5. **Review + explainer + side browser + rich terminal mode**.
6. **Service plugins**: manifest, embed panels, flows as the first one, then
   crew, trace, sketch, blueprint.
7. **Mobile**: light layout, push, multi-workspace switcher.
8. **OIDC**, team roles, install skills for company deployments.
9. **Planner + awareness digests**; ACP adapter for other agent runtimes.

## Quick start

```bash
cargo build
./target/debug/almighty serve                 # http://127.0.0.1:8092, prints a setup code on first run
./target/debug/almighty setup --code <code> --name <you> --workspace personal
./target/debug/almighty run -- claude         # session + your terminal attached (Ctrl-\ detaches)
./target/debug/almighty ls
./target/debug/almighty attach s1
./target/debug/almighty term send-keys s1 C-c # what agents use; also `almighty mcp`
cd crates/ui && dx serve --web --port 8093    # client in the browser; sign in there too
```

Other machines sign in with `almighty login --name <you>` (set `ALMIGHTY_URL`).
Accounts can also be created locally with `almighty user add <name> --role member`.
Data lives in `ALMIGHTY_DATA` (default: the user data directory, `almighty/almighty.db`).

Notes: [`docs/m2.md`](docs/m2.md) (terminal history, copy/paste, session tree),
[`docs/m1.md`](docs/m1.md) (accounts, tokens, permissions),
[`docs/spikes/m0.md`](docs/spikes/m0.md) (spike findings).

## License

[MIT](LICENSE) © 2026 Enrique Magallon Alvarez.
