use std::collections::BTreeSet;

use usb_disk_ejector::{
    model::{Disk, Partition, PartitionKind, Volume, VolumeExtent},
    policy::{evaluate_inventory, BlockReason, BusType, DiskCandidate, Inventory, Target},
};

fn target() -> Target {
    Target {
        instance_id: r"USB\VID_1234&PID_5678\SERIAL".into(),
        container_id: "{11111111-2222-3333-4444-555555555555}".into(),
        service: "USBSTOR".into(),
        name: "Safe USB".into(),
        removable: true,
        topology_complete: true,
        is_hub: false,
        has_non_storage_descendants: false,
        parent_is_valid_hub: true,
        descendant_disk_instance_ids: BTreeSet::from([r"USBSTOR\DISK\SERIAL-4".into()]),
    }
}

fn disk(number: u32) -> DiskCandidate {
    DiskCandidate {
        disk: Disk {
            instance_id: format!(r"USBSTOR\DISK\SERIAL-{number}"),
            number,
            name: "Safe USB Disk".into(),
            serial: format!("SERIAL-{number}"),
            capacity_bytes: 8_000_000_000,
            partition_count: 1,
            partitions: vec![Partition {
                number: 1,
                starting_offset: 1_048_576,
                length: 4_000_000,
                kind: PartitionKind::BasicData,
                gpt_attributes: 0,
            }],
            partition_table_supported: true,
            volume_preparation_supported: true,
        },
        bus_type: BusType::Usb,
        target: Some(target()),
    }
}

