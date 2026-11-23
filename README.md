# snow

[![Rust](https://img.shields.io/badge/Rust-stable-orange?logo=rust)](https://github.com/racetozero/snowflake-cli/blob/main/rust-toolchain.toml)
[![Downloads](https://img.shields.io/github/downloads/racetozero/snowflake-cli/total)](https://github.com/racetozero/snowflake-cli/releases)
[![Upstream](https://img.shields.io/badge/Snowflake%20CLI-3.35.0.dev0-29b5e8)](https://github.com/snowflakedb/snowflake-cli)

A [ridiculously fast](#performance) drop-in Rust port of
[Snowflake CLI](https://github.com/snowflakedb/snowflake-cli).

`snow` accepts the same command lines, reads the same `config.toml` and
`connections.toml`, and prints the same help, tables, JSON, CSV and error
panels as the Python CLI. It is one native binary: no Python environment, no
`pip`, and about 10 ms per command instead of about a second.

## Why snow

- **Start quickly.** Commands finish in 5 to 15 ms where the Python CLI needs
  about one to two seconds, more than 100 times faster.
- **Use less memory.** A command peaks at 8 to 20 MiB of resident memory
  instead of 120 to 155 MiB.
- **Change nothing.** Every command, flag, help page and output format
  matches the Python CLI. A differential test suite checks it against the
  pinned upstream release.
- **Install one file.** A single static binary for macOS, Linux (glibc or
  musl), WSL and Windows on x64 and ARM64. `snow update` keeps it current.
- **Drive it from agents.** `--agent` prints one JSON line per command, never
  prompts, and has stable error codes. One agent skill teaches the whole CLI.

## Install

macOS, Linux and WSL:

```bash
curl -LsSf https://raw.githubusercontent.com/racetozero/snowflake-cli/main/install.sh | sh
```

Windows, including ARM64:

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://raw.githubusercontent.com/racetozero/snowflake-cli/main/install.ps1 | iex"
```

The installer selects the correct release, verifies its SHA-256 checksum, and
installs `snow` in your user binary directory.

On Linux and WSL, if the glibc release needs a newer `GLIBC_*` version than
your system provides, the installer uses the matching musl release. Set
`SNOW_LIBC=musl` to choose it yourself.

On macOS, Linux and WSL, the installer adds `~/.local/bin` to your shell's
startup file (bash, zsh, fish or sh). Open a new terminal after installation,
or run `~/.local/bin/snow` immediately. Set `SNOW_INSTALL_DIR` to install
elsewhere and `SNOW_VERSION` to pin a release.

Update later with:

```bash
snow update
```

Homebrew on macOS and Linux:

```bash
brew install racetozero/tap/snowflake-cli
```

Homebrew owns this binary, so update it with
`brew upgrade racetozero/tap/snowflake-cli` instead of `snow update`.

If the Python `snow` is also on your `PATH`, whichever comes first wins. Both
read the same configuration, so you can switch between them.

## Start in two commands

Add a connection and run a query:

```bash
snow connection add -n dev --account myorg-myaccount --user me --authenticator externalbrowser --no-interactive
snow sql -q "select current_version()"
```

Existing Snowflake CLI and SnowSQL users already have connections in
`~/.snowflake`; `snow connection list` shows them.

## Work in the terminal

Everything the Python CLI does works the same way:

```bash
snow sql -f setup.sql -D env=prod          # run files with variables
snow object list table --like 'ORD%'       # list, describe, create, drop objects
snow stage copy ./data @raw/2026 --recursive
snow streamlit deploy --replace            # projects defined in snowflake.yml
snow app run                               # Native Apps
snow app deploy --target prod              # Snowflake App Runtime (app.yml)
snow spcs service list                     # Snowpark Container Services
snow dbt execute my_project run            # dbt projects
snow dcm plan                              # DCM projects
```

`snow sql` without a query opens the interactive SQL REPL:

- Statements run when they end with `;`. `!source`, `!set`, `!spool`,
  `!queries`, `!result` and the other `!` commands work as in SnowSQL.
- Press `Tab` to complete keywords, functions, types and database, schema and
  table names. Press `Tab` twice to list the choices.
- Keywords, strings, numbers and comments are highlighted.
- Use the Up arrow for history, saved in `~/.snowflake/repl_history`.
- Press `Ctrl+C` to drop the current input, and `Ctrl+D` or type `exit` to
  leave.

Set `prompt_format` in `config.toml` (or pass `--prompt-format`) to show the
user, role, warehouse, database or schema in the prompt.

## Agents

Every command accepts `--agent` (or `SNOW_AGENT=1`). Agent mode prints exactly
one JSON line on stdout and never prompts, opens a browser or starts the REPL:

```json
{"ok":true,"command":"object list","data":[{"name":"T1"}],"meta":{"messages":[]}}
{"ok":false,"command":"sql","error":{"code":"api","message":"002003 (42S02): ..."}}
```

| Code                                       | Exit | Meaning                                  |
| ------------------------------------------ | ---: | ---------------------------------------- |
| `api`                                      |    1 | Snowflake or HTTP error                  |
| `auth`                                     |    2 | Login failed or no connection configured |
| `invalid`                                  |    3 | Bad flags or input                       |
| `confirmation_required`, `input_required` |    3 | The command would have prompted          |
| `unknown`                                  |    4 | No such command                          |
| `internal`                                 |    5 | Unexpected failure                       |

One skill, `snow` in [`.agents/skills`](.agents/skills/snow/SKILL.md), teaches
agents the whole CLI:

```bash
npx skills add racetozero/snowflake-cli   # or bunx
snow skill --install                      # write it to .claude/skills/snow/SKILL.md
snow skill                                # print it
```

See [docs/agent-mode.md](docs/agent-mode.md) for details.

## Configuration

`snow` reads the files the Python CLI reads, from `SNOWFLAKE_HOME`
(default `~/.snowflake`): `config.toml`, `connections.toml` and the
`SNOWFLAKE_*` environment variables. Files must have mode 0600 on macOS and
Linux.

Connections support every authenticator of the Python connector: password,
key pair (`SNOWFLAKE_JWT`), external browser, OAuth (authorization code and
client credentials), programmatic access tokens, MFA, and workload identity
federation for AWS (including SSO profiles), Azure, GCP and OIDC.

In a terminal, release builds check GitHub at most every 20 hours and print a
one-line notice when a newer release is out. The check never runs in CI or in
agent mode. Set `SNOW_NO_UPDATE_NOTIFIER=1` to turn it off, or
`SNOW_SELF_UPDATE_COMMAND` (for example `cargo install snowflake-cli --force`)
to make `snow update` use another package manager.

## Performance

`snow` is measured against the Python Snowflake CLI on the same commands, with
the same `SNOWFLAKE_HOME`, against a local fake Snowflake server, so the
results measure the CLI and not the network. They were measured on
2026-11-22 with snow 3.35.0-dev0 and Snowflake CLI 3.35.0.dev0 on Python 3.12.

### Commands

| Command                                   | snow (Rust) | Snowflake CLI (Python) | Speedup | snow peak RSS | Python peak RSS |
| ----------------------------------------- | ----------: | ---------------------: | ------: | ------------: | --------------: |
| `snow --version`                          |     13.8 ms |              1,810.7 ms |    131x |       8.4 MiB |       118.5 MiB |
| `snow --help`                             |      8.3 ms |              1,018.6 ms |    123x |       9.6 MiB |       141.1 MiB |
| `snow connection list`                    |      9.1 ms |              1,036.0 ms |    114x |       9.5 MiB |       152.7 MiB |
| `snow sql -q "select * from orders"`      |      7.5 ms |              1,009.8 ms |    135x |      11.8 MiB |       154.0 MiB |
| `snow object list table --format json`    |      5.7 ms |                965.3 ms |    169x |       9.8 MiB |       152.7 MiB |
| `snow app bundle`                         |      9.4 ms |              1,706.5 ms |    181x |      19.2 MiB |       152.7 MiB |

The query returns 200 rows. `snow app bundle` renders the templates of a
Native App project.

### Profile-guided release builds

Release binaries are built with profile-guided optimization (PGO). The release
workflow trains a profile for each target on deterministic offline workloads,
then measures the result on separate held-out commands.

| Measure                | Standard build | Optimized build |        Change |
| ---------------------- | -------------: | --------------: | ------------: |
| `snow --help`          |      11.145 ms |       11.081 ms |  0.57% faster |
| `snow object describe` |      20.955 ms |       19.781 ms |  5.60% faster |
| `snow sql` to CSV      |      19.248 ms |       18.746 ms |  2.61% faster |
| Geometric mean latency |         1.000x |          0.980x |  2.00% faster |
| Executable size        |       9.69 MiB |        8.87 MiB | 8.44% smaller |
| gzip size              |       4.11 MiB |        3.77 MiB | 8.17% smaller |

### Method

The tests use the `dist` release profile on macOS 27.0.1 with an Apple M4 and
Rust 1.100.0. The command table shows medians of 60 runs of `snow` and 15 runs
of the Python CLI after warmups; peak RSS is the resident memory that macOS
reports for each process. PGO results are medians of five trials of 60 runs
(30 for the CSV query) of each binary, interleaved. The host had other
programs running, so absolute times vary by a few milliseconds between runs;
the ratios are stable. Lower is better.

```bash
just bench        # snow against the Python CLI (needs `just upstream`)
just pgo-bench    # standard against PGO build on held-out commands
```

Results are written to `target/bench-startup.json` and
`target/snow-pgo/results.json`.

## Compatibility

`snow` tracks Snowflake CLI 3.35.0.dev0
(upstream commit `3ba03140b05959ebf8d20e3e756c696907e8f110`, recorded in
`Cargo.toml`).

Parity is checked differentially. `scripts/parity/check.py` runs more than
1,700 cases against the pinned Python CLI and this port, both talking to the
same local fake Snowflake server. It compares stdout, stderr, exit codes, the
SQL sent and the files written. The cases cover every one of the 214 commands
and the help of all 214 commands and 33 groups at three terminal widths. Known, intended differences are listed with their
reasons in `scripts/parity/allowlist.toml`.

`snow` has no Python runtime, so features that load Python libraries are not
available:

- `snow feature` (needs `snowflake-ml-python`): it reports the library as
  missing, as the Python CLI does without it.
- External CLI plugins (`snow plugin enable` finds none).
- The `native app setup` artifact processor (behind
  `ENABLE_NATIVE_APP_PYTHON_SETUP`).
- `snow cortex search` (upstream enables it only on Python < 3.12).

The native-app `snowpark` processor and `snow snowpark build` still work: they
run a Python found in your venv, conda environment or `PATH`.

`snow upgrade` keeps the Python CLI's behaviour, which applies only to
Snowflake's own installer. Use `snow update`.

## Development

Use Rust stable and cargo-nextest:

```bash
cargo nextest run --workspace --all-targets
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
just upstream     # fetch the pinned Python reference into target/upstream
just parity       # differential parity against it
just pgo-test
just pgo-bench
```

`just spec` regenerates the command tree in `crates/snow-spec` from the
reference.

## Release

1. Set `[workspace.package].version` in `Cargo.toml`.
2. Add release notes to `CHANGELOG.md`.
3. Run `cargo check --workspace` to update `Cargo.lock`.
4. Run `just release-check VERSION`.
5. Commit the version, lock file and changelog.
6. Run `just release VERSION` from a clean `main` branch.

The release command runs all checks, creates the tag, and starts the GitHub
release workflow. The workflow builds PGO-optimized binaries for macOS, Linux
(glibc and musl) and Windows on x64 and ARM64 (Windows ARM64 without PGO,
which has no Rust profiling runtime). It then publishes the archives,
checksums, attestations and installers, and updates the Homebrew formula in
`racetozero/homebrew-tap`.

## License

Apache-2.0, like the Snowflake CLI it ports.
