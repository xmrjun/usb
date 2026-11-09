mod eject;
pub(crate) mod elevation;
mod handles;
mod inventory;
mod volume_prepare;

use std::sync::Mutex;

#[cfg(not(target_arch = "x86_64"))]
compile_error!("本版本仅支持 64 位 Windows 目标平台");

use windows_sys::Win32::Foundation::GetLastError;

use crate::{
    backend::{Backend, BackendError, Cancellation, EjectResult},
    model::Device,
    policy::ListedDevice,
};

pub struct WindowsBackend {
    diagnostic_details: String,
}

// GUI workers use separate backend instances. A scan must never open a disk or
// volume while another worker is asking PnP to remove it, including retry gaps.
static DEVICE_OPERATION: Mutex<()> = Mutex::new(());

impl WindowsBackend {
    pub fn new() -> Self {
        Self {
            diagnostic_details: String::new(),
        }
    }
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for WindowsBackend {
    fn list_devices(&mut self) -> Result<Vec<ListedDevice>, BackendError> {
        let _operation = DEVICE_OPERATION
            .lock()
            .map_err(|_| BackendError("设备操作异常中断，请重新启动程序。".into()))?;
        Ok(inventory::snapshot()?.listed)
    }

    fn request_eject(
        &mut self,
        expected: &Device,
        cancellation: &Cancellation,
    ) -> Result<EjectResult, BackendError> {
        self.diagnostic_details.clear();
        let _operation = DEVICE_OPERATION
            .lock()
            .map_err(|_| BackendError("设备操作异常中断，请重新启动程序。".into()))?;
        eject::request_eject(expected, cancellation, &mut self.diagnostic_details)
    }

    fn diagnostics(&self) -> Option<String> {
        (!self.diagnostic_details.is_empty()).then(|| self.diagnostic_details.clone())
    }
}

pub(super) fn last_error(operation: &str) -> BackendError {
    // SAFETY: GetLastError has no preconditions and is read immediately after failure.
    let error = unsafe { GetLastError() };
    BackendError(format!("{operation} 失败（Win32 错误 {error}）"))
}
