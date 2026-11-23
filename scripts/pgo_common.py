"""Shared deterministic workloads for snow PGO training and measurement.

Every workload runs offline: commands that talk to Snowflake use the parity
checker's local fake server (`scripts/parity/fake_snowflake.py`).
"""

from __future__ import annotations

import gzip
import json
import os
import platform
import socket
import statistics
import subprocess
import sys
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path
from typing import Self

REPOSITORY_ROOT = Path(__file__).resolve().parent.parent
FAKE_SERVER = REPOSITORY_ROOT / 'scripts' / 'parity' / 'fake_snowflake.py'
SENSITIVE_ENV_MARKERS = ('API_KEY', 'CREDENTIAL', 'PASSWORD', 'SECRET', 'TOKEN', 'SNOWFLAKE')

# Canned responses for the SHOW/DESCRIBE queries of the workloads.
RULES = [
    {
        'match': '(?i)^show terse tables|^show tables',
        'columns': ['created_on', 'name', 'kind', 'database_name', 'schema_name'],
        'rows': [
            ['2024-01-01 00:00:00', 'ORDERS', 'TABLE', 'DB', 'PUBLIC'],
            ['2024-01-01 00:00:00', 'CUSTOMERS', 'TABLE', 'DB', 'PUBLIC'],
        ],
    },
    {
        'match': '(?i)^show stages',
        'columns': ['created_on', 'name', 'database_name', 'schema_name', 'url'],
        'rows': [['2024-01-01 00:00:00', 'ST', 'DB', 'PUBLIC', '']],
    },
    {
        'match': '(?i)^describe table',
        'columns': ['name', 'type', 'kind', 'null?'],
        'rows': [['ID', 'NUMBER(38,0)', 'COLUMN', 'N'], ['NAME', 'VARCHAR', 'COLUMN', 'Y']],
    },
    {
        'match': '(?i)^select .* from orders',
        'columns': ['ID', 'NAME', 'AMOUNT'],
        'rows': [[str(i), f'order {i}', f'{i * 3}.50'] for i in range(200)],
    },
]


