---
name: install-almighty-company
description: Install almighty inside a company network with sign-in through the company's identity provider (OIDC: Okta, Entra, Google Workspace, Keycloak). Use for a team or company deployment.
---

# Install almighty for a company

Everything in `install-server` applies. This adds sign-in through the
company's identity provider and the rules a shared server needs. Get the
approval of whoever owns security for the network before starting, and of
the owner of each system a connector will read.

## Identity provider

1. Register an application (a "web" OIDC client, authorization code flow
   with PKCE). Redirect URI: `https://<host>/api/auth/oidc/callback`.
   Scopes: `openid email profile`.
2. Put the client secret in the service's environment (for example a
   systemd `EnvironmentFile` readable only by the service user), never in
   `workspace.toml`.
3. In `workspace.toml`:

   ```toml
   [oidc]
   issuer = "https://id.example.com"
   client_id = "<client id>"
   client_secret_env = "ALMIGHTY_OIDC_SECRET"
   public_url = "https://almighty.example.com"
   redirect_urls = ["https://almighty.example.com/"]
   allowed_domains = ["example.com"]
   admins = ["first.admin@example.com"]
   default_role = "member"
   ```

4. The first person to sign in must be on the `admins` list; they become
   the owner. Everyone else arrives with `default_role`.
5. Roles are managed in almighty (`almighty user role <name> <role>`):
   `guest` reads, `member` works, `admin` installs plugins and manages
   members, `owner` manages admins. Removing a person in the identity
   provider stops new sign-ins; also run `almighty user remove <name>` to
   end their existing sessions.

## Shared-server rules

- Plugins: only admins install them, and each install shows the hosts and
  secrets it asks for. Review the list; a connector's credentials are
  shared by the whole workspace today.
- Model provider: `[llm]` sends item content, diffs and selections to the
  provider you configure. Use one the company has approved, or leave it out:
  triage then runs on rules only and nothing leaves the server.
- Agents in terminals get a token limited to their own session; the audit
  log (`GET /api/audit`, admins) records what agents typed.
- Push: prefer a company-run ntfy; keep `content = "wake"` otherwise.

## Check before finishing

- A person outside `allowed_domains` is refused with a clear message.
- A member cannot install a plugin or change roles.
- `https://<host>/.well-known/jwks.json` answers; the almighty port is not
  reachable from the network.
- Backups include `secret.key` and `signing.key`, stored with the same care
  as the database.
