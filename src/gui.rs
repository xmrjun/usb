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
enum Operation {
    #[default]
    Idle,
    Refreshing(u64),
    Confirming(Box<Device>),
    Ejecting,
}

#[derive(Default)]
pub struct GuiState {
    devices: Vec<ListedDevice>,
    selected_id: Option<String>,
    generation: u64,
    operation: Operation,
    removal_pending: bool,
    closed: bool,
    status: String,
}

impl GuiState {
    pub fn new() -> Self {
        Self {
            status: "就绪".into(),
            ..Self::default()
        }
    }

    pub fn begin_refresh(&mut self) -> Option<u64> {
        if !self.can_refresh() {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.operation = Operation::Refreshing(self.generation);
        self.status = "正在刷新设备列表…".into();
        Some(self.generation)
    }

    pub fn finish_refresh(
        &mut self,
        generation: u64,
        result: Result<Vec<ListedDevice>, BackendError>,
    ) {
        if self.closed
            || !matches!(self.operation, Operation::Refreshing(active) if active == generation)
        {
            return;
        }
        self.operation = Operation::Idle;
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
                    "未发现支持的 USB 存储设备。请连接设备后刷新列表。".into()
                } else {
                    format!("已发现 {} 个 USB 物理设备。", devices.len())
                };
                self.devices = devices;
            }
            Err(problem) => {
                self.devices.clear();
                self.selected_id = None;
                self.status = format!("刷新失败：{problem}");
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
                            "（无盘符或挂载路径）".into()
                        } else {
                            volume.mount_paths.join(", ")
                        };
                        let capacity = volume
                            .capacity_bytes
                            .map(format_capacity)
                            .unwrap_or_else(|| "容量未知".into());
                        format!("{paths} [逻辑卷：{capacity}]")
                    })
                    .collect::<Vec<_>>();
                GuiRow {
                    public_id: item.device.public_id.clone(),
                    name: item.device.name.clone(),
                    mounts: if mounts.is_empty() {
                        "（无逻辑卷）".into()
                    } else {
                        mounts.join("; ")
                    },
                    capacity: match item.device.capacity_bytes() {
                        Some(0) => "未知".into(),
                        Some(bytes) => format_capacity(bytes),
                        None => "多个物理磁盘".into(),
                    },
                    partitions: item.device.partition_count().to_string(),
                    status: match &item.eligibility {
                        Ok(()) => "可以安全弹出".into(),
                        Err(reason) => format!("已阻止弹出：{reason}"),
                    },
                    eligible: item.eligibility.is_ok(),
                }
            })
            .collect()
    }

    pub fn select(&mut self, public_id: &str) -> bool {
        if self.closed || self.is_busy() || self.removal_pending {
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
        self.can_refresh() && self.selected_device().is_some()
    }

    pub fn confirmation_text(&self) -> Option<String> {
        if !self.can_eject() {
            return None;
        }
        let device = self.selected_device()?;
        Some(Self::device_confirmation_text(device))
    }

    fn device_confirmation_text(device: &Device) -> String {
        format!(
            "是否请求 Windows 安全弹出以下物理设备？\n\n{}\n请先保存相关文件并关闭正在使用该设备的程序。\n本工具不会强制卸载磁盘或终止程序。",
            device.impact_summary()
        )
    }

    pub fn begin_confirmation(&mut self) -> Option<String> {
        if !self.can_eject() {
            return None;
        }
        let device = self.selected_device()?.clone();
        let text = Self::device_confirmation_text(&device);
        // The modal dialog pumps window messages. Reserve the operation before
        // opening it, and keep exactly the device/impact shown to the user.
        self.operation = Operation::Confirming(Box::new(device));
        self.status = "请确认要安全弹出的设备。".into();
        Some(text)
    }

    pub fn finish_confirmation(&mut self, accepted: bool) -> Option<Device> {
        if self.closed || !matches!(self.operation, Operation::Confirming(_)) {
            return None;
        }
        let Operation::Confirming(device) =
            std::mem::replace(&mut self.operation, Operation::Idle)
        else {
            unreachable!("confirmation state checked above");
        };
        if self.removal_pending {
            self.status = "设备正在被移除，已取消本次弹出确认。".into();
            return None;
        }
        if !accepted {
            self.status = "已取消安全弹出。".into();
            return None;
        }
        self.operation = Operation::Ejecting;
        self.status = "正在重新核对设备身份并请求安全弹出…".into();
        Some(*device)
    }

    pub fn begin_eject(&mut self) -> Option<Device> {
        if !self.can_eject() {
            return None;
        }
        let device = self.selected_device()?.clone();
        self.operation = Operation::Ejecting;
        self.status = "正在重新核对设备身份并请求安全弹出…".into();
        Some(device)
    }

    pub fn finish_eject(&mut self, result: Result<EjectResult, BackendError>) {
        if self.closed || !matches!(self.operation, Operation::Ejecting) {
            return;
        }
        self.operation = Operation::Idle;
        self.removal_pending = false;
        self.status = match result {
            Ok(EjectResult::Ejected) => {
                self.selected_id = None;
                "Windows 已接受安全弹出请求。".into()
            }
            Ok(EjectResult::Rejected(failure)) => failure.user_message(),
            Err(problem) => format!("未执行安全弹出：{problem}"),
        };
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.operation = Operation::Idle;
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn request_close(&mut self) -> bool {
        if self.is_busy() {
            self.status = "请等待当前设备操作完成后再关闭窗口。".into();
            false
        } else {
            true
        }
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn is_busy(&self) -> bool {
        !matches!(self.operation, Operation::Idle)
    }

    pub fn can_refresh(&self) -> bool {
        !self.closed && !self.is_busy() && !self.removal_pending
    }

    pub fn removal_started(&mut self) {
        self.removal_pending = true;
        if !self.is_busy() {
            self.status = "设备正在被移除，已暂停刷新。".into();
        }
    }

    pub fn removal_finished(&mut self) {
        self.removal_pending = false;
    }

    pub fn removal_pending(&self) -> bool {
        self.removal_pending
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
