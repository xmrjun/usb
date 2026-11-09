use crate::{
    backend::{BackendError, EjectResult},
    model::{format_capacity, Device},
    policy::ListedDevice,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuiRow {
    pub public_id: String,
    pub name: String,
    pub mounts: String,
    pub capacity: String,
    pub partitions: String,
    pub status: String,
    pub eligible: bool,
}

#[derive(Default)]
pub struct GuiState {
    devices: Vec<ListedDevice>,
    selected_id: Option<String>,
    generation: u64,
    busy: bool,
    closed: bool,
    status: String,
}

impl GuiState {
    pub fn new() -> Self {
        Self {
            status: "Ready".into(),
            ..Self::default()
        }
    }

    pub fn begin_refresh(&mut self) -> Option<u64> {
        if self.busy || self.closed {
            return None;
        }
        self.busy = true;
        self.generation = self.generation.wrapping_add(1);
        self.status = "Refreshing device inventory…".into();
        Some(self.generation)
    }

    pub fn finish_refresh(
        &mut self,
        generation: u64,
        result: Result<Vec<ListedDevice>, BackendError>,
    ) {
        if self.closed || generation != self.generation {
            return;
        }
        self.busy = false;
        match result {
            Ok(devices) => {
                if self.selected_id.as_ref().is_some_and(|selected| {
                    !devices
                        .iter()
                        .any(|item| item.device.public_id == *selected)
                }) {
                    self.selected_id = None;
                }
                self.status = if devices.is_empty() {
                    "No supported USB storage devices found.".into()
                } else {
                    format!("Found {} physical USB device(s).", devices.len())
                };
                self.devices = devices;
            }
            Err(problem) => {
                self.devices.clear();
                self.selected_id = None;
                self.status = format!("Refresh failed: {problem}");
            }
        }
    }

    pub fn rows(&self) -> Vec<GuiRow> {
        self.devices
            .iter()
            .map(|item| {
                let mounts = item
                    .device
                    .volumes
                    .iter()
                    .map(|volume| {
                        let paths = if volume.mount_paths.is_empty() {
                            "(no mount path)".into()
                        } else {
                            volume.mount_paths.join(", ")
                        };
                        let capacity = volume
                            .capacity_bytes
                            .map(format_capacity)
                            .unwrap_or_else(|| "unknown size".into());
                        format!("{paths} [{capacity} logical volume]")
                    })
                    .collect::<Vec<_>>();
                GuiRow {
                    public_id: item.device.public_id.clone(),
                    name: item.device.name.clone(),
                    mounts: if mounts.is_empty() {
                        "(no volumes)".into()
                    } else {
                        mounts.join("; ")
                    },
                    capacity: item
                        .device
                        .capacity_bytes()
                        .map(format_capacity)
                        .unwrap_or_else(|| "Multiple disks".into()),
                    partitions: item.device.partition_count().to_string(),
                    status: match &item.eligibility {
                        Ok(()) => "Ready".into(),
                        Err(reason) => format!("Blocked: {reason}"),
                    },
                    eligible: item.eligibility.is_ok(),
                }
            })
            .collect()
    }

    pub fn select(&mut self, public_id: &str) -> bool {
        if self.closed || self.busy {
            return false;
        }
        if self
            .devices
            .iter()
            .any(|item| item.device.public_id == public_id)
        {
            self.selected_id = Some(public_id.to_owned());
            true
        } else {
            false
        }
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    pub fn can_eject(&self) -> bool {
        !self.busy && !self.closed && self.selected_device().is_some()
    }

    pub fn confirmation_text(&self) -> Option<String> {
        let device = self.selected_device()?;
        Some(format!(
            "Request Windows safe removal for this physical device?\n\n{}\nNo force-unmount or process termination will be attempted.",
            device.impact_summary()
        ))
    }

    pub fn begin_eject(&mut self) -> Option<Device> {
        let device = self.selected_device()?.clone();
        self.busy = true;
        self.status = "Revalidating device identity and requesting safe removal…".into();
        Some(device)
    }

    pub fn finish_eject(&mut self, result: Result<EjectResult, BackendError>) {
        if self.closed {
            return;
        }
        self.busy = false;
        self.status = match result {
            Ok(EjectResult::Ejected) => {
                self.selected_id = None;
                "Windows accepted the safe-removal request.".into()
            }
            Ok(EjectResult::Rejected(failure)) => failure.user_message(),
            Err(problem) => format!("Safe removal was not attempted: {problem}"),
        };
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.busy = false;
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn request_close(&mut self) -> bool {
        if self.busy {
            self.status = "Wait for the current device operation to finish before closing.".into();
            false
        } else {
            true
        }
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    fn selected_device(&self) -> Option<&Device> {
        let selected = self.selected_id.as_ref()?;
        self.devices
            .iter()
            .find(|item| item.device.public_id == *selected && item.eligibility.is_ok())
            .map(|item| &item.device)
    }
}

#[cfg(windows)]
pub mod native;
