//! Python `textwrap` as Click's `wrap_text` uses it (usage lines).

fn is_letter(c: char) -> bool {
    c.is_alphabetic()
}

/// Split into chunks the way `TextWrapper.wordsep_re` does (whitespace runs,
/// words, and hyphenated word parts).
fn chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            let s = i;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            out.push(chars[s..i].iter().collect());
            continue;
        }
        let s = i;
        loop {
            if i >= chars.len() || chars[i].is_whitespace() {
                break;
            }
            if chars[i] == '-' && i > s {
                let two_before = i >= s + 2 && is_letter(chars[i - 1]) && is_letter(chars[i - 2]);
                let lt_dash_lt = i >= s + 3
                    && is_letter(chars[i - 1])
                    && chars[i - 2] == '-'
                    && is_letter(chars[i - 3]);
                let ahead = chars.get(i + 1).copied().is_some_and(is_letter)
                    && (chars.get(i + 2).copied().is_some_and(is_letter)
                        || (chars.get(i + 2) == Some(&'-')
                            && chars.get(i + 3).copied().is_some_and(is_letter)));
                if (two_before || lt_dash_lt) && ahead {
                    i += 1;
                    break;
                }
            }
            i += 1;
        }
        out.push(chars[s..i].iter().collect());
    }
    out
}

fn clen(s: &str) -> usize {
    s.chars().count()
}

pub fn wrap(
    text: &str,
    width: usize,
    initial_indent: &str,
    subsequent_indent: &str,
) -> Vec<String> {
    let text = expand_tabs(text);
    let mut ch = chunks(&text);
    ch.reverse();
    let mut lines: Vec<String> = Vec::new();
    while !ch.is_empty() {
        let mut cur: Vec<String> = Vec::new();
        let mut cur_len = 0usize;
        let indent = if lines.is_empty() {
            initial_indent
        } else {
            subsequent_indent
        };
        let w = width.saturating_sub(clen(indent));
        if ch.last().is_some_and(|c| c.trim().is_empty()) && !lines.is_empty() {
            ch.pop();
        }
        while let Some(last) = ch.last() {
            let l = clen(last);
            if cur_len + l <= w {
                cur_len += l;
                cur.push(ch.pop().unwrap_or_default());
            } else {
                break;
            }
        }
        if ch.last().is_some_and(|c| clen(c) > w) {
            let space_left = w.saturating_sub(cur_len).max(1);
            let last = ch.pop().unwrap_or_default();
            let cut: String = last.chars().take(space_left).collect();
            let rest: String = last.chars().skip(space_left).collect();
            cur.push(cut);
            ch.push(rest);
            if ch.last().is_some_and(|c| c.is_empty()) {
                ch.pop();
            }
        }
        if cur.last().is_some_and(|c| c.trim().is_empty()) {
            cur.pop();
        }
        if !cur.is_empty() {
            lines.push(format!("{indent}{}", cur.concat()));
        }
    }
    lines
}

fn expand_tabs(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut out = String::new();
    let mut col = 0;
    for c in s.chars() {
        match c {
            '\t' => {
                let n = 8 - col % 8;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            '\n' => {
                out.push(c);
                col = 0;
            }
            _ => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

/// `HelpFormatter.write_usage` at indent 0; returns the text without the final newline.
pub fn usage(prog: &str, args: &str, width: usize) -> String {
    let prefix = format!("Usage: {prog} ");
    let plen = clen(&prefix);
    if width >= plen + 20 {
        let indent = " ".repeat(plen);
        wrap(args, width, &prefix, &indent).join("\n")
    } else {
        let indent = " ".repeat("Usage: ".len() + 4);
        let mut s = prefix.clone();
        s.push('\n');
        s.push_str(&wrap(args, width, &indent, &indent).join("\n"));
        s
    }
}
