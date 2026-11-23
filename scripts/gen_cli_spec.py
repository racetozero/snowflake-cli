#!/usr/bin/env python3
"""Extract the Snowflake CLI command tree from the pinned upstream into Rust.

Run with the reference interpreter created by scripts/upstream.sh:

    target/upstream/venv/bin/python scripts/gen_cli_spec.py

It writes crates/snow-spec/src/generated.rs (static command and parameter
tables, Markdown token streams of every help string, command documentation
panels) and scripts/parity/inventory.json (the full command tree, including
hidden commands, used by the parity checker).

Only static data is extracted. Parsing, help layout and command behaviour are
implemented by hand in Rust.
"""
from __future__ import annotations

import inspect
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUT_RS = REPO / "crates/snow-spec/src/generated.rs"
OUT_INV = REPO / "scripts/parity/inventory.json"


# ---------------------------------------------------------------------------
# Child mode: dump the raw tree for the current SNOWFLAKE_HOME.
# ---------------------------------------------------------------------------


def child_dump(out_path: str) -> None:
    import click
    import snowflake.cli._app.cli_app as cli_app
    from snowflake.cli._app.__main__ import main

    def fake(cmd):
        data = {}

        def walk(c, path):
            data[" ".join(path)] = {
                "hidden": bool(getattr(c, "hidden", False)),
                "params": {p.name: bool(getattr(p, "hidden", False)) for p in c.params},
                "help": c.help,
            }
            if isinstance(c, click.Group):
                ctx = click.Context(c)
                for n in c.list_commands(ctx):
                    walk(c.get_command(ctx, n), path + [n])

        walk(cmd, ["snow"])
        Path(out_path).write_text(json.dumps(data))
        return {}

    cli_app.build_command_catalog = fake
    try:
        main(["--structure"])
    except SystemExit:
        pass


def flag_names() -> list[str]:
    from snowflake.cli.api.feature_flags import FeatureFlag

    return [f.value.name for f in FeatureFlag]


def run_child(flags: dict[str, bool]) -> dict:
    with tempfile.TemporaryDirectory() as home:
        cfg = Path(home) / "config.toml"
        body = "[cli]\nignore_new_version_warning = true\n"
        if flags:
            body += "[cli.features]\n" + "".join(
                f"{k.lower()} = {'true' if v else 'false'}\n" for k, v in flags.items()
            )
        cfg.write_text(body)
        os.chmod(cfg, 0o600)
        out = Path(home) / "dump.json"
        env = dict(os.environ, SNOWFLAKE_HOME=home)
        subprocess.run(
            [sys.executable, __file__, "--child", str(out)],
            env=env,
            check=True,
            stdout=subprocess.DEVNULL,
        )
        return json.loads(out.read_text())


# ---------------------------------------------------------------------------
# Parent mode.
# ---------------------------------------------------------------------------


def rust_str(s: str | None) -> str:
    if s is None:
        return "None"
    return "Some(" + rust_lit(s) + ")"


def rust_lit(s: str) -> str:
    if not isinstance(s, str):
        raise TypeError(f"not a string: {s!r}")
    out = ['"']
    for ch in s:
        o = ord(ch)
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\r":
            out.append("\\r")
        elif o < 0x20 or o == 0x7F:
            out.append("\\u{%x}" % o)
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def md_tokens(text: str):
    """Flattened markdown-it tokens exactly as rich.markdown.Markdown sees them."""
    from markdown_it import MarkdownIt
    from rich.emoji import Emoji

    parser = MarkdownIt().enable("strikethrough").enable("table")
    toks = []
    text = inspect.cleandoc(text)

    def flatten(tokens):
        for t in tokens:
            is_fence = t.type == "fence"
            is_image = t.tag == "img"
            if t.children and not (is_image or is_fence):
                yield from flatten(t.children)
            else:
                yield t

    for t in flatten(parser.parse(Emoji.replace(text))):
        extra = ""
        if t.type == "link_open":
            extra = str(t.attrs.get("href", ""))
        elif t.type == "ordered_list_open":
            extra = str(t.attrs.get("start", 1))
        elif t.type in ("fence", "code_block"):
            extra = t.info or ""
        toks.append((t.type, t.tag, t.nesting, t.content or "", extra))
    return toks


