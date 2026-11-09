use usb_disk_ejector::{
    backend::{EjectFailure, EjectResult, VetoKind},
    gui::GuiState,
    model::{Device, Disk, Partition, Volume, VolumeExtent},
    policy::ListedDevice,
};

fn device(id_suffix: &str, mount: &str) -> ListedDevice {
    ListedDevice {
        device: Device::new(
            format!(r"USB\TARGET\{id_suffix}"),
            format!("{{11111111-2222-3333-4444-{id_suffix:0>12}}}"),
            "USBSTOR".into(),
            format!("USB Device {id_suffix}"),
            vec![Disk {
                instance_id: format!(r"USBSTOR\DISK\{id_suffix}"),
                number: 5,
                name: "USB Disk".into(),
                serial: id_suffix.into(),
                capacity_bytes: 16_000_000_000,
                partition_count: 1,
                partitions: vec![Partition {
                    number: 1,
                    starting_offset: 1_048_576,
                    length: 15_000_000_000,
                }],
                partition_table_supported: true,
            }],
            vec![Volume {
                id: format!("volume-{id_suffix}"),
                mount_paths: vec![mount.into()],
                capacity_bytes: Some(15_500_000_000),
                filesystem: Some("exFAT".into()),
                disk_numbers: vec![5],
                extents: vec![VolumeExtent {
                    disk_number: 5,
                    starting_offset: 1_048_576,
                    extent_length: 15_000_000_000,
                }],
                mapping_complete: true,
            }],
        ),
        eligibility: Ok(()),
    }
}

#[test]
fn rows_show_name_mount_capacity_partitions_and_status() {
    let mut state = GuiState::new();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![device("A", r"E:\")]));
    let rows = state.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "USB Device A");
    assert_eq!(rows[0].mounts, r"E:\ [逻辑卷：15.5 GB]");
    assert_eq!(rows[0].capacity, "16.0 GB");
    assert_eq!(rows[0].partitions, "1");
    assert_eq!(rows[0].status, "可以安全弹出");
}

#[test]
fn incomplete_device_stays_visible_with_unknown_capacity_and_cannot_be_ejected() {
    let mut listed = device("A", r"E:\");
    listed.device.disks[0].capacity_bytes = 0;
    listed.eligibility = Err(usb_disk_ejector::policy::BlockReason::InventoryIncomplete);
    let id = listed.device.public_id.clone();
    let mut state = GuiState::new();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed]));

    let rows = state.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].capacity, "未知");
    assert!(rows[0].status.contains("检测信息不完整，暂不允许弹出"));
    assert!(!rows[0].eligible);
    assert!(state.select(&id));
    assert_eq!(state.selected_id(), Some(id.as_str()));
    assert!(!state.is_busy());
    assert!(!state.can_eject());
    assert!(state.confirmation_text().is_none());
    assert!(state.begin_eject().is_none());
}

#[test]
fn refresh_preserves_exact_selection_but_not_a_disappeared_device() {
    let mut state = GuiState::new();
    let first = device("A", r"E:\");
    let id = first.device.public_id.clone();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![first]));
    assert!(state.select(&id));

    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![device("A", r"F:\")]));
    assert_eq!(state.selected_id(), Some(id.as_str()));

    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![device("B", r"G:\")]));
    assert_eq!(state.selected_id(), None);
}

#[test]
fn repeated_actions_are_disabled_and_late_results_are_ignored_after_close() {
    let mut state = GuiState::new();
    let generation = state.begin_refresh().unwrap();
    assert!(state.begin_refresh().is_none());
    state.close();
    state.finish_refresh(generation, Ok(vec![device("A", r"E:\")]));
    assert!(state.rows().is_empty());
    assert!(state.begin_refresh().is_none());
}

#[test]
fn close_is_deferred_while_a_device_operation_is_active() {
    let mut state = GuiState::new();
    let generation = state.begin_refresh().unwrap();
    assert!(!state.request_close());
    assert!(state.status().contains("操作完成后再关闭窗口"));
    state.finish_refresh(generation, Ok(vec![]));
    assert!(state.request_close());
}

#[test]
fn confirmation_and_veto_status_are_precise() {
    let mut state = GuiState::new();
    let listed = device("A", r"E:\");
    let id = listed.device.public_id.clone();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed]));
    state.select(&id);
    let confirmation = state.confirmation_text().unwrap();
    assert!(confirmation.contains("物理磁盘 5"));
    assert!(confirmation.contains(r"E:\"));

    state.finish_eject(Ok(EjectResult::Rejected(EjectFailure {
        config_ret: 23,
        veto: Some((VetoKind::OutstandingOpen, "device-instance".into())),
    })));
    assert!(state.status().contains("设备实例标识符/路径"));
    assert!(!state.status().contains("占用进程"));
}
