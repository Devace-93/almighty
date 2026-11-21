# almighty — notes for agents

- Brief: `README.md`. Plugin contract: `PLUGINS.md`. Keep both in sync with the code in the same change.
- Local, untracked instructions may exist in `CLAUDE.local.md` (personal rules, machine paths). When that file is present, read it first and follow it; it takes precedence over this one.
- Everything in this repo (code, comments, commits, docs) is written in English.
- Never commit, push or publish unless the repository owner asks.
- Git hooks: the scripts in `tools/hooks/` run the hook of the same name from `.local/hooks/` when one exists (enable with `git config core.hooksPath tools/hooks`). Never bypass a local hook.
- No confidential third-party material: no code, data, screenshots, names, fixtures or examples from any employer or client. Test data is synthetic.
- Sibling projects (default service plugins, each its own repository): flows (holds `CONTRACT.md`, the shared 3m4 agent-platform contract), crew, sketch, blueprint, trace; accounts (identity; future OIDC provider).
- Stack: Rust (server `almightyd`, CLI `almighty`, client core). Client UI is Dioxus (webview renderer now, Blitz later). Check the current Dioxus docs before writing UI code: it is pre-1.0 and APIs change between versions.
- No telemetry, no default external endpoints, no public server.
- Work milestone by milestone (README § Milestones). Plan mode first.
- Prefer adopting MIT/Apache-licensed crates over writing from scratch; never copy GPL/AGPL code into this repo.