class Interner:
    def __init__(self):
        self.items = []
        self.index = {}

    def add(self, key, value):
        if key not in self.index:
            self.index[key] = len(self.items)
            self.items.append(value)
        return self.index[key]


MD_TABLE = Interner()


def md_ref(text: str | None) -> int:
    """Index of the token list for text, or u16::MAX for none."""
    if text is None:
        return 0xFFFF
    toks = md_tokens(text)
    return MD_TABLE.add(json.dumps(toks), toks)


def type_spec(p) -> str:
    import click
    from click.types import FuncParamType

    t = p.type
    cls = type(t).__name__
    if isinstance(t, click.Choice):
        choices = ", ".join(rust_lit(str(getattr(c, "value", c))) for c in t.choices)
        return f"Ty::Choice {{ choices: &[{choices}], case_sensitive: {str(t.case_sensitive).lower()} }}"
    if isinstance(t, click.types.IntRange):
        def b(v):
            return "None" if v is None else f"Some({int(v)})"
        return (
            f"Ty::IntRange {{ min: {b(t.min)}, max: {b(t.max)}, min_open: {str(t.min_open).lower()}, "
            f"max_open: {str(t.max_open).lower()}, clamp: {str(t.clamp).lower()} }}"
        )
    if isinstance(t, click.types.FloatRange) or isinstance(t, click.types.FloatParamType):
        return "Ty::Float"
    if isinstance(t, click.Tuple):
        inner = ", ".join(simple_type(x) for x in t.types)
        return f"Ty::Tuple(&[{inner}])"
    if isinstance(t, click.Path):
        return (
            f"Ty::Path {{ exists: {str(t.exists).lower()}, file_okay: {str(t.file_okay).lower()}, "
            f"dir_okay: {str(t.dir_okay).lower()}, readable: {str(t.readable).lower()}, "
            f"writable: {str(t.writable).lower()}, resolve_path: {str(t.resolve_path).lower()}, "
            f"allow_dash: {str(t.allow_dash).lower()} }}"
        )
    if isinstance(t, FuncParamType):
        return f"Ty::Custom({rust_lit(getattr(t.func, '__name__', 'func'))})"
    return simple_type(t)


def simple_type(t) -> str:
    import click

    if isinstance(t, click.types.BoolParamType):
        return "Ty::Bool"
    if isinstance(t, click.types.IntParamType):
        return "Ty::Int"
    if isinstance(t, click.types.FloatParamType):
        return "Ty::Float"
    if isinstance(t, click.types.StringParamType):
        return "Ty::Str"
    if isinstance(t, click.types.UUIDParameterType):
        return "Ty::Str"
    name = type(t).__name__
    return f"Ty::Custom({rust_lit(name)})"


def json_safe(value):
    from enum import Enum

    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, Path):
        return value.as_posix()
    if isinstance(value, (list, tuple)):
        return [json_safe(v) for v in value]
    if callable(value):
        return {"__callable__": getattr(value, "__name__", "callable")}
    return str(value)


def structure_default(p):
    """Default exactly as snow --structure prints it, or None when omitted."""
    from snowflake.cli._app.dev.commands_structure import _OMIT, _json_safe

    d = _json_safe(p.default)
    if d is _OMIT or d is None:
        return None
    return json.dumps(d, ensure_ascii=False)


