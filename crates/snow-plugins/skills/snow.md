---
name: snow
description: Use the `snow` CLI (Snowflake CLI) from a shell to run SQL and manage Snowflake objects, connections, stages, git repositories, notebooks, Cortex, Snowpark, Streamlit, Native Apps, Snowflake App Runtime (app.yml), Snowpark Container Services, code bundles, dbt and DCM projects. Explains agent mode (one JSON line per command), connections and authenticators, templating, errors and exit codes, destructive commands, and the commands of every area.
---

# snow

Commands read `snow <group> [<subgroup>] <verb> [ARGS] [OPTIONS]`. Always add
`--agent` (or set `SNOW_AGENT=1`). Run `snow <group> <verb> --help --agent`
before you use a command for the first time; the help is the source of truth
for flags. `snow --structure` prints the whole command tree as JSON.

## Rules that agents get wrong

1. **Always give `snow sql` a source:** `-q`, `-f` or `-i`. Without one, `snow
   sql` starts the interactive REPL, even in agent mode. SQL piped on stdin
   without `-i` does not run.
2. **`&` is a template character in SQL.** `select 'R&D'` fails with
   `'D' is undefined`. Write `&&`, or pass `--enable-templating STANDARD`
   (or `NONE`) when the SQL has literal `&`.
3. **Browser logins block.** Agent mode never asks for typed input, but a
   connection with `authenticator = externalbrowser` or
   `OAUTH_AUTHORIZATION_CODE` still opens a browser and waits. `snow` keeps
   no token cache, so this happens on every command. Use key pair,
   programmatic access token, OAuth client credentials or workload identity
   for unattended work.
4. **Do not use `--force` on your own.** It approves every prompt, and some
   commands then drop objects (`app run --force`, `app teardown --force`,
   `dcm purge --force`, `spcs service drop --force`). Use it only when the
   user asked for that change.
5. **Bare `snow app deploy` prunes.** For a Native App it means
   `--prune --recursive` and deletes stage files that do not exist locally.
6. **`--env` has two meanings.** On project commands it overrides a template
   variable (`--env key=value`). On `snow dbt execute` it selects an
   environment from `env.yml`.
7. **Two exit-code schemes.** `--agent` codes (below) are not the
   `--enhanced-exit-codes` codes. Read `error.code`, not only the exit code.
8. **Options after `--` belong to the program.** `--agent` counts only before
   `--` or `--agent-args`; everything after them goes to the bundle, dbt or
   launched program unchanged.

## Agent output

With `--agent`, a command prints one JSON line on stdout. Warnings and
progress never reach stdout:

```json
{"ok":true,"command":"object list","data":[{"name":"T1","kind":"TABLE"}]}
{"ok":true,"command":"stage copy","data":[...],"meta":{"messages":["..."]}}
{"ok":false,"command":"sql","error":{"code":"api","message":"002003 (42S02): ..."}}
```

- `data` is the result in the shape `--format json` prints:
  - a list of row objects for a query or a list command;
  - an object for one item (`describe`, `connection test`);
  - `{"message": "..."}` for a status message;
  - a list of lists for several SQL statements. Async query IDs and `!`
    command output join that list, so check each entry;
  - `{"help": "..."}` for `--help` (the rendered help text);
  - `null` when the command has no result.
- `meta.messages` holds the progress lines. `meta` is absent when there are
  none.
- `error.remediation`, when present, tells how to fix the error.
- `command` is the command path. It is `""` for group help and unknown
  commands.
- An error message keeps only its headline. For example, a template error
  says only `SQL rendering error`. Run again without `--agent` to see the
  details on stderr.

| `error.code`            | Exit | Meaning and what to do                                                  |
| ----------------------- | ---: | ----------------------------------------------------------------------- |
| `api`                   |    1 | Snowflake or HTTP error, including SQL errors. Read the message.         |
| `auth`                  |    2 | Login failed, or no connection or `snowflake.yml` is configured.         |
| `invalid`               |    3 | Bad flags or input. The message is the usage error.                     |
| `input_required`        |    3 | The command would prompt. Pass the flag the message names.              |
| `confirmation_required` |    3 | The command would ask "are you sure". See rule 4.                       |
| `unknown`               |    4 | No such command. Check `snow <group> --help --agent`.                   |
| `internal`              |    5 | Unexpected failure.                                                     |

