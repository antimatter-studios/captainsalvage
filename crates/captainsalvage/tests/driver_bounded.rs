//! The engine reads a driver's volume a cluster at a time: it asks for the candidates that start in one cluster, stores them
//! and goes on, and never holds the volume's candidates. The driver here is a stand-in that makes four files in every cluster of
//! a large volume and counts what it is asked for.
use captainsalvage::filesystem::drivers::{DriverAccess, DriverRead, DriverRecord, ParentRef};
use captainsalvage::map::MapDb;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::source::{BlockSource, MemorySource, ReadStats};
use captainsalvage::volume::Volume;
use captainsalvage::workspace::Workspace;
use fs_core_layers::layers::{
    CandidateKind, CandidateState, Damage, Extent, ExtentWalk, Geometry, Integrity, Structure,
    VolumeHealth, VolumeSupport, WalkEnd,
};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const CLUSTERS: u64 = 40_000;
const FILES_PER_CLUSTER: u64 = 4;

/// A superblock that declares a volume of `CLUSTERS` blocks of 4096 bytes, on a source that holds only its first megabyte.
fn big_image() -> Vec<u8> {
    let mut image = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/squashfs-basic.sqfs"),
    )
    .unwrap();
    image.resize(1 << 20, 0);
    image[0x28..0x30].copy_from_slice(&(CLUSTERS * 4096).to_le_bytes());
    image
}

/// A source as long as the volume that reads as zeros beyond the bytes it is given, so a large volume costs nothing to hold.
struct Wide(MemorySource, u64);

impl BlockSource for Wide {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
        if offset + buffer.len() as u64 > self.1 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        if self.0.read_exact_at(offset, buffer).is_err() {
            buffer.fill(0);
        }
        Ok(())
    }
    fn read_stats(&self) -> ReadStats {
        self.0.read_stats()
    }
}

#[derive(Default)]
struct Counting {
    finds: AtomicUsize,
    most_in_one_find: AtomicUsize,
    widest_range: AtomicUsize,
    parent_batches: AtomicUsize,
    most_folders_held: AtomicUsize,
    most_files_in_a_batch: AtomicUsize,
}

struct Stand(Arc<Counting>);

impl DriverAccess for Stand {
    fn format(&self) -> &'static str {
        "stand-in"
    }
    fn geometry(&self) -> anyhow::Result<Geometry> {
        anyhow::bail!("not asked")
    }
    fn support(&self) -> anyhow::Result<VolumeSupport> {
        anyhow::bail!("not asked")
    }
    fn health(&self) -> anyhow::Result<VolumeHealth> {
        Ok(VolumeHealth::from_findings(Vec::new()))
    }
    fn structures(&self) -> anyhow::Result<Vec<Structure>> {
        Ok(Vec::new())
    }
    fn damage(&self) -> anyhow::Result<Vec<Damage>> {
        Ok(Vec::new())
    }
    fn find(
        &self,
        range: Extent,
        sink: &mut dyn FnMut(DriverRecord) -> ControlFlow<()>,
    ) -> anyhow::Result<()> {
        self.0.finds.fetch_add(1, Ordering::Relaxed);
        self.0
            .widest_range
            .fetch_max(range.length as usize, Ordering::Relaxed);
        let cluster = range.offset / 4096;
        let mut made = 0;
        let mut at = |id: u64, kind, name: Option<String>, k: u64| {
            made += 1;
            sink(DriverRecord {
                id,
                location: Extent::new(range.offset + k * 64, 64),
                name,
                kind,
                state: CandidateState::Live,
                integrity: Integrity::Verified,
                size: Some(10),
                candidate: String::new(),
            })
        };
        if cluster == 0 {
            // The root folder, which no name gives.
            let _ = at(1, CandidateKind::Directory, None, 8);
        }
        for k in 0..FILES_PER_CLUSTER {
            let id = 100 + cluster * FILES_PER_CLUSTER + k;
            if at(id, CandidateKind::File, Some(format!("file {id}.txt")), k).is_break() {
                break;
            }
        }
        self.0.most_in_one_find.fetch_max(made, Ordering::Relaxed);
        Ok(())
    }
    fn walk(&self, _record: &DriverRecord) -> anyhow::Result<ExtentWalk> {
        Ok(ExtentWalk {
            extents: Vec::new(),
            claimed: 10,
            end: WalkEnd::Complete,
        })
    }
    fn parents(
        &self,
        folders: &[DriverRecord],
        files: &[DriverRecord],
    ) -> anyhow::Result<Vec<Vec<ParentRef>>> {
        self.0.parent_batches.fetch_add(1, Ordering::Relaxed);
        self.0
            .most_folders_held
            .fetch_max(folders.len(), Ordering::Relaxed);
        self.0
            .most_files_in_a_batch
            .fetch_max(files.len(), Ordering::Relaxed);
        Ok(files
            .iter()
            .map(|file| {
                if file.id == 1 {
                    vec![ParentRef::Root]
                } else {
                    vec![ParentRef::Record(1)]
                }
            })
            .collect())
    }
    fn read(&self, _walk: &ExtentWalk, _start: u64, _length: u64) -> anyhow::Result<DriverRead> {
        Ok(DriverRead {
            data: Vec::new(),
            unreadable: Vec::new(),
        })
    }
}

#[test]
fn a_volume_of_a_hundred_and_sixty_thousand_files_is_scanned_a_cluster_at_a_time() {
    let counts = Arc::new(Counting::default());
    let volume = Volume::open(Arc::new(Wide(
        MemorySource::new(big_image()),
        CLUSTERS * 4096,
    )))
    .expect("the image opens")
    .with_driver(Box::new(Stand(counts.clone())));
    assert_eq!(u64::from(volume.geometry.cluster_count), CLUSTERS);
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-driver-bounded-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    let workspace = Workspace::at(&path).unwrap();
    run_scan(
        &volume,
        &workspace,
        ScanOptions {
            chunk_clusters: 1024,
        },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();

    // Every cluster was asked about once, each time for the records that start in that cluster alone.
    assert_eq!(counts.finds.load(Ordering::Relaxed) as u64, CLUSTERS);
    assert_eq!(counts.widest_range.load(Ordering::Relaxed), 4096);
    assert!(counts.most_in_one_find.load(Ordering::Relaxed) as u64 <= FILES_PER_CLUSTER + 1);
    // The parent claims were found a batch of files at a time, with the folders (one) beside them.
    assert_eq!(counts.most_folders_held.load(Ordering::Relaxed), 1);
    assert!(counts.most_files_in_a_batch.load(Ordering::Relaxed) <= 5000);
    assert!(counts.parent_batches.load(Ordering::Relaxed) >= 2);

    // And every file is in the map, in the root.
    let database = MapDb::open(&workspace.database()).unwrap();
    // The files, and the root folder.
    assert_eq!(
        database.state_u64("sets_found").unwrap(),
        CLUSTERS * FILES_PER_CLUSTER + 1
    );
    let in_root = database
        .children(volume.geometry.root_cluster, 0, 10, true, 0.0)
        .unwrap();
    assert_eq!(in_root.rows.len(), 10);
}
