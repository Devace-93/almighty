# GitHub connector

Brings open pull requests into almighty as items and shows them in a panel.

- Without a token it reads the open pull requests of the public repositories
  you list (GitHub allows 60 anonymous requests per hour).
- With a token it also reads private repositories the token can see and the
  pull requests waiting for your review.

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
