//! TOML output in tomlkit's layout.

use toml_edit::DocumentMut;

pub fn dumps(doc: &DocumentMut) -> String {
    doc.to_string()
}