def param_record(p, ctx, md_mode="markdown"):
    import click
    from typer.core import TyperArgument, TyperOption

    opts_long, opts_short, sec_long, sec_short = [], [], [], []
    for o in p.opts:
        (opts_long if "--" in o else opts_short).append(o)
    for o in p.secondary_opts:
        (sec_long if "--" in o else sec_short).append(o)

    metavar_str = p.make_metavar()
    if isinstance(p, click.Argument) and p.name and metavar_str == p.name.upper():
        metavar_str = p.type.name.upper()
    metavar = "" if metavar_str == "BOOLEAN" else metavar_str
    if (
        isinstance(p.type, click.types._NumberRangeBase)
        and isinstance(p, click.Option)
        and not (p.count and p.type.min == 0 and p.type.max is None)
    ):
        r = p.type._describe_range()
        if r:
            metavar += f" [{r}]"

    envvar = getattr(p, "envvar", None)
    envvar_str = None
    if envvar is not None:
        envvar_str = envvar if isinstance(envvar, str) else ", ".join(str(d) for d in envvar)
    show_envvar_text = None
    if envvar and getattr(p, "show_envvar", None):
        show_envvar_text = f"[env var: {envvar_str}]"

    default_text = None
    if isinstance(p, (TyperOption, TyperArgument)):
        default_value = p._extract_default_help_str(ctx=ctx)
        show_default_is_str = isinstance(p.show_default, str)
        if show_default_is_str or (default_value is not None and (p.show_default or ctx.show_default)):
            s = p._get_default_string(ctx=ctx, show_default_is_str=show_default_is_str, default_value=default_value)
            if s:
                default_text = f"[default: {s}]"

    help_text = getattr(p, "help", None)
    help_md = None
    if help_text:
        paragraphs = help_text.split("\n\n")
        help_md = inspect.cleandoc("\n".join(paragraphs).strip())

    flag_value = None
    if isinstance(p, click.Option) and p.is_flag and not isinstance(p.flag_value, bool):
        flag_value = str(p.flag_value)

    default_json = json.dumps(json_safe(p.default), ensure_ascii=False)
    mutex, inner = [], None
    cb = p.callback
    seen = 0
    while cb is not None and seen < 6:
        seen += 1
        q = getattr(cb, "__qualname__", None)
        cells = {}
        if getattr(cb, "__closure__", None):
            cells = dict(zip(cb.__code__.co_freevars, (c.cell_contents for c in cb.__closure__)))
        if cells.get("mutually_exclusive"):
            mutex = list(cells["mutually_exclusive"])
        if "prop" in cells:
            inner = f"prop:{cells['prop']}"
            break
        nxt = cells.get("callback")
        if nxt is None or not callable(nxt):
            if q and "<lambda>" not in q and not q.startswith("get_callback.<locals>") and q != "_OverrideableParameter._callback_factory.<locals>.generated_callback":
                inner = q
            break
        cb = nxt
    return {
        "mutex": mutex,
        "inner_callback": inner,
        "name": p.name,
        "is_argument": isinstance(p, click.Argument),
        "opts": list(p.opts),
        "secondary": list(p.secondary_opts),
        "col_long": ",".join(opts_long),
        "col_short": ",".join(opts_short),
        "col_sec_long": ",".join(sec_long),
        "col_sec_short": ",".join(sec_short),
        "hidden": bool(getattr(p, "hidden", False)),
        "required": bool(p.required),
        "multiple": bool(p.multiple),
        "nargs": int(p.nargs),
        "is_flag": bool(getattr(p, "is_flag", False)),
        "is_bool_flag": bool(getattr(p, "is_bool_flag", False)),
        "count": bool(getattr(p, "count", False)),
        "flag_value": flag_value,
        "is_eager": bool(p.is_eager),
        "envvar": envvar_str,
        "type": type_spec(p),
        "metavar": metavar,
        "usage_metavar": p.make_metavar(),
        "envvar_text": show_envvar_text,
        "default_text": default_text,
        "default_json": default_json,
        "structure_default": structure_default(p),
        "help": help_text,
        "help_md": help_md,
        "panel": (lambda v: v if isinstance(v, str) else None)(getattr(p, "rich_help_panel", None)),
        "callback": (getattr(p.callback, "__qualname__", None) if p.callback else None),
    }


