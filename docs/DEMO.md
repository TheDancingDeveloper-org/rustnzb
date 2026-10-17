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
