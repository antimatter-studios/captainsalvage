use captainsalvage::disk::RawDisk;
use captainsalvage::engine::{Engine, AUTHOR_USER};
use captainsalvage::filesystem::driver::Driver;
use captainsalvage::filesystem::ntfs::Recovery;
use fs_core::{BlockDevice, BlockRead};
use fs_ntfs::block_io::BlockIo;
use fs_ntfs::file::FileAPI;
use fs_ntfs::volume::Volume;
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Image(Arc<Mutex<Vec<u8>>>);

impl BlockRead for Image {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
        let data = self.0.lock().unwrap();
        let start = usize::try_from(offset).unwrap();
        let bytes = data
            .get(start..start + output.len())
            .ok_or(fs_core::Error::OutOfBounds {
                offset,
                len: output.len() as u64,
                size: data.len() as u64,
            })?;
        output.copy_from_slice(bytes);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }
}
impl BlockDevice for Image {
    fn write_at(&self, offset: u64, input: &[u8]) -> fs_core::Result<()> {
        self.0.lock().unwrap()[offset as usize..offset as usize + input.len()]
            .copy_from_slice(input);
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}
impl BlockIo for Image {
    fn read_exact_at(&mut self, offset: u64, output: &mut [u8]) -> Result<(), String> {
        self.read_at(offset, output)
            .map_err(|error| error.to_string())
    }
    fn write_all_at(&mut self, offset: u64, input: &[u8]) -> Result<(), String> {
        self.write_at(offset, input)
            .map_err(|error| error.to_string())
    }
    fn size(&self) -> u64 {
        self.size_bytes()
    }
}

fn ntfs(payload: &[u8]) -> (Image, u64, u64) {
    let mut image = Image(Arc::new(Mutex::new(vec![0; 8 * 1024 * 1024])));
    fs_ntfs::mkfs::format_filesystem(
        &mut image,
        8 * 1024 * 1024,
        4096,
        1024,
        Some("RECOVERY"),
        Some(0x123456789abcdef0),
    )
    .unwrap();
    let volume = Volume::open_writable(image.clone()).unwrap();
    let number = volume.create_file("/", "treasure.txt").unwrap();
    volume.write_file_at("/treasure.txt", 0, payload).unwrap();
    let record = volume.inspect_mft_record(number).unwrap();
    (image, record.offset, number)
}

struct Fixture(PathBuf);
impl Fixture {
    fn new(bytes: &[u8]) -> Self {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-ntfs-{}-{}.img",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(bytes).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).unwrap();
    }
}

#[test]
fn generic_driver_reads_ntfs_without_fabricating_fat_geometry() {
    let (image, _, _) = ntfs(b"treasure");
    let before = image.0.lock().unwrap().clone();
    let driver = Driver::open(image.clone()).unwrap();
    let geometry = driver.geometry();
    assert_eq!(geometry.format, "NTFS");
    assert_eq!(geometry.serial, "123456789abcdef0");
    assert_eq!(geometry.cluster_size, 4096);
    assert_eq!(geometry.bytes_per_sector, 512);
    let files = driver.root_directory().unwrap();
    let file = files
        .iter()
        .find(|file| file.name == "treasure.txt")
        .unwrap();
    let mut output = [0; 32];
    let count = driver.read_file_at(file, 0, &mut output).unwrap();
    assert_eq!(&output[..count], b"treasure");
    let other = Driver::open(image.clone()).unwrap();
    assert!(other.read_file_at(file, 0, &mut output).is_err());
    assert_eq!(*image.0.lock().unwrap(), before);
}

#[test]
fn generic_driver_still_reads_exfat() {
    use fs_exfat::fixtures::{
        bitmap_entry, entry_set, seal_boot, seal_entry_set, upcase_entry, upcase_table, Builder,
        BITMAP_CLUSTER, ROOT_CLUSTER,
    };
    use fs_exfat::primitives::{bitmap_location, BootSector, FAT_END};
    let mut builder = Builder::new();
    builder.image.resize(1024 * 1024, 0);
    let sectors = (builder.image.len() / 512) as u64;
    for copy in [0, 12 * 512] {
        builder.image[copy + 72..copy + 80].copy_from_slice(&sectors.to_le_bytes());
    }
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4, 5] {
        builder.allocate(cluster);
        builder.fat(cluster, FAT_END);
    }
    builder.fat(ROOT_CLUSTER, FAT_END);
    let boot = BootSector::parse(&builder.image).unwrap();
    let bitmap = bitmap_location(&boot, &bitmap_entry(BITMAP_CLUSTER, u64::MAX)).unwrap();
    let mut words = vec![0xffff, 97];
    words.extend(65..=90);
    words.extend([0xffff, 65413]);
    let upcase = upcase_table(&words);
    let mut file_entry = entry_set("treasure.txt", false, 5, 8, true, true);
    file_entry.copy_within(12..16, 16);
    let name_hash = "TREASURE.TXT"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .fold(0u16, |hash, byte| {
            hash.rotate_right(1).wrapping_add(u16::from(byte))
        });
    file_entry[36..38].copy_from_slice(&name_hash.to_le_bytes());
    seal_entry_set(&mut file_entry);
    builder.directory(
        ROOT_CLUSTER,
        &[
            bitmap_entry(bitmap.first_cluster, bitmap.required_length),
            upcase_entry(4, &upcase),
            file_entry,
        ],
    );
    builder.put(4, 0, &upcase);
    builder.put(5, 0, b"treasure");
    for copy in [0, 12 * 512] {
        for index in 1..=8 {
            let position = copy + (index + 1) * 512 - 4;
            builder.image[position..position + 4].copy_from_slice(&0xaa550000u32.to_le_bytes());
        }
        seal_boot(&mut builder.image[copy..copy + 12 * 512], 512);
    }
    let driver = Driver::open(Image(Arc::new(Mutex::new(builder.image)))).unwrap();
    assert_eq!(driver.geometry().format, "exFAT");
    let files = driver.root_directory().unwrap();
    let file = files
        .iter()
        .find(|file| file.name == "treasure.txt")
        .unwrap();
    let mut output = [0; 8];
    assert_eq!(driver.read_file_at(file, 0, &mut output).unwrap(), 8);
    assert_eq!(&output, b"treasure");
}

