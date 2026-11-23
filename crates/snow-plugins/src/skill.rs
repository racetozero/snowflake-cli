//! `snow skill`: print or install the agent skills (hidden; not part of the
//! upstream command tree, so help and `--structure` stay identical).
//!
//! The skills live in `.agents/skills/<name>/SKILL.md`; `crates/snow-plugins/skills/<name>.md`
//! holds the embedded copies and a test keeps them equal.

use crate::app::envelope;
use serde_json::json;
use std::io::Write;
use std::path::PathBuf;

pub const SKILLS: [(&str, &str); 1] = [("snow", include_str!("../skills/snow.md"))];

fn description(text: &str) -> &str {
    text.lines()
        .find_map(|l| l.strip_prefix("description: "))
        .unwrap_or_default()
}

/// Run `snow skill ARGS`; returns the exit code.
pub fn run(args: &[String], agent: bool) -> i32 {
    let mut list = false;
    let mut install: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" | "-l" => list = true,
            "--install" => {
                let dir = match it.peek() {
                    Some(n) if !n.starts_with('-') => it.next().cloned().unwrap_or_default(),
                    _ => ".claude/skills".to_string(),
                };
                install = Some(PathBuf::from(dir));
            }
            "--help" | "-h" => {
                let text = "Usage: snow skill [NAME] [--list] [--install [DIR]]\n\nPrint an agent skill (default: snow), list them, or install them into DIR/<name>/SKILL.md (default .claude/skills).\n";
                if agent {
                    println!("{}", envelope("skill", Ok(json!({"help": text}))));
                } else {
                    print!("{text}");
                }
                return 0;
            }
            other => name = Some(other.to_string()),
        }
    }
    let fail = |msg: String| {
        if agent {
            println!(
                "{}",
                envelope("skill", Err(json!({"code": "invalid", "message": msg})))
            );
        } else {
            eprintln!("error: {msg}");
        }
        3
    };
    if list {
        let rows: Vec<_> = SKILLS
            .iter()
            .map(|(n, t)| json!({"name": n, "description": description(t)}))
            .collect();
        if agent {
            println!("{}", envelope("skill", Ok(json!(rows))));
        } else {
            for (n, t) in SKILLS {
                println!("{n:<10} {}", description(t));
            }
        }
        return 0;
    }
    if let Some(dir) = install {
        let mut written = Vec::new();
        for (n, t) in SKILLS {
            let folder = dir.join(n);
            if let Err(e) = std::fs::create_dir_all(&folder) {
                return fail(format!("cannot create {}: {e}", folder.display()));
            }
            let path = folder.join("SKILL.md");
            if let Err(e) = std::fs::write(&path, t) {
                return fail(format!("cannot write {}: {e}", path.display()));
            }
            written.push(path.display().to_string());
        }
        if agent {
            println!("{}", envelope("skill", Ok(json!({"installed": written}))));
        } else {
            println!(
                "Installed {} skill{} in {}",
                SKILLS.len(),
                if SKILLS.len() == 1 { "" } else { "s" },
                dir.display()
            );
        }
        return 0;
    }
    let name = name.unwrap_or_else(|| "snow".into());
    let Some((_, text)) = SKILLS.iter().find(|(n, _)| *n == name) else {
        let names: Vec<&str> = SKILLS.iter().map(|(n, _)| *n).collect();
        return fail(format!(
            "there is no skill named `{name}`; use one of: {}",
            names.join(", ")
        ));
    };
    if agent {
        println!(
            "{}",
            envelope("skill", Ok(json!({"name": name, "skill": text})))
        );
    } else {
        let mut out = std::io::stdout().lock();
        let _ = out
            .write_all(text.trim_end().as_bytes())
            .and_then(|()| out.write_all(b"\n"));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded copies equal `.agents/skills/<name>/SKILL.md`.
    #[test]
    fn embedded_skills_match_agents_skills() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        let mut on_disk: Vec<String> = std::fs::read_dir(format!("{root}/.agents/skills"))
            .expect(".agents/skills")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        on_disk.sort();
        let embedded: Vec<&str> = SKILLS.iter().map(|(n, _)| *n).collect();
        assert_eq!(on_disk, embedded);
        for (name, text) in SKILLS {
            let source = std::fs::read_to_string(format!("{root}/.agents/skills/{name}/SKILL.md"))
                .expect("SKILL.md");
            assert_eq!(
                source, text,
                "{name}: copy .agents/skills/{name}/SKILL.md to crates/snow-plugins/skills/{name}.md"
            );
            assert!(text.starts_with(&format!("---\nname: {name}\n")));
            assert!(!description(text).is_empty() && !description(text).contains(": "));
        }
    }
}
