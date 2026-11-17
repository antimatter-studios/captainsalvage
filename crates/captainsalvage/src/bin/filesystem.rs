use anyhow::{bail, ensure, Context, Result};
use captainsalvage::disk::RawDisk;
use captainsalvage::filesystem::driver::{Driver, RegionDevice};
use captainsalvage::filesystem::ntfs::Recovery;
use captainsalvage::filesystem::FilesystemKind;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

const USAGE: &str = "usage: captainsalvage filesystem <list|cat|capabilities|ntfs-record|ntfs-stream> SOURCE [--start BYTES --length BYTES]\n\
capabilities: what is supported, limited or missing for the filesystem the boot sector names\n\
cat: --name ROOT_FILENAME\n\
ntfs-record and ntfs-stream: --record-offset BYTES [--record-size 1024 --sector-size 512]\n\
ntfs-stream: --attribute INDEX --cluster-size BYTES\n\
Offsets are relative to the selected source region. cat and ntfs-stream write binary data to stdout.\n\
Recovery is a hypothesis: surviving records do not prove their data clusters were not reused.";

pub fn run(arguments: &[String]) -> Result<()> {
    let Some(command) = arguments.first().map(String::as_str) else {
        println!("{USAGE}");
        return Ok(());
    };
    if matches!(command, "--help" | "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    ensure!(
        matches!(
            command,
            "list" | "cat" | "capabilities" | "ntfs-record" | "ntfs-stream"
        ),
        "{USAGE}"
    );
    let path = arguments.get(1).context(USAGE)?;
    let mut values = HashMap::new();
    let mut options = arguments[2..].iter();
    while let Some(option) = options.next() {
        let allowed = matches!(option.as_str(), "--start" | "--length")
            || match command {
                "cat" => option == "--name",
                "ntfs-record" => matches!(
                    option.as_str(),
                    "--record-offset" | "--record-size" | "--sector-size"
                ),
                "ntfs-stream" => matches!(
                    option.as_str(),
                    "--record-offset"
                        | "--record-size"
                        | "--sector-size"
                        | "--attribute"
                        | "--cluster-size"
                ),
                _ => false,
            };
        ensure!(allowed, "unknown option {option} for {command}");
        ensure!(
            values
                .insert(
                    option.as_str(),
                    options.next().context("option needs a value")?.as_str()
                )
                .is_none(),
            "duplicate option {option}"
        );
    }
    let number = |name, default| -> Result<u64> {
        match values.get(name) {
            Some(value) => Ok(value.parse()?),
            None => Ok(default),
        }
    };
    let disk = RawDisk::open(Path::new(path))?;
    let start = number("--start", 0)?;
    let length = number("--length", disk.length.saturating_sub(start))?;
    if command == "capabilities" {
        let mut boot = [0u8; 512];
        let device = RegionDevice::open(&disk, start, length)?;
        fs_core::BlockRead::read_at(&device, 0, &mut boot)?;
        let kind = FilesystemKind::detect_boot_sector(&boot)?;
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "filesystem": kind.to_string(),
                "capabilities": kind.capabilities(),
            }))?
        );
        return Ok(());
    }
    if matches!(command, "list" | "cat") {
        let driver = Driver::open(RegionDevice::open(&disk, start, length)?)?;
        let files = driver.root_directory()?;
        if command == "list" {
            println!(
                "{}",
                serde_json::to_string(
                    &serde_json::json!({"geometry": driver.geometry(), "files": files})
                )?
            );
            return Ok(());
        }
        let name = values.get("--name").context("cat requires --name")?;
        let file = files
            .iter()
            .find(|file| file.name == *name && !file.is_directory)
            .context("file not found in root directory")?;
        let mut stdout = std::io::stdout().lock();
        let mut offset = 0;
        let mut buffer = vec![0; 64 * 1024];
        while offset < file.size {
            let count = driver.read_file_at(file, offset, &mut buffer)?;
            ensure!(
                count > 0,
                "file ended before declared size at byte {offset}"
            );
            stdout.write_all(&buffer[..count])?;
            offset += count as u64;
        }
        stdout.flush()?;
        return Ok(());
    }
    ensure!(
        values.contains_key("--record-offset"),
        "--record-offset is required"
    );
    let recovery = Recovery::open(&disk, start, length)?;
    let record = recovery.inspect(
        number("--record-offset", 0)?,
        usize::try_from(number("--record-size", 1024)?)?,
        u16::try_from(number("--sector-size", 512)?)?,
    )?;
    let description = Recovery::describe(&record);
    if command == "ntfs-record" {
        println!("{}", serde_json::to_string(&description)?);
        return Ok(());
    }
    ensure!(
        values.contains_key("--attribute") && values.contains_key("--cluster-size"),
        "ntfs-stream requires --attribute and --cluster-size"
    );
    let attribute = usize::try_from(number("--attribute", 0)?)?;
    let cluster_size = number("--cluster-size", 0)?;
    let stream = description
        .streams
        .iter()
        .find(|stream| stream.attribute == attribute)
        .context("data stream attribute not found")?;
    let mut stdout = std::io::stdout().lock();
    let mut offset = 0;
    while offset < stream.length {
        let bytes = recovery.read_stream(&record, attribute, cluster_size, offset, 64 * 1024)?;
        if bytes.is_empty() {
            bail!("stream ended before declared size at byte {offset}");
        }
        stdout.write_all(&bytes)?;
        offset += bytes.len() as u64;
    }
    stdout.flush()?;
    Ok(())
}
