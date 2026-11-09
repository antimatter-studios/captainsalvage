use super::*;
use serde_json::{json, Value};

mod support;

fn capture(name: &str, data: &[u8]) -> Value {
    let read = reader(data);
    let format = format_of(name, data.len() as u64, &*read).expect("archive format");
    let listing = list(format, name, data.len() as u64, &read).expect("listing");
    let checks = listing
        .members
        .iter()
        .filter(|member| !member.is_dir)
        .map(|member| {
            json!({
                "index": member.index,
                "result": check(format, &listing, member.index, data.len() as u64, &read).expect("check")
            })
        })
        .collect::<Vec<_>>();
    json!({"sourceSha256": support::sha256(data), "listing": listing, "checks": checks})
}

#[test]
fn archive_outputs_match_the_pre_driver_reference() {
    let tar = tar_bytes();
    let plain = second();
    let mut cases = serde_json::Map::new();
    for (name, data) in [
        ("a.tar", tar.clone()),
        ("a.tar.gz", gzip(&tar)),
        ("a.tar.xz", xz(&tar)),
        ("a.tar.bz2", bzip2(&tar)),
        ("a.tar.zst", zstd(&tar)),
        ("big.bin.gz", gzip(&plain)),
        ("big.bin.xz", xz(&plain)),
        ("big.bin.bz2", bzip2(&plain)),
        ("big.bin.zst", zstd(&plain)),
    ] {
        cases.insert(name.into(), capture(name, &data));
        if name.starts_with("big.bin.") {
            let mut damaged = data;
            let middle = damaged.len() / 2;
            damaged[middle] ^= 0x5A;
            cases.insert(format!("{name}-damaged"), capture(name, &damaged));
        }
    }
    let mut seven_z =
        sevenz_rust2::ArchiveWriter::new(std::io::Cursor::new(Vec::new())).expect("7z writer");
    for (name, bytes) in [
        ("docs/first.txt", FIRST),
        ("docs/second.bin", plain.as_slice()),
    ] {
        seven_z
            .push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(name), Some(bytes))
            .expect("7z entry");
    }
    let mut seven_z = seven_z.finish().expect("7z archive").into_inner();
    cases.insert("seven-z".into(), capture("a.7z", &seven_z));
    seven_z[200] ^= 0x5A;
    cases.insert("seven-z-damaged".into(), capture("a.7z", &seven_z));
    for (name, version, solid) in [
        ("rar50", rars::ArchiveVersion::Rar50, false),
        ("rar50-solid", rars::ArchiveVersion::Rar50, true),
        ("rar29", rars::ArchiveVersion::Rar29, false),
    ] {
        let mut data = rar(version, solid);
        cases.insert(name.into(), capture("a.rar", &data));
        let middle = data.len() * 2 / 3;
        data[middle] ^= 0x5A;
        cases.insert(format!("{name}-damaged"), capture("a.rar", &data));
    }
    let packed = gzip(&tar);
    cases.insert(
        "cut-short-tar".into(),
        capture("a.tar.gz", &packed[..packed.len() * 3 / 5]),
    );
    support::record(
        "archives.json",
        json!({"cases": cases}),
        include_str!("reference/archives.json"),
    );
}
