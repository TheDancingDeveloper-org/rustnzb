# Moving from SABnzbd

rustnzb serves the same download-client API as SABnzbd, so Sonarr, Radarr,
Lidarr, Readarr, Prowlarr and mobile clients such as nzb360 connect to it as
a **SABnzbd** client. It can also import your SABnzbd settings. This guide
covers both, and lists what is the same and what differs.

## Comparison

| | SABnzbd | rustnzb |
|---|---|---|
| Language and runtime | Python | One static Rust binary |
| *arr download-client API | Native | SABnzbd-compatible at `/api` and `/sabnzbd/api`, checked against SABnzbd 5.0.4 responses |
| NNTP | TLS, multiple servers by priority | TLS (rustls), priority failover, pipelined `ARTICLE` requests per connection, optional SOCKS5 proxy and certificate pinning |
| yEnc decoding | Compiled extension (sabctools) | SIMD decoder with CRC32 checks, streamed into the file assembler |
| PAR2 verify and repair | External `par2` (par2cmdline-turbo) | Built in, pure Rust. PAR2 slices are checked while the job downloads, and recovery volumes are fetched only when damage needs them |
| Extraction | unrar, 7-Zip | unrar and 7-Zip, with direct unpack during download and a nesting limit |
| Overlapping stages | Post-processing queue | Separate limits for downloads, repair and extraction, so one job can download while another repairs |
| Duplicate detection | By NZB name, and by series episode | By name or by NZB content (`duplicate_policy`: allow, warn, skip) |
| RSS | Feeds with filters | Feeds with regex rules, auto-download and age limits |
| Watch folder | Yes | Yes (`watch_dir`) |
| Post-processing scripts | Yes | Yes. Same positional arguments and `SAB_*` environment variables, plus NZBGet-style `NZBPP_*` |
| Notifications | Many built-in services | Webhook, Apprise (which reaches most services) and ntfy, plus opt-in browser alerts |
| Newsgroup browsing | No | Header search and threaded views |
| Streaming | No | WebDAV media library that streams files from Usenet without downloading first |
| Observability | Logs | Logs (text or JSON), OpenTelemetry metrics and logs over OTLP |
| Scheduler | Yes | No. Only a timed pause |
| Interface languages | Many | English only |
| Skins | Several | One UI with three themes (two dark, one light) |

## Before you start

- **Let the SABnzbd queue finish, or note what's in it.** The import moves
  settings, not jobs. Queue and history stay in SABnzbd. Send any unfinished
  NZBs to rustnzb again afterwards.
- **Keep the download paths the same if you can.** If rustnzb writes to the
  directory SABnzbd used, your *arr apps' import paths and remote path
  mappings keep working. In a container, mount the same host directory at the
  same path that SABnzbd's config names.
- **Stop SABnzbd** before you switch the *arr apps over, so two clients don't
  take the same jobs.

## 1. Install rustnzb

Follow [Getting started](../README.md#getting-started). With Docker:

```bash
docker run -d --name rustnzb -p 9090:9090 \
  -v ./config:/config -v ./data:/data \
  -v /path/to/downloads:/downloads \
  -e PUID=1000 -e PGID=1000 -e TZ=Etc/UTC \
  ghcr.io/thedancingdeveloper-org/rustnzbd:latest
```

Open `http://localhost:9090`.

## 2. Import your SABnzbd settings

On first start rustnzb asks you to create a login, then opens the **Welcome**
screen. Later, the "Add a server" banner or `/welcome` opens it again.
Choose **Import from SABnzbd**, then either:

- **Live instance:** enter SABnzbd's URL and its API key (SABnzbd → Config →
  General → API Key). SABnzbd's API masks server passwords, so you type each
  one in on the review screen.
- **Config file (.ini):** upload `sabnzbd.ini`, usually at
  `~/.sabnzbd/sabnzbd.ini` or in SABnzbd's config directory. Server passwords
  come across with the file.

Review the result and apply it. The import brings:

