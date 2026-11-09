use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Request_Device_EjectW, PNP_VetoDevice, PNP_VetoDriver, PNP_VetoIllegalDeviceRequest,
    PNP_VetoInsufficientPower, PNP_VetoInsufficientRights, PNP_VetoLegacyDevice,
    PNP_VetoLegacyDriver, PNP_VetoNonDisableable, PNP_VetoOutstandingOpen, PNP_VetoPendingClose,
    PNP_VetoWindowsApp, PNP_VetoWindowsService, CR_SUCCESS,
};

use crate::{
    backend::{BackendError, Cancellation, EjectFailure, EjectResult, VetoKind},
    model::Device,
};

use super::inventory::{confirm_target_identity, locate_revalidated_target, snapshot};

const VETO_NAME_CAPACITY: usize = 260;

pub(super) fn request_eject(
    expected: &Device,
    cancellation: &Cancellation,
) -> Result<EjectResult, BackendError> {
    if cancellation.is_cancelled() {
        return Err(BackendError(
            "在重新验证前已取消安全弹出".into(),
        ));
    }
    let fresh = snapshot()?;
    let matches = fresh
        .listed
        .iter()
        .filter(|item| item.device.public_id == expected.public_id)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(BackendError(
            "确认后设备已断开，或无法再唯一确定所选设备".into(),
        ));
    }
    let current = matches[0];
    if let Err(reason) = &current.eligibility {
        return Err(BackendError(format!(
            "确认后设备已不满足安全弹出条件：{reason}"
        )));
    }
    if !expected.same_identity_and_impact(&current.device) {
        return Err(BackendError(
            "确认后设备标识、磁盘编号、逻辑卷、磁盘范围或挂载路径发生了变化"
                .into(),
        ));
    }
    let devinst = locate_revalidated_target(&current.device)?;

    if cancellation.is_cancelled() {
        return Err(BackendError(
            "提交 PnP 请求前已取消安全弹出".into(),
        ));
    }
    confirm_target_identity(devinst, &current.device.target_instance_id)?;
    if cancellation.is_cancelled() {
        return Err(BackendError(
            "在提交 PnP 请求前的最后检查中取消了安全弹出".into(),
        ));
    }

    let mut veto_type = 0_i32;
    let mut veto_name = vec![0_u16; VETO_NAME_CAPACITY];
    // SAFETY: devinst was resolved in the immediately preceding fresh snapshot;
    // veto outputs are valid writable buffers, and flags must be zero.
    let result = unsafe {
        CM_Request_Device_EjectW(
            devinst,
            &mut veto_type,
            veto_name.as_mut_ptr(),
            veto_name.len() as u32,
            0,
        )
    };
    if result == CR_SUCCESS {
        return Ok(EjectResult::Ejected);
    }
    let end = veto_name
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(veto_name.len());
    let name = String::from_utf16_lossy(&veto_name[..end]);
    Ok(EjectResult::Rejected(EjectFailure {
        config_ret: result,
        veto: Some((map_veto(veto_type), name)),
    }))
}

#[allow(non_upper_case_globals)]
fn map_veto(value: i32) -> VetoKind {
    match value {
        PNP_VetoLegacyDevice => VetoKind::LegacyDevice,
        PNP_VetoPendingClose => VetoKind::PendingClose,
        PNP_VetoWindowsApp => VetoKind::WindowsApp,
        PNP_VetoWindowsService => VetoKind::WindowsService,
        PNP_VetoOutstandingOpen => VetoKind::OutstandingOpen,
        PNP_VetoDevice => VetoKind::Device,
        PNP_VetoDriver => VetoKind::Driver,
        PNP_VetoIllegalDeviceRequest => VetoKind::IllegalDeviceRequest,
        PNP_VetoInsufficientPower => VetoKind::InsufficientPower,
        PNP_VetoNonDisableable => VetoKind::NonDisableable,
        PNP_VetoLegacyDriver => VetoKind::LegacyDriver,
        PNP_VetoInsufficientRights => VetoKind::InsufficientRights,
        _ => VetoKind::Unknown,
    }
}
