# rustnzb interactive demo

This directory is a production build of the current Angular UI in
`apps/rustnzb/frontend`, mounted at `/demo/`. `mock-api.js` replaces the HTTP
API and WebDAV requests in the browser, so the demo is self-contained and does
not contact an NNTP provider, indexer, or rustnzb server.

The seeded data is fictional. It covers Downloads and History, Usenet header
search, RSS feeds and rules, the WebDAV Media Library, live logs, statistics,
all ten Settings sections, login, and first-run SABnzbd import onboarding.

Serve the repository root and open `http://localhost:4173/demo/`:

```sh
python3 -m http.server 4173
```

Production hosting needs SPA fallback from `/demo/*` to `/demo/index.html`.
The website Caddy configuration contains an example of that rule for later use.

To regenerate the compiled UI, run this from `apps/rustnzb/frontend`, flatten
the generated `browser/` directory into `demo/`, restore `mock-api.js`, and
load it before the generated `main-*.js` module in `index.html`:

```sh
npm run build -- --configuration=production --base-href=/demo/ --output-path=../../../demo
```
