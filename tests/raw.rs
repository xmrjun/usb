use usb_disk_ejector::raw::{
    checked_dependency_entries_end, needs_virtual_dependency_query, parse_dependency_utf16,
    parse_disk_extents, parse_drive_layout, parse_multi_sz, parse_storage_descriptor, DiskExtent,
};

fn push_u32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn push_i64(buffer: &mut [u8], offset: usize, value: i64) {
    buffer[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn parses_bounded_multi_sz_and_requires_double_nul() {
    assert_eq!(
        parse_multi_sz(&['E' as u16, ':' as u16, '\\' as u16, 0, 0]).unwrap(),
        vec![r"E:\"]
    );
    assert!(parse_multi_sz(&['E' as u16, 0]).is_err());
}

#[test]
fn parses_every_extent_and_rejects_truncation() {
    // x64 VOLUME_DISK_EXTENTS has an 8-byte header and 24-byte extents.
    let mut bytes = vec![0_u8; 8 + 2 * 24];
    push_u32(&mut bytes, 0, 2);
    push_u32(&mut bytes, 8, 4);
    push_i64(&mut bytes, 16, 0);
    push_i64(&mut bytes, 24, 1_000);
    push_u32(&mut bytes, 32, 9);
    push_i64(&mut bytes, 40, 2_000);
    push_i64(&mut bytes, 48, 3_000);
    assert_eq!(
        parse_disk_extents(&bytes).unwrap(),
        vec![
            DiskExtent {
                disk_number: 4,
                starting_offset: 0,
                extent_length: 1_000,
            },
            DiskExtent {
                disk_number: 9,
                starting_offset: 2_000,
                extent_length: 3_000,
            }
        ]
    );
    bytes.pop();
    assert!(parse_disk_extents(&bytes).is_err());
}

#[test]
fn storage_descriptor_checks_offsets_and_size() {
    let mut bytes = vec![0_u8; 64];
    push_u32(&mut bytes, 0, 64);
    push_u32(&mut bytes, 4, 64);
    push_u32(&mut bytes, 12, 36);
    push_u32(&mut bytes, 16, 44);
    push_u32(&mut bytes, 24, 55);
    push_u32(&mut bytes, 28, 7); // BusTypeUsb
    bytes[36..44].copy_from_slice(b"Vendor\0\0");
    bytes[44..55].copy_from_slice(b"Flash Disk\0");
    bytes[55..62].copy_from_slice(b"Serial\0");

    let descriptor = parse_storage_descriptor(&bytes).unwrap();
    assert_eq!(descriptor.bus_type, 7);
    assert_eq!(descriptor.vendor, "Vendor");
    assert_eq!(descriptor.product, "Flash Disk");
    assert_eq!(descriptor.serial, "Serial");

    push_u32(&mut bytes, 16, 99);
    assert!(parse_storage_descriptor(&bytes).is_err());
}

#[test]
fn virtual_dependency_query_covers_ambiguous_and_file_backed_buses() {
    assert!(needs_virtual_dependency_query(14)); // BusTypeVirtual
    assert!(needs_virtual_dependency_query(15)); // BusTypeFileBackedVirtual
    assert!(!needs_virtual_dependency_query(7)); // BusTypeUsb
}

#[test]
fn virtual_dependency_entries_reject_truncation_and_overflow() {
    assert_eq!(checked_dependency_entries_end(128, 8, 2, 48), Ok(104));
    assert!(checked_dependency_entries_end(100, 8, 2, 48).is_err());
    assert!(checked_dependency_entries_end(128, 8, usize::MAX, 48).is_err());
}

#[test]
fn virtual_dependency_host_volume_parser_is_strict() {
    let units = r"\\?\Volume{safe}\"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let bytes = units
        .iter()
        .flat_map(|unit| unit.to_le_bytes())
        .collect::<Vec<_>>();
    assert_eq!(
        parse_dependency_utf16(&bytes, 0).unwrap(),
        r"\\?\Volume{safe}\"
    );
    assert!(parse_dependency_utf16(&bytes, 1).is_err());
    assert!(parse_dependency_utf16(&bytes, bytes.len()).is_err());
    assert!(parse_dependency_utf16(&bytes[..bytes.len() - 2], 0).is_err());
    assert!(parse_dependency_utf16(&[0x00, 0xd8, 0, 0], 0).is_err());
}

fn mbr_layout(partition_type: u8, boot: u8, recognized: u8) -> Vec<u8> {
    let mut bytes = vec![0_u8; 48 + 144];
    push_u32(&mut bytes, 0, 0); // PARTITION_STYLE_MBR
    push_u32(&mut bytes, 4, 1);
    push_u32(&mut bytes, 48, 0);
    push_i64(&mut bytes, 56, 1_048_576);
    push_i64(&mut bytes, 64, 7_000_000_000);
    push_u32(&mut bytes, 72, 1);
    bytes[80] = partition_type;
    bytes[81] = boot;
    bytes[82] = recognized;
    bytes
}

#[test]
fn drive_layout_accepts_only_simple_non_boot_data_partitions() {
    let layout = parse_drive_layout(&mbr_layout(0x0c, 0, 1)).unwrap();
    assert!(layout.simple_data_only);
    assert_eq!(layout.partitions.len(), 1);
    assert_eq!(layout.partitions[0].number, 1);
    assert_eq!(layout.partitions[0].starting_offset, 1_048_576);
    assert_eq!(layout.partitions[0].length, 7_000_000_000);

    assert!(
        !parse_drive_layout(&mbr_layout(0x42, 0, 1))
            .unwrap()
            .simple_data_only
    );
    assert!(
        !parse_drive_layout(&mbr_layout(0x0c, 1, 1))
            .unwrap()
            .simple_data_only
    );
    assert!(
        !parse_drive_layout(&mbr_layout(0x0c, 0, 0))
            .unwrap()
            .simple_data_only
    );
}

#[test]
fn drive_layout_rejects_truncation_and_raw_style() {
    let mut truncated = mbr_layout(0x0c, 0, 1);
    truncated.pop();
    assert!(parse_drive_layout(&truncated).is_err());

    let mut raw = mbr_layout(0x0c, 0, 1);
    push_u32(&mut raw, 0, 2); // PARTITION_STYLE_RAW
    assert!(!parse_drive_layout(&raw).unwrap().simple_data_only);
}

#[test]
fn drive_layout_ignores_unused_mbr_table_slots() {
    let mut bytes = vec![0_u8; 48 + 4 * 144];
    push_u32(&mut bytes, 0, 0);
    push_u32(&mut bytes, 4, 4);
    let used = mbr_layout(0x07, 0, 1);
    bytes[48..48 + 144].copy_from_slice(&used[48..]);
    let layout = parse_drive_layout(&bytes).unwrap();
    assert!(layout.simple_data_only);
    assert_eq!(layout.partitions.len(), 1);
}

#[test]
fn drive_layout_accepts_basic_gpt_data_but_rejects_efi_and_required_partitions() {
    const BASIC_DATA: [u8; 16] = [
        0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99,
        0xc7,
    ];
    const EFI_SYSTEM: [u8; 16] = [
        0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9,
        0x3b,
    ];
    let make = |partition_type: [u8; 16], attributes: u64| {
        let mut bytes = vec![0_u8; 48 + 144];
        push_u32(&mut bytes, 0, 1);
        push_u32(&mut bytes, 4, 1);
        push_u32(&mut bytes, 48, 1);
        push_i64(&mut bytes, 56, 1_048_576);
        push_i64(&mut bytes, 64, 7_000_000_000);
        push_u32(&mut bytes, 72, 1);
        bytes[80..96].copy_from_slice(&partition_type);
        bytes[112..120].copy_from_slice(&attributes.to_le_bytes());
        bytes
    };

    assert!(
        parse_drive_layout(&make(BASIC_DATA, 0))
            .unwrap()
            .simple_data_only
    );
    assert!(
        !parse_drive_layout(&make(EFI_SYSTEM, 0))
            .unwrap()
            .simple_data_only
    );
    assert!(
        !parse_drive_layout(&make(BASIC_DATA, 1))
            .unwrap()
            .simple_data_only
    );
    assert!(
        !parse_drive_layout(&make(BASIC_DATA, 1_u64 << 63))
            .unwrap()
            .simple_data_only
    );
}
