use std::fmt::Write;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Request_Device_EjectW, PNP_VetoDevice, PNP_VetoDriver, PNP_VetoIllegalDeviceRequest,
    PNP_VetoInsufficientPower, PNP_VetoInsufficientRights, PNP_VetoLegacyDevice,
    PNP_VetoLegacyDriver, PNP_VetoNonDisableable, PNP_VetoOutstandingOpen, PNP_VetoPendingClose,
    PNP_VetoWindowsApp, PNP_VetoWindowsService, CR_SUCCESS,
};

use crate::{
    backend::{BackendError, Cancellation, EjectFailure, EjectResult, VetoKind},
    eject_retry::{request_with_retry, RETRY_TIME_BUDGET, SLOW_REQUEST_THRESHOLD},
    model::Device,
    volume_prepare::prepare_volumes,
};

use super::{
    inventory::{confirm_target_identity, locate_revalidated_target, snapshot},
    volume_prepare::NativePreparation,
};

const VETO_NAME_CAPACITY: usize = 260;
const CR_REMOVE_VETOED: u32 = 0x17;

pub(super) fn request_eject(
    expected: &Device,
    cancellation: &Cancellation,
    diagnostics: &mut String,
) -> Result<EjectResult, BackendError> {
    let started = Instant::now();
    trace(
        diagnostics,
        started,
        &format!(
            "开始安全弹出; build={}; target={}; service={}; fingerprint={}",
            option_env!("USB_EJECTOR_BUILD_ID").unwrap_or("development"),
            expected.target_instance_id,
            expected.target_service,
            expected.fingerprint()
        ),
    );
    for volume in &expected.volumes {
        trace(
            diagnostics,
            started,
            &format!(
                "确认影响范围; volume={}; mounts={:?}; extents={:?}",
                volume.id, volume.mount_paths, volume.extents
            ),
        );
    }
    let result = request_eject_inner(expected, cancellation, diagnostics, started);
    trace(
        diagnostics,
        started,
        &format!("安全弹出结束; result={result:?}"),
    );
    result
}

fn request_eject_inner(
    expected: &Device,
    cancellation: &Cancellation,
    diagnostics: &mut String,
    started: Instant,
) -> Result<EjectResult, BackendError> {
    validate_snapshot(expected, cancellation, diagnostics, started)?;
    let ordinary = request_sequence(expected, cancellation, diagnostics, started, "普通弹出")?;
    if !needs_volume_preparation(&ordinary) {
        return Ok(ordinary);
    }
    if !expected.supports_volume_preparation() {
        trace(
            diagnostics,
            started,
            "该设备仅支持普通 Windows 安全弹出；已跳过管理员卷准备，保留原始 Windows 拒绝结果。",
        );
        return Ok(ordinary);
    }
    trace(
        diagnostics,
        started,
        "普通弹出的有限重试仍被打开句柄/待关闭操作拒绝；检查管理员权限。",
    );
    let elevated = super::elevation::is_elevated();
    trace(
        diagnostics,
        started,
        &format!("管理员权限检查; result={elevated:?}"),
    );
    if !elevated? {
        return Err(BackendError(
            "普通弹出仍被占用，请用管理员模式重试。原始 Windows 拒绝详情已保留。".into(),
        ));
    }
    // Revalidate the complete confirmed impact immediately before opening any
    // write-capable volume handles. A failed ordinary request may have changed it.
    validate_snapshot(expected, cancellation, diagnostics, started)?;
    trace(
        diagnostics,
        started,
        "开始受控卷准备；只有所有卷锁定成功后才会刷新和卸载。",
    );
    {
        let mut preparation = NativePreparation::new(expected, cancellation, diagnostics, started);
        prepare_volumes(&expected.volumes, cancellation, &mut preparation)?;
    }
    trace(
        diagnostics,
        started,
        "所有卷准备句柄已确认关闭；此后只复验 PnP 身份/拓扑，不再打开或扫描逻辑卷。",
    );
    request_sequence(
        expected,
        cancellation,
        diagnostics,
        started,
        "卷准备后弹出",
    )
}

fn validate_snapshot(
    expected: &Device,
    cancellation: &Cancellation,
    diagnostics: &mut String,
    started: Instant,
) -> Result<(), BackendError> {
    if cancellation.is_cancelled() {
        return Err(BackendError("在重新验证前已取消安全弹出".into()));
    }
    let began = Instant::now();
    trace(
        diagnostics,
        started,
        "开始完整磁盘/卷/关键存储用途快照复验。",
    );
    let fresh = snapshot()?;
    trace(
        diagnostics,
        started,
        &format!(
            "完整快照已返回纯数据，查询句柄作用域已结束; duration_ms={}",
            began.elapsed().as_millis()
        ),
    );
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
            "确认后设备标识、磁盘编号、逻辑卷、磁盘范围或挂载路径发生了变化".into(),
        ));
    }
    if cancellation.is_cancelled() {
        return Err(BackendError("在快照复验后已取消安全弹出".into()));
    }
    trace(
        diagnostics,
        started,
        "完整快照与确认时的身份和影响范围相同，安全策略通过。",
    );
    Ok(())
}

