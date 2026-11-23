//! Typer's Rich help (`typer.rich_utils.rich_format_help`) with Snowflake
//! CLI's condensed help, shared-option panels and documentation panels.

use crate::error::ClickError;
use crate::parse::{Hooks, Invocation, Spec, command_hidden, param_is_hidden};
use crate::spec::{DocBlock, NO_MD, Span};
use crate::term;
use crate::textwrap;
use snow_rich::{Column, Overflow, R, Style, Table, Text, theme};

pub const CONNECTION_PANEL: &str = "Connection configuration";
pub const GLOBAL_PANEL: &str = "Global configuration";
const CONDENSED_FOOTER: &str =
    "Run `snow --help` for full descriptions, or use `--help-all` on this command.";

/// What the help renderer needs to know about the configuration.
pub struct HelpEnv {
    pub condensed: bool,
    pub docs_in_help: bool,
}

fn dim() -> Style {
    Style::parse("dim")
}

fn panel(title: &str, inner: R, border: Style) -> R {
    R::Panel {
        inner: Box::new(inner),
        title: Some(title.to_string()),
        border_style: border,
    }
}

/// Click's `ctx.get_usage()` for the level at `depth`.
pub fn usage(spec: &Spec, inv: &Invocation, depth: usize) -> String {
    let cmd = spec.cmd(inv.levels[depth].cmd);
    textwrap::usage(
        &inv.command_path_to(depth),
        cmd.usage_pieces,
        term::click_width(),
    )
}

fn param_help_cell(spec: &Spec, idx: u16) -> R {
    let p = spec.param(idx);
    let mut items = Vec::new();
    let md = spec.param_md(idx);
    if p.help.is_some_and(|h| !h.is_empty()) && !md.is_empty() {
        items.push(R::Markdown {
            toks: md,
            style: Style::NULL,
        });
    }
    if let Some(e) = p.envvar_text {
        items.push(R::Text(Text::styled(e, Style::parse("dim yellow"))));
    }
    if let Some(d) = p.default_text {
        items.push(R::Text(Text::styled(d, dim())));
    }
    if p.required {
        items.push(R::Text(Text::styled("[required]", Style::parse("dim red"))));
    }
    R::Columns(items)
}

/// `_print_options_panel`.
pub fn options_panel(spec: &Spec, title: &str, params: &[u16]) -> Option<R> {
    if params.is_empty() {
        return None;
    }
    let any_required = params.iter().any(|p| spec.param(*p).required);
    let mut table = Table::new((0, 1, 0, 1), false, true);
    for idx in params {
        let p = spec.param(*idx);
        let mut row: Vec<Option<R>> = Vec::new();
        if any_required {
            row.push(Some(R::Text(if p.required {
                Text::styled("*", Style::parse("red"))
            } else {
                Text::new("")
            })));
        }
        let opt = |s: &str, st: &str| {
            R::Text(Text::styled(
                s.to_string(),
                if s.is_empty() { Style::NULL } else { theme(st) },
            ))
        };
        row.push(Some(opt(p.col_long, "option")));
        row.push(Some(opt(p.col_short, "switch")));
        row.push(Some(opt(p.col_sec_long, "negative_option")));
        row.push(Some(opt(p.col_sec_short, "negative_switch")));
        row.push(Some(R::Text(
            Text::styled(p.metavar.to_string(), theme("metavar")).with_overflow(Overflow::Fold),
        )));
        row.push(Some(param_help_cell(spec, *idx)));
        table.add_row(row);
    }
    Some(panel(title, R::Table(table), dim()))
}

fn commands_panel(spec: &Spec, title: &str, cmds: &[u16], cmd_len: usize) -> Option<R> {
    if cmds.is_empty() {
        return None;
    }
    let mut table = Table::new((0, 1, 0, 1), false, true);
    table.add_column(Column {
        width: Some(cmd_len),
        no_wrap: true,
        style: Style::parse("bold cyan"),
        ..Column::default()
    });
    table.add_column(Column {
        ratio: Some(10),
        ..Column::default()
    });
    let any_deprecated = cmds.iter().any(|c| spec.cmd(*c).deprecated);
    for c in cmds {
        let cmd = spec.cmd(*c);
        let name = if cmd.deprecated {
            Text::styled(cmd.name, dim())
        } else {
            Text::new(cmd.name)
        };
        let mut row = vec![
            Some(R::Text(name)),
            Some(R::Markdown {
                toks: spec.md(cmd.row_md),
                style: Style::NULL,
            }),
        ];
        if any_deprecated {
            row.push(
                cmd.deprecated
                    .then(|| R::Text(Text::styled("(deprecated) ", Style::parse("red")))),
            );
        }
        table.add_row(row);
    }
    Some(panel(title, R::Table(table), dim()))
}

fn spans_text(spans: &[Span]) -> Text {
    let mut t = Text::default();
    for s in spans {
        t.append(
            s.text,
            if s.code {
                theme("markdown.code")
            } else {
                Style::NULL
            },
        );
    }
    t
}

