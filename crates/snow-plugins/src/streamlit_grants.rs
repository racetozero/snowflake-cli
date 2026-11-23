//! Recording a share back into the `grants:` list of `snowflake.yml`
//! (`_plugins/streamlit/project_grants.py`).
//!
//! The file belongs to the user, so the edit inserts lines rather than
//! re-serializing the document (which would drop comments and quoting). The
//! rewritten text is re-parsed and compared against the original: anything
//! other than the expected `grants:` addition leaves the file untouched and
//! reports why, so the caller prints the entry for the user to paste.

use snow_api::project::PyV;
use std::path::Path;

/// A grant to a user (`share --to-user` records only those).
pub struct Grant {
    pub privilege: String,
    pub user: String,
    pub with_grant_option: bool,
}

fn load(text: &str) -> Result<PyV, String> {
    match snow_api::project::yaml::safe_load(text) {
        Ok(v) => Ok(v.unwrap_or(PyV::None)),
        Err(snow_api::project::yaml::LoadError::Syntax(m)) => Err(m),
        Err(snow_api::project::yaml::LoadError::Duplicate(k)) => Err(format!("duplicate key {k}")),
    }
}

fn entity_of<'a>(doc: &'a PyV, id: &str) -> Option<&'a indexmap::IndexMap<String, PyV>> {
    let PyV::Dict(d) = doc else { return None };
    match d.get("entities")? {
        PyV::Dict(e) => match e.get(id)? {
            PyV::Dict(m) => Some(m),
            _ => None,
        },
        _ => None,
    }
}

fn fold(v: Option<&PyV>) -> Option<String> {
    match v {
        Some(PyV::Str(s)) => Some(s.to_uppercase()),
        _ => None,
    }
}