For `auth`, ask the user to fix the connection, and give them
`snow connection test -c NAME --agent`. Never guess credentials. `--debug`
prints connector logs on stderr.

Without `--agent`, `--format` takes `TABLE`, `JSON`, `JSON_EXT` (parses
VARIANT, OBJECT and ARRAY values into JSON), `CSV` or `PLAIN`. In `TABLE`
and `PLAIN`, `snow sql` also prints each statement before its result.

## Connections

- `snow connection list --agent` shows the connections (secrets masked as
  `****`) and which one is the default.
- Select one with `-c NAME`. Override parts with `--account`, `--user`,
  `--role`, `--warehouse`, `--database`, `--schema`, `--host`.
- The default is `SNOWFLAKE_DEFAULT_CONNECTION_NAME`, then
  `default_connection_name` in `config.toml`, then `default`.
- Files live in `$SNOWFLAKE_HOME` (default `~/.snowflake`): `config.toml` and
  `connections.toml`. If `connections.toml` exists, it replaces the
  `[connections]` section of `config.toml`. Files that others can read stop
  the command; the fix is `chmod 0600 FILE`.
- Environment overrides:
  - `SNOWFLAKE_CONNECTIONS_<NAME>_<KEY>` overrides a key of an existing
    connection. It cannot create a connection.
  - `SNOWFLAKE_<KEY>` (for example `SNOWFLAKE_ROLE`) fills only keys that the
    connection does not set.
- Temporary connection with no config entry: `-x --account ACC --user USER
  --authenticator SNOWFLAKE_JWT --private-key-file key.p8`. `-x` cannot be
  combined with `-c`.
- Create a connection without prompts (agent mode requires `-n`):
  `snow connection add -n NAME --account ACC --user USER --authenticator
  SNOWFLAKE_JWT --private-key-file PATH [--default] --no-interactive --agent`.
- `snow connection test --agent` · `set-default NAME` · `remove NAME`.
- `snow connection generate-jwt --agent` prints a JWT valid for 120 seconds.
  `generate-workload-identity-token` prints a workload identity token.
- `snow helpers import-snowsql-connections --agent` imports SnowSQL
  connections; a name that already exists is `confirmation_required`.
  `snow helpers check-snowsql-env-vars` maps `SNOWSQL_*` variables to
  `SNOWFLAKE_*`.