fn bullets(rows: Vec<R>) -> R {
    let mut t = Table::grid((0, 1, 0, 0));
    t.add_column(Column {
        width: Some(1),
        no_wrap: true,
        ..Column::default()
    });
    t.add_column(Column::default());
    for r in rows {
        t.add_row(vec![Some(R::text("•")), Some(r)]);
    }
    R::Table(t)
}

fn usage_blocks(blocks: &[DocBlock]) -> R {
    let mut out = Vec::new();
    for b in blocks {
        let r = match b {
            DocBlock::Text(s) => R::Text(spans_text(s)),
            DocBlock::Bullets(items) => {
                bullets(items.iter().map(|i| R::Text(spans_text(i))).collect())
            }
            DocBlock::Admonition(label, s) => {
                let mut t = Text::default();
                t.append(&format!("{label}: "), Style::parse("bold"));
                t.append_text(&spans_text(s));
                R::Text(t)
            }
            DocBlock::Group(inner) => usage_blocks(inner),
        };
        if !out.is_empty() {
            out.push(R::text(""));
        }
        out.push(r);
    }
    R::Group(out)
}

fn absolute_url(href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else {
        format!(
            "https://docs.snowflake.com/en/{}",
            href.trim_start_matches('/')
        )
    }
}

fn docs_panels(spec: &Spec, cmd: u16) -> Vec<R> {
    let c = spec.cmd(cmd);
    let mut out = Vec::new();
    if !c.usage_notes.is_empty() {
        out.push(panel("Usage notes", usage_blocks(c.usage_notes), dim()));
    }
    if !c.examples.is_empty() {
        let mut lines = Vec::new();
        for (i, e) in c.examples.iter().enumerate() {
            if i > 0 {
                lines.push(R::text(""));
            }
            if let Some(d) = e.description {
                lines.push(R::Text(spans_text(d)));
            }
            if let Some(cmdline) = e.command {
                lines.push(R::Text(Text::styled(cmdline, Style::parse("cyan"))));
            }
            if let Some(o) = e.output.filter(|o| !o.is_empty()) {
                if o.contains('\n') {
                    lines.push(R::Text(Text::styled("Output:", dim())));
                    for l in o.lines() {
                        lines.push(R::Text(Text::styled(l, dim()).with_no_wrap(true)));
                    }
                } else {
                    lines.push(R::Text(Text::styled(format!("Output: {o}"), dim())));
                }
            }
        }
        out.push(panel("Examples", R::Group(lines), dim()));
    }
    if !c.related.is_empty() {
        let rows = c
            .related
            .iter()
            .map(|r| R::RelatedTopic {
                title: r.title.to_string(),
                url: absolute_url(r.href),
            })
            .collect();
        out.push(panel("Related topics", bullets(rows), dim()));
    }
    out
}

fn injected(spec: &Spec, idx: u16) -> bool {
    matches!(
        spec.param(idx).panel,
        Some(CONNECTION_PANEL) | Some(GLOBAL_PANEL)
    )
}

/// Group `items` into panels named by `panel_of` (else `default`), in first-appearance
/// order; the `default` panel always comes first.
fn group_by_panel(
    default: &'static str,
    items: &[u16],
    panel_of: impl Fn(u16) -> Option<&'static str>,
) -> Vec<(&'static str, Vec<u16>)> {
    let mut panels = vec![(default, Vec::new())];
    for &i in items {
        let name = panel_of(i).unwrap_or(default);
        match panels.iter_mut().find(|(n, _)| *n == name) {
            Some((_, v)) => v.push(i),
            None => panels.push((name, vec![i])),
        }
    }
    panels
}

pub fn visible_children(spec: &Spec, hooks: &dyn Hooks, cmd: u16) -> Vec<u16> {
    spec.cmd(cmd)
        .children
        .iter()
        .copied()
        .filter(|c| !command_hidden(spec, hooks, *c))
        .collect()
}

