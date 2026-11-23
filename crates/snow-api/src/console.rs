//! Intermediate output (`snowflake.cli.api.console.cli_console`).

use crate::markup;
use crate::sanitize;
use snow_rich::{Style, Text};
use std::io::Write;

const INDENT: usize = 2;

#[derive(Debug, Default)]
pub struct Console {
    pub silent: bool,
    extra_indent: usize,
    /// In agent mode messages are collected instead of printed.
    pub collected: Option<Vec<String>>,
    pub color: bool,
}

#[derive(Clone, Copy)]
enum Kind {
    Phase,
    Step,
    Info,
    Important,
    Plain,
}

impl Console {
    fn format(&self, message: &str, kind: Kind) -> Text {
        let mut text = match kind {
            Kind::Info => markup::render(message),
            Kind::Phase => Text::styled(message.to_string(), Style::parse("bold")),
            Kind::Step => Text::styled(message.to_string(), Style::parse("italic")),
            Kind::Important => Text::styled(message.to_string(), Style::parse("bold italic")),
            Kind::Plain => Text::new(message.to_string()),
        };
        text.pad_left(self.extra_indent * INDENT);
        text
    }

    fn emit(&mut self, text: Text) {
        if self.silent {
            return;
        }
        if let Some(c) = self.collected.as_mut() {
            c.push(text.plain.trim_start().to_string());
            return;
        }
        let segs = text.render("\n");
        let s = snow_rich::segments_to_string(&segs, self.color);
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }

    pub fn step(&mut self, message: &str) {
        let t = self.format(message, Kind::Step);
        self.emit(t);
    }
    /// Parses Rich markup.
    pub fn message(&mut self, message: &str) {
        let t = self.format(message, Kind::Info);
        self.emit(t);
    }
    pub fn plain_message(&mut self, message: &str) {
        let t = self.format(&sanitize::for_terminal(message), Kind::Plain);
        self.emit(t);
    }
    pub fn warning(&mut self, message: &str) {
        let t = self.format(message, Kind::Important);
        self.emit(t);
    }
    pub fn stderr_warning(&mut self, message: &str) {
        if self.silent {
            return;
        }
        let t = self.format(message, Kind::Important);
        if let Some(c) = self.collected.as_mut() {
            c.push(t.plain.trim_start().to_string());
            return;
        }
        let s = snow_rich::segments_to_string(&t.render("\n"), snow_click::term::color(true));
        let _ = std::io::stderr().write_all(s.as_bytes());
    }
    /// Message in a panel at console width.
    pub fn panel(&mut self, message: &str) {
        if self.silent {
            return;
        }
        if let Some(c) = self.collected.as_mut() {
            c.push(message.to_string());
            return;
        }
        let r = snow_rich::R::Panel {
            inner: Box::new(snow_rich::R::Text(markup::render(message))),
            title: None,
            border_style: Style::NULL,
        };
        let s = snow_rich::to_string(&r, snow_click::term::rich_width(), self.color);
        let _ = std::io::stdout().write_all(s.as_bytes());
    }
    /// Styled text without a trailing newline.
    pub fn styled_message(&mut self, message: &str, style: &str) {
        if self.silent {
            return;
        }
        if let Some(c) = self.collected.as_mut() {
            c.push(message.to_string());
            return;
        }
        let t = Text::styled(sanitize::for_terminal(message), Style::parse(style));
        let s = snow_rich::segments_to_string(&t.render(""), self.color);
        let _ = std::io::stdout().write_all(s.as_bytes());
    }
    pub fn phase_start(&mut self, enter: &str) {
        let t = self.format(enter, Kind::Phase);
        self.emit(t);
        self.extra_indent += 1;
    }
    pub fn phase_end(&mut self, exit: Option<&str>) {
        self.extra_indent = self.extra_indent.saturating_sub(1);
        if let Some(e) = exit {
            let t = self.format(e, Kind::Phase);
            self.emit(t);
        }
    }
    pub fn indent(&mut self) {
        self.extra_indent += 1;
    }
    pub fn dedent(&mut self) {
        self.extra_indent = self.extra_indent.saturating_sub(1);
    }
}

impl Console {
    /// End of a `cli_console.spinner()` block. Rich's transient progress
    /// leaves one empty line behind when stdout is not a terminal.
    pub fn spinner_done(&mut self) {
        use std::io::IsTerminal;
        if self.collected.is_some() || std::io::stdout().is_terminal() {
            return;
        }
        let _ = std::io::stdout().write_all(b"\n");
    }
}
