# snow

[![Rust](https://img.shields.io/badge/Rust-stable-orange?logo=rust)](https://github.com/racetozero/snowflake-cli/blob/main/rust-toolchain.toml)
[![Downloads](https://img.shields.io/github/downloads/racetozero/snowflake-cli/total)](https://github.com/racetozero/snowflake-cli/releases)
[![Upstream](https://img.shields.io/badge/Snowflake%20CLI-3.35.0.dev0-29b5e8)](https://github.com/snowflakedb/snowflake-cli)

A [ridiculously fast](#performance) drop-in Rust port of
[Snowflake CLI](https://github.com/snowflakedb/snowflake-cli).

`snow` accepts the same command lines, reads the same `config.toml` and
`connections.toml`, and prints the same help, tables, JSON, CSV, and error
panels as the Python CLI. It is one native binary: no Python environment, no
`pip`, and about 10 ms per command instead of about a second.

All 214 commands of Snowflake CLI 3.35.0.dev0 are here: SQL, objects, stages,
git repositories, notebooks, Cortex, Snowpark, Streamlit, Native Apps,
Snowflake App Runtime, Snowpark Container Services, code bundles, dbt, and
DCM projects.

## Why snow

- **Start quickly.** Commands finish in 5 to 15 ms where the Python CLI needs
  about one to two seconds, more than 100 times faster.
- **Use less memory.** A command peaks at 8 to 20 MiB of resident memory
  instead of 120 to 155 MiB.
- **Change nothing.** Every command, flag, help page, and output format
  matches the Python CLI. A differential test suite checks it against the
  pinned upstream release.
- **Install one file.** A single static binary for macOS, Linux (glibc or
  musl), WSL, and Windows on x64 and ARM64. `snow update` keeps it current.
- **Drive it from agents.** `--agent` prints one JSON line per command, never
  asks for input, and has stable error codes. One agent skill teaches the
  whole CLI.

## Install

macOS, Linux, and WSL:

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

On macOS, Linux, and WSL, the installer adds `~/.local/bin` to your shell's
startup file (bash, zsh, fish, or sh). Open a new terminal after
installation, or run `~/.local/bin/snow` immediately. Set `SNOW_INSTALL_DIR`
to install elsewhere and `SNOW_VERSION` to pin a release.

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

Existing Snowflake CLI and SnowSQL users already have connections.
`snow connection list` shows them, and
`snow helpers import-snowsql-connections` copies SnowSQL connections into
`snow`.

Run any command with `--help` to see its flags and examples. Add
`--help-all` to also show the connection and global options in full.

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

Choose the output with `--format`: `table` (the default), `json`, `json_ext`
(parses VARIANT, OBJECT, and ARRAY values into JSON), `csv`, or `plain`. Set
`cli.output_format` in `config.toml` or `SNOWFLAKE_CLI_OUTPUT_FORMAT` to
change the default.

### Run SQL

`snow sql` takes one source: a query (`-q`), one or more files (`-f`), or
standard input (`-i`). Statements run in order, and each result prints as its
own table.

```bash
snow sql -q "use role analyst; select count(*) from orders"
snow sql -f schema.sql -f seed.sql
cat report.sql | snow sql -i --format csv
```

Pass variables with `-D name=value`. Three template syntaxes are available:

| Syntax     | Example          | On by default |
| ---------- | ---------------- | ------------- |
| `LEGACY`   | `&{name}`, `&name` | Yes         |
| `STANDARD` | `<% name %>`     | Yes           |
| `JINJA`    | `{{ name }}`     | No            |

Select syntaxes with `--enable-templating`, or turn templates off with
`--enable-templating NONE`. Templates render even when you pass no `-D`, so a
literal `&` in SQL needs `&&` or `--enable-templating STANDARD`.
`<% ctx.env.NAME %>` reads `--env NAME=value`, then the environment variable.

Other options:

- `--single-transaction` runs all statements between `BEGIN` and `COMMIT`.
- `--retain-comments` sends comments to Snowflake. By default they are
  removed.
- A statement that ends with `;>` runs asynchronously and prints its query
  ID.
- `-o output_file=PATH` writes the results to a file, and `-o quiet=true`
  hides them.

Files and queries also accept SnowSQL commands:

| Command                     | Action                                            |
| --------------------------- | ------------------------------------------------- |
| `!source FILE`, `!load FILE` | Run another file, local or `https://`            |
| `!set output_format=json`   | Change an output option for the next statements   |
| `!spool FILE`, `!spool off` | Copy results to a file, or stop                   |
| `!queries`                  | List recent queries of this session               |
| `!result QUERY_ID`          | Show the result of an earlier query               |
| `!abort QUERY_ID`           | Cancel a running query                            |

`!set` accepts `output_format`, `quiet`, `timing`, `timing_in_output_file`,
`echo`, and `output_file`. `timing` and `echo` apply only in the REPL.
`--local-only` (or `cli.sql_local_only`) refuses `https://` sources;
`http://` is always refused.

### Use the SQL REPL

`snow sql` without a source opens the interactive SQL REPL:

- Statements run when they end with `;`.
- Press `Tab` to complete keywords, functions, types, and database, schema,
  and table names. Press `Tab` twice to list the choices.
- Keywords, strings, numbers, and comments are highlighted.
- Use the Up arrow for history, saved in `~/.snowflake/repl_history`.
- Press `Ctrl+C` to drop the current input, and `Ctrl+D` or type `exit` to
  leave.

Set `prompt_format` in `config.toml` (or pass `--prompt-format`) to show the
user, role, warehouse, database, or schema in the prompt. Set
`cli.auto_completion = false` to turn completion off, and
`cli.no_prompt_exit_repl = true` to leave without a confirmation.

## Agents

### Agent mode

Every command accepts `--agent` (or `SNOW_AGENT=1`). Agent mode prints
exactly one JSON line on stdout and never asks for typed input:

```json
{"ok":true,"command":"object list","data":[{"name":"T1"}]}
{"ok":true,"command":"stage copy","data":[...],"meta":{"messages":["..."]}}
{"ok":false,"command":"sql","error":{"code":"api","message":"002003 (42S02): ..."}}
```

`data` has the shape that `--format json` prints. `--help` puts the help text
in `data.help`. Progress lines go to `meta.messages`, which is absent when
there are none. An error can also carry `remediation`, which tells how to fix
it.

A value that the command would ask for is an `input_required` error. A
confirmation is a `confirmation_required` error; run again with `--force`
only when you want the change.

| Code                                      | Exit | Meaning                                                |
| ----------------------------------------- | ---: | ------------------------------------------------------ |
| `api`                                     |    1 | Snowflake or HTTP error, including SQL errors          |
| `auth`                                    |    2 | Login failed, or no connection or project is configured |
| `invalid`                                 |    3 | Bad flags or input                                     |
| `confirmation_required`, `input_required` |    3 | The command would have prompted                        |
| `unknown`                                 |    4 | No such command                                        |
| `internal`                                |    5 | Unexpected failure                                     |
| `exit`                                    |  any | The command ended with a non-zero exit code; `data` holds its output |

These codes are separate from `--enhanced-exit-codes`, which keeps the Python
CLI's scheme.

Limits:

- Give `snow sql` a source (`-q`, `-f`, or `-i`). Without one, it opens the
  REPL, also in agent mode.
- `externalbrowser` and `OAUTH_AUTHORIZATION_CODE` connections still open a
  browser and wait. Use key pair, programmatic access token, OAuth client
  credentials, or workload identity for unattended work.
- `--agent` counts only before `--` or `--agent-args`. Arguments after them
  go to the code bundle, dbt, or launched program unchanged.

See [docs/agent-mode.md](docs/agent-mode.md) for details.

### Agent skill

One skill, `snow` in [`.agents/skills`](.agents/skills/snow/SKILL.md), teaches
agents the whole CLI: agent mode, connections, templating, destructive
commands, and every command group.

```bash
npx skills add racetozero/snowflake-cli   # or bunx
snow skill --install                      # write it to .claude/skills/snow/SKILL.md
snow skill --install .agents/skills       # or another directory
snow skill                                # print it
```

`snow skill` and `snow update` are not part of the upstream command tree, so
they do not appear in `snow --help` or `snow --structure`.

## Connections and authentication

### Add and select connections

A connection is a named table in `connections.toml` (or the `[connections]`
section of `config.toml`). Select one with `-c NAME`, and override any part
with `--account`, `--user`, `--role`, `--warehouse`, `--database`,
`--schema`, or `--host`.

```bash
snow connection add -n prod --account myorg-prod --user svc \
  --authenticator SNOWFLAKE_JWT --private-key-file ~/.ssh/prod.p8 --default --no-interactive
snow connection test -c prod
snow connection set-default prod
```

`snow` selects the default connection from
`SNOWFLAKE_DEFAULT_CONNECTION_NAME`, then `default_connection_name` in
`config.toml`, then the connection named `default`.

Environment variables change connections without editing files:

- **`SNOWFLAKE_CONNECTIONS_<NAME>_<KEY>`:** Overrides a key of an existing
  connection, for example `SNOWFLAKE_CONNECTIONS_PROD_ROLE`. It cannot create
  a connection.
- **`SNOWFLAKE_<KEY>`:** Fills a key that the connection does not set, for
  example `SNOWFLAKE_WAREHOUSE`.

For a one-off connection with no configuration, pass `-x` (or
`--temporary-connection`) with the connection flags:

```bash
snow sql -x --account myorg-dev --user me --authenticator SNOWFLAKE_JWT --private-key-file key.p8 -q "select 1"
```

### Choose an authenticator

`snow` supports every authenticator of the Python connector:

| `authenticator`                       | Input                                                              |
| ------------------------------------- | ------------------------------------------------------------------ |
| `SNOWFLAKE` (default)                 | User and password                                                  |
| `SNOWFLAKE_JWT`                       | User and a PKCS#8 private key; passphrase in `PRIVATE_KEY_PASSPHRASE` |
| `PROGRAMMATIC_ACCESS_TOKEN`, `OAUTH`  | `--token` or `--token-file-path`                                   |
| `USERNAME_PASSWORD_MFA`               | User, password, and `--mfa-passcode` (otherwise a Duo push)        |
| `externalbrowser`                     | Single sign-on in your browser                                     |
| `OAUTH_AUTHORIZATION_CODE`            | OAuth sign-in in your browser                                      |
| `OAUTH_CLIENT_CREDENTIALS`            | OAuth client ID, secret, and token URL                             |
| `WORKLOAD_IDENTITY`                   | `--workload-identity-provider AWS`, `AZURE`, `GCP`, or `OIDC`      |

Workload identity federation lets a cloud workload sign in without a stored
secret. AWS includes SSO profiles. OIDC needs a token or token file;
`snow auth oidc read-token --type github` reads one in GitHub Actions.

`snow connection generate-jwt` prints a key-pair JWT that is valid for 120
seconds. `snow connection generate-workload-identity-token` prints a workload
identity token.

`snow` does not cache tokens between commands, so browser authenticators
sign in again for each command.

### Network access

`snow` sends Snowflake traffic through `HTTPS_PROXY` when it is set. The
account host defaults to `<account>.snowflakecomputing.com`; for private
link, set `host` in the connection or pass `--host`.

## Projects

### Define a project

Project commands read `snowflake.yml` in the current directory, or in the
directory you pass with `-p`. `snowflake.local.yml`, when present, merges
over it.

```bash
snow init my_app --template example_snowpark --no-interactive -D name=my_app
snow helpers v1-to-v2
```

`definition_version` is `1`, `1.1`, or `2`. Version 2 defines entities:
`function`, `procedure`, `streamlit`, `notebook`, `application package`,
`application`, `service`, `compute-pool`, `image-repository`, and
`snowflake-app`. Templates such as `<% ctx.env.schema %>` need version 1.1 or
later. `--env key=value` overrides a template variable, before the
environment and the `env:` section.

`snow init` clones its templates from
[snowflake-cli-templates](https://github.com/snowflakedb/snowflake-cli-templates)
with `git`. Set `cli.init_template_source` or pass `--template-source` to use
your own.

### Snowpark and Streamlit

```bash
snow snowpark build
snow snowpark deploy --replace
snow snowpark execute function "add_one(1)"
snow streamlit deploy --replace
snow streamlit get-url my_app
```

`snow snowpark build` runs `python -m pip wheel` with the `python` on your
`PATH` for packages that are not on the Snowflake Anaconda channel. Packages
with native code need `--allow-shared-libraries`. `snow snowpark deploy`
needs a warehouse.

`snow streamlit deploy` creates a versioned Streamlit app. Add `--legacy` to
use `ROOT_LOCATION` instead.

### Native Apps

`snow app` manages application packages and applications defined in
`snowflake.yml`:

- **`snow app run`:** Deploy the package and create or upgrade the
  application.
- **`snow app deploy`:** Sync files to the package stage only. Without
  paths, it also removes stage files that do not exist locally.
- **`snow app version create V`, `snow app publish`:** Create versions and
  publish them to release channels.
- **`snow app release-channel`, `snow app release-directive`:** Control
  which accounts get which version.
- **`snow app teardown`:** Drop the application and the package.

`--force` approves every prompt. With `snow app run`, it can drop and
re-create an application that cannot upgrade.

### Snowflake App Runtime

An `app.yml` with `version: 2` in the project root makes `snow app` deploy an
application service instead.

```bash
snow app setup --app-name my_app --dry-run
snow app deploy --target prod
snow app open --watch
snow app events --type log --since 30m
```

`snow app deploy` runs three phases: upload the source, build the artifact
repository package, and create or alter the service. Use `--upload-only`,
`--build-only`, or `--promote-only` to run one phase. A build can take up to
20 minutes.

`targets` in `app.yml` hold per-environment overrides. `--target` selects
one; without it, `snow` uses `default_target`. `snow app setup` writes an
`app.yml` and does nothing when the project already has one.

### Containers

```bash
snow spcs compute-pool create pool1 --family CPU_X64_XS
snow spcs service create echo --compute-pool pool1 --spec-path spec.yml
snow spcs service logs echo --container-name main --instance-id 0
snow spcs image-registry login
```

Service names must be unquoted identifiers. `snow spcs image-registry login`
runs `docker login`, and `snow custom-image validate` checks a local Docker
image against the Snowpark Container Services requirements.

### dbt, DCM, and code bundles

```bash
snow dbt deploy my_project --source ./dbt
snow dbt execute my_project run --select orders
snow dcm plan --target prod
snow dcm deploy --target prod
snow bundle create my_bundle --source ./src --exclude venv
snow bundle execute my_bundle --entrypoint main.py -- --day 2026-11-22
```

`snow dbt execute` takes its own options before the project name. Everything
after the dbt command goes to dbt. Its `--env` option selects an `env.yml`
environment.

`snow dcm plan` shows the changes, and `snow dcm deploy` applies them without
a confirmation. The preview commands `snow dcm test`, `refresh`, and
`preview` need the `enable_dcm_preview_features` flag.

`snow bundle` manages Snowflake code bundles. It is not `snow app bundle`.

## Stages and git repositories

`snow stage copy` copies in three directions. A path is a stage path when it
starts with `@` or `snow://`.

```bash
snow stage copy ./data @raw/2026 --recursive --overwrite
snow stage copy @raw/2026 ./out --recursive
snow stage copy @raw/2026 @archive/2026
snow stage execute "@scripts/*.sql" -D env=prod --on-error continue
```

`snow stage execute` runs `.sql` and `.py` files from a stage. Git
repository commands work the same way:

```bash
snow git fetch my_repo
snow git list-files @my_repo/branches/main/
snow git execute "@my_repo/branches/main/sql/*.sql"
```

`snow git setup` asks for the origin, secret, and API integration
interactively.

## Configuration

### Files and settings

`snow` reads the files that the Python CLI reads, from `SNOWFLAKE_HOME`
(default `~/.snowflake`, or the platform configuration directory when that
does not exist).

| Path                 | Purpose                                         |
| -------------------- | ----------------------------------------------- |
| `config.toml`        | CLI settings, connections, and feature flags    |
| `connections.toml`   | Connections; replaces `[connections]` when present |
| `logs/`              | Command logs, when `cli.logs.save_logs` is on   |
| `repl_history`       | SQL REPL history                                |

On macOS and Linux, the files must have mode 0600. A file that others can
read stops the command; run `chmod 0600` on it. Pass `--config-file` to use
another configuration file.

Common `[cli]` settings:

| Setting                | Purpose                                         |
| ---------------------- | ----------------------------------------------- |
| `output_format`        | Default `--format`                              |
| `sql_local_only`       | Refuse `https://` sources in `!source`          |
| `prompt_format`        | SQL REPL prompt                                 |
| `auto_completion`      | SQL REPL completion                             |
| `no_prompt_exit_repl`  | Leave the REPL without a confirmation           |
| `init_template_source` | Default `snow init` template source             |
| `stage_upload_workers` | Parallel stage uploads                          |

### Feature flags

Some upstream commands are hidden until you turn on a flag. Set the flag in
`config.toml`:

```toml
[cli.features]
enable_dcm_preview_features = true
```

Or set `SNOWFLAKE_CLI_FEATURES_ENABLE_DCM_PREVIEW_FEATURES=true`.

| Flag                          | Shows                                   |
| ----------------------------- | --------------------------------------- |
| `enable_dcm_preview_features` | `snow dcm test`, `refresh`, `preview`   |
| `enable_auth_commands`        | `snow auth list`, `snow auth login`     |
| `enable_spcs_build_image`     | `snow spcs service build-image`         |

### Update notices

In a terminal, release builds check GitHub at most every 20 hours and print a
one-line notice when a newer release is out. The check never runs in CI, in
agent mode, or for `snow sql`.

- `SNOW_NO_UPDATE_NOTIFIER=1`: Turn the notice off.
- `SNOW_SELF_UPDATE_COMMAND`: Make `snow update` run another package manager,
  for example `cargo install snowflake-cli --force`.

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
SQL sent, and the files written. The cases cover every one of the 214
commands and the help of all 214 commands and 33 groups at three terminal
widths. Known, intended differences are listed with their reasons in
`scripts/parity/allowlist.toml`.

`snow` has no Python runtime, so features that load Python libraries are not
available:

- `snow feature` (needs `snowflake-ml-python`): it reports the library as
  missing, as the Python CLI does without it.
- External CLI plugins (`snow plugin enable` finds none).
- The `native app setup` artifact processor (behind
  `ENABLE_NATIVE_APP_PYTHON_SETUP`).
- `snow cortex search` (upstream enables it only on Python < 3.12).

Commands that need a local Python still work when one is installed.
`snow snowpark build` uses the `python` on your `PATH`. The Native App
`snowpark` processor uses the configured environment, `VIRTUAL_ENV`,
`CONDA_DEFAULT_ENV`, or `python3` on your `PATH`.

`snow upgrade` keeps the Python CLI's behavior, which applies only to
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
reference. After you change `.agents/skills/snow/SKILL.md`, copy it to
`crates/snow-plugins/skills/snow.md`; a test keeps the two equal.

## Release

1. Set `[workspace.package].version` in `Cargo.toml`.
2. Add release notes to `CHANGELOG.md`.
3. Run `cargo check --workspace` to update `Cargo.lock`.
4. Run `just release-check VERSION`.
5. Commit the version, lock file, and changelog.
6. Run `just release VERSION` from a clean `main` branch.

The release command runs all checks, creates the tag, and starts the GitHub
release workflow. The workflow builds PGO-optimized binaries for macOS, Linux
(glibc and musl), and Windows on x64 and ARM64 (Windows ARM64 without PGO,
which has no Rust profiling runtime). It then publishes the archives,
checksums, attestations, and installers, and updates the Homebrew formula in
`racetozero/homebrew-tap`.

## License

Apache-2.0, like the Snowflake CLI it ports.
