# GitHub connector

Brings open pull requests into almighty as items and shows them in a panel.

- Without a token it reads the open pull requests of the public repositories
  you list (GitHub allows 60 anonymous requests per hour).
- With a token it also reads private repositories the token can see and the
  pull requests waiting for your review.

## Agents

Agents in almighty sessions can read a pull request (title, body and the
patch of each file) through the `github.review` command. With a token set,
that covers every repository the token can see, not only the ones listed in
`repos`, so give the token the narrowest access that works. Nothing the
plugin offers to agents writes to GitHub. The admin sees this permission when
approving the plugin.

## Build and install

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
almighty plugin add .
```

## Configure

In `workspace.toml`, inside the server's data directory:

```toml
[plugins."net.ema.github"]
repos = ["owner/name", "owner/other"]
review_requests = true   # needs a token; default true
```

Optional token (fine-grained, read-only access to pull requests is enough):

```sh
almighty plugin secret net.ema.github GITHUB_TOKEN
```

The plugin can only reach `api.github.com`. For GitHub Enterprise, change
`permissions.network` in the manifest and set `api = "https://host/api/v3"`.
