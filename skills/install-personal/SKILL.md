---
name: install-almighty-personal
description: Install almighty for one person on their own Linux, macOS or Windows machine, listening on loopback only. Use when someone wants to try almighty or run it just for themselves.
---

# Install almighty for one person

The server listens on `127.0.0.1` only. Nothing is exposed to the network.

## Steps

1. Requirements: a Rust toolchain (`rustup`), and for plugins the target
   `wasm32-wasip2` (`rustup target add wasm32-wasip2`). On Windows use the
   MSVC toolchain with the Visual Studio Build Tools.
2. Build: `cargo build --release` in the repository. The binary is
   `target/release/almighty`.
3. Start it: `almighty serve`. It prints a one-time setup code. If the port
   is taken, set `ALMIGHTY_ADDR=127.0.0.1:<port>` (and `ALMIGHTY_URL` for
   the CLI).
4. Create the owner: `almighty setup --code <code> --name <name>`.
5. Keep it running: a user service (systemd `--user` unit, launchd agent, or
   a Windows scheduled task at log-on) that runs `almighty serve`. Do not
   run it as root or Administrator.
6. Optional client: `cd crates/ui && dx build --desktop --release`.

## Where things live

`ALMIGHTY_DATA` (default: the user data directory, `almighty/`):
`almighty.db`, `secret.key` (encrypts plugin secrets), `signing.key`,
`workspace.toml`, `users/<name>/`, `plugins/`. Back the directory up as a
whole; the database without `secret.key` cannot read stored secrets.

The two key files are created readable by the owner only on Linux and
macOS. On Windows they inherit the permissions of the data directory: keep
it under the user profile (the default), or restrict the folder yourself if
you move it, so other local users cannot read the keys.

## Check before finishing

- `almighty whoami` prints the owner.
- `almighty run -d -- sh -c 'echo ok'` then `almighty ls` shows the session.
- `ss -ltn` (or `netstat -an`) shows the port bound to `127.0.0.1`, not `0.0.0.0`.
- Ask before installing any connector: each one needs credentials the
  person creates under their own account.
