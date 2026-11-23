#!/usr/bin/env python3
"""List upstream user-facing message fragments that are missing from the Rust port.

Walks the upstream plugin sources, collects the static parts (>= MIN chars) of
string literals passed to raise/console/warning/step/message calls, and reports
the ones that occur nowhere in crates/. A hit is either a missing branch or a
wording difference; triage each one.

    python3 scripts/parity/string_audit.py [plugin ...]
"""
import ast
import re
import sys
from pathlib import Path

UP = Path("/tmp/upstream/snowflake-cli/src/snowflake/cli/_plugins")
CRATES = Path(__file__).resolve().parents[2] / "crates"
MIN = 24
CALLS = {"CliError", "ClickException", "UsageError", "BadParameter", "warning", "step", "message",
         "MessageResult", "SandboxExecutionError", "InvalidManifestError", "UploadError", "ValueError",
         "confirm", "prompt", "_warn", "fail", "MalformedExtensionFunctionError"}


def fragments(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        yield node.value
    elif isinstance(node, ast.JoinedStr):
        for v in node.values:
            if isinstance(v, ast.Constant):
                yield v.value
    elif isinstance(node, ast.BinOp):
        yield from fragments(node.left)
        yield from fragments(node.right)


def call_name(c):
    f = c.func
    return f.attr if isinstance(f, ast.Attribute) else getattr(f, "id", "")


def main():
    plugins = sys.argv[1:] or ["apps", "nativeapp", "dcm", "streamlit"]
    rust = "\n".join(p.read_text(errors="ignore") for p in CRATES.rglob("*.rs"))
    rust_flat = re.sub(r"\\\n\s*", "", rust)
    missing = 0
    for plugin in plugins:
        for py in sorted((UP / plugin).rglob("*.py")):
            tree = ast.parse(py.read_text())
            seen = set()
            for node in ast.walk(tree):
                if not isinstance(node, ast.Call) or call_name(node) not in CALLS:
                    continue
                for arg in node.args:
                    for frag in fragments(arg):
                        for piece in re.split(r"\n", frag):
                            piece = piece.strip()
                            if len(piece) < MIN or piece in seen:
                                continue
                            seen.add(piece)
                            if piece not in rust_flat and piece.replace('"', '\\"') not in rust_flat:
                                missing += 1
                                print(f"{py.relative_to(UP)}:{node.lineno}: {piece!r}")
    print(f"{missing} fragment(s) missing", file=sys.stderr)


if __name__ == "__main__":
    main()
