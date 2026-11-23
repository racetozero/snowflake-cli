#!/usr/bin/env bash
# Recreate the pinned Python reference implementation of Snowflake CLI.
#
# The Rust port is verified differentially against upstream
# snowflakedb/snowflake-cli at the commit recorded in Cargo.toml
# ([workspace.metadata.snowflake-cli].commit). This script clones that commit
# into target/upstream/snowflake-cli and installs it into
# target/upstream/venv, so `target/upstream/venv/bin/snow` is the reference.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

commit="$(sed -n '/^\[workspace.metadata.snowflake-cli\]/,/^\[/{s/^commit = "\(.*\)"/\1/p;}' Cargo.toml)"
[[ -n "$commit" ]] || { echo "commit missing from Cargo.toml" >&2; exit 2; }

base="${SNOW_UPSTREAM_DIR:-$repo_root/target/upstream}"
mkdir -p "$base"
if [[ ! -d "$base/snowflake-cli/.git" ]]; then
  git clone --quiet https://github.com/snowflakedb/snowflake-cli "$base/snowflake-cli"
fi
git -C "$base/snowflake-cli" fetch --quiet origin "$commit" 2>/dev/null || true
git -C "$base/snowflake-cli" checkout --quiet "$commit"

if [[ ! -x "$base/venv/bin/snow" ]]; then
  uv venv --quiet --python 3.12 "$base/venv"
  VIRTUAL_ENV="$base/venv" uv pip install --quiet -e "$base/snowflake-cli"
fi
"$base/venv/bin/snow" --version