| SABnzbd | rustnzb |
|---------|---------|
| Servers (host, port, SSL, certificate check, username, password, connections, priority, enabled, retention, optional) | `[[servers]]`, which **replace** any servers already configured |
| Categories (name, folder, post-processing level) | `[[categories]]`, which replace the existing ones |
| RSS feeds (URL, filter, category, enabled) | `[[rss_feeds]]` |
| API key | `general.api_key`, so the *arr apps keep their key |
| Completed folder, temporary download folder | `general.complete_dir`, `general.incomplete_dir` (must be absolute paths) |
| Bandwidth limit | `general.speed_limit_bps` |

The review screen lists anything it couldn't import. That includes category
scripts, sections rustnzb doesn't support, and SABnzbd's duplicate settings.
Set rustnzb's own `duplicate_policy` instead.

You can run the same import over the API: `POST /api/setup/import-sabnzbd`
(the INI file as a multipart upload) or `POST /api/setup/import-sabnzbd-api`
(JSON) return a preview, and `POST /api/setup/apply` applies it.

## 3. Point the *arr apps at rustnzb

In each app, edit the existing SABnzbd download client, or add one
(**Settings → Download Clients → + → SABnzbd**):

| Field | Value |
|-------|-------|
| Host | rustnzb's host |
| Port | `9090`, or the port you published |
| URL Base | empty. rustnzb answers at `/api` and also at `/sabnzbd/api` |
| API Key | `general.api_key`. If you imported it, it's SABnzbd's key. Otherwise see Settings → API & SABnzbd compat |
| Category | a rustnzb category name, such as `tv` or `movies` |

Press **Test**, then save. Do the same in Prowlarr if it sends to the
download client.

*arr apps put the API key in the request query string, so it appears in the
URL — and in the access log of any reverse proxy in front of rustnzb. That is
unchanged for compatibility. A client that can set headers instead should
send `X-Api-Key: <key>` and omit `apikey` from the URL:

```bash
curl -H "X-Api-Key: $API_KEY" \
  "http://localhost:9090/sabnzbd/api?mode=queue&output=json"
```

The header is checked exactly like `apikey` and wins when both are present.

## 4. Check the rest of the settings

These have no SABnzbd import, or work differently:

- **Post-processing scripts:** copy your scripts into `scripts_dir` and set
  `script_success` / `script_failure`. They get SABnzbd's arguments and
  environment, so most work unchanged.
- **Notifications:** set a webhook, Apprise or ntfy target under
  `[notifications]`. See [NOTIFICATIONS.md](NOTIFICATIONS.md).
- **Indexers on your LAN:** rustnzb refuses to fetch NZB URLs from private
  addresses by default. If Prowlarr or NZBHydra2 hands out LAN URLs, set
  `fetch_allowed_hosts` or `fetch_allow_private`.
- **Watch folder:** set `watch_dir`.
- **Concurrency:** `max_active_downloads`, `max_post_processing_jobs`,
  `max_repair_workers` and `max_extract_workers`.

Every key is described in [CONFIGURATION.md](CONFIGURATION.md).

## SABnzbd API coverage

The compatibility layer supports these modes: `version`, `auth`, `queue`,
`history`, `fullstatus`, `server_stats`, `warnings`, `get_config`, `config`,
`get_cats`, `get_scripts`, `addfile`, `addurl`, `addlocalfile`, `pause`,
`resume`, `delete`, `retry`, `retry_all`, `switch`, `priority`, `change_cat`,
`rename`, `change_complete_action`, `speedlimit` and `set_pause`.

Not supported yet: `get_files`, `queue&name=delete_nzf`, `change_opts`,
`change_script`, `test_server`, `restart` and `rss_now`. The *arr apps don't
need them for normal operation.

`change_complete_action` can run a `script_<name>` from `scripts_dir` when the
queue empties. Power actions (shutdown, standby, hibernate) are refused.

## Going back

rustnzb doesn't change SABnzbd's files. To switch back, start SABnzbd again
and point the *arr apps at it.
