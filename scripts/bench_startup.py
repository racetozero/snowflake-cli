"""Compare the Rust `snow` with the Python Snowflake CLI on the same workloads.

Both run the same commands against the local fake Snowflake server with the same
SNOWFLAKE_HOME, so the numbers measure the CLI, not the network or Snowflake.
Reports the median wall time and the median peak RSS of each workload.

    python3 scripts/bench_startup.py [--rust PATH] [--python PATH] [--samples N] [--json FILE]
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from pgo_common import (
    REPOSITORY_ROOT,
    FakeSnowflake,
    Workload,
    host_description,
    prepare_fixture,
)

WORKLOADS = (
    Workload('version', ('--version',), 'Snowflake CLI version'),
    Workload('help', ('--help',), 'Usage:'),
    Workload('connection_list', ('connection', 'list'), 'dev'),
    Workload('sql_query', ('sql', '-q', 'select * from orders'), 'order 1'),
    Workload('object_list_json', ('object', 'list', 'table', '--format', 'json'), 'ORDERS'),
    Workload('app_bundle', ('app', 'bundle'), 'Bundle generated'),
)


def run_once(binary: Path, w: Workload, cwd: Path, env: dict[str, str]) -> tuple[float, int]:
    """Wall seconds and peak RSS in bytes of one run."""
    started = time.perf_counter()
    proc = subprocess.Popen(
        [str(binary), *w.arguments], cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
    )
    out = proc.stdout.read() if proc.stdout else b''
    if hasattr(os, 'wait4'):
        _, status, usage = os.wait4(proc.pid, 0)
        proc.returncode = os.waitstatus_to_exitcode(status)
        rss = usage.ru_maxrss * (1 if sys.platform == 'darwin' else 1024)
    else:
        proc.wait()
        rss = 0
    elapsed = time.perf_counter() - started
    text = out.decode(errors='replace')
    if proc.returncode != 0 or w.expected_output not in text:
        raise RuntimeError(f'{binary.name} {w.name} failed ({proc.returncode}): {text[-400:]}')
    return elapsed, rss


def measure(binary: Path, w: Workload, cwd: Path, env: dict[str, str], samples: int, warmups: int) -> dict:
    for _ in range(warmups):
        run_once(binary, w, cwd, env)
    times, rss = [], []
    for _ in range(samples):
        t, r = run_once(binary, w, cwd, env)
        times.append(t)
        rss.append(r)
    return {'median_ms': statistics.median(times) * 1000, 'median_rss_mib': statistics.median(rss) / 2**20}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--rust', type=Path, default=REPOSITORY_ROOT / 'target' / 'release' / 'snow')
    parser.add_argument('--python', type=Path, default=REPOSITORY_ROOT / 'target' / 'upstream' / 'venv' / 'bin' / 'snow')
    parser.add_argument('--samples', type=int, default=20)
    parser.add_argument('--python-samples', type=int, default=5)
    parser.add_argument('--warmups', type=int, default=2)
    parser.add_argument('--json', type=Path)
    args = parser.parse_args()
    args.rust, args.python = args.rust.resolve(), args.python.absolute()
    for b in (args.rust, args.python):
        if not b.is_file():
            print(f'error: {b} not found (build with `cargo build --release`, fetch the reference with `just upstream`)', file=sys.stderr)
            return 1
    rows = []
    with tempfile.TemporaryDirectory(prefix='snow-bench-') as tmp, FakeSnowflake(Path(tmp)) as server:
        cwd, env = prepare_fixture(Path(tmp), server.port)
        for w in WORKLOADS:
            rust = measure(args.rust, w, cwd, env, args.samples, args.warmups)
            python = measure(args.python, w, cwd, env, args.python_samples, 1)
            rows.append({'name': w.name, 'command': 'snow ' + ' '.join(w.arguments), 'rust': rust, 'python': python})
    print(f'Host: {host_description()}')
    print('command\trust_ms\tpython_ms\tspeedup\trust_rss_mib\tpython_rss_mib')
    for r in rows:
        print(
            f"{r['command']}\t{r['rust']['median_ms']:.1f}\t{r['python']['median_ms']:.1f}\t"
            f"{r['python']['median_ms'] / r['rust']['median_ms']:.0f}x\t"
            f"{r['rust']['median_rss_mib']:.1f}\t{r['python']['median_rss_mib']:.1f}"
        )
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(
            json.dumps({'host': host_description(), 'platform': platform.platform(), 'workloads': rows}, indent=2) + '\n',
            encoding='utf-8',
        )
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
