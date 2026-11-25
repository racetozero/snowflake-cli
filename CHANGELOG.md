# Changelog

## 0.0.3 - 2026-11-25

- `connection add` stores the secret of a `PROGRAMMATIC_ACCESS_TOKEN` or `OAUTH` connection as `token`, not as `password`, which these authenticators ignore. With `--authenticator` the prompt asks for the token. When a connection has the old form, the failed login says how to fix it. Upstream has the same bug.
- Security: password, token and passphrase prompts no longer show what you type.
- Security: the OAuth code flow checks `state`, and workload identity sends credentials only to Snowflake hosts.
- `OAUTH_AUTHORIZATION_CODE` (and `auth login`) uses the built-in client, the configured redirect port, and a pasted URL when no browser opens. `externalbrowser` ignores requests that carry no token.
- `OAUTH_CLIENT_CREDENTIALS` requires the client ID and secret, and supports `oauth_credentials_in_body` and `{host}` and `{port}` in URLs.
- MFA: `--mfa-passcode` works with every authenticator, `passcode` and `passcode_in_password` work in the configuration, and a Duo push completes.
- Key pair: EC keys (P-256, P-384), `JWT_LIFETIME_IN_SECONDS`, 60-second login tokens, and 24-hour `generate-jwt` tokens, as upstream.
- Workload identity: OIDC reads the GitHub Actions token when the connection has none or an expired one.
- Connections: `region`, `.global` and China accounts, upper-case `secondary_roles`, account identifier checks, and checked session tokens, as upstream. Token files may end with a newline.

## 0.0.2 - 2026-11-23

- TLS trusts the operating system's certificate store instead of the bundled Mozilla roots, so corporate root CAs and TLS-inspecting proxies work. `SSL_CERT_FILE` and `SSL_CERT_DIR` replace the system store. This covers every HTTPS and WebSocket connection: Snowflake, cloud storage, workload identity, `snow update` and `!source` URLs.
- `snow` warns when the certificate store has no certificates, and says how to fix it.

## 0.0.1 - 2026-11-23

- Rust port of Snowflake CLI 3.35.0.dev0 (upstream `3ba03140b05959ebf8d20e3e756c696907e8f110`): every command, help text and output format, checked against the Python CLI by the differential parity suite.
- `--agent` mode (one JSON line per command) and the `snow` agent skill (`snow skill`).
- `snow update` self-update and the release-build update notice.
- Profile-guided optimized release builds.
- Faster startup: no CoreFoundation on macOS (chrono's local-time fallback reads `/etc/localtime`), and identifier checks no longer compile regexes on every call (`snow app bundle` uses about 45% less CPU).
- Telemetry events of one command go in one request, as the Python connector batches them.
- Fixed: ANSI sanitizing of `ESC ]`, `ESC ^` and `ESC -` now matches upstream; `connection add` writes fields in the upstream order; `!rehash` clears the completion cache; `--agent` prints one envelope when a command exits with a non-zero code (`error.code` is `exit`).