def docs_record(cmd):
    """Command documentation panels (usage notes, examples, related topics)."""
    from snowflake.cli.api.commands import command_docs as cd
    from snowflake.cli.api.sanitizers import sanitize_for_terminal

    docs = cd.get_command_docs(cmd)

    def spans(parts):
        out = []
        for part in parts:
            if isinstance(part, cd.Code):
                out.append(("code", sanitize_for_terminal(part.value) or ""))
            elif isinstance(part, cd.Ref):
                out.append(("plain", cd.REFERENCE_TEXT.get(part.name, f"%{part.name}%")))
            elif isinstance(part, cd.RelatedLink):
                out.append(("plain", sanitize_for_terminal(part.title) or sanitize_for_terminal(part.href) or ""))
            else:
                out.append(("plain", sanitize_for_terminal(part) or ""))
        return out

    def blocks(bs):
        out = []
        for b in bs:
            if isinstance(b, cd.Admonition):
                label = __import__(
                    "snowflake.cli.api.commands.command_docs_rendering", fromlist=["x"]
                ).ADMONITION_HELP_LABELS[b.admonition_type]
                out.append({"kind": "admonition", "label": label, "spans": spans(b.parts)})
            elif isinstance(b, cd.PlainText):
                out.append({"kind": "text", "spans": spans(b.parts)})
            elif isinstance(b, cd.BulletList):
                out.append({"kind": "bullets", "items": [spans(i.parts) for i in b.items]})
            elif isinstance(b, cd.Include):
                out.append({"kind": "group", "blocks": blocks(b.help_content or ())})
            else:
                raise TypeError(type(b))
        return out

    examples = []
    for e in docs.examples or ():
        examples.append(
            {
                "description": spans(e.description.parts) if e.description else None,
                "command": sanitize_for_terminal(e.command) if e.command else None,
                "output": sanitize_for_terminal(e.output) if e.output else None,
            }
        )
    related = [
        {"title": sanitize_for_terminal(r.title) or "", "href": sanitize_for_terminal(r.href) or ""}
        for r in (docs.related or ())
    ]
    return {"usage": blocks(docs.usage_notes or ()), "examples": examples, "related": related}


