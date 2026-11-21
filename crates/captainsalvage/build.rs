//! Linking on Linux with two copies of rust-fs-core.
//!
//! The FAT driver (and every driver on the layers) is built on rust-fs-core 0.5, whose `BlockRead` is another trait than the
//! 0.4.5 the NTFS driver still uses, so this crate links both. Each copy exports the same C ABI (`fs_core_file_open`,
//! `fs_core_device_close`, ... as `#[no_mangle]`), and a Linux linker refuses a program that defines a symbol twice, where
//! the macOS one does not. Captain Salvage calls none of those functions (they are for a C or Swift consumer of a driver's
//! static library), so the linker is told to keep the first definition of each. This goes when every driver is on one
//! rust-fs-core, or when rust-fs-core stops exporting its C ABI to a Rust consumer that did not ask for it.
//!
//! A build script's link argument reaches every target of the package, the tests and the binary alike, and is not undone by a
//! `RUSTFLAGS` of the environment, as a flag in `.cargo/config.toml` is.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-Wl,--allow-multiple-definition");
    }
}