fn volume(disks: Vec<u32>) -> Volume {
    Volume {
        id: r"\\?\Volume{safe}\".into(),
        mount_paths: vec![r"E:\".into(), r"C:\mnt\usb\".into()],
        capacity_bytes: Some(31_000_000_000),
        filesystem: Some("exFAT".into()),
        disk_numbers: disks,
        extents: vec![VolumeExtent {
            disk_number: 4,
            starting_offset: 1_048_576,
            extent_length: 4_000_000,
        }],
        mapping_complete: true,
    }
}

fn inventory() -> Inventory {
    Inventory {
        disks: vec![disk(4)],
        volumes: vec![volume(vec![4])],
        critical_disk_numbers: BTreeSet::from([0]),
        critical_scan_complete: true,
        virtual_dependency_scan_complete: true,
        virtual_host_volume_ids: BTreeSet::new(),
        complete: true,
    }
}

fn msr_gpt_inventory() -> Inventory {
    // PhysicalDrive2's observed layout: one MSR and three NTFS data partitions.
    const MSR: [u8; 16] = [
        0x16, 0xe3, 0xc9, 0xe3, 0x5c, 0x0b, 0xb8, 0x4d, 0x81, 0x7d, 0xf9, 0x2d, 0xf0, 0x02, 0x15,
        0xae,
    ];
    const BASIC: [u8; 16] = [
        0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99,
        0xc7,
    ];
    let records = [
        (17_408_i64, 16_759_808_i64, MSR),
        (16_777_216, 629_145_600_000, BASIC),
        (629_162_377_216, 644_245_094_400, BASIC),
        (1_273_407_471_616, 726_990_323_712, BASIC),
    ];
    let mut bytes = vec![0_u8; 48 + records.len() * 144];
    bytes[0..4].copy_from_slice(&1_u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&(records.len() as u32).to_le_bytes());
    for (index, (start, length, kind)) in records.iter().enumerate() {
        let offset = 48 + index * 144;
        bytes[offset..offset + 4].copy_from_slice(&1_u32.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&start.to_le_bytes());
        bytes[offset + 16..offset + 24].copy_from_slice(&length.to_le_bytes());
        bytes[offset + 24..offset + 28].copy_from_slice(&((index + 1) as u32).to_le_bytes());
        bytes[offset + 32..offset + 48].copy_from_slice(kind);
    }
    let layout = usb_disk_ejector::raw::parse_drive_layout(&bytes).unwrap();
    let mut candidate = inventory();
    let disk = &mut candidate.disks[0].disk;
    disk.capacity_bytes = 2_000_400_000_000;
    disk.partition_count = layout.partitions.len() as u32;
    disk.partition_table_supported = layout.ordinary_pnp_supported;
    disk.volume_preparation_supported = layout.volume_preparation_supported;
    disk.partitions = layout
        .partitions
        .into_iter()
        .map(|partition| Partition {
            number: partition.number,
            starting_offset: partition.starting_offset,
            length: partition.length,
            kind: partition.kind,
            gpt_attributes: partition.gpt_attributes,
        })
        .collect();
    candidate.volumes = disk.partitions[1..]
        .iter()
        .map(|partition| {
            let mut mapped = volume(vec![disk.number]);
            mapped.id = format!("volume-{}", partition.number);
            mapped.mount_paths = vec![format!("{}:\\", (b'E' + partition.number as u8) as char)];
            mapped.capacity_bytes = Some(partition.length as u64 - 4096);
            mapped.filesystem = Some("NTFS".into());
            mapped.extents[0].starting_offset = partition.starting_offset;
            mapped.extents[0].extent_length = partition.length;
            mapped
        })
        .collect();
    candidate
}

#[test]
fn known_msr_without_a_volume_allows_ordinary_pnp_for_three_fully_mapped_data_partitions() {
    let listed = evaluate_inventory(msr_gpt_inventory());
    assert_eq!(listed[0].eligibility, Ok(()));
    assert_eq!(listed[0].device.partition_count(), 4);
    assert_eq!(listed[0].device.volumes.len(), 3);
    assert!(!listed[0].device.supports_volume_preparation());
}

#[test]
fn msr_exception_does_not_hide_missing_data_unknown_roles_or_invalid_ranges() {
    for case in 0..10 {
        let mut candidate = msr_gpt_inventory();
        match case {
            0 => {
                candidate.volumes.pop();
            }
            1 => candidate.volumes[0].extents[0].extent_length -= 4096,
            2 => candidate.disks[0].disk.partitions[0].kind = PartitionKind::Unsupported,
            3 => candidate.disks[0].disk.partitions[0].gpt_attributes = 1,
            4 => candidate.disks[0].disk.partitions[0].length += 4096,
            5 => candidate.disks[0].disk.capacity_bytes = 1_000_000_000,
            6 => candidate.disks[0].disk.partitions[1].number = 1,
            7 => candidate.disks[0].disk.partitions[0].length = i64::MAX,
            8 => {
                let msr = &candidate.disks[0].disk.partitions[0];
                let mut unexpected = candidate.volumes[0].clone();
                unexpected.id = "unexpected-msr-volume".into();
                unexpected.extents[0].starting_offset = msr.starting_offset;
                unexpected.extents[0].extent_length = msr.length;
                candidate.volumes.push(unexpected);
            }
            9 => {
                candidate.disks[0].disk.partitions.truncate(1);
                candidate.disks[0].disk.partition_count = 1;
                candidate.volumes.clear();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            evaluate_inventory(candidate)[0].eligibility,
            Err(BlockReason::PartitionMappingIncomplete),
            "case {case}"
        );
    }
}

#[test]
fn msr_layout_still_requires_system_and_virtual_dependency_checks() {
    let mut system = msr_gpt_inventory();
    system.critical_disk_numbers.insert(4);
    assert_eq!(
        evaluate_inventory(system)[0].eligibility,
        Err(BlockReason::SystemDisk)
    );
    let mut unknown = msr_gpt_inventory();
    unknown.critical_scan_complete = false;
    assert_eq!(
        evaluate_inventory(unknown)[0].eligibility,
        Err(BlockReason::CriticalUseUnknown)
    );
    let mut virtual_host = msr_gpt_inventory();
    virtual_host.virtual_host_volume_ids.insert("volume-2".into());
    assert_eq!(
        evaluate_inventory(virtual_host)[0].eligibility,
        Err(BlockReason::MountedVirtualDiskDependency)
    );
}

#[test]
fn eligible_device_contains_all_mount_paths() {
    let devices = evaluate_inventory(inventory());
    assert_eq!(devices.len(), 1);
    assert!(devices[0].eligibility.is_ok());
    assert_eq!(devices[0].device.volumes[0].mount_paths.len(), 2);
}

#[test]
fn active_fat32_mbr_with_complete_mapping_can_use_ordinary_pnp_only() {
    // The observed ESD-USB layout has one active FAT32-LBA partition and
    // three unused MBR slots. Filesystem size is smaller than its raw extent.
    let offset = 1_048_576_i64;
    let length = 34_359_738_368_i64;
    let mut bytes = vec![0_u8; 48 + 4 * 144];
    bytes[4..8].copy_from_slice(&4_u32.to_le_bytes());
    bytes[56..64].copy_from_slice(&offset.to_le_bytes());
    bytes[64..72].copy_from_slice(&length.to_le_bytes());
    bytes[72..76].copy_from_slice(&1_u32.to_le_bytes());
    bytes[80] = 0x0c;
    bytes[81] = 1;
    bytes[82] = 1;
    let layout = usb_disk_ejector::raw::parse_drive_layout(&bytes).unwrap();
    assert!(layout.simple_data_only);
    assert!(!layout.volume_preparation_supported);

    let mut candidate = inventory();
    let disk = &mut candidate.disks[0].disk;
    disk.capacity_bytes = 62_000_000_000;
    disk.partition_count = layout.partitions.len() as u32;
    disk.partition_table_supported = layout.ordinary_pnp_supported;
    disk.volume_preparation_supported = layout.volume_preparation_supported;
    disk.partitions = layout
        .partitions
        .into_iter()
        .map(|partition| Partition {
            number: partition.number,
            starting_offset: partition.starting_offset,
            length: partition.length,
            kind: partition.kind,
            gpt_attributes: partition.gpt_attributes,
        })
        .collect();
    candidate.volumes[0].capacity_bytes = Some(34_347_155_456);
    candidate.volumes[0].filesystem = Some("FAT32".into());
    candidate.volumes[0].extents[0].starting_offset = offset;
    candidate.volumes[0].extents[0].extent_length = length;

    let listed = evaluate_inventory(candidate.clone());
    assert_eq!(listed[0].eligibility, Ok(()));
    assert!(!listed[0].device.supports_volume_preparation());

    let mut critical = candidate.clone();
    critical.critical_disk_numbers.insert(4);
    assert_eq!(
        evaluate_inventory(critical)[0].eligibility,
        Err(BlockReason::SystemDisk)
    );
    let mut unknown_critical = candidate.clone();
    unknown_critical.critical_scan_complete = false;
    assert_eq!(
        evaluate_inventory(unknown_critical)[0].eligibility,
        Err(BlockReason::CriticalUseUnknown)
    );
    let mut mismatched = candidate.clone();
    mismatched.volumes[0].extents[0].extent_length -= 4096;
    assert_eq!(
        evaluate_inventory(mismatched)[0].eligibility,
        Err(BlockReason::PartitionMappingIncomplete)
    );
    candidate.volumes.clear();
    assert_eq!(
        evaluate_inventory(candidate)[0].eligibility,
        Err(BlockReason::PartitionMappingIncomplete)
    );
}

#[test]
fn rejects_incomplete_inventory_and_extent_mapping() {
    let mut incomplete = inventory();
    incomplete.complete = false;
    assert_eq!(
        evaluate_inventory(incomplete)[0].eligibility,
        Err(BlockReason::InventoryIncomplete)
    );

    let mut bad_extent = inventory();
    bad_extent.volumes[0].mapping_complete = false;
    assert_eq!(
        evaluate_inventory(bad_extent)[0].eligibility,
        Err(BlockReason::VolumeMappingIncomplete)
    );

    let mut inconsistent_extent = inventory();
    inconsistent_extent.volumes[0].extents[0].disk_number = 99;
    assert_eq!(
        evaluate_inventory(inconsistent_extent)[0].eligibility,
        Err(BlockReason::VolumeMappingIncomplete)
    );
}

#[test]
fn rejects_cross_disk_volume_and_system_disk_overlap() {
    let mut cross_disk = inventory();
    cross_disk.volumes[0].disk_numbers = vec![4, 9];
    cross_disk.volumes[0].extents.push(VolumeExtent {
        disk_number: 9,
        starting_offset: 1_048_576,
        extent_length: 2_000_000,
    });
    assert_eq!(
        evaluate_inventory(cross_disk)[0].eligibility,
        Err(BlockReason::CrossDiskVolume)
    );

    let mut system = inventory();
    system.critical_disk_numbers.insert(4);
    assert_eq!(
        evaluate_inventory(system)[0].eligibility,
        Err(BlockReason::SystemDisk)
    );
}

#[test]
fn rejects_missing_partition_volume_unsupported_layout_and_unknown_filesystem() {
    let mut no_volume = inventory();
    no_volume.volumes.clear();
    assert_eq!(
        evaluate_inventory(no_volume)[0].eligibility,
        Err(BlockReason::PartitionMappingIncomplete)
    );

    let mut unsupported_layout = inventory();
    unsupported_layout.disks[0].disk.partition_table_supported = false;
    assert_eq!(
        evaluate_inventory(unsupported_layout)[0].eligibility,
        Err(BlockReason::PartitionMappingIncomplete)
    );

    let mut raw = inventory();
    raw.volumes[0].filesystem = None;
    assert_eq!(
        evaluate_inventory(raw)[0].eligibility,
        Err(BlockReason::UnsupportedFilesystem)
    );
}

#[test]
fn rejects_when_critical_storage_scan_is_incomplete() {
    let mut candidate = inventory();
    candidate.critical_scan_complete = false;
    assert_eq!(
        evaluate_inventory(candidate)[0].eligibility,
        Err(BlockReason::CriticalUseUnknown)
    );
}

#[test]
fn rejects_when_virtual_disk_dependencies_cannot_be_proven() {
    let mut candidate = inventory();
    candidate.virtual_dependency_scan_complete = false;
    assert_eq!(
        evaluate_inventory(candidate)[0].eligibility,
        Err(BlockReason::VirtualDiskDependencyUnknown)
    );
}

#[test]
fn rejects_when_an_attached_virtual_disk_is_backed_by_the_usb_volume() {
    let mut candidate = inventory();
    candidate
        .virtual_host_volume_ids
        .insert(r"\\?\VOLUME{SAFE}".into());
    assert_eq!(
        evaluate_inventory(candidate)[0].eligibility,
        Err(BlockReason::MountedVirtualDiskDependency)
    );
}

#[test]
fn rejects_incomplete_hub_or_composite_topology() {
    let mut cases = vec![
        (BlockReason::TopologyIncomplete, target()),
        (BlockReason::HubTarget, target()),
        (BlockReason::CompositeDevice, target()),
    ];
    cases[0].1.topology_complete = false;
    cases[1].1.is_hub = true;
    cases[2].1.has_non_storage_descendants = true;

    for (expected, bad_target) in cases {
        let mut candidate = inventory();
        candidate.disks[0].target = Some(bad_target);
        assert_eq!(evaluate_inventory(candidate)[0].eligibility, Err(expected));
    }
}

#[test]
fn rejects_unknown_transport_missing_container_or_unproven_parent_hub() {
    for mutate in [0, 1, 2] {
        let mut candidate = inventory();
        let target = candidate.disks[0].target.as_mut().unwrap();
        match mutate {
            0 => target.service = "usbccgp".into(),
            1 => target.container_id.clear(),
            2 => target.parent_is_valid_hub = false,
            _ => unreachable!(),
        }
        assert_eq!(
            evaluate_inventory(candidate)[0].eligibility,
            Err(BlockReason::TopologyIncomplete)
        );
    }
}

#[test]
fn rejects_when_a_descendant_disk_is_missing_from_interface_inventory() {
    let mut candidate = inventory();
    candidate.disks[0]
        .target
        .as_mut()
        .unwrap()
        .descendant_disk_instance_ids
        .insert(r"USBSTOR\DISK\UNENUMERATED".into());
    assert_eq!(
        evaluate_inventory(candidate)[0].eligibility,
        Err(BlockReason::TopologyIncomplete)
    );
}

#[test]
fn rejects_non_removable_or_missing_target() {
    let mut not_removable = inventory();
    not_removable.disks[0].target.as_mut().unwrap().removable = false;
    assert_eq!(
        evaluate_inventory(not_removable)[0].eligibility,
        Err(BlockReason::NotRemovable)
    );

    let mut missing = inventory();
    missing.disks[0].target = None;
    assert_eq!(
        evaluate_inventory(missing)[0].eligibility,
        Err(BlockReason::TargetMissing)
    );
}

#[test]
fn does_not_list_non_usb_disks() {
    let mut internal = inventory();
    internal.disks[0].bus_type = BusType::Other(17);
    assert!(evaluate_inventory(internal).is_empty());
}