def parent_main() -> None:
    import click
    import snowflake.cli._app.cli_app as cli_app
    import typer.rich_utils as ru
    from snowflake.cli._app.__main__ import main

    # 1. Which hidden attributes depend on which feature flag.
    names = flag_names()
    cache = REPO / "target/gen_cli_spec_flags.json"
    flag_effects = {}  # (path, param or None) -> flag name that un-hides / hides
    if cache.exists():
        for k, v in json.loads(cache.read_text()):
            flag_effects[tuple(k)] = tuple(v)
        names = []
    else:
        base = run_child({})
    for n in names:
        d = run_child({n: True})
        for path, node in d.items():
            b = base.get(path)
            if b is None:
                flag_effects[(path, None)] = ("added", n)
                continue
            if node["hidden"] != b["hidden"]:
                flag_effects[(path, None)] = (node["hidden"], n)
            for pn, ph in node["params"].items():
                if b["params"].get(pn) != ph:
                    flag_effects[(path, pn)] = (ph, n)
        for path in base:
            if path not in d:
                flag_effects[(path, None)] = ("removed", n)

    if names:
        cache.parent.mkdir(parents=True, exist_ok=True)
        cache.write_text(json.dumps([[list(k), list(v)] for k, v in flag_effects.items()]))

    # 2. The full tree, in process, with the default configuration.
    captured = {}

    def fake(cmd):
        captured["root"] = cmd
        return {}

    cli_app.build_command_catalog = fake
    with tempfile.TemporaryDirectory() as home:
        cfg = Path(home) / "config.toml"
        cfg.write_text("[cli]\nignore_new_version_warning = true\n")
        os.chmod(cfg, 0o600)
        os.environ["SNOWFLAKE_HOME"] = home
        try:
            main(["--structure"])
        except SystemExit:
            pass
        root = captured["root"]

        commands = []
        params = Interner()
        inventory = []

        def walk(cmd, path, parent_ctx):
            ctx = click.Context(cmd, info_name=path[-1], parent=parent_ctx)
            for attr in ("help", "short_help", "epilog"):
                if not isinstance(getattr(cmd, attr, None), (str, type(None))):
                    setattr(cmd, attr, None)
            idx = len(commands)
            rec = {"path": " ".join(path), "name": path[-1]}
            commands.append(rec)
            help_text = inspect.cleandoc(cmd.help or "").partition("\f")[0]
            first, *rest = help_text.split("\n\n")
            rec["help"] = cmd.help
            rec["structure_help"] = (cmd.help or cmd.short_help or "").strip()
            rec["help_first_md"] = md_ref(first.strip()) if cmd.help else 0xFFFF
            rec["help_rest_md"] = md_ref("\n\n".join(rest)) if (cmd.help and rest) else 0xFFFF
            short = cmd.short_help or cmd.help or ""
            paras = inspect.cleandoc(short).split("\n\n")
            if not paras[0].startswith("\b"):
                paras[0] = paras[0].replace("\n", " ")
            rec["row_md"] = md_ref(paras[0].strip())
            rec["epilog_md"] = 0xFFFF
            if cmd.epilog:
                lines = cmd.epilog.split("\n\n")
                rec["epilog_md"] = md_ref("\n".join(x.replace("\n", " ").strip() for x in lines))
            rec["hidden"] = bool(cmd.hidden)
            rec["deprecated"] = bool(cmd.deprecated)
            rec["is_group"] = isinstance(cmd, click.Group)
            rec["no_args_is_help"] = bool(getattr(cmd, "no_args_is_help", False))
            rec["invoke_without_command"] = bool(getattr(cmd, "invoke_without_command", False))
            cs = cmd.context_settings or {}
            rec["allow_extra_args"] = bool(cs.get("allow_extra_args", cmd.allow_extra_args))
            rec["ignore_unknown_options"] = bool(cs.get("ignore_unknown_options", cmd.ignore_unknown_options))
            rec["allow_interspersed_args"] = bool(cs.get("allow_interspersed_args", cmd.allow_interspersed_args))
            rec["markup"] = getattr(cmd, "rich_markup_mode", None) is not None
            rec["usage_pieces"] = " ".join(cmd.collect_usage_pieces(ctx))
            panel = getattr(cmd, "rich_help_panel", None)
            rec["panel"] = panel if isinstance(panel, str) else None
            rec["has_help_option"] = cmd.get_help_option(ctx) is not None
            rec["agent_args"] = type(cmd).__name__ == "AgentArgsCommand"
            rec["smart_app_group"] = type(cmd).__name__ == "SmartAppGroup"
            rec["docs"] = docs_record(cmd) if not rec["is_group"] else {"usage": [], "examples": [], "related": []}
            eff = flag_effects.get((rec["path"], None))
            rec["hidden_flag"] = eff[1] if eff and eff[0] in (True, False) else None
            plist = []
            inv_params = []
            for p in cmd.params:
                pr = param_record(p, ctx)
                peff = flag_effects.get((rec["path"], p.name))
                pr["hidden_flag"] = peff[1] if peff else None
                pr["md"] = md_ref(pr["help_md"]) if pr["help_md"] else 0xFFFF
                key = json.dumps(pr, sort_keys=True)
                plist.append(params.add(key, pr))
                inv_params.append({k: pr[k] for k in ("name", "opts", "secondary", "hidden", "required", "is_flag", "multiple", "nargs")})
            rec["params"] = plist
            inventory.append(
                {
                    "path": rec["path"],
                    "hidden": rec["hidden"],
                    "hidden_flag": rec["hidden_flag"],
                    "group": rec["is_group"],
                    "params": inv_params,
                }
            )
            rec["children"] = []
            if rec["is_group"]:
                for n in cmd.list_commands(ctx):
                    child = cmd.get_command(ctx, n)
                    rec["children"].append(walk(child, path + [n], ctx))
            return idx

        walk(root, ["snow"], None)

        # The root help panels list options from a reference command.
        from snowflake.cli.api.commands.help_options import iter_shared_configuration_params
        from snowflake.cli.api.commands.flags import CONNECTION_CONFIGURATION_PANEL, GLOBAL_CONFIGURATION_PANEL

        ref_ctx = click.Context(root, info_name="snow")
        shared = {}
        for panel in (CONNECTION_CONFIGURATION_PANEL, GLOBAL_CONFIGURATION_PANEL):
            lst = []
            for p in iter_shared_configuration_params(panel):
                pr = param_record(p, ref_ctx)
                pr["hidden_flag"] = None
                pr["md"] = md_ref(pr["help_md"]) if pr["help_md"] else 0xFFFF
                lst.append(params.add(json.dumps(pr, sort_keys=True), pr))
            shared[panel] = lst

    write_rust(commands, params.items, shared)
    OUT_INV.parent.mkdir(parents=True, exist_ok=True)
    OUT_INV.write_text(json.dumps(inventory, indent=1) + "\n")
    print(f"commands={len(commands)} params={len(params.items)} md={len(MD_TABLE.items)}")


