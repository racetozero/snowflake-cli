#!/usr/bin/env python3
"""Differential parity checker: Python Snowflake CLI vs the Rust port.

Runs every case in scripts/parity/cases/*.toml (plus generated --help cases for
every command in scripts/parity/inventory.json) against the reference
(target/upstream/venv/bin/snow) and the Rust binary (target/release/snow by
default), each against a fresh fake Snowflake server, and compares stdout,
stderr, exit code, recorded SQL and files. Prints one status line per leaf
command and exits 1 unless every leaf is PASS or ALLOW.

Reference results are cached in target/parity/ref-cache (keyed by the case
content and the upstream commit) because the Python CLI takes about a second
per invocation. Pass --no-cache to ignore the cache.

Usage:
    python3 scripts/parity/check.py [--filter TEXT] [--jobs N] [--rust PATH]
        [--no-help] [--only-help] [--no-cache] [--verbose]
"""
from __future__ import annotations

import argparse
import concurrent.futures as futures
import difflib
import hashlib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
PARITY = REPO / "scripts/parity"
CASES = PARITY / "cases"
OUT = REPO / "target/parity"
CACHE = OUT / "ref-cache"
REF = REPO / "target/upstream/venv/bin/snow"
FAKE = PARITY / "fake_snowflake.py"
HELP_WIDTHS = (60, 80, 120)


