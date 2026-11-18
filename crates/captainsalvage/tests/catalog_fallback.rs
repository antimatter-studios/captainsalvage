//! A volume still opens when the catalog cannot be read, in the folder of its own that every
//! version before the catalog used, and the unreadable database is left as it is. Its own test
//! file, because it points HOME at a folder with a database that is not one.

use captainsalvage::session::Session;
use fs_exfat::fixtures::{self, Builder};

#[test]
fn a_volume_opens_in_its_own_folder_when_the_catalog_cannot_be_read() {
    let home = std::env::temp_dir().join(format!("captainsalvage-fallback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let app = home.join("Library/Application Support/Captain Salvage");
    std::fs::create_dir_all(&app).unwrap();
    std::env::set_var("HOME", &home);
    let broken = vec![0x42u8; 8192];
    std::fs::write(app.join("raw-scans.sqlite"), &broken).unwrap();

    let mut builder = Builder {
        image: fixtures::build_volume(),
    };
    builder.image.resize(2 * 1024 * 1024, 0);
    builder.write_boot();
    let image = home.join("volume.img");
    std::fs::write(&image, &builder.image).unwrap();

    let session = Session::open(&image).unwrap();
    assert!(session.scan.is_none(), "there is no scan to name");
    assert_eq!(session.workspace.dir(), app.join("vol-abcd1234"));
    assert!(session.workspace.database().exists());
    assert_eq!(
        std::fs::read(app.join("raw-scans.sqlite")).unwrap(),
        broken,
        "a database that cannot be read is not replaced"
    );
    assert!(!app.join("backups").exists());
    assert_eq!(std::fs::read(&image).unwrap(), builder.image);
    let _ = std::fs::remove_dir_all(&home);
}
