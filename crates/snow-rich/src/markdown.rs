//! A port of `rich.markdown.Markdown.__rich_console__` that consumes the
//! flattened markdown-it token stream recorded by `scripts/gen_cli_spec.py`.

use crate::render::{Opts, R, render_lines};
use crate::segment::Segment;
use crate::style::{Style, theme};
use crate::text::{Justify, Text};

/// One flattened markdown-it token.
#[derive(Debug, Clone, Copy)]
pub struct MdTok {
    pub kind: MdKind,
    pub tag: &'static str,
    pub nesting: i8,
    pub content: &'static str,
    /// `href` for links, `start` for ordered lists, `info` for code blocks.
    pub extra: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdKind {
    Text,
    Inline,
    Softbreak,
    Hardbreak,
    CodeInline,
    HtmlInline,
    HtmlBlock,
    ParagraphOpen,
    ParagraphClose,
    BulletListOpen,
    BulletListClose,
    OrderedListOpen,
    OrderedListClose,
    ListItemOpen,
    ListItemClose,
    CodeBlock,
    Fence,
    StrongOpen,
    StrongClose,
    EmOpen,
    EmClose,
    StrikeOpen,
    StrikeClose,
    LinkOpen,
    LinkClose,
    HeadingOpen,
    HeadingClose,
    Hr,
    BlockquoteOpen,
    BlockquoteClose,
    Other,
}

enum Elem {
    Paragraph(Text),
    Heading(&'static str, Text),
    CodeBlock(Text),
    BlockQuote(Vec<R>),
    List {
        ordered: bool,
        start: usize,
        items: Vec<Vec<R>>,
    },
    ListItem(Vec<R>),
    Hr,
    Unknown,
}

impl Elem {
    fn is_text(&self) -> bool {
        matches!(
            self,
            Elem::Paragraph(_)
                | Elem::Heading(..)
                | Elem::CodeBlock(_)
                | Elem::BlockQuote(_)
                | Elem::ListItem(_)
        )
    }
    fn new_line(&self) -> bool {
        !matches!(self, Elem::Hr)
    }
    fn on_text(&mut self, s: &str, style: Style) {
        match self {
            Elem::Paragraph(t) | Elem::Heading(_, t) | Elem::CodeBlock(t) => t.append(s, style),
            _ => {}
        }
    }
    /// Absorb a closed child; returns it when the main loop must render it instead.
    fn on_child_close(&mut self, child: Elem) -> Option<Elem> {
        match self {
            Elem::List { items, .. } => {
                if let Elem::ListItem(els) = child {
                    items.push(els);
                }
                None
            }
            Elem::ListItem(els) | Elem::BlockQuote(els) => {
                if let Some(r) = child.into_renderable() {
                    els.push(r);
                }
                None
            }
            _ => Some(child),
        }
    }
    fn into_renderable(self) -> Option<R> {
        match self {
            Elem::Paragraph(mut t) => {
                t.justify = Some(Justify::Left);
                Some(R::Text(t))
            }
            Elem::Heading(tag, mut t) => {
                t.justify = Some(Justify::Center);
                if tag == "h2" {
                    Some(R::Group(vec![R::text(""), R::Text(t)]))
                } else {
                    Some(R::Text(t))
                }
            }
            Elem::CodeBlock(t) => Some(R::Syntax(t.plain.trim_end().to_string())),
            Elem::BlockQuote(els) => Some(R::Group(els)),
            Elem::List {
                ordered,
                start,
                items,
            } => Some(R::MdList {
                items,
                ordered,
                start,
            }),
            Elem::ListItem(els) => Some(R::Group(els)),
            Elem::Hr => Some(R::text("─".repeat(3))),
            Elem::Unknown => None,
        }
    }
}

struct Ctx {
    styles: Vec<Style>,
}

impl Ctx {
    fn current(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::NULL, |acc, s| acc.combine(*s))
    }
    fn enter(&mut self, s: Style) -> Style {
        self.styles.push(s);
        self.current()
    }
    fn leave(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }
}

fn create(tok: &MdTok, ctx: &mut Ctx, entering_or_self: bool) -> Elem {
    let enter = |ctx: &mut Ctx, name: &str| -> Style {
        if entering_or_self {
            ctx.enter(theme(name))
        } else {
            Style::NULL
        }
    };
    match tok.kind {
        MdKind::ParagraphOpen | MdKind::ParagraphClose => {
            enter(ctx, "markdown.paragraph");
            Elem::Paragraph(Text::default().with_justify(Justify::Left))
        }
        MdKind::HeadingOpen | MdKind::HeadingClose => {
            if entering_or_self {
                ctx.enter(theme(&format!("markdown.{}", tok.tag)));
            }
            Elem::Heading(tok.tag, Text::default())
        }
        MdKind::CodeBlock | MdKind::Fence => {
            enter(ctx, "markdown.code_block");
            Elem::CodeBlock(Text::default().with_justify(Justify::Left))
        }
        MdKind::BlockquoteOpen | MdKind::BlockquoteClose => {
            enter(ctx, "markdown.block_quote");
            Elem::BlockQuote(Vec::new())
        }
        MdKind::Hr => Elem::Hr,
        MdKind::BulletListOpen | MdKind::BulletListClose => Elem::List {
            ordered: false,
            start: 1,
            items: Vec::new(),
        },
        MdKind::OrderedListOpen | MdKind::OrderedListClose => Elem::List {
            ordered: true,
            start: tok.extra.parse().unwrap_or(1),
            items: Vec::new(),
        },
        MdKind::ListItemOpen | MdKind::ListItemClose => {
            enter(ctx, "markdown.item");
            Elem::ListItem(Vec::new())
        }
        _ => Elem::Unknown,
    }
}

/// Render a token stream at `o.max_width`.
pub fn render(toks: &[MdTok], style: Style, o: Opts) -> Vec<Segment> {
    let mut ctx = Ctx {
        styles: vec![style],
    };
    let mut stack: Vec<Elem> = Vec::new();
    let mut new_line = false;
    let mut out = Vec::new();
    let inline_tags = ["em", "strong", "code", "s"];
    for tok in toks {
        let entering = tok.nesting == 1;
        let exiting = tok.nesting == -1;
        let self_closing = tok.nesting == 0;
        match tok.kind {
            MdKind::Text => on_text(&mut stack, &ctx, tok.content),
            MdKind::Hardbreak => on_text(&mut stack, &ctx, "\n"),
            MdKind::Softbreak => on_text(&mut stack, &ctx, " "),
            MdKind::LinkOpen => {
                ctx.enter(theme("markdown.link_url"));
            }
            MdKind::LinkClose => ctx.leave(),
            _ if inline_tags.contains(&tok.tag)
                && tok.kind != MdKind::Fence
                && tok.kind != MdKind::CodeBlock =>
            {
                let name = format!("markdown.{}", tok.tag);
                if entering {
                    ctx.enter(theme(&name));
                } else if exiting {
                    ctx.leave();
                } else {
                    ctx.enter(theme(&name));
                    if !tok.content.is_empty() {
                        on_text(&mut stack, &ctx, tok.content);
                    }
                    ctx.leave();
                }
            }
            _ => {
                let created = create(tok, &mut ctx, entering || self_closing);
                if entering {
                    stack.push(created);
                    continue;
                }
                let element = if exiting {
                    match stack.pop() {
                        Some(e) => e,
                        None => continue,
                    }
                } else {
                    let mut e = created;
                    e.on_text(tok.content, ctx.current());
                    e
                };
                let was_text = element.is_text();
                let nl = element.new_line();
                let to_render = match stack.last_mut() {
                    None => Some(element),
                    Some(top) => top.on_child_close(element),
                };
                if let Some(el) = to_render {
                    if new_line && (exiting || tok.kind != MdKind::Inline) {
                        out.push(Segment::line());
                    }
                    if let Some(r) = el.into_renderable() {
                        out.extend(crate::render::render(&r, o));
                    }
                }
                // on_leave: text elements pop the style they pushed.
                if was_text {
                    ctx.leave();
                }
                new_line = nl;
            }
        }
    }
    out
}

fn on_text(stack: &mut [Elem], ctx: &Ctx, s: &str) {
    if let Some(top) = stack.last_mut() {
        top.on_text(s, ctx.current());
    }
}

/// `ListElement.__rich_console__` with `ListItem.render_bullet/render_number`.
pub fn render_list(items: &[Vec<R>], ordered: bool, start: usize, o: Opts) -> Vec<Segment> {
    let mut out = Vec::new();
    let last_number = start + items.len();
    for (i, item) in items.iter().enumerate() {
        let group = R::Group(item.clone());
        if ordered {
            let nw = last_number.to_string().len() + 2;
            let lines = render_lines(
                &group,
                o.width(o.max_width.saturating_sub(nw)),
                Style::NULL,
                true,
            );
            let numeral = format!("{:>w$} ", start + i, w = nw - 1);
            for (li, line) in lines.into_iter().enumerate() {
                out.push(Segment::new(
                    if li == 0 {
                        numeral.clone()
                    } else {
                        " ".repeat(nw)
                    },
                    theme("markdown.item.number"),
                ));
                out.extend(line);
                out.push(Segment::line());
            }
        } else {
            let lines = render_lines(
                &group,
                o.width(o.max_width.saturating_sub(3)),
                Style::NULL,
                true,
            );
            for (li, line) in lines.into_iter().enumerate() {
                out.push(Segment::new(
                    if li == 0 { " • " } else { "   " },
                    theme("markdown.item.bullet"),
                ));
                out.extend(line);
                out.push(Segment::line());
            }
        }
    }
    out
}
