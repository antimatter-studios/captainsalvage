# Captain Salvage engine and CLI

The Rust package, library crate, and command-line entrypoint are all named
`captainsalvage`. The macOS application uses this same engine; it does not run a
separate recovery implementation.

## Filesystem adapter

`filesystem` is the boundary between the application and filesystem drivers.
The session, scan, map, browse, and export paths consume normalized geometry,
allocation state, directory findings, and streams. Format-specific boot fields
are returned separately for the UI. The adapter uses permissive primitives for
salvage rather than requiring damaged media to pass the normal FileAPI checks.

Each driver has three distinct API layers:

- **FileAPI:** access a valid filesystem as files and directories.
- **PrimitiveAPI:** inspect filesystem structures, including partial or damaged
  directory records and best-effort chains that are not valid files.
- **VolumeAPI:** read raw bytes relative to the volume, below filesystem parsing.
  Captain Salvage exposes no write path to a source volume.

exFAT and FAT12/16/32 use the same salvage algorithms. The FAT-family driver
handles each variant's encoding and root layout. NTFS opens through the same workspace,
but its entries come from FILE records and run lists, not from a table of links, and its
limits are reported by `FilesystemKind::capabilities`.

## Development checks

```sh
cargo test -p captainsalvage -p rust-fs-fat32
cargo run -p captainsalvage --bin captainsalvage -- --help
```
