//! The generic adapter over rust-fs-core's layers, run beside the FAT adapter the app uses, on the same images.
use captainsalvage::filesystem::generic::GenericVolume;
use captainsalvage::filesystem::FilesystemKind;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

mod fat_support;
use fat_support::*;

/// An image in memory, read through the next rust-fs-core's trait, which the FAT driver is built on.
struct Memory(Vec<u8>);

impl fs_core_layers::BlockRead for Memory {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core_layers::Result<()> {
        let start = usize::try_from(offset).map_err(|_| fs_core_layers::Error::OutOfBounds {
            offset,
            len: output.len() as u64,
            size: self.0.len() as u64,
        })?;
        let bytes =
            self.0
                .get(start..start + output.len())
                .ok_or(fs_core_layers::Error::OutOfBounds {
                    offset,
                    len: output.len() as u64,
                    size: self.0.len() as u64,
                })?;
        output.copy_from_slice(bytes);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

fn workspace(label: &str) -> Workspace {
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-generic-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    Workspace::at(&path).unwrap()
}

type Fat = GenericVolume<fs_fat32::Volume<Memory>>;

const KINDS: [FilesystemKind; 3] = [
    FilesystemKind::Fat12,
    FilesystemKind::Fat16,
    FilesystemKind::Fat32,
];

/// What each side says of one entry, in terms both have.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Seen {
    /// Where the entry's 32 bytes are on the volume.
    at: u64,
    name: String,
    live: bool,
    is_dir: bool,
    first_cluster: u32,
    /// The size of a file; a folder's is not compared (the app counts the clusters of its chain, the driver says none).
    size: Option<u64>,
}

/// An entry the app found, with its id and the folder it sits in.
type AppEntry = (Seen, i64, Option<String>);

fn app_side(fixture: &Fixture, session: &Arc<Session>) -> (Vec<Seen>, Vec<AppEntry>) {
    let rows = session
        .query_map("SELECT id FROM sets ORDER BY source_cluster, source_offset")
        .unwrap();
    let mut seen = Vec::new();
    let mut with_ids = Vec::new();
    for row in rows["rows"].as_array().unwrap() {
        let id = row["id"].as_i64().unwrap();
        let entry = session
            .with_database(|database| Ok(database.entry(id)?.unwrap()))
            .unwrap();
        let cluster = entry.source_cluster.unwrap();
        let offset = u64::from(entry.source_offset.unwrap());
        let base = if cluster == 0 {
            fixture.root_sector as u64 * 512
        } else {
            (fixture.data_sector as u64 + u64::from(cluster) - 2) * 512
        };
        let view = Seen {
            at: base + offset,
            name: entry.name.clone(),
            live: entry.in_use,
            is_dir: entry.kind != "file",
            first_cluster: entry.first_cluster,
            size: (entry.kind == "file").then_some(entry.length),
        };
        with_ids.push((view.clone(), id, entry.path.clone()));
        seen.push(view);
    }
    seen.sort();
    (seen, with_ids)
}

fn generic_side(generic: &Fat) -> Vec<Seen> {
    use fs_core_layers::layers::{CandidateKind, CandidateState};
    let mut seen: Vec<Seen> = generic
        .candidates()
        .unwrap()
        .iter()
        .map(|candidate| {
            let view = Fat::view(candidate);
            Seen {
                at: view.id,
                name: view.name.clone().unwrap_or_default(),
                live: view.state == CandidateState::Live,
                is_dir: view.kind == CandidateKind::Directory,
                first_cluster: view.detail["firstCluster"].as_u64().unwrap() as u32,
                size: (view.kind == CandidateKind::File).then(|| view.size.unwrap()),
            }
        })
        .collect();
    seen.sort();
    seen
}

fn scanned(fixture: &Fixture, label: &str) -> Arc<Session> {
    let session = Session::with_source(
        Path::new("/test/fat-image"),
        Arc::new(MemorySource::new(fixture.image.clone())),
        workspace(label),
    )
    .unwrap();
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions {
            chunk_clusters: 1024,
        },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    session
}

#[test]
fn the_generic_adapter_finds_the_candidates_the_fat_adapter_finds_and_the_differences_are_the_known_ones(
) {
    for kind in KINDS {
        let fixture = Fixture::new(kind);
        let generic = Fat::open_for_salvage(Memory(fixture.image.clone())).unwrap();
        let session = scanned(&fixture, &format!("cand-{kind}"));
        let (app, _) = app_side(&fixture, &session);
        let driver = generic_side(&generic);
        assert_eq!(
            generic.format(),
            kind.to_string().to_lowercase().replace(' ', ""),
            "{kind}"
        );
        // Every entry the generic adapter finds, the app finds, with the same name, state, size and first cluster, in
        // the same 32 bytes.
        for found in &driver {
            assert!(
                app.contains(found),
                "{kind}: only the generic adapter finds {found:?}\napp: {app:#?}"
            );
        }
        // The app also finds two things the driver does not, and each is a known difference, said here so it is a decision
        // to change and not a surprise:
        let only_app: Vec<&str> = app
            .iter()
            .filter(|seen| !driver.contains(seen))
            .map(|seen| seen.name.as_str())
            .collect();
        assert_eq!(
            only_app,
            ["child by.tes", "ORPHAN.TXT"],
            "{kind}: file bytes in a cluster that reads like a directory entry, which the app's scan of every cluster \
             takes for one; and a directory cluster nobody points to and that has no . and .. to name it, which the driver's \
             discovery does not look for"
        );
        assert_eq!(
            driver.len(),
            4,
            "{kind}: the long-named file, the folder, the deleted entry and the folder's child"
        );
    }
}

/// The clusters a walk's extents cover, from the volume's own numbering: cluster 2 is at the first data byte.
fn clusters_of(walk: &fs_core_layers::layers::ExtentWalk, fixture: &Fixture) -> Vec<u32> {
    let data = fixture.data_sector as u64 * 512;
    walk.extents
        .iter()
        .flat_map(|extent| {
            let first = (extent.source.offset - data) / 512 + 2;
            let count = extent.source.length.div_ceil(512);
            (first..first + count).map(|cluster| cluster as u32)
        })
        .collect()
}

#[test]
fn the_generic_adapter_places_the_extents_the_tree_and_the_bytes_as_the_fat_adapter_does() {
    for kind in KINDS {
        let fixture = Fixture::new(kind);
        let generic = Fat::open_for_salvage(Memory(fixture.image.clone())).unwrap();
        let session = scanned(&fixture, &format!("same-{kind}"));
        let (_, app) = app_side(&fixture, &session);
        let candidates = generic.candidates().unwrap();
        let (tree, placed) = generic.tree(&candidates).unwrap();
        assert_eq!(
            tree.nodes.len(),
            candidates.len(),
            "{kind}: one node for each candidate, in order"
        );
        assert_eq!(
            tree.orphans().count(),
            0,
            "{kind}: the driver's four candidates all sit under the root or the folder"
        );
        for candidate in &candidates {
            let view = Fat::view(candidate);
            let name = view.name.clone().unwrap();
            let (seen, id, path) = app
                .iter()
                .find(|(seen, _, _)| seen.at == view.id)
                .unwrap_or_else(|| panic!("{kind}: {name} is not in the app's scan"));
            let at = format!("{kind}: {name}");

            // The tree: the folder each is in.
            let folder = placed
                .iter()
                .find(|place| place.id == view.id)
                .unwrap()
                .folder
                .clone()
                .unwrap();
            assert_eq!(
                &folder,
                path.as_deref().unwrap().trim_start_matches('/'),
                "{at}"
            );

            // The extents: the clusters each claims. A deleted entry's chain is cleared, so the driver follows the claim
            // as one run, which is what the app does with `chainFallbackContiguous` on (below).
            let walk = generic.extents(candidate).unwrap();
            assert_eq!(walk.end, fs_core_layers::layers::WalkEnd::Complete, "{at}");
            if seen.live {
                let clusters = session
                    .volume
                    .clusters_for(seen.first_cluster, seen.size.unwrap_or(512), false)
                    .clusters;
                assert_eq!(clusters_of(&walk, &fixture), clusters, "{at}");
            }

            // The copied bytes.
            if seen.is_dir {
                continue;
            }
            let mut copied = Vec::new();
            let (_, report) = generic.copy(candidate, &mut copied).unwrap();
            assert_eq!(report.unreadable().count(), 0, "{at}");
            if seen.live {
                let read = session
                    .read_entry_range(*id, 0, seen.size.unwrap() + 10)
                    .unwrap();
                assert_eq!(copied, read.data, "{at}");
            }
        }
        // The deleted entry: its chain is gone, so the app reads its first cluster until told to follow the claim, and then
        // reads what the driver does.
        let deleted = candidates
            .iter()
            .find(|candidate| candidate.name.as_deref() == Some("_OST.TXT"))
            .unwrap();
        let mut copied = Vec::new();
        generic.copy(deleted, &mut copied).unwrap();
        let (_, id, _) = app
            .iter()
            .find(|(seen, _, _)| seen.name == "_OST.TXT")
            .unwrap();
        assert_eq!(
            session.read_entry_range(*id, 0, 520).unwrap().data,
            vec![b'D'; 512]
        );
        let engine = captainsalvage::engine::Engine::new();
        engine.set_session(session.clone());
        engine
            .call(
                "set_parameters",
                &serde_json::json!({"chainFallbackContiguous": true}),
                captainsalvage::engine::AUTHOR_USER,
            )
            .unwrap();
        let followed = session.read_entry_range(*id, 0, 520).unwrap().data;
        assert_eq!(copied, followed, "{kind}: the claim followed as one run");
        assert_eq!(copied, [vec![b'D'; 512], vec![b'E'; 8]].concat());
    }
}

#[test]
fn the_generic_adapter_browses_the_volume_like_the_fat_adapter_lists_it() {
    for kind in KINDS {
        let fixture = Fixture::new(kind);
        let generic = Fat::open_for_salvage(Memory(fixture.image.clone())).unwrap();
        let browsed = generic.browse().unwrap();
        let paths: Vec<&str> = browsed.keys().map(String::as_str).collect();
        assert_eq!(
            paths,
            ["/ARCHIVE", "/ARCHIVE/CHILD.TXT", "/Captain log.txt"],
            "{kind}"
        );
        assert_eq!(
            generic.read("/Captain log.txt").unwrap(),
            fixture.contents,
            "{kind}"
        );
        assert_eq!(
            generic.read("/ARCHIVE/CHILD.TXT").unwrap(),
            b"child bytes",
            "{kind}"
        );
        // The app's own browse of the root.
        let volume =
            captainsalvage::filesystem::FilesystemVolume::open(AppDisk(fixture.image.clone()))
                .unwrap();
        let mut root: Vec<(String, u64)> = volume
            .root_directory()
            .unwrap()
            .into_iter()
            .map(|entry| (entry.name, entry.size))
            .collect();
        root.sort();
        let mut generic_root: Vec<(String, u64)> = generic
            .list("/")
            .unwrap()
            .into_iter()
            .map(|entry| (entry.name, entry.size))
            .collect();
        generic_root.sort();
        // The app's root lists the folder with its size 0 and the file's name and size as stored, as the driver's does.
        assert_eq!(root, generic_root, "{kind}");
    }
}

/// The same image through the app's own device trait, for its FilesystemVolume.
struct AppDisk(Vec<u8>);

impl fs_core::BlockRead for AppDisk {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
        let size = self.0.len() as u64;
        let start = usize::try_from(offset).map_err(|_| fs_core::Error::OutOfBounds {
            offset,
            len: output.len() as u64,
            size,
        })?;
        let bytes = self
            .0
            .get(start..start + output.len())
            .ok_or(fs_core::Error::OutOfBounds {
                offset,
                len: output.len() as u64,
                size,
            })?;
        output.copy_from_slice(bytes);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

impl fs_core_layers::BlockRead for AppDisk {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core_layers::Result<()> {
        fs_core_layers::BlockRead::read_at(&Memory(self.0.clone()), offset, output)
    }
    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

#[test]
fn the_support_rows_come_from_what_the_driver_lacks() {
    use captainsalvage::filesystem::generic::capability_rows;
    use captainsalvage::filesystem::Support;
    use fs_core_layers::layers::{Capabilities, Feature, HealthState, VolumeHealth, VolumeSupport};
    let row = |rows: &[captainsalvage::filesystem::generic::DerivedCapability], name: &str| {
        rows.iter()
            .find(|row| row.feature == name)
            .cloned()
            .unwrap_or_else(|| panic!("no row {name}"))
    };
    let clean = VolumeHealth::from_findings(Vec::new());
    assert_eq!(clean.state, HealthState::Clean);

    // A driver that lacks nothing supports every row.
    let whole = capability_rows(
        &VolumeSupport {
            files: true,
            salvage: Capabilities::default(),
            unsupported: vec![],
        },
        &clean,
    );
    assert!(
        whole.iter().all(|row| row.support == Support::Supported),
        "{whole:#?}"
    );

    // What it lacks makes a row Limited when the row still works, and Missing when it cannot.
    let support = VolumeSupport {
        files: true,
        salvage: Capabilities {
            missing: vec![
                Feature::DeletedCandidates,
                Feature::EncodedExtents,
                Feature::ClusterSteps,
                Feature::GeometryHypotheses,
            ],
        },
        unsupported: vec!["an incompat flag".into()],
    };
    let rows = capability_rows(&support, &clean);
    let scan = row(&rows, "scan");
    assert_eq!(scan.support, Support::Limited);
    assert!(scan.note.contains("deleted-candidates"), "{}", scan.note);
    assert_eq!(row(&rows, "export").support, Support::Limited);
    assert!(row(&rows, "export").note.contains("encoded-extents"));
    assert_eq!(row(&rows, "rebuilds").support, Support::Limited);
    assert_eq!(row(&rows, "geometry_hypotheses").support, Support::Missing);
    let open = row(&rows, "open");
    assert_eq!(open.support, Support::Limited);
    assert!(open.note.contains("an incompat flag"), "{}", open.note);

    let nothing = VolumeSupport {
        files: false,
        salvage: Capabilities {
            missing: vec![
                Feature::FindCandidates,
                Feature::RebuildTree,
                Feature::WalkExtents,
                Feature::Materialize,
                Feature::ReportDamage,
            ],
        },
        unsupported: vec![],
    };
    let rows = capability_rows(&nothing, &clean);
    for name in ["scan", "browse", "export", "rebuilds", "damage_report"] {
        assert_eq!(row(&rows, name).support, Support::Missing, "{name}");
    }
    // Namespace that did not validate: browsing by path is not offered, though the tree can be rebuilt.
    let unvalidated = VolumeSupport {
        files: false,
        salvage: Capabilities::default(),
        unsupported: vec![],
    };
    assert_eq!(
        row(&capability_rows(&unvalidated, &clean), "browse").support,
        Support::Limited
    );
}

#[test]
fn the_generic_adapter_reports_what_the_driver_says_of_the_volume() {
    use captainsalvage::filesystem::Support;
    for kind in KINDS {
        let fixture = Fixture::new(kind);
        let generic = Fat::open_for_salvage(Memory(fixture.image.clone())).unwrap();
        let rows = generic.capability_rows().unwrap();
        let get = |name: &str| rows.iter().find(|row| row.feature == name).unwrap();
        // What FAT's driver lacks, said by the driver: nothing is stored encoded, and no geometry can be guessed.
        assert_eq!(get("scan").support, Support::Supported, "{kind}");
        assert_eq!(get("browse").support, Support::Supported, "{kind}");
        assert_eq!(get("export").support, Support::Limited, "{kind}");
        assert!(get("export").note.contains("encoded-extents"), "{kind}");
        assert_eq!(
            get("geometry_hypotheses").support,
            Support::Missing,
            "{kind}"
        );
        let geometry = generic.geometry().unwrap();
        assert_eq!(geometry.sector_size, 512, "{kind}");
        assert_eq!(geometry.allocation_unit, 512, "{kind}");
        // The fixture's first FAT entry does not carry the media descriptor, which the driver reports as it is, never hidden.
        let health = generic.health().unwrap();
        assert!(
            !health.findings.is_empty()
                && health.state == fs_core_layers::layers::HealthState::Degraded,
            "{kind}: {health:?}"
        );
        let (damage, report) = generic.damage().unwrap();
        assert!(
            report.reported >= health.findings.len() as u64 && !damage.is_empty(),
            "{kind}"
        );
    }
}
