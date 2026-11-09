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
            "Windows rejected safe removal (CONFIGRET {}).",
            self.config_ret
        );
        let Some((kind, name)) = &self.veto else {
            return format!(
                "{prefix} Windows did not provide a veto reason or identify an occupying program or file."
            );
        };
        let clean_name = if name.trim().is_empty() {
            "(no name supplied)"
        } else {
            name.trim()
        };
        let detail = match kind {
            VetoKind::OutstandingOpen => format!(
                "Windows reports outstanding open handles. The veto name is a device instance identifier/path, not a process or file: {clean_name}"
            ),
            VetoKind::WindowsApp => {
                format!("Windows reports an application veto named: {clean_name}")
            }
            VetoKind::WindowsService => {
                format!("Windows reports a service veto named: {clean_name}")
            }
            VetoKind::Device => {
                format!("Windows reports a related device veto: {clean_name}")
            }
            VetoKind::Driver => format!("Windows reports a driver veto: {clean_name}"),
            VetoKind::PendingClose => {
                "Windows reports that a close operation is still pending.".into()
            }
            VetoKind::InsufficientRights => {
                "Windows reports insufficient rights for this device.".into()
            }
            VetoKind::NonDisableable => {
                "Windows reports that this device cannot be disabled safely.".into()
            }
            VetoKind::IllegalDeviceRequest => {
                "Windows reports that safe removal is not valid for this device.".into()
            }
            VetoKind::InsufficientPower => {
                "Windows reports an insufficient-power veto.".into()
            }
            VetoKind::LegacyDevice => {
                format!("Windows reports a legacy-device veto: {clean_name}")
            }
            VetoKind::LegacyDriver => {
                format!("Windows reports a legacy-driver veto: {clean_name}")
            }
            VetoKind::Unknown => format!("Windows returned an unspecified veto: {clean_name}"),
        };
        format!("{prefix} {detail}")
    }
}
