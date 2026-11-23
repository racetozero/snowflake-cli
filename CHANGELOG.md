# Changelog

## 0.0.1 - 2026-11-23

- Rust port of Snowflake CLI 3.35.0.dev0 (upstream `3ba03140b05959ebf8d20e3e756c696907e8f110`): every command, help text and output format, checked against the Python CLI by the differential parity suite.
- `--agent` mode (one JSON line per command) and the `snow` agent skill (`snow skill`).
- `snow update` self-update and the release-build update notice.
- Profile-guided optimized release builds.
- Faster startup: no CoreFoundation on macOS (chrono's local-time fallback reads `/etc/localtime`), and identifier checks no longer compile regexes on every call (`snow app bundle` uses about 45% less CPU).
- Telemetry events of one command go in one request, as the Python connector batches them.
- Fixed: ANSI sanitizing of `ESC ]`, `ESC ^` and `ESC -` now matches upstream; `connection add` writes fields in the upstream order; `!rehash` clears the completion cache; `--agent` prints one envelope when a command exits with a non-zero code (`error.code` is `exit`).
