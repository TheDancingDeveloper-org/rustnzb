# Known issues

This page records confirmed user-visible limitations. Track implementation and
discussion in the linked GitHub issues.

## Benchmark results

Benchmark artifacts are generated locally and are not published as project
claims by default. See `benchnzb/METHODOLOGY.md` and `benchnzb/issues.md` for
methodology limits that must be addressed or disclosed before publishing any
comparison. The current baseline, validation gate, and accepted/rejected
optimization experiments are recorded in the
[performance status](PERFORMANCE_STATUS.md).

## Archive passwords

- **Command-line exposure with older extractors.** rustnzb writes a job's
  archive password to the extractor's standard input, so it does not appear
  in `ps` or `/proc/<pid>/cmdline`, when the extractor is rarlab UNRAR/RAR 6
  or later or 7-Zip 21 or later (the container image ships both). Other
  extractors — older unrar releases, `unrar-free`, and p7zip 16.02 (the `7z`
  from Debian's `p7zip-full`, which reads passwords from the terminal) — still
  receive it as a `-p<password>` argument, where any local user can read it
  while the extraction runs. So does a password that contains a line break.
  Install a current unrar or 7-Zip, or mount `/proc` with `hidepid=2`, on
  multi-user hosts.
- **Stored in plaintext.** Extraction needs the original password, so it is
  kept unencrypted in the queue database (`queue.password` in the SQLite
  database under the data directory) and in the stored NZB. Keep the data
  directory readable only by the rustnzb user. The SABnzbd-compatible queue
  returns it in each slot's `password`, as SABnzbd does.
