//! Archives of every kind the app reads, built here, then listed and checked the way the app
//! reads them from a disk: a range at a time. Damaged copies must list as far as they go and
//! say which files cannot be trusted.

use salvage_core::archive::{CHECK_DAMAGED, CHECK_INTACT};
use salvage_core::archives::{check, format_of, list, Format, Packing};
use std::io::Write;

const FIRST: &[u8] = b"the first file, which is short";

fn second() -> Vec<u8> {
    // Enough varied data that compressing it leaves a body to damage.
    (0..200_000u32)
        .flat_map(|n| (n.wrapping_mul(2_654_435_761)).to_le_bytes())
        .collect()
}

fn tar_bytes() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut add = |path: &str, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        builder.append_data(&mut header, path, data).unwrap();
    };
    add("docs/first.txt", FIRST);
    add("docs/second.bin", &second());
    builder.into_inner().unwrap()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn xz(data: &[u8]) -> Vec<u8> {
    let mut writer =
        lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default()).unwrap();
    writer.write_all(data).unwrap();
    writer.finish().unwrap()
}

fn bzip2(data: &[u8]) -> Vec<u8> {
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn zstd(data: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
}

fn names(data: &[u8], name: &str) -> (Format, Vec<String>, bool) {
    let read =
        |start: u64, length: u64| Ok(data[start as usize..(start + length) as usize].to_vec());
    let format = format_of(name, data.len() as u64, &read).expect("a known format");
    let listed = list(format, name, data.len() as u64, &read).unwrap();
    (
        format,
        listed
            .members
            .iter()
            .map(|member| member.path.clone())
            .collect(),
        listed.complete,
    )
}

fn status(data: &[u8], name: &str, index: u32) -> &'static str {
    let read =
        |start: u64, length: u64| Ok(data[start as usize..(start + length) as usize].to_vec());
    let format = format_of(name, data.len() as u64, &read).unwrap();
    let listed = list(format, name, data.len() as u64, &read).unwrap();
    check(format, &listed, index, data.len() as u64, &read)
        .unwrap()
        .status
}

#[test]
fn every_kind_of_tar_lists_and_checks() {
    let tar = tar_bytes();
    let files = vec!["docs/first.txt".to_string(), "docs/second.bin".to_string()];
    let kinds: Vec<(Vec<u8>, &str, Format)> = vec![
        (tar.clone(), "a.tar", Format::Tar(Packing::Plain)),
        (gzip(&tar), "a.tar.gz", Format::Tar(Packing::Gzip)),
        (gzip(&tar), "a.tgz", Format::Tar(Packing::Gzip)),
        (xz(&tar), "a.tar.xz", Format::Tar(Packing::Xz)),
        (bzip2(&tar), "a.tar.bz2", Format::Tar(Packing::Bzip2)),
        (zstd(&tar), "a.tar.zst", Format::Tar(Packing::Zstd)),
        // Named only as compressed, or not at all: the tar inside is still found.
        (gzip(&tar), "a.gz", Format::Tar(Packing::Gzip)),
        (xz(&tar), "lost file", Format::Tar(Packing::Xz)),
    ];
    for (data, name, expected) in kinds {
        let (format, found, complete) = names(&data, name);
        assert_eq!(format, expected, "{name}");
        assert_eq!(found, files, "{name}");
        assert!(complete, "{name}");
        assert_eq!(status(&data, name, 1), CHECK_INTACT, "{name}");
    }
}

#[test]
fn a_compressed_file_checks_against_its_own_checksum() {
    let data = second();
    for (packed, name) in [
        (gzip(&data), "big.bin.gz"),
        (xz(&data), "big.bin.xz"),
        (bzip2(&data), "big.bin.bz2"),
    ] {
        let (_, found, _) = names(&packed, name);
        assert_eq!(found, vec!["big.bin".to_string()], "{name}");
        assert_eq!(status(&packed, name, 0), CHECK_INTACT, "{name}");
        // One byte changed in the middle, as a bad cluster would.
        let mut damaged = packed.clone();
        let middle = damaged.len() / 2;
        damaged[middle] ^= 0x5A;
        assert_eq!(status(&damaged, name, 0), CHECK_DAMAGED, "{name}");
    }
}

#[test]
fn a_cut_short_tar_lists_what_is_left_and_says_so() {
    let packed = gzip(&tar_bytes());
    let cut = &packed[..packed.len() * 3 / 5];
    let read =
        |start: u64, length: u64| Ok(cut[start as usize..(start + length) as usize].to_vec());
    let format = format_of("a.tar.gz", cut.len() as u64, &read).unwrap();
    let listed = list(format, "a.tar.gz", cut.len() as u64, &read).unwrap();
    assert_eq!(
        listed.members.first().map(|member| member.path.as_str()),
        Some("docs/first.txt")
    );
    assert!(!listed.complete || listed.members.len() == 2);
    assert_eq!(status(cut, "a.tar.gz", 0), CHECK_INTACT);
    if listed.members.len() == 2 {
        assert_eq!(status(cut, "a.tar.gz", 1), CHECK_DAMAGED);
    }
}

#[test]
fn a_seven_z_lists_and_checks_and_notices_damage() {
    let folder = std::env::temp_dir().join(format!("salvage-7z-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(folder.join("in/docs")).unwrap();
    std::fs::write(folder.join("in/docs/first.txt"), FIRST).unwrap();
    std::fs::write(folder.join("in/docs/second.bin"), second()).unwrap();
    let target = folder.join("a.7z");
    sevenz_rust2::compress_to_path(folder.join("in"), &target).unwrap();
    let data = std::fs::read(&target).unwrap();
    let (format, found, _) = names(&data, "a.7z");
    assert_eq!(format, Format::SevenZ);
    assert!(
        found.iter().any(|name| name.ends_with("second.bin")),
        "{found:?}"
    );
    let index = found
        .iter()
        .position(|name| name.ends_with("second.bin"))
        .unwrap() as u32;
    assert_eq!(status(&data, "a.7z", index), CHECK_INTACT);
    let mut damaged = data.clone();
    damaged[200] ^= 0x5A;
    assert_eq!(status(&damaged, "a.7z", index), CHECK_DAMAGED);
    let _ = std::fs::remove_dir_all(&folder);
}
