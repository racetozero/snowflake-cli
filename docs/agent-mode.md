# Agents

The Rust `snow` adds an agent mode to the Snowflake CLI command line, modelled
on `atl --agent`. It does not change anything else: without `--agent` every
command behaves exactly like the Python Snowflake CLI.

## `--agent`

Add `--agent` anywhere before `--` (or set `SNOW_AGENT=1`):

- stdout is exactly one JSON line, on success and on failure:

  ```json
  {"ok":true,"command":"object list","data":[{"name":"T1"}],"meta":{"messages":["..."]}}
  {"ok":false,"command":"object describe","error":{"code":"api","message":"002003 (42S02): ..."}}
  ```

- `data` is the command's result in the JSON shape `--format json` would print;
  `--help` puts the help text in `data.help`.
- Progress messages the command would print go to `meta.messages`.
- Nothing prompts, nothing opens a browser, the SQL REPL never starts. A value
  the command would ask for is an `input_required` error; a confirmation is a
  `confirmation_required` error (re-run with `--force` only when the user asked).
- Error codes and exit codes:

  | code | exit | meaning |
  | --- | --- | --- |
  | `api` | 1 | Snowflake returned an error (SQL, REST) |
  | `auth` | 2 | login failed or no connection is configured |
  | `invalid` | 3 | bad flags or input (Click usage errors) |
  | `confirmation_required`, `input_required` | 3 | the command would have prompted |
  | `unknown` | 4 | no such command |
  | `internal` | 5 | unexpected failure |
  | `exit` | any | the command ended with a non-zero exit code; `data` holds its output |

## Skills

One skill, `snow`, teaches agents the whole CLI. It lives in [`.agents/skills/snow`](../.agents/skills/snow/SKILL.md).

```sh
npx skills add racetozero/snowflake-cli   # or bunx
snow skill                                # print the skill
snow skill --install                      # write it to .claude/skills/snow/SKILL.md
snow skill --install .agents/skills       # or another directory
snow skill --list                         # name and description
```

`snow skill` and `snow update` are not part of the upstream command tree, so they
do not appear in `snow --help` or `snow --structure`.

## Updating

`snow update` installs the latest GitHub release over the running binary (it
runs that release's installer, which checks the archive's SHA-256). Homebrew
installs are updated with `brew upgrade racetozero/tap/snowflake-cli`;
`SNOW_SELF_UPDATE_COMMAND` (e.g. `cargo install snowflake-cli --force`) runs
another package manager instead. In a terminal, release builds print a one-line
notice when a newer release exists (checked at most every 20 hours; disable
with `SNOW_NO_UPDATE_NOTIFIER=1`; never in CI or agent mode).
