//! Python-compatible formatting of scalar values.

/// `repr(float)`: shortest round-trip digits, scientific notation when the
/// exponent is < -4 or >= 16.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    // `{:e}` gives the shortest round-trip mantissa.
    let e = format!("{:e}", f);
    let (mant, exp) = e.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("int");
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    if (-4..16).contains(&exp) {
        let s = if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                format!("{}{}.0", digits, "0".repeat(int_len - digits.len()))
            } else {
                format!("{}.{}", &digits[..int_len], &digits[int_len..])
            }
        } else {
            format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits)
        };
        if neg { format!("-{s}") } else { s }
    } else {
        let m = if digits.len() == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        let sign = if exp < 0 { "-" } else { "+" };
        format!(
            "{}{}e{}{:02}",
            if neg { "-" } else { "" },
            m,
            sign,
            exp.abs()
        )
    }
}

/// Python `json.dumps` string escaping (`ensure_ascii=True`).
pub fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python `json.dumps(value, indent=indent)` for a serde_json value.
pub fn json_dumps(v: &serde_json::Value, indent: Option<usize>) -> String {
    let mut out = String::new();
    write_json(v, indent, 0, &mut out);
    out
}

pub fn write_json(v: &serde_json::Value, indent: Option<usize>, level: usize, out: &mut String) {
    use serde_json::Value as J;
    match v {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Number(n) => {
            if n.is_f64()
                && !n.to_string().contains(['e', 'E', '.'])
                && n.as_i64().is_none()
                && n.as_u64().is_none()
            {
                out.push_str(&float_repr(n.as_f64().unwrap_or(0.0)));
            } else if let Some(f) = n
                .as_f64()
                .filter(|_| n.to_string().contains(['.', 'e', 'E']))
            {
                out.push_str(&float_repr(f));
            } else {
                out.push_str(&n.to_string());
            }
        }
        J::String(s) => json_str(s, out),
        J::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if indent.is_none() {
                        out.push(' ');
                    }
                }
                if let Some(n) = indent {
                    out.push('\n');
                    out.push_str(&" ".repeat(n * (level + 1)));
                }
                write_json(item, indent, level + 1, out);
            }
            if let Some(n) = indent {
                out.push('\n');
                out.push_str(&" ".repeat(n * level));
            }
            out.push(']');
        }
        J::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if indent.is_none() {
                        out.push(' ');
                    }
                }
                if let Some(n) = indent {
                    out.push('\n');
                    out.push_str(&" ".repeat(n * (level + 1)));
                }
                json_str(k, out);
                out.push_str(": ");
                write_json(item, indent, level + 1, out);
            }
            if let Some(n) = indent {
                out.push('\n');
                out.push_str(&" ".repeat(n * level));
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::float_repr;

    #[test]
    fn float_repr_matches_python() {
        for (f, want) in [
            (1.5, "1.5"),
            (1.0, "1.0"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-2.5e-7, "-2.5e-07"),
            (123456789.123, "123456789.123"),
        ] {
            assert_eq!(float_repr(f), want, "{f}");
        }
    }
}
