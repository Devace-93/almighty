# Slack connector

Brings recent Slack messages into almighty as items. Direct messages and
messages that mention you are marked as addressed to you, so triage puts
them under "reply".

It reads with a token you create in your own Slack app; almighty has no
shared app. Only install it in a workspace where you are allowed to connect
tools.

## Token

Create a Slack app for your workspace, add **user** token scopes
`channels:history`, `groups:history`, `im:history`, `mpim:history`,
`channels:read`, `groups:read`, `im:read`, `mpim:read`, `users:read`,
install it, and store the user token (`xoxp-…`):

```sh
almighty plugin secret net.ema.slack SLACK_TOKEN
```

## Configure (optional)

```toml
[plugins."net.ema.slack"]
channels = ["C0123456789"]   # only these conversations; default: every one you are in (up to 40)
hours = 24                   # how far back the first sync looks
```

Add your Slack user name to `me = [...]` in your `triage.toml` so mentions
count as addressed to you.
