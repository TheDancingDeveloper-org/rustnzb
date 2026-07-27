# rustnzb fuzz targets

Install `cargo-fuzz`, then run a target from this directory:

```bash
cargo fuzz run nzb_xml
```

The scheduled CI workflow runs each target with a bounded time budget. Crash
artifacts are ignored by Git and can be replayed with `cargo fuzz run <target>
<artifact>`.
