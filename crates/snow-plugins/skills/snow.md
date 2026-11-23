---
name: snow
description: Use the `snow` CLI (Snowflake CLI) from a shell to run SQL and manage Snowflake objects, connections, stages, git repositories, notebooks, Cortex, Snowpark, Streamlit, Native Apps, Snowflake App Runtime (app.yml), Snowpark Container Services, dbt and DCM projects. Covers agent mode (one JSON line per command), connections, errors and exit codes, and the commands of every area.
---

# snow

Commands read `snow <group> [<subgroup>] <verb>`. Always add `--agent` (or set `SNOW_AGENT=1`). `snow <group> --help --agent` lists a group's commands and flags; prefer it over guessing.

## Agent output

With `--agent`, every command prints exactly one JSON line on stdout and never prompts, opens a browser or starts the SQL REPL:

```json
{"ok":true,"command":"object list","data":[{"name":"T1","kind":"TABLE"}],"meta":{"messages":["..."]}}
{"ok":false,"command":"sql","error":{"code":"api","message":"002003 (42S02): ...: SQL compilation error ..."}}
```

- `data` is the command result:
  - a list of rows for queries;
  - `{"message": ...}` for messages;
  - a dict for single objects;
  - a list for multi-statement results;
  - `{"help": ...}` for `--help`.
- `meta.messages` holds the progress lines the command would have printed.
- `error.code` and exit code:

  | code | exit |
  | --- | --- |
  | `api` (Snowflake or HTTP error) | 1 |
  | `auth` | 2 |
  | `invalid` (bad flags or input) | 3 |
  | `confirmation_required` | 3 |
  | `input_required` | 3 |
  | `unknown` (no such command) | 4 |
  | `internal` | 5 |

- Without `--agent`, `--format json` prints the same JSON as the Python Snowflake CLI. Other formats: `csv`, `plain`, and `json_ext` (which parses VARIANT columns).

## Errors

- `auth`: ask the user to fix the connection (`snow connection test -c NAME --agent`). Never guess credentials.
- `input_required`: pass the flag named in the message.
- `confirmation_required`: the command would ask "are you sure". Re-run with `--force` only when the user asked for the change.
- `invalid`: read `error.message`; it is the usage error (missing argument, unknown option, bad choice).
- `--debug` prints connector logs on stderr.

## Connections

- `snow connection list --agent` shows the configured connections (secrets masked) and the default.
- Pick one with `-c NAME`. Override parts with `--account`, `--user`, `--role`, `--warehouse`, `--database`, `--schema`.
- Temporary connection without config: `-x --account ACC --user USER --authenticator SNOWFLAKE_JWT --private-key-file key.p8`. Or set `SNOWFLAKE_ACCOUNT`, `SNOWFLAKE_USER`, `SNOWFLAKE_PASSWORD`, ...
- Create a connection without prompts: `snow connection add -n NAME --account ACC --user USER --authenticator SNOWFLAKE_JWT --private-key-file PATH --no-interactive --agent`.
- `snow connection test --agent` checks a connection. `snow connection generate-jwt` and `generate-workload-identity-token` print tokens.
- Workload identity: `--authenticator WORKLOAD_IDENTITY --workload-identity-provider AWS|AZURE|GCP|OIDC`.
- Config lives in `$SNOWFLAKE_HOME/config.toml` and `connections.toml` (default `~/.snowflake`). Files must be mode 0600.

## SQL

- One query: `snow sql -q "select current_version()" --agent`.
- Several statements: `snow sql -q "use role r; select 1" --agent`. `data` has one entry per statement.
- Files: `snow sql -f setup.sql -f data.sql --agent`. Stdin: `cat x.sql | snow sql -i --agent`.
- Variables: `snow sql -q "select <% n %>" -D n=5 --agent`.
  - `&{n}` and `<% n %>` are on by default.
  - `--enable-templating JINJA` adds `{{ n }}`; `NONE` turns templating off.
- `--single-transaction` wraps the statements in BEGIN/COMMIT. `--retain-comments` sends comments to Snowflake.
- In files, `!source other.sql` includes a file, and `!set output_format=json` / `!set timing=true` change output. `--local-only` refuses `https://` sources.
- Always pass `-q`, `-f` or `-i`; the interactive REPL is for people.
- Prefer `-f FILE` for long or multi-line SQL rather than shell quoting.

## Objects

- `snow object list TYPE [--like 'pat%'] [--in database DB] [--in-account] --agent`. Types:
  - table, view, schema, database, warehouse;
  - role, user, stage, function, procedure;
  - task, stream, service, compute-pool, secret;
  - integration, network-rule, notebook, streamlit, git-repository;
  - image-repository, external-access-integration.
- `snow object describe TYPE NAME --agent` · `snow object drop TYPE NAME [--if-exists] --agent`
- `snow object create TYPE name=x comment='...' --agent`, or `--json '{"name": "x"}'`

## Stages, git, notebooks, Cortex, logs

- `snow stage list [--like 'p%'] --agent` · `describe @st` · `drop st` · `create st [--encryption SNOWFLAKE_SSE] [--enable-directory]`
- `snow stage list-files @st/dir [--pattern '.*[.]csv'] --agent`
- Copy:
  - upload: `snow stage copy ./data @st/dir [--recursive] [--overwrite] [--auto-compress] --agent`
  - download: `snow stage copy @st/dir ./out --recursive --agent`
  - stage to stage: `snow stage copy @a/dir @b/dir --agent`
