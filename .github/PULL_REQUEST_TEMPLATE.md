## Summary

<!-- What changes, and why. Keep the pull request focused on one change. -->

## User-visible behaviour

<!-- What users, operators or API clients will notice. Write "None" for internal-only changes. -->

## Compatibility

<!-- Native /api/... changes, SABnzbd-compatible responses (crates/nzb-web/src/sabnzbd_compat.rs),
config keys (add new settings to apps/rustnzb/config.example.toml), or on-disk state. Write "None" if there are none. -->

## Tests

<!-- Which tests cover the change, and the output of the checks you ran. Do not present unrun checks as passing. -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace` (or the focused crates you touched, with the reason)
- [ ] Frontend `npm test -- --watch=false` and production build, if `apps/rustnzb/frontend` changed
- [ ] UI changes checked at 1440px and 390px against DESIGN.md

## Checklist

- [ ] No credentials, NZB URLs, private hostnames or personal paths in the diff, fixtures or logs.
- [ ] Behaviour changes have tests.
- [ ] Docs updated where behaviour or setup changed.