fn already_recorded(entity: &indexmap::IndexMap<String, PyV>, g: &Grant) -> bool {
    let Some(PyV::List(recorded)) = entity.get("grants") else {
        return false;
    };
    let wanted = (
        Some(g.privilege.to_uppercase()),
        None,
        Some(g.user.to_uppercase()),
    );
    recorded.iter().any(|e| {
        let PyV::Dict(m) = e else { return false };
        let key = (
            fold(m.get("privilege")),
            fold(m.get("role")),
            fold(m.get("user")),
        );
        key == wanted
            && (!g.with_grant_option || m.get("with_grant_option").is_some_and(PyV::truthy))
    })
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_content(line: &str) -> bool {
    let s = line.trim();
    !s.is_empty() && !s.starts_with('#')
}

/// `_KEY_LINE`: the mapping key this line sets.
fn key_name(line: &str) -> Option<String> {
    let re = regex::Regex::new(r"^([^\s#:]+):(?:\s|$)").expect("re");
    re.captures(line.trim()).map(|c| c[1].to_string())
}

/// Whether a nested block follows this `key:` line.
fn opens_block(line: &str) -> bool {
    let t = line.trim();
    let rest = t.split_once(':').map(|(_, r)| r).unwrap_or("");
    for tok in rest.split_whitespace() {
        if tok.starts_with('#') {
            break;
        }
        if !(tok.starts_with('&') || tok.starts_with('!')) {
            return false;
        }
    }
    true
}

fn block_end(lines: &[&str], key_line: usize, key_indent: usize) -> usize {
    (key_line + 1..lines.len())
        .find(|&i| is_content(lines[i]) && indent_of(lines[i]) <= key_indent)
        .unwrap_or(lines.len())
}

fn last_content_line(lines: &[&str], span: std::ops::Range<usize>) -> isize {
    let mut last = span.start as isize - 1;
    for i in span {
        if is_content(lines[i]) {
            last = i as isize;
        }
    }
    last
}

fn entity_key_line(lines: &[&str], id: &str) -> Option<usize> {
    let entities = (0..lines.len()).find(|&i| {
        indent_of(lines[i]) == 0
            && key_name(lines[i]).as_deref() == Some("entities")
            && opens_block(lines[i])
    })?;
    let body = entities + 1..block_end(lines, entities, 0);
    let child = body
        .clone()
        .find(|&i| is_content(lines[i]))
        .map(|i| indent_of(lines[i]))?;
    body.into_iter()
        .find(|&i| indent_of(lines[i]) == child && key_name(lines[i]).as_deref() == Some(id))
}

fn render(g: &Grant, indent: usize) -> Vec<String> {
    let pad = " ".repeat(indent);
    let mut out = vec![
        format!("{pad}- privilege: {}\n", g.privilege),
        format!("{pad}  user: {}\n", g.user),
    ];
    if g.with_grant_option {
        out.push(format!("{pad}  with_grant_option: true\n"));
    }
    out
}

fn insert_grants(text: &str, id: &str, grants: &[Grant]) -> Option<String> {
    // Python `str.splitlines(keepends=True)` for `\n` / `\r\n` files.
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let start = entity_key_line(&lines, id)?;
    if !opens_block(lines[start]) {
        return None;
    }
    let key_indent = indent_of(lines[start]);
    let body = start + 1..block_end(&lines, start, key_indent);
    let field_indent = body
        .clone()
        .find(|&i| !lines[i].trim().is_empty())
        .map(|i| indent_of(lines[i]))
        .unwrap_or(key_indent + 2);
    if field_indent <= key_indent {
        return None;
    }
    let grants_key = body.clone().find(|&i| {
        key_name(lines[i]).as_deref() == Some("grants") && indent_of(lines[i]) == field_indent
    });
    let (insert_at, mut rendered, item_indent) = match grants_key {
        None => (
            (last_content_line(&lines, body) + 1) as usize,
            vec![format!("{}grants:\n", " ".repeat(field_indent))],
            field_indent + 2,
        ),
        Some(k) => {
            if !opens_block(lines[k]) {
                return None;
            }
            let items = k + 1..block_end(&lines, k, field_indent);
            let item_indent = items
                .clone()
                .find(|&i| !lines[i].trim().is_empty())
                .map(|i| indent_of(lines[i]))
                .unwrap_or(field_indent + 2);
            (
                (last_content_line(&lines, items) + 1) as usize,
                vec![],
                item_indent,
            )
        }
    };
    for g in grants {
        rendered.extend(render(g, item_indent));
    }
    // As upstream: a last line without a newline runs into the inserted text,
    // and the re-parse check below then rejects the edit.
    let mut out = lines[..insert_at].concat();
    out.push_str(&rendered.concat());
    out.push_str(&lines[insert_at..].concat());
    Some(out)
}

fn as_mapping(g: &Grant) -> PyV {
    let mut m = indexmap::IndexMap::new();
    m.insert("privilege".into(), PyV::Str(g.privilege.clone()));
    m.insert("user".into(), PyV::Str(g.user.clone()));
    if g.with_grant_option {
        m.insert("with_grant_option".into(), PyV::Bool(true));
    }
    PyV::Dict(m)
}

fn without_grants(doc: &PyV, id: &str) -> PyV {
    let mut d = doc.clone();
    if let PyV::Dict(top) = &mut d
        && let Some(PyV::Dict(ents)) = top.get_mut("entities")
        && let Some(PyV::Dict(e)) = ents.get_mut(id)
    {
        e.shift_remove("grants");
    }
    d
}

fn adds_only_the_grants(before: &PyV, edited: &str, id: &str, grants: &[Grant]) -> bool {
    let Ok(after) = load(edited) else {
        return false;
    };
    let Some(entity) = entity_of(&after, id) else {
        return false;
    };
    let mut expected: Vec<PyV> = match entity_of(before, id).and_then(|e| e.get("grants")) {
        Some(PyV::List(l)) => l.clone(),
        _ => vec![],
    };
    expected.extend(grants.iter().map(as_mapping));
    let got = match entity.get("grants") {
        Some(PyV::List(l)) => l.clone(),
        None | Some(PyV::None) => vec![],
        _ => return false,
    };
    got == expected && without_grants(before, id) == without_grants(&after, id)
}

/// `add_grants(project_file, entity_id, grants)`: `None` on success, else why not.
pub fn add_grants(path: &Path, id: &str, grants: Vec<Grant>) -> Option<String> {
    let shown = path.display();
    if !path.exists() {
        return Some(format!("{shown} does not exist"));
    }
    let original = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return Some(e.to_string()),
    };
    let before = match load(&original) {
        Ok(v) => v,
        Err(e) => return Some(format!("{shown} is not readable as YAML: {e}")),
    };
    let Some(entity) = entity_of(&before, id) else {
        return Some(format!("{shown} has no '{id}' entity under 'entities'"));
    };
    let missing: Vec<Grant> = grants
        .into_iter()
        .filter(|g| !already_recorded(entity, g))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let Some(edited) = insert_grants(&original, id, &missing) else {
        return Some(format!(
            "the '{id}' entity's 'grants' could not be edited in {shown}"
        ));
    };
    if !adds_only_the_grants(&before, &edited, id, &missing) {
        return Some(format!(
            "editing {shown} would have changed more than 'grants'"
        ));
    }
    if let Err(e) = std::fs::write(path, edited) {
        return Some(e.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(u: &str) -> Grant {
        Grant {
            privilege: "USAGE".into(),
            user: u.into(),
            with_grant_option: false,
        }
    }

    #[test]
    fn appends_to_existing_grants_keeping_comments() {
        let text = "definition_version: 2\nentities:\n  app:  # mine\n    type: streamlit\n    grants:\n      - privilege: USAGE\n        role: R1\n    main_file: a.py\n";
        let edited = insert_grants(text, "app", &[user("bob")]).unwrap();
        assert_eq!(
            edited,
            "definition_version: 2\nentities:\n  app:  # mine\n    type: streamlit\n    grants:\n      - privilege: USAGE\n        role: R1\n      - privilege: USAGE\n        user: bob\n    main_file: a.py\n"
        );
    }

    #[test]
    fn adds_a_grants_key() {
        let text = "entities:\n  app:\n    type: streamlit\n\n# tail\n";
        assert_eq!(
            insert_grants(text, "app", &[user("bob")]).unwrap(),
            "entities:\n  app:\n    type: streamlit\n    grants:\n      - privilege: USAGE\n        user: bob\n\n# tail\n"
        );
    }
}
