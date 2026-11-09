use std::fmt::Write;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Disk {
    pub instance_id: String,
    pub number: u32,
    pub name: String,
    pub serial: String,
    pub capacity_bytes: u64,
    pub partition_count: u32,
    pub partitions: Vec<Partition>,
    pub partition_table_supported: bool,
    /// Additional permission for the administrator volume preparation path.
    /// Ordinary PnP eligibility still requires the complete policy checks.
    pub volume_preparation_supported: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PartitionKind {
    BasicData,
    MicrosoftReserved,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Partition {
    pub number: u32,
    pub starting_offset: i64,
    pub length: i64,
    pub kind: PartitionKind,
    pub gpt_attributes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeExtent {
    pub disk_number: u32,
    pub starting_offset: i64,
    pub extent_length: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Volume {
    pub id: String,
    pub mount_paths: Vec<String>,
    /// Logical size reported by the filesystem volume. This is deliberately
    /// kept separate from the containing physical disk capacity.
    pub capacity_bytes: Option<u64>,
    pub filesystem: Option<String>,
    pub disk_numbers: Vec<u32>,
    pub extents: Vec<VolumeExtent>,
    pub mapping_complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Device {
    pub public_id: String,
    pub target_instance_id: String,
    pub target_container_id: String,
    pub target_service: String,
    pub name: String,
    pub disks: Vec<Disk>,
    pub volumes: Vec<Volume>,
}

impl Device {
    pub fn new(
        target_instance_id: String,
        target_container_id: String,
        target_service: String,
        name: String,
        disks: Vec<Disk>,
        volumes: Vec<Volume>,
    ) -> Self {
        let public_id = stable_public_id(&target_instance_id);
        Self {
            public_id,
            target_instance_id,
            target_container_id,
            target_service,
            name,
            disks,
            volumes,
        }
    }

    /// A physical size is only meaningful for one physical disk. Multiple LUNs
    /// are deliberately not summed and presented as one device capacity.
    pub fn capacity_bytes(&self) -> Option<u64> {
        (self.disks.len() == 1).then(|| self.disks[0].capacity_bytes)
    }

    pub fn partition_count(&self) -> u32 {
        self.disks.iter().map(|disk| disk.partition_count).sum()
    }

    pub fn supports_volume_preparation(&self) -> bool {
        !self.disks.is_empty()
            && self.disks.iter().all(|disk| {
                disk.partition_table_supported
                    && disk.volume_preparation_supported
                    && !disk.partitions.is_empty()
                    && disk.partitions.iter().all(|partition| {
                        partition.kind == PartitionKind::BasicData && partition.gpt_attributes == 0
                    })
            })
    }

    pub fn impact_summary(&self) -> String {
        let mut summary = format!("设备：{}\n", self.name);
        let mut disks = self.disks.iter().collect::<Vec<_>>();
        disks.sort_by_key(|disk| disk.number);
        for disk in disks {
            let _ = writeln!(
                summary,
                "物理磁盘 {}：{}（{}）",
                disk.number,
                disk.name,
                format_capacity(disk.capacity_bytes)
            );
        }

        let mut volumes = self.volumes.iter().collect::<Vec<_>>();
        volumes.sort_by(|left, right| left.id.cmp(&right.id));
        for volume in volumes {
            let mut mounts = volume.mount_paths.clone();
            mounts.sort();
            let logical_size = volume
                .capacity_bytes
                .map(|bytes| format!("逻辑卷容量：{}", format_capacity(bytes)))
                .unwrap_or_else(|| "逻辑卷容量未知".into());
            if mounts.is_empty() {
                let _ = writeln!(
                    summary,
                    "逻辑卷 {}（{logical_size}；无盘符或挂载路径）",
                    volume.id
                );
            } else {
                for mount in mounts {
                    let _ = writeln!(summary, "盘符 / 挂载路径：{mount}（{logical_size}）");
                }
            }
        }
        summary
    }

    pub fn fingerprint(&self) -> String {
        let mut canonical = format!(
            "{}|C:{}|S:{}",
            self.target_instance_id.to_uppercase(),
            self.target_container_id.to_uppercase(),
            self.target_service.to_uppercase()
        );
        let mut disks = self.disks.clone();
        disks.sort_by(|left, right| {
            (left.number, &left.instance_id).cmp(&(right.number, &right.instance_id))
        });
        for disk in disks {
            let _ = write!(
                canonical,
                "|D:{}:{}:{}:{}:{}:{:?}:{}:{}",
                disk.number,
                disk.instance_id.to_uppercase(),
                disk.serial.to_uppercase(),
                disk.capacity_bytes,
                disk.partition_count,
                disk.partitions,
                disk.partition_table_supported,
                disk.volume_preparation_supported
            );
        }

        let mut volumes = self.volumes.clone();
        volumes.sort_by(|left, right| left.id.cmp(&right.id));
        for mut volume in volumes {
            volume.mount_paths.sort();
            volume.disk_numbers.sort_unstable();
            volume.extents.sort_by_key(|extent| {
                (
                    extent.disk_number,
                    extent.starting_offset,
                    extent.extent_length,
                )
            });
            let _ = write!(
                canonical,
                "|V:{}:{:?}:{:?}:{:?}:{:?}:{:?}:{}",
                volume.id.to_uppercase(),
                volume
                    .mount_paths
                    .iter()
                    .map(|path| path.to_uppercase())
                    .collect::<Vec<_>>(),
                volume.disk_numbers,
                volume.capacity_bytes,
                volume.filesystem.as_ref().map(|value| value.to_uppercase()),
                volume
                    .extents
                    .iter()
                    .map(|extent| (
                        extent.disk_number,
                        extent.starting_offset,
                        extent.extent_length
                    ))
                    .collect::<Vec<_>>(),
                volume.mapping_complete
            );
        }
        format!("{:016x}", fnv1a64(canonical.as_bytes()))
    }

    pub fn same_identity_and_impact(&self, current: &Self) -> bool {
        self.target_instance_id
            .eq_ignore_ascii_case(&current.target_instance_id)
            && self.fingerprint() == current.fingerprint()
    }
}

pub fn stable_public_id(instance_id: &str) -> String {
    let canonical = instance_id.to_uppercase();
    format!("usb-{:016x}", fnv1a64(canonical.as_bytes()))
}

pub fn format_capacity(bytes: u64) -> String {
    const UNITS: [(&str, f64); 4] = [
        ("TB", 1_000_000_000_000.0),
        ("GB", 1_000_000_000.0),
        ("MB", 1_000_000.0),
        ("KB", 1_000.0),
    ];
    for (unit, divisor) in UNITS {
        if (bytes as f64) >= divisor {
            return format!("{:.1} {unit}", bytes as f64 / divisor);
        }
    }
    format!("{bytes} B")
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
