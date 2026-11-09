use usb_disk_ejector::{
    backend::{BackendError, EjectFailure, EjectResult, VetoKind},
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
    assert_eq!(rows[0].status, "就绪（尚未弹出）");
    assert!(rows[0].eligible);
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

    assert!(state.begin_eject().is_some());
    state.finish_eject(Ok(EjectResult::Rejected(EjectFailure {
        config_ret: 23,
        veto: Some((VetoKind::OutstandingOpen, "device-instance".into())),
    })));
    assert!(state.status().contains("设备实例标识符/路径"));
    assert!(!state.status().contains("占用进程"));
}

#[test]
fn confirmation_reserves_the_operation_and_ejects_only_the_displayed_snapshot() {
    let mut state = GuiState::new();
    let listed = device("A", r"E:\");
    let confirmed = listed.device.clone();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed, device("B", r"G:\")]));
    assert!(state.select(&confirmed.public_id));

    let text = state.begin_confirmation().unwrap();
    assert!(text.contains(r"E:\"));
    assert!(state.is_busy());
    assert!(state.begin_refresh().is_none());
    assert!(state.begin_eject().is_none());
    assert!(state.begin_confirmation().is_none());
    assert!(!state.select(&device("B", r"G:\").device.public_id));

    // A queued result processed inside MessageBox must not replace the device
    // shown in the dialog or release the reservation.
    state.finish_refresh(generation, Ok(vec![device("A", r"F:\")]));
    assert!(state.is_busy());
    assert_eq!(state.finish_confirmation(true), Some(confirmed));
    assert!(state.is_busy());
    assert!(state.begin_refresh().is_none());

    // The same stale callback must not clear a running eject either.
    state.finish_refresh(generation, Err(BackendError("late refresh".into())));
    assert!(state.is_busy());
    assert_eq!(state.rows().len(), 2);
    assert!(state.rows()[0].mounts.contains(r"E:\"));
    assert!(state.begin_eject().is_none());
    state.finish_eject(Err(BackendError("test rejection".into())));
    assert!(!state.is_busy());
}

#[test]
fn cancelling_confirmation_restores_selection_and_allows_future_refresh() {
    let mut state = GuiState::new();
    let listed = device("A", r"E:\");
    let id = listed.device.public_id.clone();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed]));
    assert!(state.select(&id));

    assert!(state.begin_confirmation().is_some());
    assert!(state.finish_confirmation(false).is_none());
    assert_eq!(state.status(), "已取消安全弹出。");
    assert!(!state.is_busy());
    assert!(state.can_eject());
    assert_eq!(state.selected_id(), Some(id.as_str()));
    assert!(state.finish_confirmation(true).is_none());
    assert!(state.begin_refresh().is_some());
}

#[test]
fn direct_eject_cannot_overlap_refresh_or_start_after_close() {
    let mut state = GuiState::new();
    let listed = device("A", r"E:\");
    let id = listed.device.public_id.clone();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed.clone()]));
    assert!(state.select(&id));

    let active_refresh = state.begin_refresh().unwrap();
    assert!(state.begin_eject().is_none());
    assert!(state.begin_confirmation().is_none());
    state.finish_eject(Ok(EjectResult::Ejected));
    assert!(state.is_busy());
    state.finish_refresh(generation, Ok(vec![]));
    assert!(state.is_busy());
    state.finish_refresh(active_refresh, Ok(vec![listed]));
    assert!(state.can_eject());

    state.close();
    assert!(state.begin_eject().is_none());
    assert!(state.begin_confirmation().is_none());
    assert!(state.finish_confirmation(true).is_none());
}

#[test]
fn external_removal_suppresses_refresh_and_invalidates_an_open_confirmation() {
    let mut state = GuiState::new();
    let listed = device("A", r"E:\");
    let id = listed.device.public_id.clone();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed]));
    assert!(state.select(&id));
    assert!(state.begin_confirmation().is_some());

    state.removal_started();
    assert!(state.removal_pending());
    assert!(!state.can_refresh());
    assert!(state.begin_refresh().is_none());
    assert!(state.finish_confirmation(true).is_none());
    assert!(state.status().contains("取消本次弹出确认"));
    assert!(!state.can_eject());
    state.removal_finished();
    assert!(state.can_refresh());
    assert!(state.begin_refresh().is_some());
}

#[test]
fn operation_details_survive_refresh_and_cancelled_confirmation_until_a_new_eject() {
    let mut state = GuiState::new();
    let listed = device("A", r"E:\");
    let id = listed.device.public_id.clone();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed.clone()]));
    assert!(state.select(&id));
    assert!(state.begin_eject().is_some());
    state.set_operation_details(Some("volume lock failed: Win32 32".into()));
    state.finish_eject(Err(BackendError("test rejection".into())));
    assert!(state.status().contains("未能完成安全弹出"));

    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed]));
    assert!(state.begin_confirmation().is_some());
    assert!(state.finish_confirmation(false).is_none());
    assert_eq!(
        state.operation_details(),
        Some("volume lock failed: Win32 32")
    );

    assert!(state.begin_confirmation().is_some());
    assert!(state.finish_confirmation(true).is_some());
    assert_eq!(state.operation_details(), None);
    state.set_operation_details(Some("second operation".into()));
    state.finish_eject(Err(BackendError("test rejection".into())));
    assert!(state.begin_eject().is_some());
    assert_eq!(state.operation_details(), None);
}

#[test]
fn details_dialog_reserves_state_and_keeps_the_diagnostic_snapshot() {
    let mut state = GuiState::new();
    state.set_operation_details(Some("original diagnostics".into()));
    assert_eq!(state.begin_details().as_deref(), Some("original diagnostics"));
    assert!(state.begin_refresh().is_none());
    assert!(!state.begin_elevation());
    assert!(!state.request_close());
    state.finish_details();
    assert!(!state.is_busy());
    assert_eq!(state.operation_details(), Some("original diagnostics"));

    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Err(BackendError("refresh failed".into())));
    assert_eq!(state.operation_details(), Some("original diagnostics"));
    state.close();
    state.set_operation_details(Some("late callback".into()));
    assert_eq!(state.operation_details(), Some("original diagnostics"));
    assert!(state.begin_details().is_none());
}

#[test]
fn elevation_reserves_the_window_and_success_prevents_queued_work() {
    let mut state = GuiState::new();
    assert!(state.begin_elevation());
    assert!(!state.begin_elevation());
    assert!(state.begin_refresh().is_none());
    assert!(state.begin_eject().is_none());
    assert!(!state.request_close());
    assert!(!state.finish_elevation(Err(BackendError("UAC cancelled".into()))));
    assert!(!state.is_busy());
    assert!(state.can_refresh());

    assert!(state.begin_elevation());
    assert!(state.finish_elevation(Ok(())));
    assert!(state.request_close());
    assert!(state.begin_refresh().is_none());
    assert!(!state.begin_elevation());
}

#[test]
fn background_device_changes_update_rows_without_erasing_the_eject_failure() {
    let mut state = GuiState::new();
    let listed = device("A", r"G:\");
    let id = listed.device.public_id.clone();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed]));
    assert!(state.select(&id));
    assert!(state.begin_eject().is_some());
    assert!(state.begin_background_refresh().is_none());
    state.set_operation_details(Some("CONFIGRET=23; veto=5; name=test-volume".into()));
    state.finish_eject(Ok(EjectResult::Rejected(EjectFailure {
        config_ret: 23,
        veto: Some((VetoKind::OutstandingOpen, "test-volume".into())),
    })));
    let failure = state.status().to_owned();

    // Removal notifications must not temporarily replace the rejection either.
    state.removal_started();
    assert_eq!(state.status(), failure);
    state.removal_finished();
    let background = state.begin_background_refresh().unwrap();
    assert_eq!(state.status(), failure);
    state.finish_refresh(background, Ok(vec![device("B", r"H:\")]));
    assert_eq!(state.status(), failure);
    assert_eq!(state.selected_id(), None);
    assert_eq!(state.rows()[0].name, "USB Device B");
    assert_eq!(
        state.operation_details(),
        Some("CONFIGRET=23; veto=5; name=test-volume")
    );
}

#[test]
fn failed_background_refresh_preserves_the_operation_error_until_manual_refresh() {
    let mut state = GuiState::new();
    let listed = device("A", r"G:\");
    let id = listed.device.public_id.clone();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed.clone()]));
    state.select(&id);
    assert!(state.begin_eject().is_some());
    state.set_operation_details(Some("FSCTL_LOCK_VOLUME failed: Win32 32".into()));
    state.finish_eject(Err(BackendError("锁卷失败 Win32 32".into())));

    let background = state.begin_background_refresh().unwrap();
    state.finish_refresh(background, Err(BackendError("枚举失败 Win32 5".into())));
    assert!(state.status().contains("锁卷失败 Win32 32"));
    assert!(state.status().contains("枚举失败 Win32 5"));
    assert!(state.rows().is_empty());
    assert_eq!(
        state.operation_details(),
        Some("FSCTL_LOCK_VOLUME failed: Win32 32")
    );

    let manual = state.begin_refresh().unwrap();
    state.finish_refresh(manual, Ok(vec![listed]));
    assert_eq!(state.status(), "已发现 1 个 USB 物理设备。");
    assert_eq!(
        state.operation_details(),
        Some("FSCTL_LOCK_VOLUME failed: Win32 32")
    );
}

#[test]
fn successful_eject_keeps_its_result_when_the_completion_refresh_removes_the_row() {
    let mut state = GuiState::new();
    let listed = device("A", r"G:\");
    let id = listed.device.public_id.clone();
    let generation = state.begin_refresh().unwrap();
    state.finish_refresh(generation, Ok(vec![listed]));
    state.select(&id);
    assert!(state.begin_eject().is_some());
    state.finish_eject(Ok(EjectResult::Ejected));
    let background = state.begin_background_refresh().unwrap();
    state.finish_refresh(background, Ok(vec![]));
    assert!(state.rows().is_empty());
    assert!(!state.can_eject());
    assert_eq!(state.status(), "Windows 已接受安全弹出请求。");
}
