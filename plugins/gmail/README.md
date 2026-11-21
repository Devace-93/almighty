# Gmail connector

Brings recent inbox mail into almighty as items: sender, subject and
Gmail's own snippet, never the full body. Mail that Gmail files under
Promotions, Social, Updates or Forums, or that comes from a mailing list, is
not marked as addressed to you. Mail you already read counts as done.

It uses the Gmail API with an OAuth client you create under your own Google
account (read-only scope `https://www.googleapis.com/auth/gmail.readonly`);
almighty has no shared client.

```sh
almighty plugin secret net.ema.gmail GMAIL_CLIENT_ID
almighty plugin secret net.ema.gmail GMAIL_CLIENT_SECRET
almighty plugin secret net.ema.gmail GMAIL_REFRESH_TOKEN
```

Optional, in `workspace.toml`:

```toml
[plugins."net.ema.gmail"]
query = "in:inbox newer_than:2d"   # any Gmail search
max = 30                           # messages per sync, up to 50
```

Plain IMAP is not available yet: light plugins can only make HTTPS requests.
