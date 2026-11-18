//! exFAT folders of more than one cluster, whose entry sets cross from one cluster into the next.
//! Hand-built images from the driver's fixtures; the sources never change.

use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use fs_exfat::fixtures::{
    bitmap_entry, entry_set, upcase_entry, Builder, BITMAP_CLUSTER, CLUSTER, ROOT_CLUSTER,
};
use fs_exfat::primitives::{bitmap_location, BootSector, FAT_END};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const FILES: u32 = 30;

fn name(number: u32) -> String {
    format!("entry file with a long name {number:04}.txt")
}

/// A volume whose folder `many` is two clusters, 5 and `second`, holding thirty files of five
/// entries each: a cluster holds 128 entries, so the twenty-sixth file starts in the first cluster
/// and ends in the second.
fn volume(second: u32) -> Vec<u8> {
    let mut builder = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4, 5, second] {
        builder.allocate(cluster);
    }
    builder.fat(ROOT_CLUSTER, FAT_END);
    let boot = BootSector::parse(&builder.image).unwrap();
    let location = bitmap_location(&boot, &bitmap_entry(BITMAP_CLUSTER, u64::MAX)).unwrap();
    builder.directory(
        ROOT_CLUSTER,
        &[
            bitmap_entry(location.first_cluster, location.required_length),
            upcase_entry(4, &[0; 100]),
            entry_set("many", true, 5, 2 * CLUSTER as u64, false, true),
        ],
    );
    builder.fat(5, second);
    builder.fat(second, FAT_END);
    let mut entries = Vec::new();
    for number in 0..FILES {
        entries.extend(entry_set(
            &name(number),
            false,
            100 + number,
            10,
            true,
            true,
        ));
    }
    assert!(
        entries.len() > CLUSTER,
        "the sets must reach the second cluster"
    );
    builder.put(5, 0, &entries[..CLUSTER]);
    builder.put(second, 0, &entries[CLUSTER..]);
    builder.image
}

#[test]
fn an_entry_set_that_crosses_into_the_next_folder_cluster_is_listed_wherever_that_cluster_is() {
    for (second, chunk) in [(6, 1024), (6, 3), (30, 1024), (30, 3)] {
        let workspace = Workspace::at(std::env::temp_dir().join(format!(
            "captainsalvage-exfat-folder-{}-{second}-{chunk}",
            std::process::id()
        )))
        .unwrap();
        let session = Session::with_source(
            Path::new("/test/exfat-folder"),
            Arc::new(MemorySource::new(volume(second))),
            workspace,
        )
        .unwrap();
        run_scan(
            &session.volume,
            &session.workspace,
            ScanOptions {
                chunk_clusters: chunk,
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let rows = session
            .query_map("SELECT name FROM sets WHERE in_use = 1 AND first_cluster >= 100 ORDER BY first_cluster")
            .unwrap();
        let names: Vec<&str> = rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        let wanted: Vec<String> = (0..FILES).map(name).collect();
        assert_eq!(
            names, wanted,
            "the folder continues at cluster {second}, scan chunk {chunk}"
        );
    }
}