| `--authenticator`                    | Needs                                                                               |
| ------------------------------------ | ----------------------------------------------------------------------------------- |
| `SNOWFLAKE` (default)                | `--user`, `--password`                                                              |
| `SNOWFLAKE_JWT`                      | `--user`, `--private-key-file` (PKCS#8); passphrase in `PRIVATE_KEY_PASSPHRASE`     |
| `PROGRAMMATIC_ACCESS_TOKEN`, `OAUTH` | `--token` or `--token-file-path`                                                    |
| `USERNAME_PASSWORD_MFA`              | `--user`, `--password`, `--mfa-passcode CODE` (otherwise a Duo push)                |
| `OAUTH_CLIENT_CREDENTIALS`           | `--oauth-client-id`, `--oauth-client-secret`, `--oauth-token-request-url`           |
| `WORKLOAD_IDENTITY`                  | `--workload-identity-provider AWS\|AZURE\|GCP\|OIDC`; OIDC also needs a token       |
| `externalbrowser`, `OAUTH_AUTHORIZATION_CODE` | A person at a browser. Do not use in agent mode (rule 3).                  |

`password` is not an authenticator value. The proxy is read from
`HTTPS_PROXY`. For private link, set `--host` or the connection's `host`.

## SQL

- One query: `snow sql -q "select current_version()" --agent`.
- Several statements: `snow sql -q "use role r; select 1" --agent`. `data`
  has one list per statement.
- Files run in order: `snow sql -f setup.sql -f data.sql --agent`. Stdin:
  `cat x.sql | snow sql -i --agent`. `-q`, `-f` and `-i` exclude each other.
- Prefer `-f FILE` for long or multi-line SQL over shell quoting.
- Variables: `snow sql -q "select <% n %>" -D n=5 --agent`.
  - `--enable-templating` takes `LEGACY` (`&{n}`, also bare `&n`),
    `STANDARD` (`<% n %>`), `JINJA` (`{{ n }}`), `ALL` or `NONE`. The default
    is `LEGACY,STANDARD`. Templates render even without `-D`.
  - An undefined variable fails before anything runs.
  - `<% ctx.env.X %>` reads `--env X=...`, then the environment variable `X`.
- `--single-transaction` runs the statements in `BEGIN` … `COMMIT`.
  `--retain-comments` sends comments to Snowflake (they are stripped by
  default).
- A statement that ends with `;>` runs asynchronously and returns its query
  ID.
- `!` commands in files and `-q`: `!source FILE` (or `!load`), `!set
  output_format=json`, `!spool FILE|off`, `!queries`, `!result QUERY_ID`,
  `!abort QUERY_ID`. `https://` sources are fetched unless `--local-only`;
  `http://` is refused. `!set timing` and `!set echo` work only in the REPL.
- `-o output_file=PATH` writes the result to a file. `-o quiet=true` hides
  it.

## Objects

- `snow object list TYPE [--like 'pat%'] [--in database DB] [--in-account] --agent`.
  Types:
  - table, view, schema, database, warehouse;
  - role, user, stage, function, procedure;
  - task, stream, service, compute-pool, secret;
  - integration, network-rule, notebook, streamlit, git-repository;
  - image-repository, external-access-integration.
- `snow object describe TYPE NAME --agent` · `snow object drop TYPE NAME [--if-exists] --agent`
- `snow object create TYPE name=x comment='...' [--if-not-exists|--replace] --agent`,
  or `--json '{"name": "x"}'`.

## Stages and git

A path is a stage path only when it starts with `@` or `snow://`. Everything
else is local.

- `snow stage list [--like 'p%'] --agent` · `describe @st` · `drop st` ·
  `create st [--encryption SNOWFLAKE_SSE] [--enable-directory]`
- `snow stage list-files @st/dir [--pattern '.*[.]csv'] --agent`
- Copy:
  - upload: `snow stage copy ./data @st/dir [--recursive] [--overwrite] [--auto-compress] [--refresh] --agent`
  - download: `snow stage copy @st/dir ./out --recursive --agent`
  - stage to stage: `snow stage copy @a/dir @b/dir --agent` (destination
    cannot be `@~`). Local to local is an error.
- `snow stage remove st file.csv --agent`
- `snow stage execute "@st/scripts/*.sql" [-D key=value] [--on-error continue] --agent`
  runs `.sql` and `.py` files. `--on-error` defaults to `break`.
- Git repositories:
  - `snow git list --agent` · `describe REPO` · `fetch REPO` · `drop REPO`
  - `list-branches REPO` · `list-tags REPO` · `list-files @REPO/branches/main/`
  - `copy @REPO/branches/main/path ./local` (a trailing `/` copies the
    directory contents)
  - `execute "@REPO/branches/main/sql/*.sql" [-D key=value]`
  - `snow git setup` only prompts. Create repositories with `snow sql`
    (`CREATE GIT REPOSITORY ...`) instead.

## Notebooks, Cortex, logs, code bundles

- `snow notebook execute NB --agent` · `get-url NB` · `create NB --notebook-file @st/nb.ipynb`
  · `deploy [ENTITY_ID] [--replace] [--prune]` (needs a `snowflake.yml`
  version 2 notebook entity). `notebook open` opens a browser; use `get-url`.
- `snow cortex complete "question" [--model llama3.1-70b] --agent` (or
  `--file history.json` for a conversation). Also `summarize`, `sentiment`,
  `translate --to de [--from en]` and `extract-answer QUESTION TEXT`.
- `snow logs TYPE NAME [--from TIME] [--to TIME] [--log-level INFO] [--table EVENT_TABLE] --agent`
- Code bundles (not `snow app bundle`):
  - `snow bundle create NAME --source ./dir|@st/dir [--exclude 'venv'] [--overwrite|--skip-if-exists] --agent`
  - `snow bundle execute NAME --entrypoint src/main.py [--async] --agent -- ARGS...`
  - `snow bundle list|delete|alter NAME --agent` · `status|cancel QUERY_ID` · `history [--bundle-name NAME]`

## Projects (snowflake.yml)

Run from the project directory, or pass `-p DIR`. `snowflake.local.yml`
merges over `snowflake.yml`. `definition_version` is `1`, `1.1` or `2`;
templates such as `<% ctx.env.x %>` need 1.1 or higher. `--env key=value`
overrides a template variable before the environment and the `env:` section.

- `snow init DIR --template example_snowpark --no-interactive -D name=value --agent`.
  `DIR` must not exist. Remote templates need `git`.
- `snow helpers v1-to-v2 --agent` converts a v1 file and keeps it as
  `snowflake_V1.yml`. Add `-t` when the file has templates.
- Snowpark:
  - `snow snowpark build --agent` writes `output/bundle/snowpark`. It runs
    `python -m pip wheel` with the `python` on `PATH` for packages that are
    not on the Snowflake Anaconda channel. Add `--allow-shared-libraries` for
    packages with native code.
  - `snow snowpark deploy [--replace] [--prune] --agent` needs a warehouse.
    It fails for existing objects unless `--replace`.
  - `list function|procedure` · `describe|drop function "name(int)"` ·
    `execute function "fn(1)"`
  - `snow snowpark package lookup|create NAME --agent` ·
    `package upload --file pkg.zip --stage @st --agent`
- Streamlit:
  - `snow streamlit deploy [ENTITY_ID] [--replace] [--prune] [--legacy] --agent`
  - `get-url NAME` · `list` · `describe|drop|execute|status NAME`
  - `share NAME ROLE --agent` · `logs [ENTITY_ID] [--name NAME] [--tail N]`
  - Do not pass `--open`; it opens a browser.

## Native Apps (application / application package entities)

- `snow app run --agent` deploys the package and creates or upgrades the
  application. `--version V [--patch N]` or `--from-release-directive`
  installs a version instead.
- `snow app deploy [PATHS] --agent` only syncs files to the package stage
  (rule 5). `snow app bundle|validate --agent`.
- `snow app version create V [--patch N] [--label L] --agent` · `version list` ·
  `version drop V` · `snow app publish --version V [--channel C]`
- Release channels and directives:
  `snow app release-channel list|add-accounts|remove-accounts|set-accounts|add-version|remove-version CHANNEL`
  and `snow app release-directive list|set|unset|add-accounts|remove-accounts DIRECTIVE`.
- `snow app events --agent` · `snow app open --agent` (returns the URL)
- `snow app teardown --agent` drops the application and package. If the app
  owns objects, it stops unless you pass `--cascade`; this is destructive.

## Snowflake App Runtime (app.yml, version 2)

An `app.yml` with `version: 2` in the project root makes `snow app` deploy an
application service. An `app.yml` without version 2 is ignored. Its
`targets` hold per-environment overrides. `--target` selects one; without it,
`default_target` in `app.yml` is used, and with neither the command fails.

- `snow app setup [--app-name N] [--database DB --schema S] [--warehouse WH] [--dry-run] --agent`
  writes an `app.yml`. It does nothing when `app.yml` or `snowflake.yml`
  exists. Missing values are `input_required`.
- `snow app validate [--target T] --agent` · `snow app bundle --agent`
- `snow app deploy [--target T] [--upload-only|--build-only|--promote-only] --agent`
  uploads the source, builds the artifact repository package (up to 20
  minutes), then creates or alters the service. It ends with
  `App ready at URL`. Use only one of the `--*-only` flags.
- `snow app open --print-only --agent` · `open --settings` · `open --watch`
  waits up to 20 minutes for the endpoint.
- `snow app events [--type log|metric|lifecycle] [--since 1h] [--until ...] [--last N] [--metric cpu|memory|network] [--raw] [--instance N] --agent`
- `snow app teardown --force --agent` drops the service and its code storage
  (rule 4).

## Containers (SPCS)

Service, database and schema names must be unquoted identifiers.

- Compute pools:
  - `snow spcs compute-pool create POOL --family CPU_X64_XS [--min-nodes 1] [--max-nodes 2] [--auto-suspend-secs 3600] [--if-not-exists] --agent`
  - `list|describe|drop|status|suspend|resume|stop-all POOL --agent` ·
    `set POOL ...` · `unset POOL --comment` · `deploy [ENTITY_ID] [--upgrade]`
- Services:
  - `snow spcs service create SVC --compute-pool POOL --spec-path spec.yml [--min-instances 1] [--eai-name EAI] --agent`
  - `execute-job JOB --compute-pool POOL --spec-path spec.yml [--async]`
  - `list|describe|status|suspend|resume|list-endpoints|list-instances|list-containers|list-roles SVC --agent`
  - `upgrade SVC --spec-path spec.yml` · `set|unset SVC ...` · `deploy [ENTITY_ID] [--upgrade]` · `drop SVC`
  - `logs SVC --container-name main --instance-id 0 [--num-lines 100] [--since 2024-11-21T09:30:00Z] --agent`.
    `--container-name` and `--instance-id` are required.
  - `metrics SVC --container-name main --instance-id 0 [--since '5 minutes'] --agent`
    has the same required flags. `events SVC [--instance-id 0 [--container-name main]] [--since '5 minutes'] --agent`
    does not. Both need an event table.
  - `remote-build --build-context-dir DIR --compute-pool POOL ...` ·
    `remote-build-status --job-name JOB` · `remote-build-history`
- Images:
  - `snow spcs image-registry url|token --agent`. `image-registry login`
    runs `docker login`.
  - `snow spcs image-repository create|list|list-images|list-tags|url|drop REPO --agent` ·
    `deploy [ENTITY_ID]`
  - `snow custom-image validate IMAGE --agent` checks a local Docker image
    (needs `docker`; `--scan-vulnerabilities` needs `grype`).

## dbt and DCM

- `snow dbt list|describe|drop PROJECT --agent`
- `snow dbt deploy PROJECT --source ./my_dbt --agent` needs `dbt_project.yml`
  and `profiles.yml`. `--force` replaces the project and deletes its versions
  and run history.
- `snow dbt execute [--run-async] [-c CONN] PROJECT run --select model --agent`.
  `snow` options go before `PROJECT`; everything after the dbt command goes
  to dbt.
- `snow dbt copy SRC DEST` copies files like `snow stage copy`.
- `snow dcm list|describe|create|drop PROJECT --agent`. `--from DIR` is the
  project directory and `--target T` a `manifest.yml` target.
- `snow dcm plan [PROJECT] [--from ./dir] [--target T] --agent` shows the
  changes. `snow dcm deploy` applies them without a confirmation, so show the
  plan to the user first.
- `snow dcm list-deployments` · `drop-deployment --deployment D` ·
  `purge --force` (destructive). `--save-output` deletes `./out` first.
- `snow dcm test`, `refresh` and `preview` exist only with
  `SNOWFLAKE_CLI_FEATURES_ENABLE_DCM_PREVIEW_FEATURES=true`.
- Variables that `manifest.yml` declares in `templating.env_vars` or
  `env_secrets` come from the shell environment.

## Other commands

- `snow --info --agent` shows the version and paths. `snow --version`.
- `snow auth oidc read-token --type github` reads a GitHub Actions OIDC
  token.
- `snow feedback -r 1..4 -m "note" --yes --agent`. Agents must pass `-m`.
  Do not send secrets or account data, and send feedback only when the user
  asks.
- Feature flags are `[cli.features]` keys in `config.toml` or
  `SNOWFLAKE_CLI_FEATURES_<NAME>=true`. Hidden commands such as
  `snow stage diff STAGE DIR`, `snow ws deploy|drop|validate --entity-id ID` and
  `snow ws dump`
  still work; read their `--help` first.

## Not available in this build

`snow` is one binary without Python. These do not work:

- `snow feature` (needs `snowflake-ml-python`);
- external CLI plugins;
- the `native app setup` artifact processor;
- `snow cortex search`.

The Native App `snowpark` processor runs a Python from the configured
environment, `VIRTUAL_ENV`, `CONDA_DEFAULT_ENV`, or `python3`/`python` on
`PATH`.

## Skill and updates

- `snow skill --agent` prints this skill. `snow skill --install [DIR]` writes
  it to `DIR/snow/SKILL.md` (default `.claude/skills`).
- `snow update --agent` installs the latest release. A Homebrew install uses
  `brew upgrade racetozero/tap/snowflake-cli`. Run either only when the user
  asks.
