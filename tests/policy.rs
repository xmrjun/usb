use std::collections::BTreeSet;

use usb_disk_ejector::{
    model::{Disk, Partition, Volume, VolumeExtent},
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
            }],
            partition_table_supported: true,
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

#[test]
fn eligible_device_contains_all_mount_paths() {
    let devices = evaluate_inventory(inventory());
    assert_eq!(devices.len(), 1);
    assert!(devices[0].eligibility.is_ok());
    assert_eq!(devices[0].device.volumes[0].mount_paths.len(), 2);
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