def upstream_commit() -> str:
    m = re.search(r'^commit = "([0-9a-f]+)"', (REPO / "Cargo.toml").read_text(), re.M)
    return m.group(1) if m else "unknown"


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Server:
    def __init__(self, rules, log, stages=""):
        self.port = free_port()
        env = dict(os.environ, FAKE_SF_LOG=str(log), FAKE_SF_RULES=str(rules), FAKE_SF_STAGES=str(stages))
        self.proc = subprocess.Popen(
            [sys.executable, str(FAKE), str(self.port)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self.proc.stdout.readline()

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def write_files(root: Path, files: dict):
    for rel, content in (files or {}).items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(content, dict) and "mode" in content:
            p.write_text(content.get("text", ""))
            os.chmod(p, int(content["mode"], 8))
        else:
            p.write_text(content)


def snapshot(root: Path, hidden: bool = False) -> dict:
    out = {}
    if not root.exists():
        return out
    for p in sorted(root.rglob("*")):
        rel = p.relative_to(root).as_posix()
        if p.is_dir():
            continue
        if rel.startswith(".") and not hidden:
            continue
        try:
            out[rel] = p.read_text()
        except UnicodeDecodeError:
            out[rel] = "<binary %d bytes>" % p.stat().st_size
    return out


def run_one(binary: str, case: dict) -> dict:
    with tempfile.TemporaryDirectory(prefix="snowparity-") as tmp:
        tmp = Path(tmp).resolve()
        home = tmp / "home"
        cwd = tmp / "cwd"
        home.mkdir()
        cwd.mkdir()
        log = tmp / "requests.jsonl"
        rules = tmp / "rules.json"
        rules.write_text(json.dumps(case.get("rules", [])))
        stages = tmp / "stages"
        stages.mkdir()
        write_files(stages, case.get("stage_files"))
        use_stages = bool(case.get("stage_files") or case.get("stages"))
        server = Server(rules, log, stages if use_stages else "") if case.get("connection") or case.get("server") else None
        port = server.port if server else 0
        cfg = ('default_connection_name = "dev"\n' if case.get("connection") else "") + "[cli]\nignore_new_version_warning = true\n"
        if case.get("connection"):
            cfg += (
                "[connections.dev]\n"
                'account = "test_account"\nuser = "test_user"\npassword = "test_password"\n'
                f'host = "127.0.0.1"\nport = {port}\nprotocol = "http"\n'
                'database = "db"\nschema = "public"\nrole = "test_role"\nwarehouse = "xs"\n'
            )
        if case.get("config"):
            cfg += case["config"].replace("<PORT>", str(port)).replace("<CWD>", str(cwd)).replace("<HOME>", str(home)) + "\n"
        if not case.get("no_config"):
            (home / "config.toml").write_text(cfg)
            os.chmod(home / "config.toml", 0o600)
        write_files(home, case.get("home_files"))
        for rel, mode in (case.get("home_modes") or {}).items():
            os.chmod(home / rel, int(mode, 8))
        write_files(cwd, case.get("files"))
        path = os.environ.get("PATH", "")
        if "bins" in case:
            # Fake executables (docker, grype, claude, ...) on a hermetic PATH, so a
            # real agent or docker on this machine is never found by either side.
            bindir = tmp / "bin"
            bindir.mkdir()
            write_files(bindir, case["bins"])
            for b in bindir.iterdir():
                b.chmod(0o755)
            path = f"{bindir}:/usr/bin:/bin"
        env = {
            "PATH": path,
            "HOME": str(tmp / "userhome"),
            "SNOWFLAKE_HOME": str(home),
            "COLUMNS": str(case.get("columns", 100)),
            "LANG": "en_US.UTF-8",
            "TZ": "UTC",
        }
        (tmp / "userhome").mkdir()
        write_files(tmp / "userhome", case.get("user_files"))
        for k, v in (case.get("env") or {}).items():
            env[k] = str(v).replace("<PORT>", str(port)).replace("<HOME>", str(home)).replace("<CWD>", str(cwd))
        argv = [a.replace("<PORT>", str(port)).replace("<HOME>", str(home)).replace("<CWD>", str(cwd)) for a in case["argv"]]
        started = time.time()
        try:
            proc = subprocess.run(
                [binary, *argv],
                cwd=cwd,
                env=env,
                input=case.get("stdin", "").encode(),
                capture_output=True,
                timeout=case.get("timeout", 60),
            )
            stdout, stderr, code = proc.stdout.decode(errors="replace"), proc.stderr.decode(errors="replace"), proc.returncode
        except subprocess.TimeoutExpired:
            stdout, stderr, code = "", "<timeout>", -1
        elapsed = time.time() - started
        if server:
            server.stop()
        requests = []
        if log.exists():
            for line in log.read_text().splitlines():
                entry = json.loads(line)
                if entry.get("kind") in ("telemetry",):
                    continue
                requests.append(entry)

        def norm(s: str) -> str:
            s = s.replace(str(home), "<HOME>").replace(str(cwd), "<CWD>").replace(str(tmp), "<TMP>")
            if port:
                s = s.replace(str(port), "<PORT>")
            # Python's warnings.warn prefixes the source path and echoes the line.
            s = re.sub(r"(?ms)^\S+\.py:\d+: (\w*Warning): (.*?)\n  [^\n]+$", r"\1: \2", s)
            for pat, rep in case.get("normalize", []):
                s = re.sub(pat, rep, s)
            return s

        files = {}
        if "files" in case.get("compare", ["files"]):
            files = {k: norm(v) for k, v in snapshot(cwd).items()}
            for k, v in snapshot(home).items():
                if k.startswith("logs/") or (k == "config.toml" and not case.get("compare_config")):
                    continue
                if k == "last_cli_error.json":
                    v = re.sub(r'"(id|timestamp)": "[^"]*"', r'"\1": "<X>"', v)
                files["<HOME>/" + k] = norm(v)
            for k, v in snapshot(stages, hidden=True).items():
                files["<STAGE>/" + norm(k)] = norm(v)
            if case.get("user_files"):
                for k, v in snapshot(tmp / "userhome", hidden=True).items():
                    files["<USERHOME>/" + k] = norm(v)
        return {
            "stdout": sort_runs(norm(stdout), case.get("sort_lines")),
            "stderr": norm(stderr),
            "exit": code,
            "sql": (sorted if case.get("sort_sql") else list)(norm(json.dumps(r, sort_keys=True)) for r in requests),
            "files": files,
            "elapsed": elapsed,
        }


# Bump when the way cases are run changes (config layout, normalisation).
HARNESS_VERSION = 7


def case_key(case: dict) -> str:
    blob = str(HARNESS_VERSION) + json.dumps({k: v for k, v in case.items() if k not in ("covers", "name")}, sort_keys=True)
    return hashlib.sha256((upstream_commit() + blob).encode()).hexdigest()[:32]


def reference(case: dict, use_cache: bool) -> dict:
    key = case_key(case)
    path = CACHE / f"{key}.json"
    if use_cache and path.exists() and not case.get("no_cache"):
        return json.loads(path.read_text())
    res = run_one(str(REF), case)
    if res.get("stderr") == "<timeout>":
        return res  # never cache a timeout: it is load, not behaviour
    CACHE.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(res))
    return res


# Python tracebacks the connector logs (logger.error(..., exc_info=True)) name
# site-packages paths; the port logs the message line only.
_TRACEBACK = re.compile(r"(?m)^Traceback \(most recent call last\):\n(?:[ \t].*\n)*\S[^\n]*\n?")


def compare(case: dict, ref: dict, got: dict) -> list[str]:
    diffs = []
    for field in case.get("compare", ["stdout", "stderr", "exit", "sql", "files"]):
        a, b = ref.get(field), got.get(field)
        if field == "stderr" and isinstance(a, str):
            a = _TRACEBACK.sub("", a)
        if a == b:
            continue
        if isinstance(a, str) and isinstance(b, str):
            d = "".join(difflib.unified_diff(a.splitlines(True), b.splitlines(True), "python", "rust"))
            diffs.append(f"--- {field} differs\n{d}")
        else:
            diffs.append(
                f"--- {field} differs\n"
                + "".join(
                    difflib.unified_diff(
                        json.dumps(a, indent=1, sort_keys=True).splitlines(True),
                        json.dumps(b, indent=1, sort_keys=True).splitlines(True),
                        "python",
                        "rust",
                    )
                )
            )
    return diffs


def load_cases() -> list[dict]:
    cases = []
    for f in sorted(CASES.glob("*.toml")):
        data = tomllib.loads(f.read_text())
        for c in data.get("case", []):
            c["_file"] = f.name
            cases.append(c)
    return cases


def help_cases(inventory) -> list[dict]:
    out = []
    for node in inventory:
        path = node["path"].split()[1:]
        for w in HELP_WIDTHS:
            out.append(
                {
                    "name": f"help {node['path']} @{w}",
                    "covers": [],
                    "help_of": node["path"],
                    "argv": [*path, "--help"],
                    "columns": w,
                    "compare": ["stdout", "stderr", "exit"],
                }
            )
    out.append({"name": "structure", "covers": [], "argv": ["--structure"], "compare": ["stdout", "stderr", "exit"]})
    return out


def sort_runs(text: str, pattern):
    """Sort each contiguous run of lines matching `pattern` (output printed in
    completion order by parallel workers upstream)."""
    if not pattern:
        return text
    rx = re.compile(pattern)
    lines = text.split("\n")
    out, run = [], []
    for line in lines:
        if rx.search(line):
            run.append(line)
            continue
        out.extend(sorted(run))
        run = []
        out.append(line)
    out.extend(sorted(run))
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--filter")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    ap.add_argument("--rust", default=str(REPO / "target/release/snow"))
    ap.add_argument("--no-help", action="store_true")
    ap.add_argument("--only-help", action="store_true")
    ap.add_argument("--no-cache", action="store_true")
    ap.add_argument("--verbose", "-v", action="store_true")
    args = ap.parse_args()
    args.rust = str(Path(args.rust).resolve())

    if not REF.exists():
        print("reference missing: run scripts/upstream.sh", file=sys.stderr)
        return 2
    inventory = json.loads((PARITY / "inventory.json").read_text())
    leaves = [n["path"] for n in inventory if not n["group"]]
    allow = {}
    allow_file = PARITY / "allowlist.toml"
    if allow_file.exists():
        allow = tomllib.loads(allow_file.read_text()).get("allow", {})

    cases = [] if args.only_help else load_cases()
    if not args.no_help:
        cases += help_cases(inventory)
    if args.filter:
        pat = re.compile(args.filter)
        cases = [c for c in cases if pat.search(c["name"]) or any(pat.search(x) for x in c.get("covers", []))]

    if OUT.exists():
        for f in OUT.glob("*.diff"):
            f.unlink()
    OUT.mkdir(parents=True, exist_ok=True)

    def work(case):
        ref = reference(case, not args.no_cache)
        got = run_one(args.rust, case)
        return case, ref, got, compare(case, ref, got)

    results = []
    with futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for case, ref, got, diffs in pool.map(work, cases):
            results.append((case, diffs, ref, got))
            if diffs:
                safe = re.sub(r"[^A-Za-z0-9_.@-]+", "_", case["name"])[:120]
                (OUT / f"{safe}.diff").write_text("\n".join(diffs))
                if args.verbose:
                    print(f"FAIL case {case['name']}\n" + "\n".join(diffs)[:4000])

    help_fail = [c["name"] for c, d, _, _ in results if d and "help_of" in c]
    case_fail = [c["name"] for c, d, _, _ in results if d and "help_of" not in c]
    needs_conn = {n["path"] for n in inventory if any(p["name"] == "connection" for p in n["params"])}
    per_leaf = {leaf: [] for leaf in leaves}
    success = {leaf: False for leaf in leaves}
    for case, diffs, ref, _ in results:
        for leaf in case.get("covers", []):
            per_leaf.setdefault(leaf, []).append(not diffs)
            ok = ref.get("exit") == 0
            if leaf in needs_conn and not case.get("offline_success"):
                ok = ok and any('"query"' in r or '"rest"' in r for r in ref.get("sql", []))
            if ok and not diffs:
                success[leaf] = True
        if "help_of" in case and case["help_of"] in per_leaf:
            per_leaf[case["help_of"]].append(None if not diffs else False)

    counts = {"PASS": 0, "PARTIAL": 0, "FAIL": 0, "UNCOVERED": 0, "ALLOW": 0}
    lines = []
    for leaf in leaves:
        st = per_leaf.get(leaf, [])
        behavioural = [x for x in st if x is not None]
        if leaf in allow:
            status = "ALLOW"
        elif any(x is False for x in st):
            status = "FAIL"
        elif not behavioural:
            status = "UNCOVERED"
        elif not success.get(leaf):
            status = "PARTIAL"
        else:
            status = "PASS"
        counts[status] += 1
        if not args.filter or st:
            lines.append(f"{status:9} {leaf}" + (f"  ({allow[leaf]})" if status == "ALLOW" else ""))
    print("\n".join(lines))
    print()
    if help_fail:
        print(f"help/structure cases failing: {len(help_fail)}")
        for n in help_fail[:30]:
            print("  ", n)
    if case_fail:
        print(f"behavioural cases failing: {len(case_fail)}")
        for n in case_fail[:50]:
            print("  ", n)
    total_cases = len(results)
    print(
        f"cases: {total_cases - len(help_fail) - len(case_fail)}/{total_cases} pass | leaves: "
        + ", ".join(f"{k}={v}" for k, v in counts.items())
    )
    if args.filter:
        return 1 if (help_fail or case_fail) else 0
    return 0 if counts["FAIL"] == 0 and counts["UNCOVERED"] == 0 and counts["PARTIAL"] == 0 and not help_fail and not case_fail else 1


if __name__ == "__main__":
    sys.exit(main())
