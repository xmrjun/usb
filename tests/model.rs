use usb_disk_ejector::model::{
    format_capacity, stable_public_id, Device, Disk, Partition, Volume, VolumeExtent,
};

fn sample_device() -> Device {
    Device::new(
        r"USB\VID_1234&PID_5678\SERIAL-A".into(),
        "{11111111-2222-3333-4444-555555555555}".into(),
        "USBSTOR".into(),
        "Example Flash Drive".into(),
        vec![Disk {
            instance_id: r"USBSTOR\DISK&VEN_EXAMPLE\SERIAL-A".into(),
            number: 7,
            name: "Example Flash Drive USB Device".into(),
            serial: "SERIAL-A".into(),
            capacity_bytes: 64 * 1024 * 1024 * 1024,
            partition_count: 2,
            partitions: vec![
                Partition {
                    number: 1,
                    starting_offset: 1_048_576,
                    length: 10_000,
                },
                Partition {
                    number: 2,
                    starting_offset: 20_000,
                    length: 30_000,
                },
            ],
            partition_table_supported: true,
        }],
        vec![
            Volume {
                id: r"\\?\Volume{one}\".into(),
                mount_paths: vec![r"E:\".into()],
                capacity_bytes: Some(20_000_000_000),
                filesystem: Some("exFAT".into()),
                disk_numbers: vec![7],
                extents: vec![VolumeExtent {
                    disk_number: 7,
                    starting_offset: 1_048_576,
                    extent_length: 10_000,
                }],
                mapping_complete: true,
            },
            Volume {
                id: r"\\?\Volume{two}\".into(),
                mount_paths: vec![r"C:\mnt\camera\".into()],
                capacity_bytes: Some(40_000_000_000),
                filesystem: Some("NTFS".into()),
                disk_numbers: vec![7],
                extents: vec![VolumeExtent {
                    disk_number: 7,
                    starting_offset: 20_000,
                    extent_length: 30_000,
                }],
                mapping_complete: true,
            },
        ],
    )
}

#[test]
fn public_id_is_stable_and_case_insensitive() {
    assert_eq!(
        stable_public_id(r"USB\VID_1234&PID_5678\SERIAL-A"),
        stable_public_id(r"usb\vid_1234&pid_5678\serial-a")
    );
    assert!(stable_public_id("device").starts_with("usb-"));
}

#[test]
fn formats_capacity_without_overstating_precision() {
    assert_eq!(format_capacity(0), "0 B");
    assert_eq!(format_capacity(1_500), "1.5 KB");
    assert_eq!(format_capacity(64 * 1024 * 1024 * 1024), "68.7 GB");
}

#[test]
fn exposes_physical_capacity_and_multi_volume_impact() {
    let device = sample_device();
    assert_eq!(device.capacity_bytes(), Some(64 * 1024 * 1024 * 1024));
    assert!(device.impact_summary().contains("Physical disk 7"));
    assert!(device.impact_summary().contains(r"C:\mnt\camera\"));
    assert!(device.impact_summary().contains(r"E:\"));
    assert!(device.impact_summary().contains("20.0 GB logical volume"));
    assert!(device.impact_summary().contains("40.0 GB logical volume"));
}

#[test]
fn volume_capacity_is_not_summed_or_confused_with_physical_capacity() {
    let device = sample_device();
    assert_eq!(device.capacity_bytes(), Some(64 * 1024 * 1024 * 1024));
    assert_eq!(device.volumes[0].capacity_bytes, Some(20_000_000_000));
    assert_eq!(device.volumes[1].capacity_bytes, Some(40_000_000_000));
}

#[test]
fn multiple_physical_disks_do_not_get_reported_as_one_summed_capacity() {
    let mut device = sample_device();
    let mut second = device.disks[0].clone();
    second.number = 8;
    device.disks.push(second);
    assert_eq!(device.capacity_bytes(), None);
}

#[test]
fn fingerprint_changes_when_windows_reuses_a_disk_number() {
    let original = sample_device();
    let mut replacement = sample_device();
    replacement.disks[0].instance_id = r"USBSTOR\OTHER\SERIAL-B".into();
    assert_ne!(original.fingerprint(), replacement.fingerprint());
}

#[test]
fn fingerprint_covers_container_service_serial_and_partition_layout() {
    let original = sample_device();
    let mut container = sample_device();
    container.target_container_id = "{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}".into();
    assert_ne!(original.fingerprint(), container.fingerprint());
    let mut service = sample_device();
    service.target_service = "UASPStor".into();
    assert_ne!(original.fingerprint(), service.fingerprint());
    let mut serial = sample_device();
    serial.disks[0].serial = "OTHER".into();
    assert_ne!(original.fingerprint(), serial.fingerprint());
    let mut partition = sample_device();
    partition.disks[0].partitions[0].starting_offset += 4096;
    assert_ne!(original.fingerprint(), partition.fingerprint());
}

#[test]
fn fingerprint_changes_when_volume_extent_layout_changes() {
    let original = sample_device();
    let mut changed = sample_device();
    changed.volumes[0].extents[0].starting_offset += 4096;
    assert_ne!(original.fingerprint(), changed.fingerprint());
}

#[test]
fn fingerprint_changes_when_logical_volume_capacity_changes() {
    let original = sample_device();
    let mut changed = sample_device();
    changed.volumes[0].capacity_bytes = Some(19_000_000_000);
    assert_ne!(original.fingerprint(), changed.fingerprint());
}

#[test]
fn confirmation_revalidation_compares_target_and_complete_impact() {
    let original = sample_device();
    assert!(original.same_identity_and_impact(&sample_device()));

    let mut changed = sample_device();
    changed.volumes[0].mount_paths = vec![r"F:\".into()];
    assert!(!original.same_identity_and_impact(&changed));
}