@dataclass(frozen=True, slots=True)
class Workload:
    """One process invocation and its expected output."""

    name: str
    arguments: tuple[str, ...]
    expected_output: str
    repetitions: int = 1
    stdin: str | None = None


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class FakeSnowflake:
    """The parity checker's fake Snowflake server, on a loopback port."""

    def __init__(self, root: Path) -> None:
        self.port = _free_port()
        rules = root / 'rules.json'
        rules.write_text(json.dumps(RULES), encoding='utf-8')
        env = dict(
            os.environ,
            FAKE_SF_LOG=str(root / 'requests.jsonl'),
            FAKE_SF_RULES=str(rules),
            FAKE_SF_STAGES='',
        )
        self._proc = subprocess.Popen(
            [sys.executable, str(FAKE_SERVER), str(self.port)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )

    def __enter__(self) -> Self:
        assert self._proc.stdout is not None
        self._proc.stdout.readline()
        return self

    def __exit__(self, *exc_info: object) -> None:
        self._proc.terminate()
        try:
            self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self._proc.kill()


def prepare_fixture(root: Path, port: int) -> tuple[Path, dict[str, str]]:
    """Create an isolated SNOWFLAKE_HOME, a project, and the child environment."""

    home = root / 'snowflake-home'
    project = root / 'project'
    home.mkdir(parents=True, exist_ok=True)
    (project / 'app').mkdir(parents=True, exist_ok=True)
    config = home / 'config.toml'
    config.write_text(
        'default_connection_name = "dev"\n'
        '[cli]\nignore_new_version_warning = true\n'
        '[connections.dev]\n'
        'account = "test_account"\nuser = "test_user"\npassword = "test_password"\n'
        f'host = "127.0.0.1"\nport = {port}\nprotocol = "http"\n'
        'database = "db"\nschema = "public"\nrole = "test_role"\nwarehouse = "xs"\n'
        '[connections.other]\naccount = "other_account"\nuser = "u2"\n',
        encoding='utf-8',
    )
    if os.name != 'nt':
        os.chmod(config, 0o600)
    (project / 'snowflake.yml').write_text(
        'definition_version: 2\n'
        'entities:\n'
        '  pkg:\n'
        '    type: application package\n'
        '    identifier: my_pkg\n'
        '    artifacts:\n'
        '      - src: app/*\n'
        '        dest: ./\n'
        '        processors: [templates]\n',
        encoding='utf-8',
    )
    (project / 'app' / 'manifest.yml').write_text(
        'manifest_version: 1\nartifacts:\n  setup_script: setup.sql\n', encoding='utf-8'
    )
    (project / 'app' / 'setup.sql').write_text(
        'create application role app_public;\n-- <% ctx.entities.pkg.identifier %>\n',
        encoding='utf-8',
    )
    (project / 'queries.sql').write_text(
        'select 1;\nselect * from orders;\n', encoding='utf-8'
    )

    environment = {
        key: value
        for key, value in os.environ.items()
        if not any(marker in key.upper() for marker in SENSITIVE_ENV_MARKERS)
    }
    environment.update(
        {
            'SNOWFLAKE_HOME': str(home),
            'HOME': str(root / 'user'),
            'USERPROFILE': str(root / 'user'),
            'COLUMNS': '120',
            'NO_COLOR': '1',
            'SNOW_NO_UPDATE_NOTIFIER': '1',
            'NO_PROXY': '127.0.0.1,localhost',
            'no_proxy': '127.0.0.1,localhost',
        }
    )
    (root / 'user').mkdir(exist_ok=True)
    return project, environment


def training_workloads() -> tuple[Workload, ...]:
    return (
        Workload('version', ('--version',), 'Snowflake CLI version', repetitions=6),
        Workload('help', ('--help',), 'Usage:', repetitions=4),
        Workload('sql_help', ('sql', '--help'), 'Usage:', repetitions=3),
        Workload('object_list_help', ('object', 'list', '--help'), 'Usage:', repetitions=2),
        Workload('connection_list', ('connection', 'list'), 'dev', repetitions=4),
        Workload('connection_test', ('connection', 'test'), 'Status', repetitions=3),
        Workload('sql_table', ('sql', '-q', 'select * from orders'), 'order 1', repetitions=4),
        Workload(
            'sql_json',
            ('sql', '-q', 'select * from orders', '--format', 'json'),
            '"NAME"',
            repetitions=3,
        ),
        Workload('sql_file', ('sql', '-f', 'queries.sql'), 'order 2', repetitions=2),
        Workload('object_list', ('object', 'list', 'table'), 'ORDERS', repetitions=3),
        Workload('stage_list', ('stage', 'list', '--format', 'json'), 'ST', repetitions=2),
        Workload('app_bundle', ('app', 'bundle'), 'Bundle generated', repetitions=2),
        Workload('agent', ('object', 'list', 'table', '--agent'), '"ok":true', repetitions=3),
    )


def held_out_workloads() -> tuple[Workload, ...]:
    return (
        Workload('startup_help', ('--help',), 'Usage:'),
        Workload('group_help', ('stage', '--help'), 'Usage:'),
        Workload('connections_json', ('connection', 'list', '--format', 'json'), '"other"'),
        Workload(
            'describe_json',
            ('object', 'describe', 'table', 'orders', '--format', 'json'),
            'NUMBER',
        ),
        Workload(
            'sql_csv',
            ('sql', '-q', 'select 2; select * from orders', '--format', 'csv'),
            'order 199',
        ),
    )


def run_workload(
    binary: Path,
    workload: Workload,
    *,
    cwd: Path,
    environment: dict[str, str],
    profile_directory: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    child_environment = environment.copy()
    if profile_directory is not None:
        child_environment['LLVM_PROFILE_FILE'] = str(
            profile_directory / f'snow-{workload.name}-%m-%p.profraw'
        )
    completed = subprocess.run(
        [str(binary), *workload.arguments],
        cwd=cwd,
        env=child_environment,
        input=workload.stdin,
        text=True,
        capture_output=True,
        check=False,
        timeout=60,
    )
    output = completed.stdout + completed.stderr
    if completed.returncode != 0:
        raise RuntimeError(
            f'{workload.name} returned {completed.returncode}: {output.strip()}'
        )
    if workload.expected_output not in output:
        raise RuntimeError(
            f'{workload.name} did not contain {workload.expected_output!r}: '
            f'{output.strip()}'
        )
    return completed


def executable_size(path: Path) -> tuple[int, int]:
    data = path.read_bytes()
    return len(data), len(gzip.compress(data, compresslevel=9, mtime=0))


def host_description() -> str:
    return f'{platform.system()} {platform.machine()}'


def geometric_mean(values: Iterable[float]) -> float:
    values = tuple(values)
    if not values or any(value <= 0 for value in values):
        raise ValueError('geometric mean needs positive values')
    return statistics.geometric_mean(values)
