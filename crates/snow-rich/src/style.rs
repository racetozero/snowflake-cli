//! Text styles: a compact subset of Rich's `Style` (attributes plus colours).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    /// One of the 16 standard ANSI colours (0-15).
    Ansi(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// Bit set of attributes that are explicitly on.
    pub on: u8,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

pub const BOLD: u8 = 1;
pub const DIM: u8 = 2;
pub const ITALIC: u8 = 4;
pub const UNDERLINE: u8 = 8;
pub const STRIKE: u8 = 16;
pub const REVERSE: u8 = 32;

impl Style {
    pub const NULL: Style = Style {
        on: 0,
        fg: None,
        bg: None,
    };

    pub fn is_null(&self) -> bool {
        *self == Style::NULL
    }

    /// `self + other` in Rich: `other` wins where it sets something.
    pub fn combine(self, other: Style) -> Style {
        Style {
            on: self.on | other.on,
            fg: other.fg.or(self.fg),
            bg: other.bg.or(self.bg),
        }
    }

    /// Parse a Rich style definition such as `"bold cyan"` or `"dim red"`.
    pub fn parse(def: &str) -> Style {
        let mut s = Style::NULL;
        let mut on_bg = false;
        for word in def.split_whitespace() {
            if word == "on" {
                on_bg = true;
                continue;
            }
            let attr = match word {
                "bold" | "b" => Some(BOLD),
                "dim" | "d" => Some(DIM),
                "italic" | "i" => Some(ITALIC),
                "underline" | "u" => Some(UNDERLINE),
                "strike" | "s" => Some(STRIKE),
                "reverse" | "r" => Some(REVERSE),
                "none" => Some(0),
                _ => None,
            };
            if let Some(a) = attr {
                s.on |= a;
                continue;
            }
            if let Some(c) = parse_color(word) {
                if on_bg {
                    s.bg = Some(c);
                } else {
                    s.fg = Some(c);
                }
            }
            on_bg = false;
        }
        s
    }

    /// The SGR parameters for this style, e.g. `"1;36"`.
    pub fn sgr(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (bit, code) in [
            (BOLD, "1"),
            (DIM, "2"),
            (ITALIC, "3"),
            (UNDERLINE, "4"),
            (REVERSE, "7"),
            (STRIKE, "9"),
        ] {
            if self.on & bit != 0 {
                parts.push(code.to_string());
            }
        }
        if let Some(c) = self.fg {
            match c {
                Color::Default => parts.push("39".into()),
                Color::Ansi(n) if n < 8 => parts.push(format!("{}", 30 + n)),
                Color::Ansi(n) => parts.push(format!("{}", 90 + n - 8)),
                Color::Rgb(r, g, b) => parts.push(format!("38;2;{r};{g};{b}")),
            }
        }
        if let Some(c) = self.bg {
            match c {
                Color::Default => parts.push("49".into()),
                Color::Ansi(n) if n < 8 => parts.push(format!("{}", 40 + n)),
                Color::Ansi(n) => parts.push(format!("{}", 100 + n - 8)),
                Color::Rgb(r, g, b) => parts.push(format!("48;2;{r};{g};{b}")),
            }
        }
        parts.join(";")
    }
}

fn parse_color(word: &str) -> Option<Color> {
    const NAMES: [&str; 8] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    if let Some(i) = NAMES.iter().position(|n| *n == word) {
        return Some(Color::Ansi(i as u8));
    }
    if let Some(rest) = word.strip_prefix("bright_")
        && let Some(i) = NAMES.iter().position(|n| *n == rest)
    {
        return Some(Color::Ansi(i as u8 + 8));
    }
    if word == "default" {
        return Some(Color::Default);
    }
    if let Some(hex) = word.strip_prefix('#')
        && hex.len() == 6
    {
        let p = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        return Some(Color::Rgb(p(0)?, p(2)?, p(4)?));
    }
    None
}

/// Named styles from Rich's default theme and Typer's help theme that the
/// snow CLI uses.
pub fn theme(name: &str) -> Style {
    let def = match name {
        "markdown.paragraph" | "markdown.item" | "none" | "" => "",
        "markdown.code" => "bold cyan on black",
        "markdown.code_block" => "cyan on black",
        "markdown.strong" => "bold",
        "markdown.em" => "italic",
        "markdown.s" => "strike",
        "markdown.link" => "bright_blue",
        "markdown.link_url" => "underline blue",
        "markdown.item.bullet" | "markdown.item.number" => "bold yellow",
        "markdown.block_quote" => "magenta",
        "markdown.hr" => "yellow",
        "markdown.h1" => "bold",
        "markdown.h2" => "bold underline",
        "markdown.h3" => "bold",
        "markdown.h4" => "bold dim",
        "markdown.h5" => "underline",
        "markdown.h6" => "italic",
        "option" => "bold cyan",
        "switch" => "bold green",
        "negative_option" => "bold magenta",
        "negative_switch" => "bold red",
        "metavar" => "bold yellow",
        "metavar_sep" => "dim",
        "usage" => "yellow",
        other => other,
    };
    Style::parse(def)
}
