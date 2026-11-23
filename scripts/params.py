#!/usr/bin/env python3
"""Print a command's own click parameters from the reference CLI.

    SNOWFLAKE_HOME=/tmp/upstream/work/home target/upstream/venv/bin/python scripts/params.py bundle history
"""
import sys

import click
import snowflake.cli._app.cli_app as cli_app
from snowflake.cli._app.__main__ import main

GLOBAL = set()


def fake(root):
    ctx = click.Context(root)
    cmd = root
    for part in sys.argv[1:]:
        cmd = cmd.get_command(ctx, part)
    conn = {p.name for p in root.get_command(ctx, "connection").get_command(ctx, "test").params}
    for p in cmd.params:
        if p.name in conn:
            continue
        kind = "flag" if getattr(p, "is_flag", False) else ("multi" if getattr(p, "multiple", False) else "")
        print(f"{p.name:28} {'/'.join(p.opts):40} {kind:5} default={p.default!r}")
    return {}


cli_app.build_command_catalog = fake
try:
    main(["--structure"])
except SystemExit:
    pass