- `snow stage remove st file.csv --agent` · `snow stage execute "@st/scripts/*.sql" [-D key=value] [--on-error continue] --agent`
- Git:
  - `snow git list-branches REPO --agent` · `list-tags` · `fetch`
  - `list-files @REPO/branches/main/` · `copy @REPO/branches/main/path ./local`
  - `execute "@REPO/branches/main/sql/*.sql"`
  - `snow git setup` prompts, so create repositories with `snow sql` instead.
- `snow notebook execute NB --agent` · `get-url NB` · `create NB --notebook-file @st/nb.ipynb`
- `snow cortex complete "question" [--model llama3.1-70b] --agent`. Also `summarize`, `sentiment`, `translate --to de`, `extract-answer`.
- `snow logs TYPE NAME [--from TIME] [--to TIME] [--log-level INFO] --agent`

## Projects (snowflake.yml)

Run from the project directory, or pass `-p DIR`. `--env key=value` overrides template variables.

- `snow init DIR --template example_snowpark --no-interactive -D name=value --agent` · `snow helpers v1-to-v2 --agent`
- Snowpark:
  - `snow snowpark build --agent` · `snow snowpark deploy [--replace] --agent`
  - `list function|procedure` · `describe|drop function "name(int)"` · `execute function "fn(1)"`
  - `snow snowpark package lookup|create NAME --agent` · `package upload --file pkg.zip --stage @st --agent`
- Streamlit: `snow streamlit deploy [ENTITY_ID] [--replace] [--legacy] --agent` · `get-url NAME` · `list|describe|drop|execute NAME` · `logs --name NAME [--tail N]`
- Native Apps (application / application package entities):
  - `snow app run --agent` deploys the package and installs the app; `snow app deploy --agent` only syncs files. `snow app bundle|validate --agent`.
  - `snow app version create V --agent` · `version list` · `version drop V --force` · `snow app publish --version V`
  - `snow app events --agent` · `snow app open --agent` (returns the URL)
  - `snow app teardown --force --agent` is destructive: only use it when asked.
- `snow ws bundle|deploy|drop|validate --entity-id ID --agent` acts on one entity.

## Snowflake App Runtime (app.yml, version 2)

An `app.yml` in the project root makes `snow app` deploy an application service. Its `targets` hold per-environment overrides; pick one with `--target`.

- `snow app validate [--target T] --agent` · `snow app bundle --agent`
- `snow app deploy [--target T] [--upload-only|--build-only|--promote-only] [--skip-certs-check] --agent` uploads the source, builds the artifact repository package, then creates or alters the service. It ends with `App ready at URL`.
- `snow app open --print-only --agent` · `open --settings` · `open --watch` waits for the endpoint.
- `snow app events [--type log|metric|lifecycle] [--since 1h] [--until ...] [--last N] [--metric cpu|memory|network] [--raw] [--instance N] --agent`
- `snow app teardown --force --agent` drops the service and its code storage.
- `snow app setup [--app-name N] [--database DB] [--schema S] [--warehouse WH] [--dry-run] --agent` writes an `app.yml` (missing values are `input_required` errors).

## Containers (SPCS)

Names must be unquoted identifiers.

- `snow spcs compute-pool create POOL --family CPU_X64_XS [--min-nodes 1] [--max-nodes 2] [--auto-suspend-secs 3600] [--if-not-exists] --agent`
- `snow spcs compute-pool list|describe|drop|status|suspend|resume|stop-all POOL --agent` · `set POOL ...` · `unset POOL --comment`
- `snow spcs service create SVC --compute-pool POOL --spec-path spec.yml [--min-instances 1] [--eai-name EAI] --agent` · `execute-job JOB --compute-pool POOL --spec-path spec.yml [--async]`
- `snow spcs service list|describe|status|suspend|resume|list-endpoints|list-instances|list-containers SVC --agent` · `upgrade SVC --spec-path spec.yml` · `drop SVC --force`
- `snow spcs service logs SVC --container-name main --instance-id 0 [--num-lines 100] --agent`
- `snow spcs image-registry url|token --agent` · `image-repository create|list|list-images|url|drop REPO --agent`

## dbt and DCM

- `snow dbt list|describe|drop PROJECT --agent` · `snow dbt deploy PROJECT --source ./my_dbt [--force] --agent`
- `snow dbt execute [--env prod] [--run-async] PROJECT run --select model --agent`. Options of `snow` go before PROJECT; everything after the dbt command goes to dbt.
- `snow dcm list|describe|create PROJECT --agent` · `snow dcm plan [--from ./dir] [--target T] --agent` shows the changes · `snow dcm deploy [--from ./dir] --agent` applies them.
- `snow dcm test [--script NAME | --all-scripts] [--expectations] --agent` · `snow dcm refresh` · `preview --object NAME [--limit N]` · `list-deployments` · `purge --force`.
- Env variables declared in `manifest.yml` (`templating.env_vars` / `env_secrets`) come from the shell environment.

## Not available in this build

This `snow` is a standalone binary without Python. The following are unsupported:
- `snow feature` (needs snowflake-ml-python);
- external CLI plugins;
- the `native app setup` artifact processor;
- `snow cortex search`.

The native-app `snowpark` processor runs a Python found on PATH or in the configured venv/conda environment.

## Keeping snow current

`snow update` installs the latest release. Homebrew installs use `brew upgrade racetozero/tap/snowflake-cli`. Do not run it unless the user asks.
