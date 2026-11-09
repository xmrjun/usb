use std::error::Error;
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::model::Device;
use crate::policy::ListedDevice;

pub trait Backend {
    fn list_devices(&mut self) -> Result<Vec<ListedDevice>, BackendError>;

    /// Implementations must re-enumerate and compare the complete identity and
    /// impact fingerprint after user confirmation, before requesting removal.
    fn request_eject(
        &mut self,
        expected: &Device,
        cancellation: &Cancellation,
    ) -> Result<EjectResult, BackendError>;

    /// Diagnostic details from the last eject operation, when available.
    fn diagnostics(&self) -> Option<String> {
        None
    }
}

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendError(pub String);

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for BackendError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EjectResult {
    Ejected,
    Rejected(EjectFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EjectFailure {
    pub config_ret: u32,
    pub veto: Option<(VetoKind, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VetoKind {
    Unknown,
    LegacyDevice,
    PendingClose,
    WindowsApp,
    WindowsService,
    OutstandingOpen,
    Device,
    Driver,
    IllegalDeviceRequest,
    InsufficientPower,
    NonDisableable,
    LegacyDriver,
    InsufficientRights,
}

impl EjectFailure {
    pub fn user_message(&self) -> String {
        let prefix = format!(
            "Windows 拒绝了安全弹出请求（CONFIGRET {}）。",
            self.config_ret
        );
        let Some((kind, name)) = &self.veto else {
            return format!(
                "{prefix} Windows 未提供阻止原因，也未指出占用设备的程序或文件。"
            );
        };
        let clean_name = if name.trim().is_empty() {
            "（未提供名称）"
        } else {
            name.trim()
        };
        let detail = match kind {
            VetoKind::OutstandingOpen => format!(
                "Windows 报告仍有打开的句柄。返回的是设备实例标识符/路径，并非进程或文件名称：{clean_name}"
            ),
            VetoKind::WindowsApp => {
                format!("Windows 报告某个应用程序阻止了弹出，名称：{clean_name}")
            }
            VetoKind::WindowsService => {
                format!("Windows 报告某个服务阻止了弹出，名称：{clean_name}")
            }
            VetoKind::Device => {
                format!("Windows 报告相关设备阻止了弹出：{clean_name}")
            }
            VetoKind::Driver => format!("Windows 报告某个驱动程序阻止了弹出：{clean_name}"),
            VetoKind::PendingClose => {
                "Windows 报告设备仍有待完成的关闭操作。".into()
            }
            VetoKind::InsufficientRights => {
                "Windows 报告当前权限不足，无法弹出此设备。".into()
            }
            VetoKind::NonDisableable => {
                "Windows 报告无法安全禁用此设备。".into()
            }
            VetoKind::IllegalDeviceRequest => {
                "Windows 报告此设备不支持该安全弹出请求。".into()
            }
            VetoKind::InsufficientPower => {
                "Windows 因供电不足而阻止了弹出。".into()
            }
            VetoKind::LegacyDevice => {
                format!("Windows 报告旧式设备阻止了弹出：{clean_name}")
            }
            VetoKind::LegacyDriver => {
                format!("Windows 报告旧式驱动程序阻止了弹出：{clean_name}")
            }
            VetoKind::Unknown => format!("Windows 未说明阻止弹出的具体原因：{clean_name}"),
        };
        format!("{prefix} {detail}")
    }
}
