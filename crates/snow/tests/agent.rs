//! Agent mode: every invocation prints exactly one JSON line on stdout.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Server(Child, u16);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn server(rules: &str, dir: &Path) -> Server {
    let rules_path = dir.join("rules.json");
    std::fs::write(&rules_path, rules).unwrap();
    let mut child = Command::new("python3")
        .arg(repo().join("scripts/parity/fake_snowflake.py"))
        .arg("0")
        .env("FAKE_SF_RULES", &rules_path)
        .stdout(Stdio::piped())
        .spawn()
        .expect("python3 is required for agent tests");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port = line.trim().parse().unwrap();
    Server(child, port)
}

fn home(port: u16) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let cfg = format!(
        "default_connection_name = \"dev\"\n[cli]\nignore_new_version_warning = true\n[connections.dev]\naccount = \"a\"\nuser = \"u\"\npassword = \"p\"\nhost = \"127.0.0.1\"\nport = {port}\nprotocol = \"http\"\n"
    );
    let p = d.path().join("config.toml");
    std::fs::write(&p, cfg).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    d
}

fn run(home: &Path, args: &[&str]) -> (serde_json::Value, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_snow"))
        .args(args)
        .env("SNOWFLAKE_HOME", home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one stdout line, got: {stdout:?}");
    (
        serde_json::from_str(lines[0]).unwrap(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn agent_mode_envelopes() {
    let tmp = tempfile::tempdir().unwrap();
    let srv = server(
        r#"[{"match": "^show tables", "columns": ["name"], "rows": [["T1"]]},
            {"match": "bad", "error": {"code": "002003", "message": "SQL compilation error", "sqlState": "42S02"}}]"#,
        tmp.path(),
    );
    let h = home(srv.1);

    let (v, code) = run(h.path(), &["object", "list", "table", "--agent"]);
    assert_eq!(code, 0);
    assert_eq!(v["ok"], true);
    assert_eq!(v["command"], "object list");
    assert_eq!(v["data"][0]["name"], "T1");

    let (v, code) = run(h.path(), &["--agent", "object", "describe", "table", "bad"]);
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "api");

    let (v, code) = run(h.path(), &["object", "list", "--agent"]);
    assert_eq!(code, 3);
    assert_eq!(v["error"]["code"], "invalid");

    let (v, code) = run(h.path(), &["objec", "--agent"]);
    assert_eq!(code, 4);
    assert_eq!(v["error"]["code"], "unknown");

    let (v, code) = run(h.path(), &["object", "list", "--help", "--agent"]);
    assert_eq!(code, 0);
    assert!(
        v["data"]["help"]
            .as_str()
            .unwrap()
            .contains("Usage: snow object list")
    );

    let (v, code) = run(h.path(), &["connection", "add", "--agent"]);
    assert_eq!(code, 3);
    assert_eq!(v["error"]["code"], "input_required");

    // A command that prints a result and then exits with a code (no `--yes`).
    let (v, code) = run(h.path(), &["feedback", "-m", "hi", "-r", "3", "--agent"]);
    assert_eq!(code, 2);
    assert_eq!(v["ok"], false);
    assert_eq!(v["command"], "feedback");
    assert_eq!(v["error"]["code"], "exit");
    assert!(
        v["data"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("--yes"))
    );
}
