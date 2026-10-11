# nzb-decode

yEnc decoding, file assembly, and article cache for NZB download clients.

Part of the `nzb-*` usenet crate stack. Decodes yEnc-encoded NNTP article bodies using SIMD acceleration (via `yenc-simd`), assembles multi-part articles into complete files, and manages an in-memory article cache.

## Part of the nzb-* stack

| Crate | Role |
|-------|------|
| [nzb-nntp](https://crates.io/crates/nzb-nntp) | Async NNTP client, connection pool |
| [nzb-core](https://crates.io/crates/nzb-core) | Shared models, config, SQLite DB |
| **nzb-decode** | yEnc decode + file assembly (this crate) |
| [nzb-news](https://crates.io/crates/nzb-news) | NNTP fetch engine |
| [nzb-dispatch](https://crates.io/crates/nzb-dispatch) | Article dispatcher, retry, hopeless tracking |
| [nzb-postproc](https://crates.io/crates/nzb-postproc) | PAR2 repair, archive extraction |
| [nzb-web](https://crates.io/crates/nzb-web) | Queue manager, download orchestration |

## C ABI (`ffi/`)

The `nzb-decode-ffi` workspace crate builds `libnzbyenc.so`, a shared library that exports RapidYenc-compatible symbols (`rapidyenc_decode_ex`, `rapidyenc_crc`, `rapidyenc_version` and the init functions). Programs written against RapidYenc through FFI, such as NNTmux's PHP decoder, can load it in place of RapidYenc:

```sh
cargo build --release -p nzb-decode-ffi
# target/release/libnzbyenc.so
```

It decodes a bare yEnc payload (the data between `=ybegin`/`=ypart` and `=yend`), skips line endings, and treats a line-leading `=y` as an escaped byte rather than a keyword.

## License

MIT
