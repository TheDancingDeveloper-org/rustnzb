# rustnzb interactive demo

The demo is a production build of the current Angular UI in
`apps/rustnzb/frontend`, published from `website/demo/` and mounted at
`/demo/`. `website/demo/mock-api.js` replaces the HTTP API and WebDAV requests
in the browser, so the demo is self-contained and does not contact an NNTP
provider, indexer, or rustnzb server.

The seeded data is fictional. It covers Downloads and History, Usenet header
search, RSS feeds and rules, the WebDAV Media Library, live logs, statistics,
all ten Settings sections, login, and first-run SABnzbd import onboarding.

Serve the `website/` directory and open `http://localhost:4173/demo/`:

```sh
cd website
python3 -m http.server 4173
```

Production hosting needs SPA fallback from `/demo/*` to `/demo/index.html`.
The website Caddy configuration contains an example of that rule for later use.

To regenerate the compiled UI, run this from `apps/rustnzb/frontend` into a
scratch directory, flatten the generated `browser/` directory into
`website/demo/`, restore `mock-api.js` from git, and keep it loaded before the
generated `main-*.js` module in `index.html`:

```sh
npm run build -- --configuration=production --base-href=/demo/ --output-path=../../../website/demo-build
```

## README screenshots

The images in `docs/img/` are taken from a fresh production build of the UI
with `mock-api.js`, served under a path other than `/demo/` so the demo
banner and "Exit demo" button stay hidden. Change `<base href>` to match that
path, and shift the history dates in the scratch copy of `mock-api.js` to
recent ones so the times read naturally. Capture at 1440×900 (Downloads,
Settings), 1440×1700 for the History section (open `/history`), and 390×844
for the phone view. Headless Chrome works:

```sh
google-chrome --headless=new --hide-scrollbars --window-size=1440,900 \
  --virtual-time-budget=8000 --screenshot=downloads.png \
  http://127.0.0.1:4173/ui/downloads
```

