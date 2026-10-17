# Notifications

rustnzb can tell you when a job completes, when a job fails, and when
downloads stop because free disk space fell below `min_free_space_bytes`.
There are two independent channels: server-side targets set in the config
file, and opt-in browser alerts set in the web UI.

## Server-side targets

Configure targets under `[notifications]` in `config.toml`. Each target is off
until its URL or topic is set. Changes take effect after a restart.

| Key | Purpose |
|-----|---------|
| `webhook_url` | POSTs one JSON document per event. |
| `apprise_api_url` | An Apprise API notify endpoint, for example `http://apprise:8000/notify`. |
| `apprise_urls` | Apprise URLs (comma-separated) sent with each message. Leave empty if the endpoint carries a key, as in `/notify/<key>`. |
| `ntfy_server` | ntfy server base URL. Defaults to `https://ntfy.sh`. |
| `ntfy_topic` | ntfy topic to publish to. Empty disables ntfy. |
| `ntfy_token` | Optional ntfy access token, sent as a bearer token. |

The URLs and token often carry secrets. Keep them in a private config file.
`GET /api/config` returns them masked as `********`.

### Webhook payload

```json
{
  "event": "complete",
  "title": "Download complete",
  "message": "Ubuntu.iso",
  "job": "Ubuntu.iso",
  "category": "tv",
  "timestamp": "2026-10-17T00:00:00+00:00",
  "source": "rustnzb"
}
```

`event` is one of `complete`, `failed` or `disk_full`. `job` and `category`
are `null` for disk events, and `category` is `null` when the job has none.

### Delivery

Deliveries run on a background task with a 10 second timeout and are not
retried. A failure is logged with the target kind but not its URL. A
post-processing stop for lack of disk space is reported once, as `disk_full`,
not also as `failed`. A download held for low disk space is reported once per
hold, not on every re-check.

## Browser alerts

Open **Settings → General → Browser notifications** and turn the switch on.
The browser asks for permission, and the choice is stored in this browser
only. Alerts appear while the tab is in the background. Outcomes that finished
before the switch was turned on are not announced.
