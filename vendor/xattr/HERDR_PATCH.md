# Vendored xattr 0.2.3

Pulled in only by the desktop app (`--features gui`) through
`gpui` -> `gpui_http_client` -> `zed-async-tar` (default `xattr` feature).

xattr 0.2.3 uses `libc::ENOATTR`, which current libc no longer defines on Linux,
so `--features gui` fails to build for Linux. The only change from the crates.io
source is in `src/sys/mod.rs`: `ENOATTR` is `libc::ENODATA` on Linux/Android and
`libc::ENOATTR` elsewhere.

Remove this directory and its `[patch.crates-io]` entry once gpui no longer
depends on xattr 0.2.
