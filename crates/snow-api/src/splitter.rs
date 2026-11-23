//! `snowflake.connector.util_text.split_statements` with the default `;`
//! delimiter, ported line by line so statement text matches byte for byte.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub text: String,
    /// The statement is a PUT/GET (`None`: empty or comment only).
    pub is_put_or_get: Option<bool>,
}

/// Every statement `split_statements` yields, including empty ones.
pub fn split_raw(sql: &str, remove_comments: bool) -> Vec<Statement> {
    let mut out = Vec::new();
    let mut in_quote = false;
    let mut ch_quote = '\0';
    let mut in_comment = false;
    let mut in_dd = false;
    let mut statement: Vec<(String, bool)> = Vec::new();
    for raw_line in sql.split_inclusive('\n') {
        let line: Vec<char> = raw_line.chars().collect();
        let n = line.len();
        let s = |a: usize, b: usize| -> String { line[a.min(n)..b.min(n)].iter().collect() };
        let starts = |at: usize, p: &str| -> bool {
            let pat: Vec<char> = p.chars().collect();
            line.get(at..at + pat.len()) == Some(&pat[..])
        };
        let mut col = 0usize;
        let mut col0 = 0usize;
        loop {
            if col >= n {
                if col0 < col {
                    if !in_comment && !in_quote && !in_dd {
                        statement.push((s(col0, col), true));
                        if statement.len() == 1 && statement[0].0.is_empty() {
                            statement.clear();
                        }
                        break;
                    } else if !in_comment && (in_quote || in_dd) {
                        statement.push((s(col0, col), true));
                    } else if !remove_comments {
                        statement.push((s(col0, col), false));
                    }
                }
                break;
            } else if in_comment {
                if starts(col, "*/") {
                    in_comment = false;
                    if !remove_comments {
                        statement.push((s(col0, col + 2), false));
                    }
                    col += 2;
                    col0 = col;
                } else {
                    col += 1;
                }
            } else if in_dd {
                if starts(col, "$$") {
                    in_dd = false;
                    statement.push((s(col0, col + 2), false));
                    col += 2;
                    col0 = col;
                } else {
                    col += 1;
                }
            } else if in_quote {
                if line[col] == '\\'
                    && col + 1 < n
                    && (line[col + 1] == ch_quote || line[col + 1] == '\\')
                {
                    col += 2;
                } else if line[col] == ch_quote {
                    if (col + 1 < n && line[col + 1] != ch_quote) || col + 1 == n {
                        in_quote = false;
                        statement.push((s(col0, col + 1), true));
                        col += 1;
                        col0 = col;
                    } else {
                        col += 2;
                    }
                } else {
                    col += 1;
                }
            } else {
                let c = line[col];
                let no_url = || !s(col0, col + 3).contains("://");
                if c == '\'' || c == '"' {
                    in_quote = true;
                    ch_quote = c;
                    col += 1;
                } else if c == ' ' || c == '\t' {
                    statement.push((s(col0, col + 1), true));
                    col += 1;
                    col0 = col;
                } else if (starts(col, "--") || starts(col, "//")) && no_url() {
                    statement.push((s(col0, col), true));
                    if !remove_comments {
                        statement.push((s(col, n), false));
                    } else {
                        statement.push(("\n".into(), true));
                    }
                    col = n + 1;
                    col0 = col;
                } else if starts(col, "/*") && no_url() {
                    if !remove_comments {
                        statement.push((s(col0, col + 2), false));
                    } else {
                        statement.push((s(col0, col), false));
                    }
                    col += 2;
                    col0 = col;
                    in_comment = true;
                } else if starts(col, "$$") {
                    statement.push((s(col0, col + 2), true));
                    col += 2;
                    col0 = col;
                    in_dd = true;
                } else if c == ';' {
                    statement.push((s(col0, col) + ";", true));
                    col += 1;
                    if col < n && line[col] == '>' {
                        col += 1;
                        if let Some(last) = statement.last_mut() {
                            last.0.push('>');
                        }
                    }
                    let rest = s(col, n);
                    let rest_trim = rest.trim_start();
                    if rest_trim.starts_with("--") || rest_trim.is_empty() {
                        if !remove_comments {
                            statement.push((rest, false));
                        }
                        col = n;
                    }
                    while col < n && (line[col] == ' ' || line[col] == '\t') {
                        col += 1;
                    }
                    out.push(concatenate(&statement));
                    col0 = col;
                    statement.clear();
                } else if col == 0 && c == '!' {
                    if !statement.is_empty() {
                        out.push(concatenate(&statement));
                        statement.clear();
                    }
                    let t = raw_line.trim();
                    let t = t.strip_suffix(';').unwrap_or(t).trim();
                    out.push(Statement {
                        text: t.to_string(),
                        is_put_or_get: Some(false),
                    });
                    break;
                } else {
                    col += 1;
                }
            }
        }
    }
    if !statement.is_empty() {
        out.push(concatenate(&statement));
    }
    out
}

fn concatenate(parts: &[(String, bool)]) -> Statement {
    let mut text = String::new();
    let mut put_get = None;
    for (t, is_statement) in parts {
        text.push_str(t);
        if put_get.is_none() && *is_statement && t.trim().chars().count() >= 3 {
            let head: String = t.chars().take(3).collect::<String>().to_uppercase();
            put_get = Some(head == "PUT" || head == "GET");
        }
    }
    Statement {
        text: text.trim().to_string(),
        is_put_or_get: put_get,
    }
}

/// The non-empty statements (`execute_stream`).
pub fn split_statements(sql: &str, remove_comments: bool) -> Vec<Statement> {
    split_raw(sql, remove_comments)
        .into_iter()
        .filter(|s| !s.text.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::split_statements;

    fn texts(s: &str, rc: bool) -> Vec<String> {
        split_statements(s, rc)
            .into_iter()
            .map(|s| s.text)
            .collect()
    }

    #[test]
    fn splits_like_the_connector() {
        assert_eq!(
            texts("select 1; select 2;", true),
            ["select 1;", "select 2;"]
        );
        assert_eq!(
            texts("select ';'; select 2", true),
            ["select ';';", "select 2"]
        );
        assert_eq!(
            texts("create procedure p() as $$ a; b $$; call p()", true),
            ["create procedure p() as $$ a; b $$;", "call p()"]
        );
        assert_eq!(texts("/* a; */ select 1", true), ["select 1"]);
        assert_eq!(
            texts("select 1 -- x\n; /* c */ select 2 /* d */;", true),
            ["select 1 \n;", "select 2 ;"]
        );
        assert_eq!(
            texts("-- hello\nselect 1; /* x */ select 2 -- t", false),
            ["-- hello\nselect 1;", "/* x */ select 2 -- t"]
        );
        assert_eq!(
            texts("select 1;> select 2;", true),
            ["select 1;>", "select 2;"]
        );
        assert_eq!(
            texts("select 1;\n!source a.sql;\nselect 2", true),
            ["select 1;", "!source a.sql", "select 2"]
        );
        assert_eq!(
            texts("    select 1;\n;\n  ;", true),
            ["select 1;", ";", ";"]
        );
    }
}
