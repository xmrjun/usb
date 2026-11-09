use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::model::{Device, Disk, Partition, Volume};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BusType {
    Usb,
    Other(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    pub instance_id: String,
    pub container_id: String,
    pub service: String,
    pub name: String,
    pub removable: bool,
    pub topology_complete: bool,
    pub is_hub: bool,
    pub has_non_storage_descendants: bool,
    pub parent_is_valid_hub: bool,
    pub descendant_disk_instance_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiskCandidate {
    pub disk: Disk,
    pub bus_type: BusType,
    pub target: Option<Target>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inventory {
    pub disks: Vec<DiskCandidate>,
    pub volumes: Vec<Volume>,
    pub critical_disk_numbers: BTreeSet<u32>,
    pub critical_scan_complete: bool,
    pub virtual_dependency_scan_complete: bool,
    pub virtual_host_volume_ids: BTreeSet<String>,
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockReason {
    InventoryIncomplete,
    TargetMissing,
    NotRemovable,
    TopologyIncomplete,
    HubTarget,
    CompositeDevice,
    VolumeMappingIncomplete,
    PartitionMappingIncomplete,
    UnsupportedFilesystem,
    CrossDiskVolume,
    SystemDisk,
    CriticalUseUnknown,
    VirtualDiskDependencyUnknown,
    MountedVirtualDiskDependency,
    IdentityCollision,
}

impl fmt::Display for BlockReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InventoryIncomplete => "检测信息不完整，暂不允许弹出",
            Self::TargetMissing => "未找到通过验证的物理弹出目标",
            Self::NotRemovable => "Windows 未将此设备标记为可移除",
            Self::TopologyIncomplete => "无法完整验证设备的 PnP 连接关系",
            Self::HubTarget => "本工具不支持弹出 USB 集线器",
            Self::CompositeDevice => "该设备还关联了非存储功能或其他设备",
            Self::VolumeMappingIncomplete => "无法确认一个或多个逻辑卷对应的物理磁盘范围",
            Self::PartitionMappingIncomplete => {
                "分区表不受支持，或分区与简单卷的对应关系不完整"
            }
            Self::UnsupportedFilesystem => {
                "某个逻辑卷已脱机、为 RAW 格式，或使用了不支持的文件系统"
            }
            Self::CrossDiskVolume => "某个逻辑卷跨越了此设备和其他物理磁盘",
            Self::SystemDisk => {
                "此设备包含 Windows、分页文件、崩溃转储、启动或系统服务所需的存储内容"
            }
            Self::CriticalUseUnknown => {
                "无法完整确认 Windows 关键存储的使用情况"
            }
            Self::VirtualDiskDependencyUnknown => {
                "无法完整确认已挂载虚拟磁盘所依赖的存储位置"
            }
            Self::MountedVirtualDiskDependency => {
                "已挂载的虚拟磁盘使用了此设备上的文件"
            }
            Self::IdentityCollision => "设备标识符存在冲突，无法唯一确定设备",
        };
        formatter.write_str(message)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedDevice {
    pub device: Device,
    pub eligibility: Result<(), BlockReason>,
}

pub fn evaluate_inventory(inventory: Inventory) -> Vec<ListedDevice> {
    let mut groups: BTreeMap<String, Vec<DiskCandidate>> = BTreeMap::new();
    for candidate in inventory
        .disks
        .into_iter()
        .filter(|candidate| candidate.bus_type == BusType::Usb)
    {
        let key = candidate
            .target
            .as_ref()
            .map(|target| target.instance_id.to_uppercase())
            .unwrap_or_else(|| format!("MISSING:{}", candidate.disk.instance_id.to_uppercase()));
        groups.entry(key).or_default().push(candidate);
    }

    let mut listed = Vec::with_capacity(groups.len());
    for candidates in groups.into_values() {
        let disk_numbers = candidates
            .iter()
            .map(|candidate| candidate.disk.number)
            .collect::<BTreeSet<_>>();
        let mut volumes = inventory
            .volumes
            .iter()
            .filter(|volume| {
                volume
                    .disk_numbers
                    .iter()
                    .any(|number| disk_numbers.contains(number))
            })
            .cloned()
            .collect::<Vec<_>>();
        volumes.sort_by(|left, right| left.id.cmp(&right.id));

        let target = candidates
            .first()
            .and_then(|candidate| candidate.target.clone());
        let target_id = target
            .as_ref()
            .map(|target| target.instance_id.clone())
            .unwrap_or_else(|| candidates[0].disk.instance_id.clone());
        let target_container_id = target
            .as_ref()
            .map(|target| target.container_id.clone())
            .unwrap_or_default();
        let target_service = target
            .as_ref()
            .map(|target| target.service.clone())
            .unwrap_or_default();
        let name = target
            .as_ref()
            .map(|target| target.name.clone())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| candidates[0].disk.name.clone());
        let mut disks = candidates
            .iter()
            .map(|candidate| candidate.disk.clone())
            .collect::<Vec<Disk>>();
        disks.sort_by_key(|disk| disk.number);

        let mut eligibility = if !inventory.complete {
            Err(BlockReason::InventoryIncomplete)
        } else if target.is_none() {
            Err(BlockReason::TargetMissing)
        } else {
            Ok(())
        };

        if eligibility.is_ok() {
            let expected = target.as_ref().expect("target checked above");
            let inconsistent = candidates
                .iter()
                .any(|candidate| candidate.target.as_ref() != Some(expected));
            let actual_disk_ids = candidates
                .iter()
                .map(|candidate| candidate.disk.instance_id.to_uppercase())
                .collect::<BTreeSet<_>>();
            let expected_disk_ids = expected
                .descendant_disk_instance_ids
                .iter()
                .map(|instance_id| instance_id.to_uppercase())
                .collect::<BTreeSet<_>>();
            eligibility = if inconsistent
                || !expected.topology_complete
                || !expected.parent_is_valid_hub
                || expected.container_id.trim().is_empty()
                || !matches!(
                    expected.service.to_ascii_lowercase().as_str(),
                    "usbstor" | "uaspstor"
                )
                || expected_disk_ids != actual_disk_ids
            {
                Err(BlockReason::TopologyIncomplete)
            } else if expected.is_hub {
                Err(BlockReason::HubTarget)
            } else if expected.has_non_storage_descendants {
                Err(BlockReason::CompositeDevice)
            } else if !expected.removable {
                Err(BlockReason::NotRemovable)
            } else if volumes.iter().any(|volume| !volume_mapping_valid(volume)) {
                Err(BlockReason::VolumeMappingIncomplete)
            } else if volumes.iter().any(|volume| {
                volume
                    .disk_numbers
                    .iter()
                    .any(|number| !disk_numbers.contains(number))
            }) {
                Err(BlockReason::CrossDiskVolume)
            } else if candidates.iter().any(|candidate| {
                !candidate.disk.partition_table_supported
                    || candidate.disk.partition_count as usize != candidate.disk.partitions.len()
                    || candidate.disk.partitions.is_empty()
            }) || !partitions_fully_mapped(&disks, &volumes)
            {
                Err(BlockReason::PartitionMappingIncomplete)
            } else if volumes.iter().any(|volume| {
                volume
                    .filesystem
                    .as_deref()
                    .map_or(true, |filesystem| !supported_filesystem(filesystem))
            }) {
                Err(BlockReason::UnsupportedFilesystem)
            } else if !inventory.virtual_dependency_scan_complete {
                Err(BlockReason::VirtualDiskDependencyUnknown)
            } else if volumes.iter().any(|volume| {
                inventory
                    .virtual_host_volume_ids
                    .iter()
                    .any(|host| normalize_volume_id(host) == normalize_volume_id(&volume.id))
            }) {
                Err(BlockReason::MountedVirtualDiskDependency)
            } else if !inventory.critical_scan_complete {
                Err(BlockReason::CriticalUseUnknown)
            } else if disk_numbers
                .iter()
                .any(|number| inventory.critical_disk_numbers.contains(number))
            {
                Err(BlockReason::SystemDisk)
            } else {
                Ok(())
            };
        }

        listed.push(ListedDevice {
            device: Device::new(
                target_id,
                target_container_id,
                target_service,
                name,
                disks,
                volumes,
            ),
            eligibility,
        });
    }

    let mut public_ids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for item in &listed {
        public_ids
            .entry(item.device.public_id.clone())
            .or_default()
            .insert(item.device.target_instance_id.to_uppercase());
    }
    for item in &mut listed {
        if public_ids
            .get(&item.device.public_id)
            .is_some_and(|targets| targets.len() > 1)
        {
            item.eligibility = Err(BlockReason::IdentityCollision);
        }
    }
    listed
}

fn supported_filesystem(filesystem: &str) -> bool {
    matches!(
        filesystem.to_ascii_uppercase().as_str(),
        "NTFS" | "REFS" | "FAT" | "FAT32" | "EXFAT" | "UDF"
    )
}

fn normalize_volume_id(value: &str) -> String {
    value.trim_end_matches(['\\', '/']).to_ascii_uppercase()
}

fn partitions_fully_mapped(disks: &[Disk], volumes: &[Volume]) -> bool {
    let partitions = disks
        .iter()
        .flat_map(|disk| {
            disk.partitions
                .iter()
                .map(move |partition| (disk.number, partition))
        })
        .collect::<Vec<(u32, &Partition)>>();
    let extents = volumes
        .iter()
        .flat_map(|volume| volume.extents.iter())
        .collect::<Vec<_>>();
    partitions.len() == extents.len()
        && partitions.iter().all(|(disk_number, partition)| {
            extents
                .iter()
                .filter(|extent| {
                    extent.disk_number == *disk_number
                        && extent.starting_offset == partition.starting_offset
                        && extent.extent_length == partition.length
                })
                .count()
                == 1
        })
        && extents.iter().all(|extent| {
            partitions
                .iter()
                .filter(|(disk_number, partition)| {
                    extent.disk_number == *disk_number
                        && extent.starting_offset == partition.starting_offset
                        && extent.extent_length == partition.length
                })
                .count()
                == 1
        })
}

fn volume_mapping_valid(volume: &Volume) -> bool {
    if !volume.mapping_complete || volume.extents.is_empty() {
        return false;
    }
    let declared = volume.disk_numbers.iter().copied().collect::<BTreeSet<_>>();
    let from_extents = volume
        .extents
        .iter()
        .map(|extent| extent.disk_number)
        .collect::<BTreeSet<_>>();
    declared == from_extents
        && volume
            .extents
            .iter()
            .all(|extent| extent.starting_offset >= 0 && extent.extent_length > 0)
}
