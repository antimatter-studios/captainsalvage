//! SquashFS through the generic layers adapter: the second driver on it, and the first whose records share bytes.
//!
//! The image is the driver's committed fixture (`tests/fixtures/squashfs-basic.sqfs`, gzip, 4096-byte blocks, made by
//! mksquashfs): a symbolic link, a file of three bytes in a fragment, a nested file, a 20000-byte file of several blocks
//! and a tail, and an empty file. The kernel-made images of the oracle are in `squashfs_oracle.rs`.
use captainsalvage::filesystem::generic::GenericVolume;
use captainsalvage::filesystem::Support;
use fs_core_layers::layers::{CandidateKind, CandidateState, Integrity, NodeKind};
use fs_squashfs::layers::SquashfsVolume;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// An image in memory, read through rust-fs-core's trait, which the SquashFS driver is built on.
struct Memory(Vec<u8>);

impl fs_core_layers::BlockRead for Memory {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core_layers::Result<()> {
        let outside = || fs_core_layers::Error::OutOfBounds {
            offset,
            len: output.len() as u64,
            size: self.0.len() as u64,
        };
        let start = usize::try_from(offset).map_err(|_| outside())?;
        let bytes = self
            .0
            .get(start..start.saturating_add(output.len()))
            .ok_or_else(outside)?;
        output.copy_from_slice(bytes);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

type Squashfs = GenericVolume<SquashfsVolume>;

fn fixture() -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/squashfs-basic.sqfs"),
    )
    .unwrap()
}

fn open(image: Vec<u8>) -> Squashfs {
    Squashfs::open_for_salvage(Arc::new(Memory(image))).expect("the driver opens the image")
}

/// The driver's own pattern for `/sub/deep/big.bin`: an LCG, as its build script writes it.
fn big() -> Vec<u8> {
    let mut state: u32 = 0x1234_5678;
    (0..20000)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (state >> 16) as u8
        })
        .collect()
}

#[test]
fn the_driver_names_the_format_and_the_volume() {
    let volume = open(fixture());
    assert_eq!(volume.format(), "squashfs");
    let geometry = volume.geometry().unwrap();
    assert_eq!(geometry.allocation_unit, 4096);
    assert_eq!(geometry.sector_size, 512);
    assert!(volume.health().unwrap().is_clean());
    assert!(volume.support().unwrap().files);
}

#[test]
fn every_candidate_has_an_id_of_its_own_though_the_inodes_share_one_metadata_block() {
    let volume = open(fixture());
    let candidates = volume.candidates().unwrap();
    assert!(candidates.len() >= 8, "{}", candidates.len());
    // The inodes of a small image sit in one compressed metadata block, so every candidate's `location` is that
    // block: naming candidates by it would make them all one. The driver's `candidate_id` is the inode reference.
    let locations: BTreeSet<u64> = candidates
        .iter()
        .map(|candidate| candidate.location.offset)
        .collect();
    assert_eq!(
        locations.len(),
        1,
        "the fixture's inodes share a block: {locations:?}"
    );
    let ids: BTreeSet<u64> = candidates
        .iter()
        .map(|candidate| volume.id(candidate))
        .collect();
    assert_eq!(ids.len(), candidates.len(), "ids must be unique");
    for candidate in &candidates {
        let view = volume.view(candidate);
        assert_eq!(view.id, view.detail["inode_ref"].as_u64().unwrap());
        assert_eq!(view.state, CandidateState::Live);
        assert_eq!(view.integrity, Integrity::Verified);
    }
}

#[test]
fn the_tree_places_each_candidate_in_its_folder() {
    let volume = open(fixture());
    let candidates = volume.candidates().unwrap();
    let (tree, placed) = volume.tree(&candidates).unwrap();
    assert_eq!(tree.nodes.len(), candidates.len());
    let found: BTreeMap<String, Option<String>> = placed
        .iter()
        .filter_map(|candidate| Some((candidate.name.clone()?, candidate.folder.clone())))
        .collect();
    let folder = |name: &str| {
        found
            .get(name)
            .unwrap_or_else(|| panic!("{name}: {found:?}"))
    };
    assert_eq!(folder("hello.txt").as_deref(), Some(""));
    assert_eq!(folder("empty.txt").as_deref(), Some(""));
    assert_eq!(folder("link").as_deref(), Some(""));
    assert_eq!(folder("sub").as_deref(), Some(""));
    assert_eq!(folder("note.md").as_deref(), Some("sub"));
    assert_eq!(folder("deep").as_deref(), Some("sub"));
    assert_eq!(folder("big.bin").as_deref(), Some("sub/deep"));
    // Every record has one parent. A folder says it twice, once in its parent's listing and once in its own record,
    // and the two agree.
    for node in &tree.nodes {
        let parents: BTreeSet<String> = node
            .parents
            .iter()
            .map(|claim| format!("{:?}", claim.parent))
            .collect();
        assert_eq!(parents.len(), 1, "{node:?}");
    }
}

