---
name: install-almighty-server
description: Install almighty on a VPS or home server so its owner reaches it over HTTPS from other devices. Use when the server must be reachable from phones or other machines.
---

# Install almighty on a server

almighty speaks plain HTTP on loopback; a reverse proxy in front of it adds
TLS. Never expose the almighty port itself.

## Steps

1. A dedicated system user without a shell or sudo; data directory owned by
   it, mode 0700. Set `ALMIGHTY_DATA` to that directory.
2. Build the server (`cargo build --release`) and the web client
   (`cd crates/ui && dx build --web --release`); set `ALMIGHTY_WEB_DIR` to
   the web client's output directory (the one with `index.html`) so the
   server serves it at `/`.
3. A systemd service running `almighty serve` as that user with
   `ALMIGHTY_ADDR=127.0.0.1:8092`, `NoNewPrivileges=yes`,
   `ProtectSystem=strict`, `ReadWritePaths=<data dir>`.
4. Reverse proxy with automatic certificates, proxying everything
   (WebSockets included) to `127.0.0.1:8092`. With Caddy:

   ```
   almighty.example.org {
       reverse_proxy 127.0.0.1:8092
   }
   ```

   If a second host name exists only as an alias (a common typo of the real
   one, for instance), make it a permanent redirect that keeps the path, so
   nobody signs in on the alias:

   ```
   alias.example.org {
       redir https://almighty.example.org{uri} permanent
   }
   ```

5. First owner: `almighty setup --code <code from the service log> --name <name>`
   on the server, or `almighty user add <name> --role owner`.
6. Phone alerts (optional): list the ntfy host under `[push] allowed_hosts`
   in `workspace.toml`. Leave `content` at its default unless the ntfy
   server is your own.
7. Backups: the whole data directory, daily, to another machine. Test one
   restore.

## Check before finishing

- From another network: `https://<host>/api/health` answers, and the plain
  port does not (`curl http://<host>:8092` must fail).
- The certificate is valid and renews by itself.
- Sign in from a phone browser; open a terminal; the page's WebSocket stays
  connected for a few minutes.
- The service restarts after a reboot. Terminal sessions do not survive a
  restart: say so.