#[test]
fn recovery_adapter_reads_surviving_records_after_the_boot_is_destroyed() {
    let payload = vec![0x69; 14000];
    let (image, offset, _) = ntfs(&payload);
    image.write_at(0, &[0; 512]).unwrap();
    let bytes = image.0.lock().unwrap().clone();
    let fixture = Fixture::new(&bytes);
    let disk = RawDisk::open(&fixture.0).unwrap();
    let recovery = Recovery::open(&disk, 0, disk.length).unwrap();
    let record = recovery.inspect(offset, 1024, 512).unwrap();
    let description = Recovery::describe(&record);
    assert!(description.validated);
    let stream = description
        .streams
        .iter()
        .find(|stream| stream.name.is_empty())
        .unwrap();
    assert!(!stream.resident);
    assert_eq!(
        recovery
            .read_stream(&record, stream.attribute, 4096, 0, 64 * 1024)
            .unwrap(),
        payload
    );
    assert!(recovery
        .read_stream(&record, stream.attribute, 4096, 0, 65537)
        .is_err());
    assert_eq!(std::fs::read(&fixture.0).unwrap(), bytes);
}

#[test]
fn recovery_adapter_respects_partition_relative_mapping_pairs() {
    let (image, offset, _) = ntfs(&vec![0x77; 12000]);
    let start = 1024 * 1024;
    let bytes = image.0.lock().unwrap();
    let mut whole = vec![0xcc; start];
    whole.extend_from_slice(&bytes);
    let fixture = Fixture::new(&whole);
    let disk = RawDisk::open(&fixture.0).unwrap();
    assert!(Recovery::open(&disk, disk.length, 1).is_err());
    let recovery = Recovery::open(&disk, start as u64, bytes.len() as u64).unwrap();
    let record = recovery.inspect(offset, 1024, 512).unwrap();
    let stream = Recovery::describe(&record).streams.remove(0);
    assert_eq!(
        recovery
            .read_stream(&record, stream.attribute, 4096, 0, 12000)
            .unwrap(),
        vec![0x77; 12000]
    );
    assert!(recovery.inspect(bytes.len() as u64, 1024, 512).is_err());
}

