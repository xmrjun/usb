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
            status: "就绪".into(),
            ..Self::default()
        }
    }

    pub fn begin_refresh(&mut self) -> Option<u64> {
        if self.busy || self.closed {
            return None;
        }
        self.busy = true;
        self.generation = self.generation.wrapping_add(1);
        self.status = "正在刷新设备列表…".into();
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
            "是否请求 Windows 安全弹出以下物理设备？\n\n{}\n请先保存相关文件并关闭正在使用该设备的程序。\n本工具不会强制卸载磁盘或终止程序。",
            device.impact_summary()
        ))
    }

    pub fn begin_eject(&mut self) -> Option<Device> {
        let device = self.selected_device()?.clone();
        self.busy = true;
        self.status = "正在重新核对设备身份并请求安全弹出…".into();
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
                "Windows 已接受安全弹出请求。".into()
            }
            Ok(EjectResult::Rejected(failure)) => failure.user_message(),
            Err(problem) => format!("未执行安全弹出：{problem}"),
        };
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.busy = false;
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn request_close(&mut self) -> bool {
        if self.busy {
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
