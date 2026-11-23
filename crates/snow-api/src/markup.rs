//! Rich console markup (`[bold]text[/bold]`) as used by `cli_console.message`.

use snow_rich::{Style, Text};

/// `rich.markup.render`: parse markup into styled text.
pub fn render(markup: &str) -> Text {
    let mut out = Text::default();
    let mut stack: Vec<(String, Style)> = Vec::new();
    let bytes: Vec<char> = markup.chars().collect();
    let mut i = 0;
    let mut plain = String::new();
    let current =
        |stack: &Vec<(String, Style)>| stack.iter().fold(Style::NULL, |a, (_, s)| a.combine(*s));
    while i < bytes.len() {
        if bytes[i] == '[' {
            // Count preceding backslashes already pushed to `plain`.
            let mut backslashes = 0;
            while plain.ends_with('\\') {
                plain.pop();
                backslashes += 1;
            }
            let close = bytes[i + 1..]
                .iter()
                .position(|c| *c == ']')
                .map(|p| p + i + 1);
            let tag_ok = close.is_some_and(|c| {
                let inner: String = bytes[i + 1..c].iter().collect();
                !inner.contains('[')
                    && inner
                        .chars()
                        .next()
                        .is_some_and(|f| f.is_ascii_lowercase() || f == '#' || f == '/' || f == '@')
            });
            for _ in 0..backslashes / 2 {
                plain.push('\\');
            }
            if !tag_ok || backslashes % 2 == 1 {
                if let (true, Some(c)) = (backslashes % 2 == 1 && tag_ok, close) {
                    plain.extend(&bytes[i..=c]);
                    i = c + 1;
                    continue;
                }
                plain.push('[');
                i += 1;
                continue;
            }
            let c = close.expect("checked");
            let tag: String = bytes[i + 1..c].iter().collect();
            out.append(&plain, current(&stack));
            plain.clear();
            if let Some(name) = tag.strip_prefix('/') {
                if name.is_empty() {
                    stack.pop();
                } else if let Some(pos) = stack.iter().rposition(|(n, _)| n == name) {
                    stack.remove(pos);
                }
            } else {
                let style = if tag.starts_with("link") {
                    Style::NULL
                } else {
                    Style::parse(&tag)
                };
                stack.push((tag, style));
            }
            i = c + 1;
            continue;
        }
        plain.push(bytes[i]);
        i += 1;
    }
    out.append(&plain, current(&stack));
    out
}