MD_KINDS = {
    "text": "Text",
    "inline": "Inline",
    "softbreak": "Softbreak",
    "hardbreak": "Hardbreak",
    "code_inline": "CodeInline",
    "html_inline": "HtmlInline",
    "html_block": "HtmlBlock",
    "paragraph_open": "ParagraphOpen",
    "paragraph_close": "ParagraphClose",
    "bullet_list_open": "BulletListOpen",
    "bullet_list_close": "BulletListClose",
    "ordered_list_open": "OrderedListOpen",
    "ordered_list_close": "OrderedListClose",
    "list_item_open": "ListItemOpen",
    "list_item_close": "ListItemClose",
    "code_block": "CodeBlock",
    "fence": "Fence",
    "strong_open": "StrongOpen",
    "strong_close": "StrongClose",
    "em_open": "EmOpen",
    "em_close": "EmClose",
    "s_open": "StrikeOpen",
    "s_close": "StrikeClose",
    "link_open": "LinkOpen",
    "link_close": "LinkClose",
    "heading_open": "HeadingOpen",
    "heading_close": "HeadingClose",
    "hr": "Hr",
    "blockquote_open": "BlockquoteOpen",
    "blockquote_close": "BlockquoteClose",
}


def spans_rs(spans):
    return "&[" + ", ".join(f"Span {{ code: {str(k == 'code').lower()}, text: {rust_lit(t)} }}" for k, t in spans) + "]"


def block_rs(b):
    if b["kind"] == "text":
        return f"DocBlock::Text({spans_rs(b['spans'])})"
    if b["kind"] == "admonition":
        return f"DocBlock::Admonition({rust_lit(b['label'])}, {spans_rs(b['spans'])})"
    if b["kind"] == "bullets":
        return "DocBlock::Bullets(&[" + ", ".join(spans_rs(i) for i in b["items"]) + "])"
    if b["kind"] == "group":
        return "DocBlock::Group(&[" + ", ".join(block_rs(x) for x in b["blocks"]) + "])"
    raise ValueError(b)


