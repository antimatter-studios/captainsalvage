//! Whether agents may see file names and file contents.
//!
//! A disk being recovered can hold private files. The person using the app decides whether an
//! agent working through the tools sees their names and previews. While they are kept back,
//! names and paths in every tool's answer are replaced by a mark that keeps only the extension,
//! tools that show a file's contents are refused, and SQL from `query_map` and scripts reads
//! the name columns as NULL. Counts, sizes, types, verdicts and cluster numbers stay visible, so
//! analysis still works.
//!
//! This guards against showing a name or a picture by accident. It is not a lock: a script can
//! still read a directory cluster's raw bytes and decode the names in it.

use anyhow::Result;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::Connection;
use serde_json::Value;
use std::fs;

const SETTING_FILE: &str = "agents-see-names";
/// What a hidden name becomes. The extension is kept, because a type is not private.
pub const HIDDEN: &str = "[hidden]";

/// Tools whose answer is a file's contents: a picture, text or the window itself.
pub const CONTENT_TOOLS: &[&str] = &["materialize", "zip_member_preview", "screenshot_app"];

/// Keys whose text is a file or folder name, a path, or text that quotes them.
const NAME_KEYS: &[&str] = &[
    "name",
    "oName",
    "path",
    "paths",
    "hint",
    "names",
    "target",
    "source",
    "rootEntries",
    "detail",
    "note",
    "label",
    "title",
    "folder",
    "destination",
    "manifest",
    "current",
];

/// Columns that hold names or text quoting them, by table.
const NAME_COLUMNS: &[(&str, &str)] = &[
    ("sets", "name"),
    ("sets", "o_name"),
    ("nodes", "name"),
    ("canvases", "name"),
    ("canvas_items", "name"),
    ("canvas_choices", "path"),
    ("activity", "detail"),
    ("overlays", "payload"),
    ("overlays", "note"),
    ("canvas_suggestions", "note"),
];

/// The person's last choice, as saved in the app's folder. Agents see names until told not to.
pub fn saved_names_hidden() -> bool {
    crate::workspace::app_support()
        .ok()
        .and_then(|folder| fs::read_to_string(folder.join(SETTING_FILE)).ok())
        .is_some_and(|text| text.trim() == "no")
}

/// Saves the person's choice in the app's folder.
pub fn save_names_hidden(hidden: bool) -> Result<()> {
    let folder = crate::workspace::app_support()?;
    fs::write(folder.join(SETTING_FILE), if hidden { "no" } else { "yes" })?;
    Ok(())
}

fn mask(text: &str) -> String {
    match text.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty()
                && !extension.is_empty()
                && extension.len() <= 8
                && !extension.contains('/') =>
        {
            format!("{HIDDEN}.{extension}")
        }
        _ => HIDDEN.to_string(),
    }
}

/// Replaces every name, path and quoting text in a tool's answer.
pub fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if NAME_KEYS.contains(&key.as_str()) {
                    hide(item);
                } else {
                    redact(item);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

fn hide(value: &mut Value) {
    match value {
        Value::String(text) if !text.is_empty() => *text = mask(text),
        Value::Array(items) => items.iter_mut().for_each(hide),
        Value::Object(_) => redact(value),
        _ => {}
    }
}

/// Makes a connection read the name columns as NULL while names are hidden.
pub fn guard_connection(connection: &mut Connection, names_hidden: bool) -> Result<()> {
    if !names_hidden {
        return Ok(());
    }
    connection.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Read {
            table_name,
            column_name,
        } if NAME_COLUMNS
            .iter()
            .any(|(table, column)| *table == table_name && *column == column_name) =>
        {
            Authorization::Ignore
        }
        _ => Authorization::Allow,
    }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_and_paths_are_masked_and_numbers_kept() {
        let mut answer = json!({
            "rows": [{"name": "holiday.mp4", "path": "/a/b", "length": 10, "verdict": "partial"}],
            "rootEntries": [".fseventsd"],
            "hint": "beach, sunset",
        });
        redact(&mut answer);
        assert_eq!(answer["rows"][0]["name"], "[hidden].mp4");
        assert_eq!(answer["rows"][0]["path"], "[hidden]");
        assert_eq!(answer["rows"][0]["length"], 10);
        assert_eq!(answer["rows"][0]["verdict"], "partial");
        assert_eq!(answer["rootEntries"][0], "[hidden]");
        assert_eq!(answer["hint"], "[hidden]");
    }
}