#[test]
fn a_candidates_extents_rebuild_the_bytes_the_file_layer_reads() {
    let volume = open(fixture());
    let expected: BTreeMap<&str, Vec<u8>> = BTreeMap::from([
        ("hello.txt", b"hi\n".to_vec()),
        ("note.md", b"# note\nsome words here\n".to_vec()),
        ("big.bin", big()),
        ("empty.txt", Vec::new()),
    ]);
    let paths = BTreeMap::from([
        ("hello.txt", "/hello.txt"),
        ("note.md", "/sub/note.md"),
        ("big.bin", "/sub/deep/big.bin"),
        ("empty.txt", "/empty.txt"),
    ]);
    let mut copied = 0;
    for candidate in volume.candidates().unwrap() {
        let view = volume.view(&candidate);
        if view.kind != CandidateKind::File {
            continue;
        }
        let name = view.name.clone().unwrap();
        let mut bytes = Vec::new();
        let (_walk, report) = volume.copy(&candidate, &mut bytes).unwrap();
        assert_eq!(report.unreadable().count(), 0, "{name}: {report:?}");
        assert_eq!(report.length, bytes.len() as u64, "{name}");
        assert_eq!(
            bytes,
            expected[name.as_str()],
            "{name}: rebuilt from its extents"
        );
        assert_eq!(
            volume.read(paths[name.as_str()]).unwrap(),
            bytes,
            "{name}: the file layer"
        );
        assert_eq!(view.size, Some(bytes.len() as u64), "{name}");
        copied += 1;
    }
    assert_eq!(copied, 4);
}

#[test]
fn the_file_layer_lists_the_image_by_path() {
    let volume = open(fixture());
    let found = volume.browse().unwrap();
    let kinds: BTreeMap<&str, NodeKind> = found
        .iter()
        .map(|(path, (kind, _))| (path.as_str(), *kind))
        .collect();
    assert_eq!(
        kinds,
        BTreeMap::from([
            ("/empty.txt", NodeKind::File),
            ("/hello.txt", NodeKind::File),
            ("/link", NodeKind::Symlink),
            ("/sub", NodeKind::Directory),
            ("/sub/deep", NodeKind::Directory),
            ("/sub/deep/big.bin", NodeKind::File),
            ("/sub/note.md", NodeKind::File),
        ])
    );
    assert_eq!(found["/sub/deep/big.bin"].1, 20000);
    assert_eq!(found["/hello.txt"].1, 3);
}

#[test]
fn the_support_rows_come_from_what_the_driver_lacks() {
    use captainsalvage::filesystem::generic::DerivedCapability;
    let volume = open(fixture());
    let rows = volume.capability_rows().unwrap();
    let row = |feature: &str| -> &DerivedCapability {
        rows.iter()
            .find(|row| row.feature == feature)
            .unwrap_or_else(|| panic!("no {feature}: {rows:?}"))
    };
    // The driver lacks deleted candidates, deleted extents, cluster steps and geometry hypotheses: SquashFS has no
    // deleted entries and no clusters, and nothing to guess a geometry from.
    assert_eq!(row("open").support, Support::Supported);
    assert_eq!(row("browse").support, Support::Supported);
    assert_eq!(row("damage_report").support, Support::Supported);
    assert_eq!(row("scan").support, Support::Limited);
    assert!(
        row("scan").note.contains("deleted-candidates"),
        "{}",
        row("scan").note
    );
    assert_eq!(row("export").support, Support::Limited);
    assert!(
        row("export").note.contains("deleted-extents"),
        "{}",
        row("export").note
    );
    assert_eq!(row("rebuilds").support, Support::Limited);
    assert_eq!(row("geometry_hypotheses").support, Support::Missing);
}

#[test]
fn an_image_that_claims_more_bytes_than_it_has_opens_for_salvage_and_says_so() {
    let mut image = fixture();
    let claimed = image.len() as u64 + 4096;
    image[0x28..0x30].copy_from_slice(&claimed.to_le_bytes());
    let volume = open(image);
    let health = volume.health().unwrap();
    assert!(!health.is_clean(), "{health:?}");
    let (damage, _) = volume.damage().unwrap();
    assert!(!damage.is_empty());
    // The listing still comes: the damage is in a field the readers do not need.
    assert!(volume
        .candidates()
        .unwrap()
        .iter()
        .any(|candidate| candidate.name.as_deref() == Some("hello.txt")));
}