def write_rust(commands, params, shared):
    out = []
    w = out.append
    w("// @generated by scripts/gen_cli_spec.py from the pinned upstream Snowflake CLI.")
    w("// Do not edit by hand. Regenerate with:")
    w("//     target/upstream/venv/bin/python scripts/gen_cli_spec.py")
    w("#![allow(clippy::all)]")
    w("use snow_click::spec::*;\n")
    w("pub static MD: &[&[MdTok]] = &[")
    for toks in MD_TABLE.items:
        parts = []
        for (typ, tag, nesting, content, extra) in toks:
            kind = MD_KINDS.get(typ)
            if kind is None:
                kind = "Other"
            parts.append(
                f"MdTok {{ kind: MdKind::{kind}, tag: {rust_lit(tag)}, nesting: {nesting}, content: {rust_lit(content)}, extra: {rust_lit(extra)} }}"
            )
        w("    &[" + ", ".join(parts) + "],")
    w("];\n")
    w("pub static PARAMS: &[Param] = &[")
    for p in params:
        w(
            "    Param { "
            f"name: {rust_lit(p['name'])}, "
            f"is_argument: {str(p['is_argument']).lower()}, "
            f"opts: &[{', '.join(rust_lit(o) for o in p['opts'])}], "
            f"secondary: &[{', '.join(rust_lit(o) for o in p['secondary'])}], "
            f"col_long: {rust_lit(p['col_long'])}, col_short: {rust_lit(p['col_short'])}, "
            f"col_sec_long: {rust_lit(p['col_sec_long'])}, col_sec_short: {rust_lit(p['col_sec_short'])}, "
            f"hidden: {str(p['hidden']).lower()}, hidden_flag: {rust_str(p['hidden_flag'])}, "
            f"required: {str(p['required']).lower()}, multiple: {str(p['multiple']).lower()}, "
            f"nargs: {p['nargs']}, is_flag: {str(p['is_flag']).lower()}, is_bool_flag: {str(p['is_bool_flag']).lower()}, "
            f"count: {str(p['count']).lower()}, flag_value: {rust_str(p['flag_value'])}, "
            f"is_eager: {str(p['is_eager']).lower()}, envvar: {rust_str(p['envvar'])}, ty: {p['type']}, "
            f"metavar: {rust_lit(p['metavar'])}, usage_metavar: {rust_lit(p['usage_metavar'])}, "
            f"envvar_text: {rust_str(p['envvar_text'])}, default_text: {rust_str(p['default_text'])}, "
            f"default_json: {rust_lit(p['default_json'])}, structure_default: {rust_str(p['structure_default'])}, "
            f"help: {rust_str(p['help'])}, md: {p['md']}, panel: {rust_str(p['panel'])}, "
            f"callback: {rust_str(p['callback'])}, mutex: &[{', '.join(rust_lit(m) for m in p['mutex'])}], "
            f"inner_callback: {rust_str(p['inner_callback'])} }},"
        )
    w("];\n")
    w("pub static COMMANDS: &[Command] = &[")
    for c in commands:
        d = c["docs"]
        usage = "&[" + ", ".join(block_rs(b) for b in d["usage"]) + "]"
        ex = "&[" + ", ".join(
            "Example { "
            f"description: {('Some(' + spans_rs(e['description']) + ')') if e['description'] else 'None'}, "
            f"command: {rust_str(e['command'])}, output: {rust_str(e['output'])} }}"
            for e in d["examples"]
        ) + "]"
        rel = "&[" + ", ".join(f"Related {{ title: {rust_lit(r['title'])}, href: {rust_lit(r['href'])} }}" for r in d["related"]) + "]"
        w(
            "    Command { "
            f"path: {rust_lit(c['path'])}, name: {rust_lit(c['name'])}, "
            f"help: {rust_str(c['help'])}, structure_help: {rust_lit(c['structure_help'])}, "
            f"help_first_md: {c['help_first_md']}, help_rest_md: {c['help_rest_md']}, row_md: {c['row_md']}, epilog_md: {c['epilog_md']}, "
            f"hidden: {str(c['hidden']).lower()}, hidden_flag: {rust_str(c['hidden_flag'])}, deprecated: {str(c['deprecated']).lower()}, "
            f"is_group: {str(c['is_group']).lower()}, no_args_is_help: {str(c['no_args_is_help']).lower()}, "
            f"invoke_without_command: {str(c['invoke_without_command']).lower()}, "
            f"allow_extra_args: {str(c['allow_extra_args']).lower()}, ignore_unknown_options: {str(c['ignore_unknown_options']).lower()}, "
            f"allow_interspersed_args: {str(c['allow_interspersed_args']).lower()}, markup: {str(c['markup']).lower()}, "
            f"usage_pieces: {rust_lit(c['usage_pieces'])}, panel: {rust_str(c['panel'])}, has_help_option: {str(c['has_help_option']).lower()}, "
            f"agent_args: {str(c['agent_args']).lower()}, smart_app_group: {str(c['smart_app_group']).lower()}, "
            f"params: &[{', '.join(str(i) for i in c['params'])}], children: &[{', '.join(str(i) for i in c['children'])}], "
            f"usage_notes: {usage}, examples: {ex}, related: {rel} }},"
        )
    w("];\n")
    from snowflake.cli.api.commands.flags import CONNECTION_CONFIGURATION_PANEL, GLOBAL_CONFIGURATION_PANEL

    w(f"pub static SHARED_CONNECTION_PARAMS: &[u16] = &[{', '.join(map(str, shared[CONNECTION_CONFIGURATION_PANEL]))}];")
    w(f"pub static SHARED_GLOBAL_PARAMS: &[u16] = &[{', '.join(map(str, shared[GLOBAL_CONFIGURATION_PANEL]))}];")
    OUT_RS.parent.mkdir(parents=True, exist_ok=True)
    OUT_RS.write_text("\n".join(out) + "\n")


if __name__ == "__main__":
    if len(sys.argv) > 2 and sys.argv[1] == "--child":
        child_dump(sys.argv[2])
    else:
        parent_main()
