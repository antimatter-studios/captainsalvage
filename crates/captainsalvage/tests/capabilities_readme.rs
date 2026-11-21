//! The support table in the README is the table the app reports. It is written from
//! `capabilities_markdown`, never by hand: when a note or a level changes, run
//! `UPDATE_README_CAPABILITIES=1 cargo test -p captainsalvage --test capabilities_readme` and
//! commit the README.

use captainsalvage::filesystem::{capabilities_markdown, FilesystemKind};

const START: &str = "<!-- capabilities:start -->\n";
const END: &str = "<!-- capabilities:end -->";

fn readme_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md")
}

#[test]
fn the_readme_support_table_is_the_one_the_app_reports() {
    let path = readme_path();
    let readme = std::fs::read_to_string(&path).unwrap();
    let start = readme
        .find(START)
        .expect("the README has the table's start marker")
        + START.len();
    let end = readme
        .find(END)
        .expect("the README has the table's end marker");
    let generated = format!("{}\n", capabilities_markdown().trim_end());
    if readme[start..end] == generated {
        return;
    }
    if std::env::var_os("UPDATE_README_CAPABILITIES").is_some() {
        let updated = format!("{}{}{}", &readme[..start], generated, &readme[end..]);
        std::fs::write(&path, updated).unwrap();
        return;
    }
    panic!(
        "the README's support table differs from the app's. Run `UPDATE_README_CAPABILITIES=1 cargo test -p captainsalvage --test capabilities_readme` and commit the README.\n--- the app reports:\n{generated}"
    );
}

#[test]
fn the_three_fat_variants_report_one_table_and_no_filesystem_claims_a_feature_of_another() {
    let fat = FilesystemKind::Fat32.capabilities();
    assert_eq!(FilesystemKind::Fat12.capabilities(), fat);
    assert_eq!(FilesystemKind::Fat16.capabilities(), fat);
    for kind in [
        FilesystemKind::Exfat,
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
        FilesystemKind::Ntfs,
        FilesystemKind::Squashfs,
    ] {
        let table = kind.capabilities();
        let mut seen = std::collections::HashSet::new();
        for row in &table {
            assert!(seen.insert(row.feature), "{kind}: {} twice", row.feature);
            assert!(!row.note.is_empty(), "{kind}: {} has no note", row.feature);
        }
        for feature in [
            "open",
            "scan",
            "map",
            "browse",
            "preview",
            "resume",
            "baseline",
            "file_tests",
            "rebuilds",
            "export",
            "mount_block",
        ] {
            assert!(
                seen.contains(feature),
                "{kind} does not say about {feature}"
            );
        }
    }
    // FAT has no allocation bitmap on disk, and its table must not say it reads one.
    for row in fat {
        let note = row.note.to_lowercase();
        if note.contains("bitmap") {
            assert!(
                note.contains("no allocation bitmap") || note.contains("no bitmap"),
                "{}: {}",
                row.feature,
                row.note
            );
        }
    }
    // Stream and attribute features belong to NTFS alone.
    for kind in [FilesystemKind::Exfat, FilesystemKind::Fat16] {
        assert!(kind.capabilities().iter().all(|row| ![
            "named_streams",
            "hard_links",
            "attribute_lists",
            "compression",
            "encryption",
            "journal"
        ]
        .contains(&row.feature)));
    }
}

#[test]
fn the_mount_block_of_ntfs_is_limited_for_a_volume_macos_gives_no_uuid() {
    use captainsalvage::filesystem::Support;
    let row = |kind: FilesystemKind| {
        kind.capabilities()
            .into_iter()
            .find(|row| row.feature == "mount_block")
            .unwrap()
    };
    for kind in [FilesystemKind::Exfat, FilesystemKind::Fat32] {
        assert_eq!(row(kind).support, Support::Supported, "{kind}");
    }
    let ntfs = row(FilesystemKind::Ntfs);
    assert_eq!(ntfs.support, Support::Limited);
    for said in ["volume UUID", "offset and length", "fingerprint"] {
        assert!(ntfs.note.contains(said), "{said}: {}", ntfs.note);
    }
}
