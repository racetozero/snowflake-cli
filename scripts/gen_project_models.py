#!/usr/bin/env python3
"""Emit the snowflake.yml pydantic models as JSON for the Rust port.

Run with the reference venv:
    SNOWFLAKE_HOME=/tmp/upstream/work/home target/upstream/venv/bin/python scripts/gen_project_models.py

Writes crates/snow-api/data/project_models.json: every model reachable from the
definition versions, with fields in pydantic order (name, alias, required,
default, type descriptor) and the names of the hand-ported validators. The
`wrap` validators UpdatableModel adds for template skipping are not listed;
the Rust engine applies that rule to every field.
"""
import dataclasses
import enum
import json
import types
import typing
from pathlib import Path

import pydantic
from pydantic_core import PydanticUndefined
from snowflake.cli.api.project.schemas.project_definition import get_version_map

MODELS: dict = {}


def tname(m):
    return m.__name__.replace("[", "<").replace("]", ">")


def desc(t):
    origin = typing.get_origin(t)
    args = typing.get_args(t)
    if isinstance(t, str):
        t = {"str": str, "int": int, "bool": bool, "float": float}.get(t, typing.Any)
    if t is type(None):
        return {"k": "none"}
    if t is typing.Any:
        return {"k": "any"}
    if origin is typing.Annotated:
        inner = desc(args[0])
        for meta in args[1:]:
            if isinstance(meta, pydantic.fields.FieldInfo) and meta.discriminator:
                inner["discriminator"] = meta.discriminator
            if type(meta).__name__ == "Discriminator":
                inner["discriminator"] = meta.discriminator
        return inner
    if origin in (typing.Union, types.UnionType):
        return {"k": "union", "of": [desc(a) for a in args]}
    if origin is typing.Literal:
        return {"k": "literal", "values": [a.value if isinstance(a, enum.Enum) else a for a in args]}
    if origin in (list, typing.List):
        return {"k": "list", "of": desc(args[0]) if args else {"k": "any"}}
    if origin in (set, frozenset):
        return {"k": "set", "of": desc(args[0]) if args else {"k": "any"}}
    if origin in (dict, typing.Dict):
        return {"k": "dict", "key": desc(args[0]) if args else {"k": "any"}, "of": desc(args[1]) if args else {"k": "any"}}
    if origin is tuple:
        return {"k": "list", "of": desc(args[0]) if args else {"k": "any"}}
    if isinstance(t, type) and issubclass(t, pydantic.BaseModel):
        walk(t)
        return {"k": "model", "name": tname(t)}
    if t is dict:
        return {"k": "dict", "key": {"k": "any"}, "of": {"k": "any"}}
    if dataclasses.is_dataclass(t):
        return {"k": "dataclass", "name": t.__name__, "fields": [[f.name, desc(f.type)] for f in dataclasses.fields(t)]}
    if isinstance(t, type) and issubclass(t, enum.Enum):
        return {"k": "enum", "name": t.__name__, "values": [m.value for m in t]}
    for py, k in ((bool, "bool"), (int, "int"), (float, "float"), (str, "str")):
        if t is py:
            return {"k": k}
    if isinstance(t, type) and issubclass(t, Path):
        return {"k": "path"}
    if isinstance(t, typing.TypeVar):
        return {"k": "any"}
    return {"k": "other", "repr": repr(t)}


def default_of(f):
    if f.default_factory is not None:
        try:
            return {"factory": json.loads(json.dumps(f.default_factory(), default=str))}
        except Exception as e:  # noqa: BLE001
            return {"factory_error": repr(e)}
    if f.default is PydanticUndefined:
        return None
    d = f.default
    if isinstance(d, enum.Enum):
        d = d.value
    try:
        return {"value": json.loads(json.dumps(d, default=str))}
    except Exception:  # noqa: BLE001
        return {"value": repr(d)}


def constraints_of(f):
    out = {}
    for meta in f.metadata:
        for attr in ("max_length", "min_length", "pattern", "ge", "le", "gt", "lt"):
            v = getattr(meta, attr, None)
            if v is not None:
                out[attr] = v
    return out


def walk(m):
    name = tname(m)
    if name in MODELS:
        return
    MODELS[name] = None
    deco = m.__pydantic_decorators__
    fields = []
    for fname, f in m.model_fields.items():
        fields.append(
            {
                "name": fname,
                "alias": f.alias,
                "required": f.is_required(),
                "default": default_of(f),
                "type": desc(f.annotation),
                "constraints": constraints_of(f),
                "discriminator_field": bool(
                    isinstance(f.json_schema_extra, dict) and f.json_schema_extra.get("is_discriminator_field")
                ),
                "validators": [
                    {"fn": k, "mode": v.info.mode}
                    for k, v in deco.field_validators.items()
                    if fname in v.info.fields and v.info.mode != "wrap"
                ],
            }
        )
    MODELS[name] = {
        "module": m.__module__,
        "title": m.model_config.get("title") or m.__name__,
        "extra": m.model_config.get("extra"),
        "fields": fields,
        "model_validators": [{"fn": k, "mode": v.info.mode} for k, v in deco.model_validators.items()],
    }


from snowflake.cli.api.project.schemas.project_definition import _ProjectDefinitionBase

walk(_ProjectDefinitionBase)
# app.yml (Snowflake App Runtime) is validated by the same engine.
from snowflake.cli._plugins.apps.app_yml import AppYmlDefinition

walk(AppYmlDefinition)
versions = {}
for v, m in get_version_map().items():
    walk(m)
    versions[v] = tname(m)

out = Path(__file__).resolve().parent.parent / "crates/snow-api/data/project_models.json"
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(json.dumps({"versions": versions, "models": MODELS}, indent=1) + "\n")
print(f"wrote {out} ({len(MODELS)} models)")