#[test]
fn mcp_can_inspect_and_read_a_surviving_record_without_a_filesystem_session() {
    assert!(captainsalvage::privacy::CONTENT_TOOLS.contains(&"read_ntfs_stream"));
    let (mut image, offset, _) = ntfs(b"treasure");
    fs_ntfs::write::write_named_stream_io(&mut image, "/treasure.txt", "private-name", b"hidden")
        .unwrap();
    image.write_at(0, &[0; 512]).unwrap();
    let fixture = Fixture::new(&image.0.lock().unwrap());
    let engine = Engine::default();
    let opened = engine
        .call("open_raw_disk", &json!({"path": fixture.0}), AUTHOR_USER)
        .unwrap();
    let record = engine
        .call(
            "inspect_ntfs_record",
            &json!({"diskId": opened["id"], "recordOffset": offset}),
            AUTHOR_USER,
        )
        .unwrap();
    assert_eq!(record["validated"], true);
    let data = engine.call("read_ntfs_stream", &json!({"diskId": opened["id"], "recordOffset": offset, "attribute": record["streams"][0]["attribute"], "clusterSize": 4096, "offset": 0, "length": 8}), AUTHOR_USER).unwrap();
    assert_eq!(data["hex"], "7472656173757265");
    assert!(data["warning"].as_str().unwrap().contains("reused"));
    engine.set_names_hidden(true);
    assert!(engine.call("read_ntfs_stream", &json!({"diskId": opened["id"], "recordOffset": offset, "attribute": record["streams"][0]["attribute"], "clusterSize": 4096, "offset": 0, "length": 8}), "claude").unwrap_err().to_string().contains("kept file names and contents"));
    let hidden = engine
        .call(
            "inspect_ntfs_record",
            &json!({"diskId": opened["id"], "recordOffset": offset}),
            "claude",
        )
        .unwrap();
    assert!(hidden["streams"]
        .as_array()
        .unwrap()
        .iter()
        .any(|stream| stream["name"] == captainsalvage::privacy::HIDDEN));
    assert!(!hidden.to_string().contains("private-name"));
    assert!(engine
        .call(
            "inspect_ntfs_record",
            &json!({"diskId": "wrong", "recordOffset": offset}),
            AUTHOR_USER
        )
        .is_err());
}

#[test]
fn cli_lists_files_and_salvages_without_writing_the_source() {
    let (image, offset, _) = ntfs(b"treasure");
    let original = image.0.lock().unwrap().clone();
    let fixture = Fixture::new(&original);
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_captainsalvage"))
            .arg("filesystem")
            .args(args)
            .output()
            .unwrap()
    };
    let path = fixture.0.to_str().unwrap();
    let listed = run(&["list", path]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&listed.stdout).unwrap()["geometry"]["format"],
        "NTFS"
    );
    let copied = run(&["cat", path, "--name", "treasure.txt"]);
    assert!(copied.status.success());
    assert_eq!(copied.stdout, b"treasure");
    let described = run(&["ntfs-record", path, "--record-offset", &offset.to_string()]);
    assert!(described.status.success());
    let record: serde_json::Value = serde_json::from_slice(&described.stdout).unwrap();
    let attribute = record["streams"][0]["attribute"]
        .as_u64()
        .unwrap()
        .to_string();
    let salvaged = run(&[
        "ntfs-stream",
        path,
        "--record-offset",
        &offset.to_string(),
        "--attribute",
        &attribute,
        "--cluster-size",
        "4096",
    ]);
    assert!(
        salvaged.status.success(),
        "{}",
        String::from_utf8_lossy(&salvaged.stderr)
    );
    assert_eq!(salvaged.stdout, b"treasure");
    assert!(
        !run(&["ntfs-stream", path, "--record-offset", &offset.to_string()])
            .status
            .success()
    );
    assert!(!run(&["list", path, "--bogus", "0"]).status.success());
    assert_eq!(std::fs::read(&fixture.0).unwrap(), original);
}
