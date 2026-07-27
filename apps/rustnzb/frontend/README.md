# rustnzb Angular frontend

Angular 21 single-page application for the rustnzb web UI. The production
output is written to `dist/frontend/browser` and embedded into the Rust binary
by `apps/rustnzb/src/server.rs`.

## Local development

From this directory:

```bash
npm ci
npm start
```

The development server listens on `http://localhost:4200` and uses
`proxy.conf.json` to reach the Rust API. Start the backend separately from the
repository root when exercising API-backed views.

## Build and unit tests

```bash
npm ci --no-audit --no-fund
npm run build -- --configuration=production
npm test -- --watch=false
```

The production build must create
`dist/frontend/browser/index.html`. Do not commit `node_modules`, `.angular`,
or `dist`; CI creates and removes them as generated task output.

The repository-level equivalents are:

```bash
./ci/run frontend-test
./ci/run frontend-audit
./ci/run e2e
```

Browser E2E tests live in the root `e2e/` project and use Playwright.

## Rust embedding behavior

The `rust-embed` folder is resolved from the app crate's
`CARGO_MANIFEST_DIR`, not the process working directory. Debug and release
builds embed the built SPA so a running server does not depend on a mutable
`dist` directory.

`index.html` is served with revalidation headers; content-hashed Angular assets
receive immutable caching. Unknown frontend routes fall back to `index.html`
for client-side routing.