/// Render the help of the command at `depth` (what `--help` prints, including
/// the trailing newline from `typer.echo(ctx.get_help())`).
pub fn render_help(
    spec: &Spec,
    hooks: &dyn Hooks,
    env: &HelpEnv,
    inv: &Invocation,
    depth: usize,
) -> Vec<R> {
    let level = &inv.levels[depth];
    let cmd_idx = level.cmd;
    let cmd = spec.cmd(cmd_idx);
    let params = spec.cmd_params(cmd_idx);
    let has_panel = |name| params.iter().any(|p| spec.param(*p).panel == Some(name));
    let has_conn = has_panel(CONNECTION_PANEL);
    let has_glob = has_panel(GLOBAL_PANEL);
    let has_injected = has_conn || has_glob;
    let condensed = env.condensed && !level.show_all_help && has_injected && !cmd.agent_args;
    let mut out: Vec<R> = Vec::new();

    let mut usage_text = usage(spec, inv, depth);
    if condensed {
        let mut parts = Vec::new();
        if has_glob {
            parts.push("[global options]");
        }
        if has_conn {
            parts.push("[connection options]");
        }
        if !parts.is_empty() {
            usage_text = format!("{} {}", usage_text.trim_end(), parts.join(" "));
        }
    }
    out.push(R::Padding {
        inner: Box::new(R::Text(Text::new(usage_text))),
        pad: (1, 1, 1, 1),
        expand: true,
        style: Style::parse("bold"),
    });

    if cmd.help.is_some() {
        let mut items = Vec::new();
        if cmd.deprecated {
            items.push(R::Text(Text::styled("(deprecated) ", Style::parse("red"))));
        }
        items.push(R::Markdown {
            toks: spec.md(cmd.help_first_md),
            style: Style::NULL,
        });
        if cmd.help_rest_md != NO_MD {
            items.push(R::text(""));
            items.push(R::Markdown {
                toks: spec.md(cmd.help_rest_md),
                style: dim(),
            });
        }
        out.push(R::Padding {
            inner: Box::new(R::Align(Box::new(R::Group(items)))),
            pad: (0, 1, 1, 1),
            expand: true,
            style: Style::NULL,
        });
    }

    let (args, opts): (Vec<u16>, Vec<u16>) = params
        .iter()
        .filter(|p| !param_is_hidden(spec, hooks, **p) && !(condensed && injected(spec, **p)))
        .partition(|p| spec.param(**p).is_argument);
    let param_panel = |p: u16| spec.param(p).panel;
    let arg_panels = group_by_panel("Arguments", &args, param_panel);
    let opt_panels = group_by_panel("Options", &opts, param_panel);
    for (name, ps) in arg_panels.iter().chain(opt_panels.iter()) {
        if let Some(r) = options_panel(spec, name, ps) {
            out.push(r);
        }
    }

    if cmd.is_group {
        let children = visible_children(spec, hooks, cmd_idx);
        let panels = group_by_panel("Commands", &children, |c| spec.cmd(c).panel);
        let max_len = children
            .iter()
            .map(|c| spec.cmd(*c).name.chars().count())
            .max()
            .unwrap_or(0);
        for (name, cs) in &panels {
            if let Some(r) = commands_panel(spec, name, cs, max_len) {
                out.push(r);
            }
        }
    }

    if cmd.epilog_md != NO_MD {
        out.push(R::Padding {
            inner: Box::new(R::Align(Box::new(R::Markdown {
                toks: spec.md(cmd.epilog_md),
                style: Style::NULL,
            }))),
            pad: (1, 1, 1, 1),
            expand: true,
            style: Style::NULL,
        });
    }

    if condensed {
        let names = |list: &[u16]| {
            list.iter()
                .filter_map(|p| spec.param(*p).opts.iter().find(|o| o.starts_with("--")))
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut sections = Vec::new();
        if has_conn {
            let n = names(spec.shared_connection);
            if !n.is_empty() {
                sections.push(R::Text(Text::new(format!("Connection options: {n}"))));
            }
        }
        if has_glob {
            let n = names(spec.shared_global);
            if !n.is_empty() {
                sections.push(R::Text(Text::new(format!("Global options: {n}"))));
            }
        }
        if !sections.is_empty() {
            sections.push(R::text(""));
            sections.push(R::text(CONDENSED_FOOTER));
            let title = match (has_conn, has_glob) {
                (true, true) => "Global and connection options",
                (true, false) => "Connection options",
                (false, true) => "Global options",
                _ => "",
            };
            out.push(panel(title, R::Group(sections), dim()));
        }
    }

    if cmd.markup && env.docs_in_help && !cmd.is_group && !cmd.agent_args {
        out.extend(docs_panels(spec, cmd_idx));
    }
    // typer.echo(ctx.get_help()) prints the (empty) return value and a newline.
    out.push(R::text(""));
    out
}

/// The connection and global option panels printed after the root help.
pub fn shared_panels(spec: &Spec, env: &HelpEnv) -> Vec<R> {
    if !env.condensed {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(r) = options_panel(spec, CONNECTION_PANEL, spec.shared_connection) {
        out.push(r);
    }
    if let Some(r) = options_panel(spec, GLOBAL_PANEL, spec.shared_global) {
        out.push(r);
    }
    out
}

/// `typer.rich_utils.rich_format_error` for a Click error.
pub fn render_error(spec: &Spec, inv: &Invocation, err: &ClickError) -> Vec<R> {
    let mut out = Vec::new();
    if let ClickError::Usage { depth: Some(d), .. } = err {
        let d = (*d).min(inv.levels.len().saturating_sub(1));
        out.push(R::Text(Text::new(usage(spec, inv, d))));
        if spec.cmd(inv.levels[d].cmd).has_help_option {
            out.push(R::Text(Text::styled(
                format!("Try '{} --help' for help.", inv.command_path_to(d)),
                dim(),
            )));
        }
    }
    out.push(R::Panel {
        inner: Box::new(R::Text(Text::new(err.message()))),
        title: Some("Error".into()),
        border_style: Style::parse("red"),
    });
    out
}

/// Render a list of renderables at the Rich console width.
pub fn print_to_string(rs: &[R], stderr: bool) -> String {
    let width = term::rich_width();
    let color = term::color(stderr);
    let mut s = String::new();
    for r in rs {
        s.push_str(&snow_rich::to_string(r, width, color));
    }
    s
}
