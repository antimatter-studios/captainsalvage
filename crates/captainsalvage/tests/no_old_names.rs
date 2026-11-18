//! The names from before "Disk Rebuild" must not come back into what ships. Only the module that
//! renames an old database, and the fixture that builds one for its tests, may spell them.

use std::path::{Path, PathBuf};

const OLD: &[&str] = &["canvas", "basket", "rebuild tree"];

fn files(folder: &Path, extensions: &[&str], found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, extensions, found);
        } else if path
            .extension()
            .is_some_and(|extension| extensions.iter().any(|wanted| extension == *wanted))
        {
            found.push(path);
        }
    }
}

#[test]
fn the_shipped_code_and_schema_never_use_the_old_names() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut found = Vec::new();
    for (folder, extensions) in [
        ("crates/captainsalvage/src", &["rs", "sql"][..]),
        ("src-tauri/src", &["rs"][..]),
        ("macos", &["swift", "plist", "entitlements"][..]),
    ] {
        files(&root.join(folder), extensions, &mut found);
    }
    assert!(found.len() > 50, "the source folders were not found");
    let mut offences = Vec::new();
    for path in found {
        // The module that renames old databases and its tests name them on purpose.
        if path
            .components()
            .any(|part| part.as_os_str() == "disk_rebuild_rename")
            || path
                .file_name()
                .is_some_and(|name| name == "disk_rebuild_rename.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap().to_lowercase();
        for old in OLD {
            if text.contains(old) {
                offences.push(format!("{}: {old}", path.display()));
            }
        }
    }
    assert!(offences.is_empty(), "{offences:#?}");
}

#[test]
fn the_front_end_has_no_old_names_but_the_html_element() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src");
    let mut found = Vec::new();
    files(&root, &["ts", "tsx", "css", "json"], &mut found);
    assert!(found.len() > 20, "the front end was not found");
    let mut offences = Vec::new();
    for path in found {
        // The one file that names the old storage key, to move a saved choice to the new one.
        if path
            .file_name()
            .is_some_and(|name| name == "rememberedDiskRebuild.ts")
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap().to_lowercase();
        // `<canvas>` elements, `HTMLCanvasElement` and their variables are drawing surfaces.
        let text = text
            .replace("htmlcanvaselement", "")
            .replace("<canvas", "")
            .replace("canvas.", "")
            .replace("canvas =", "")
            .replace("canvas)", "")
            .replace("canvas,", "")
            .replace("canvas;", "")
            .replace("canvas ", "")
            .replace("canvas>", "");
        for old in OLD {
            if text.contains(old) {
                offences.push(format!("{}: {old}", path.display()));
            }
        }
    }
    assert!(offences.is_empty(), "{offences:#?}");
}
