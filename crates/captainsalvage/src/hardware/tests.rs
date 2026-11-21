use super::*;

/// The shape of `ioreg -l -w0` for a USB drive: the bridge keeps the serial number on its device
/// entry, and the drive's media is several entries below it.
const USB_DRIVE: &str = r#"
+-o Root  <class IORegistryEntry, id 0x100000100, retain 20>
  +-o AppleT8112PCIe  <class IOPCIDevice>
    | +-o XHC1@0  <class AppleUSBXHCI>
    |   +-o Elements SE  <class IOUSBHostDevice, id 0x10000a1b2>
    |     | {
    |     |   "USB Vendor Name" = "WD"
    |     |   "USB Product Name" = "Elements SE 2623"
    |     |   "USB Serial Number" = "575847314138324E4B4D4E34"
    |     | }
    |     |
    |     +-o IOUSBMassStorageDriverNub  <class IOUSBMassStorageDriverNub>
    |       +-o IOBlockStorageServices  <class IOBlockStorageServices>
    |         | {
    |         |   "Device Characteristics" = {"Product Name"="Elements SE 2623","Vendor Name"="WD"}
    |         | }
    |         +-o IOMedia  <class IOMedia>
    |             {
    |               "BSD Name" = "disk4"
    |               "Whole" = Yes
    |             }
    +-o OtherDrive  <class IOUSBHostDevice>
      | {
      |   "USB Serial Number" = "OTHERSERIAL1"
      | }
      +-o IOMedia  <class IOMedia>
          {
            "BSD Name" = "disk5"
          }
"#;

#[test]
fn the_registry_names_the_drive_by_its_model_and_serial_number() {
    let found = parse_ioreg(USB_DRIVE, "disk4");
    assert_eq!(found.serial.as_deref(), Some("575847314138324E4B4D4E34"));
    assert_eq!(found.model.as_deref(), Some("WD Elements SE 2623"));
    let other = parse_ioreg(USB_DRIVE, "disk5");
    assert_eq!(other.serial.as_deref(), Some("OTHERSERIAL1"));
    assert_ne!(found.identity(), other.identity());
}

#[test]
fn a_drive_the_registry_does_not_list_has_no_hardware() {
    assert_eq!(parse_ioreg(USB_DRIVE, "disk9"), Hardware::default());
    assert_eq!(parse_ioreg("", "disk4"), Hardware::default());
}

#[test]
fn a_serial_number_that_tells_nothing_is_no_identity() {
    let with = |serial: &str| Hardware {
        model: Some("Bridge".into()),
        serial: Some(serial.into()),
    };
    for fake in ["", "0000000000", "0123456789", "123456789012", "ab", "NA"] {
        assert_eq!(with(fake).identity(), None, "{fake}");
    }
    assert_eq!(
        with("SN-7781").identity().as_deref(),
        Some("Bridge|SN-7781")
    );
    // A model alone is the same for every drive of its kind.
    assert_eq!(
        Hardware {
            model: Some("Bridge".into()),
            serial: None
        }
        .identity(),
        None
    );
}

#[test]
fn a_partition_belongs_to_its_whole_disk_and_other_names_belong_to_none() {
    assert_eq!(
        whole_disk_name(Path::new("/dev/rdisk4s1")).as_deref(),
        Some("disk4")
    );
    assert_eq!(
        whole_disk_name(Path::new("/dev/disk12")).as_deref(),
        Some("disk12")
    );
    assert_eq!(whole_disk_name(Path::new("/tmp/image.img")), None);
}

#[test]
fn the_model_falls_back_to_what_diskutil_says() {
    let info =
        "   Device / Media Name:       Elements SE 2623 Media\n   Device Location:   External\n";
    assert_eq!(
        parse_diskutil_model(info).as_deref(),
        Some("Elements SE 2623 Media")
    );
}