fn request_sequence(
    expected: &Device,
    cancellation: &Cancellation,
    diagnostics: &mut String,
    started: Instant,
    phase: &str,
) -> Result<EjectResult, BackendError> {
    // The full snapshot above has released its disk/volume handles. Between
    // attempts, inspect only PnP properties so delayed closes can settle without
    // opening the volumes again. The entire removable target and its descendant
    // disk identities must still match the user's confirmed device each time.
    trace(
        diagnostics,
        started,
        &format!(
            "{phase}; 仅快速短暂拒绝重试; slow_request_ms={}; retry_budget_ms={}; 同步 Windows 请求不能中途超时取消",
            SLOW_REQUEST_THRESHOLD.as_millis(),
            RETRY_TIME_BUDGET.as_millis()
        ),
    );
    let mut attempt = 0;
    let result = request_with_retry(
        cancellation,
        || {
            let began = Instant::now();
            let devinst = locate_revalidated_target(expected)?;
            confirm_target_identity(devinst, &expected.target_instance_id)?;
            Ok((devinst, began.elapsed().as_millis()))
        },
        |(devinst, validation_ms)| {
            attempt += 1;
            trace(
                diagnostics,
                started,
                &format!(
                    "{phase}; attempt={attempt}; target={}; devinst={devinst}; PnP_validation_ms={validation_ms}",
                    expected.target_instance_id
                ),
            );
            request_once(devinst, diagnostics, started)
        },
        std::thread::sleep,
    );
    if let Err(problem) = &result {
        trace(diagnostics, started, &format!("{phase} 阶段停止：{problem}"));
    } else {
        trace(
            diagnostics,
            started,
            &format!("{phase} 阶段结束; attempts={attempt}"),
        );
    }
    result
}

fn request_once(
    devinst: u32,
    diagnostics: &mut String,
    started: Instant,
) -> Result<(EjectResult, Duration), BackendError> {
    let mut veto_type = -1_i32;
    let mut veto_name = vec![0_u16; VETO_NAME_CAPACITY];
    let began = Instant::now();
    // SAFETY: devinst was resolved and revalidated immediately before this call;
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
    let duration = began.elapsed();
    if result == CR_SUCCESS {
        // Veto outputs are meaningful only on failure; success is CONFIGRET alone.
        trace(
            diagnostics,
            started,
            &format!(
                "CM_Request_Device_EjectW; CONFIGRET=0 (0x00000000); duration_ms={}; 成功",
                duration.as_millis()
            ),
        );
        return Ok((EjectResult::Ejected, duration));
    }
    let end = veto_name
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(veto_name.len());
    let name = String::from_utf16_lossy(&veto_name[..end]);
    trace(
        diagnostics,
        started,
        &format!(
            "CM_Request_Device_EjectW; CONFIGRET={result} (0x{result:08x}); veto={veto_type} ({:?}); name={name:?}; duration_ms={}",
            map_veto(veto_type),
            duration.as_millis()
        ),
    );
    Ok((
        EjectResult::Rejected(EjectFailure {
            config_ret: result,
            veto: Some((map_veto(veto_type), name)),
        }),
        duration,
    ))
}

fn needs_volume_preparation(result: &EjectResult) -> bool {
    matches!(
        result,
        EjectResult::Rejected(failure)
            if failure.config_ret == CR_REMOVE_VETOED
                && matches!(
                    failure.veto.as_ref(),
                    Some((VetoKind::OutstandingOpen | VetoKind::PendingClose, _))
                )
    )
}

pub(super) fn trace(diagnostics: &mut String, started: Instant, message: &str) {
    let _ = writeln!(
        diagnostics,
        "[elapsed_ms={}] {message}",
        started.elapsed().as_millis()
    );
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

#[cfg(test)]
mod tests {
    use super::needs_volume_preparation;
    use crate::backend::{EjectFailure, EjectResult, VetoKind};

    #[test]
    fn preparation_is_limited_to_open_handle_and_pending_close_vetoes() {
        for kind in [VetoKind::OutstandingOpen, VetoKind::PendingClose] {
            let rejected = |config_ret| {
                EjectResult::Rejected(EjectFailure {
                    config_ret,
                    veto: Some((kind.clone(), "test-volume".into())),
                })
            };
            assert!(needs_volume_preparation(&rejected(23)));
            assert!(!needs_volume_preparation(&rejected(5)));
        }
        for kind in [
            VetoKind::IllegalDeviceRequest,
            VetoKind::InsufficientRights,
            VetoKind::Driver,
            VetoKind::Unknown,
        ] {
            assert!(!needs_volume_preparation(&EjectResult::Rejected(
                EjectFailure {
                    config_ret: 23,
                    veto: Some((kind, String::new())),
                }
            )));
        }
        assert!(!needs_volume_preparation(&EjectResult::Ejected));
        assert!(!needs_volume_preparation(&EjectResult::Rejected(
            EjectFailure {
                config_ret: 23,
                veto: None,
            }
        )));
    }
}
