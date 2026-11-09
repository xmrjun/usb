use std::collections::BTreeSet;
use std::ffi::c_void;
use std::mem::{size_of, MaybeUninit};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{null, null_mut};

use windows_sys::Win32::{
    Devices::DeviceAndDriverInstallation::{
        CM_Get_Child, CM_Get_DevNode_PropertyW, CM_Get_DevNode_Registry_PropertyW,
        CM_Get_Device_IDW, CM_Get_Parent, CM_Get_Sibling, CM_Locate_DevNodeW,
        SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
        CM_DEVCAP_REMOVABLE, CM_DRP_CAPABILITIES, CM_DRP_CLASS, CM_DRP_DEVICEDESC,
        CM_DRP_ENUMERATOR_NAME, CM_DRP_FRIENDLYNAME, CM_DRP_SERVICE, CR_BUFFER_SMALL,
        CR_NO_SUCH_DEVNODE, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, MAX_DEVICE_ID_LEN,
        SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
    },
    Devices::Properties::{DEVPKEY_Device_ContainerId, DEVPROP_TYPE_GUID},
    Foundation::{
        GetLastError, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
        ERROR_NO_MORE_FILES, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        BusTypeUsb, FindFirstVolumeW, FindNextVolumeW, GetDiskFreeSpaceExW, GetVolumeInformationW,
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, GetVolumePathNamesForVolumeNameW,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
    },
    Storage::Vhd::{
        GetStorageDependencyInformation, GET_STORAGE_DEPENDENCY_FLAG_DISK_HANDLE,
        GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES, STORAGE_DEPENDENCY_INFO,
        STORAGE_DEPENDENCY_INFO_TYPE_2, STORAGE_DEPENDENCY_INFO_VERSION_2,
    },
    System::{
        Environment::ExpandEnvironmentStringsW,
        Ioctl::{
            PropertyStandardQuery, StorageDeviceProperty, GUID_DEVINTERFACE_DISK,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
            IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
            STORAGE_DESCRIPTOR_HEADER, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY,
        },
        ProcessStatus::{K32EnumPageFilesW, ENUM_PAGE_FILE_INFORMATION},
        Registry::{
            RegGetValueW, HKEY_LOCAL_MACHINE, REG_EXPAND_SZ, REG_MULTI_SZ, REG_SZ, RRF_NOEXPAND,
            RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ,
        },
        SystemInformation::GetWindowsDirectoryW,
        IO::DeviceIoControl,
    },
};

use crate::{
    backend::BackendError,
    model::{Disk, Partition, Volume, VolumeExtent},
    policy::{evaluate_inventory, BusType, DiskCandidate, Inventory, ListedDevice, Target},
    raw::{
        checked_dependency_entries_end, needs_virtual_dependency_query, parse_dependency_utf16,
        parse_disk_extents, parse_disk_geometry_capacity, parse_drive_layout, parse_multi_sz,
        parse_storage_descriptor,
    },
};

use super::handles::{DeviceInfoSet, OwnedHandle, VolumeFindHandle};

const MAX_BUFFER: usize = 64 * 1024;
const MAX_TOPOLOGY_NODES: usize = 256;
const MAX_TOPOLOGY_DEPTH: usize = 32;

pub(super) struct Snapshot {
    pub listed: Vec<ListedDevice>,
}

pub(super) fn snapshot() -> Result<Snapshot, BackendError> {
    let disks = enumerate_disks()?;
    let volumes = match enumerate_volumes() {
        Ok(volumes) => volumes,
        Err(problem) => {
            let mut diagnostics = ScanDiagnostics::default();
            diagnostics.record("volume enumeration", problem);
            VolumeEnumeration {
                volumes: Vec::new(),
                diagnostics,
            }
        }
    };
    let critical = critical_storage_usage(&volumes.volumes);
    assemble_snapshot(disks, volumes, critical)
}

fn assemble_snapshot(
    disks: DiskEnumeration,
    volumes: VolumeEnumeration,
    critical: Result<(BTreeSet<u32>, bool), BackendError>,
) -> Result<Snapshot, BackendError> {
    let mut critical_diagnostics = ScanDiagnostics::default();
    let (critical_disk_numbers, critical_scan_complete) = critical_diagnostics
        .capture("critical-storage scan", critical)
        .unwrap_or_default();
    let complete = disks.diagnostics.is_complete()
        && volumes.diagnostics.is_complete()
        && disks.virtual_dependency_scan_complete
        && critical_scan_complete;
    let first_error = disks
        .diagnostics
        .first_error
        .or(volumes.diagnostics.first_error)
        .or(critical_diagnostics.first_error);
    let listed = evaluate_inventory(Inventory {
        disks: disks.candidates,
        volumes: volumes.volumes,
        critical_disk_numbers,
        critical_scan_complete,
        virtual_dependency_scan_complete: disks.virtual_dependency_scan_complete,
        virtual_host_volume_ids: disks.virtual_host_volume_ids,
        complete,
    });
    // A partial inventory is useful for display, but policy blocks every row.
    // If no USB row is known, report the actual query failure instead of an
    // inaccurate "no devices" result.
    if listed.is_empty() {
        if let Some(problem) = first_error {
            return Err(problem);
        }
    }
    Ok(Snapshot { listed })
}

#[derive(Default)]
struct ScanDiagnostics {
    first_error: Option<BackendError>,
}

impl ScanDiagnostics {
    fn is_complete(&self) -> bool {
        self.first_error.is_none()
    }

    fn record(&mut self, context: &str, problem: BackendError) {
        if self.first_error.is_none() {
            self.first_error = Some(BackendError(format!("{context}: {problem}")));
        }
    }

    fn capture<T>(&mut self, context: &str, result: Result<T, BackendError>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(problem) => {
                self.record(context, problem);
                None
            }
        }
    }
}

pub(super) fn locate_revalidated_target(
    expected: &crate::model::Device,
) -> Result<u32, BackendError> {
    let mut devinst = 0_u32;
    let mut instance_id = wide(&expected.target_instance_id);
    // SAFETY: devinst is writable and instance_id is a live NUL-terminated device ID.
    let result = unsafe { CM_Locate_DevNodeW(&mut devinst, instance_id.as_mut_ptr(), 0) };
    if result != CR_SUCCESS {
        return Err(BackendError(format!(
            "CM_Locate_DevNodeW failed with CONFIGRET {result}"
        )));
    }
    confirm_target_identity(devinst, &expected.target_instance_id)?;
    let enumerator = devnode_string(devinst, CM_DRP_ENUMERATOR_NAME)?;
    let capabilities = devnode_dword(devinst, CM_DRP_CAPABILITIES)?;
    if !enumerator.eq_ignore_ascii_case("USB") || capabilities & CM_DEVCAP_REMOVABLE == 0 {
        return Err(BackendError(
            "the eject target is no longer a removable USB devnode".into(),
        ));
    }
    let disk_id = expected
        .disks
        .first()
        .map(|disk| disk.instance_id.as_str())
        .ok_or_else(|| BackendError("the selected device has no validated disk".into()))?;
    // Apply the same hub boundary as discovery. A removable parent hub is a
    // connection boundary, never a second eject target or part of our impact.
    let (actual, resolved_devinst) = resolve_target(devinst, disk_id)?.ok_or_else(|| {
        BackendError("the eject target no longer has a validated USB hub boundary".into())
    })?;
    let expected_disk_ids = expected
        .disks
        .iter()
        .map(|disk| disk.instance_id.to_uppercase())
        .collect::<BTreeSet<_>>();
    let actual_disk_ids = actual
        .descendant_disk_instance_ids
        .iter()
        .map(|id| id.to_uppercase())
        .collect::<BTreeSet<_>>();
    if resolved_devinst != devinst
        || !actual
            .instance_id
            .eq_ignore_ascii_case(&expected.target_instance_id)
        || !actual
            .container_id
            .eq_ignore_ascii_case(&expected.target_container_id)
        || !actual
            .service
            .eq_ignore_ascii_case(&expected.target_service)
        || !actual.topology_complete
        || actual.is_hub
        || actual.has_non_storage_descendants
        || !actual.parent_is_valid_hub
        || actual_disk_ids != expected_disk_ids
    {
        return Err(BackendError(
            "the PnP target identity or topology changed after confirmation".into(),
        ));
    }
    Ok(devinst)
}

pub(super) fn confirm_target_identity(
    devinst: u32,
    expected_instance_id: &str,
) -> Result<(), BackendError> {
    let actual = device_id(devinst)?;
    if !actual.eq_ignore_ascii_case(expected_instance_id) {
        return Err(BackendError(
            "the PnP devnode identity changed before the removal request".into(),
        ));
    }
    Ok(())
}

struct DiskEnumeration {
    candidates: Vec<DiskCandidate>,
    virtual_dependency_scan_complete: bool,
    virtual_host_volume_ids: BTreeSet<String>,
    diagnostics: ScanDiagnostics,
}

fn enumerate_disks() -> Result<DiskEnumeration, BackendError> {
    // SAFETY: all pointers are valid and the returned handle is wrapped immediately.
    let raw_set = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_DISK,
            null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if raw_set == INVALID_HANDLE_VALUE as isize {
        return Err(super::last_error("SetupDiGetClassDevsW"));
    }
    let set = DeviceInfoSet(raw_set);
    let mut candidates = Vec::new();
    let mut virtual_dependency_scan_complete = true;
    let mut virtual_host_volume_ids = BTreeSet::new();
    let mut diagnostics = ScanDiagnostics::default();

    for index in 0_u32.. {
        let mut interface = zeroed_interface_data();
        // SAFETY: set and interface are valid for the duration of the call.
        let found = unsafe {
            SetupDiEnumDeviceInterfaces(
                set.0,
                null(),
                &GUID_DEVINTERFACE_DISK,
                index,
                &mut interface,
            )
        };
        if found == 0 {
            // SAFETY: GetLastError has no preconditions.
            let error = unsafe { GetLastError() };
            if error == ERROR_NO_MORE_ITEMS {
                break;
            }
            diagnostics.record(
                "disk enumeration",
                BackendError(format!(
                    "SetupDiEnumDeviceInterfaces failed with Win32 error {error}"
                )),
            );
            break;
        }

        let Some((path, devinfo)) = diagnostics.capture(
            &format!("disk interface {index}"),
            interface_detail(&set, &interface),
        ) else {
            continue;
        };
        let Some(instance_id) = diagnostics.capture(&path, device_id(devinfo.DevInst)) else {
            continue;
        };
        let name = devnode_string(devinfo.DevInst, CM_DRP_FRIENDLYNAME)
            .or_else(|_| devnode_string(devinfo.DevInst, CM_DRP_DEVICEDESC))
            .unwrap_or_else(|_| "USB storage device".into());
        let Some(handle) = diagnostics.capture(&path, OwnedHandle::open(&wide(&path))) else {
            continue;
        };
        let Some(descriptor) = diagnostics.capture(&path, storage_descriptor(&handle)) else {
            continue;
        };
        if needs_virtual_dependency_query(descriptor.bus_type) {
            match virtual_disk_host_volumes(&handle) {
                Ok(host_volumes) if !host_volumes.is_empty() => {
                    virtual_host_volume_ids.extend(host_volumes);
                }
                Ok(_) => {
                    virtual_dependency_scan_complete = false;
                    diagnostics.record(
                        &path,
                        BackendError(
                            "virtual-disk dependency query returned no host volumes".into(),
                        ),
                    );
                }
                Err(problem) => {
                    virtual_dependency_scan_complete = false;
                    diagnostics.record(&path, problem);
                }
            }
        }
        if descriptor.bus_type != BusTypeUsb as u32 {
            continue;
        }
        let Some(device_number) = diagnostics.capture(
            &path,
            ioctl_fixed::<STORAGE_DEVICE_NUMBER>(&handle, IOCTL_STORAGE_GET_DEVICE_NUMBER),
        ) else {
            continue;
        };
        // Keep identified USB disks visible when later inspection fails. These
        // display placeholders can never be approved: diagnostics makes the
        // entire snapshot incomplete, including during eject revalidation.
        let capacity_bytes = diagnostics.capture(&path, disk_capacity(&handle)).unwrap_or(0);
        let layout = diagnostics
            .capture(&path, partition_layout(&handle))
            .unwrap_or(crate::raw::DriveLayout {
                partitions: Vec::new(),
                simple_data_only: false,
            });
        let resolved_target = diagnostics
            .capture(&path, resolve_target(devinfo.DevInst, &instance_id))
            .flatten();

        candidates.push(DiskCandidate {
            disk: Disk {
                instance_id,
                number: device_number.DeviceNumber,
                name,
                serial: descriptor.serial,
                capacity_bytes,
                partition_count: layout.partitions.len() as u32,
                partitions: layout
                    .partitions
                    .into_iter()
                    .map(|partition| Partition {
                        number: partition.number,
                        starting_offset: partition.starting_offset,
                        length: partition.length,
                    })
                    .collect(),
                partition_table_supported: layout.simple_data_only,
            },
            bus_type: BusType::Usb,
            target: resolved_target.map(|(target, _)| target),
        });
    }
    Ok(DiskEnumeration {
        candidates,
        virtual_dependency_scan_complete,
        virtual_host_volume_ids,
        diagnostics,
    })
}

fn virtual_disk_host_volumes(handle: &OwnedHandle) -> Result<BTreeSet<String>, BackendError> {
    let word_count = MAX_BUFFER.div_ceil(size_of::<u64>());
    let mut storage = vec![0_u64; word_count];
    let buffer = storage.as_mut_ptr().cast::<u8>();
    let info = buffer.cast::<STORAGE_DEPENDENCY_INFO>();
    // SAFETY: storage is aligned for this C structure and contains at least MAX_BUFFER bytes.
    unsafe {
        (*info).Version = STORAGE_DEPENDENCY_INFO_VERSION_2;
        (*info).NumberEntries = 0;
    }
    let mut size_used = 0_u32;
    // SAFETY: handle is live; info points to a writable aligned buffer of the stated size.
    let result = unsafe {
        GetStorageDependencyInformation(
            handle.as_raw(),
            GET_STORAGE_DEPENDENCY_FLAG_DISK_HANDLE | GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES,
            MAX_BUFFER as u32,
            info,
            &mut size_used,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(BackendError(format!(
            "GetStorageDependencyInformation failed with Win32 error {result}"
        )));
    }
    let used = size_used as usize;
    let entries_offset = std::mem::offset_of!(STORAGE_DEPENDENCY_INFO, Anonymous);
    if used < entries_offset || used > MAX_BUFFER {
        return Err(BackendError(
            "virtual-disk dependency buffer size was invalid".into(),
        ));
    }
    // SAFETY: the fixed header was populated by the successful API call above.
    let count = unsafe { (*info).NumberEntries as usize };
    checked_dependency_entries_end(
        used,
        entries_offset,
        count,
        size_of::<STORAGE_DEPENDENCY_INFO_TYPE_2>(),
    )
    .map_err(|error| BackendError(error.to_string()))?;

    let mut host_volumes = BTreeSet::new();
    for index in 0..count {
        // SAFETY: the checked entry range is inside storage; read_unaligned avoids layout assumptions.
        let entry = unsafe {
            std::ptr::read_unaligned(
                buffer
                    .add(entries_offset + index * size_of::<STORAGE_DEPENDENCY_INFO_TYPE_2>())
                    .cast::<STORAGE_DEPENDENCY_INFO_TYPE_2>(),
            )
        };
        let host_volume = utf16_from_dependency_buffer(
            buffer.cast_const(),
            used,
            entry.HostVolumeName.cast_const(),
        )?;
        if host_volume.trim().is_empty() {
            return Err(BackendError(
                "virtual-disk dependency omitted its host volume".into(),
            ));
        }
        host_volumes.insert(host_volume.to_ascii_uppercase());
    }
    Ok(host_volumes)
}

fn utf16_from_dependency_buffer(
    buffer: *const u8,
    used: usize,
    value: *const u16,
) -> Result<String, BackendError> {
    if value.is_null() {
        return Err(BackendError(
            "virtual-disk dependency contained a null string".into(),
        ));
    }
    let start = buffer as usize;
    let end = start
        .checked_add(used)
        .ok_or_else(|| BackendError("virtual-disk dependency buffer overflowed".into()))?;
    let value_start = value as usize;
    if value_start < start || value_start >= end || value_start % std::mem::align_of::<u16>() != 0 {
        return Err(BackendError(
            "virtual-disk dependency string was outside its buffer".into(),
        ));
    }
    // SAFETY: buffer and used describe the live allocation passed to the successful API call.
    let bytes = unsafe { std::slice::from_raw_parts(buffer, used) };
    parse_dependency_utf16(bytes, value_start - start)
        .map_err(|error| BackendError(error.to_string()))
}

fn interface_detail(
    set: &DeviceInfoSet,
    interface: &SP_DEVICE_INTERFACE_DATA,
) -> Result<(String, SP_DEVINFO_DATA), BackendError> {
    let mut required = 0_u32;
    let mut devinfo = zeroed_devinfo_data();
    // SAFETY: this is the documented sizing call; output pointers are valid.
    let first = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set.0,
            interface,
            null_mut(),
            0,
            &mut required,
            &mut devinfo,
        )
    };
    // SAFETY: GetLastError has no preconditions.
    let sizing_error = unsafe { GetLastError() };
    if first != 0 || sizing_error != ERROR_INSUFFICIENT_BUFFER {
        return Err(BackendError(format!(
            "device interface sizing failed with Win32 error {sizing_error}"
        )));
    }
    let required_usize = required as usize;
    if required_usize < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>()
        || required_usize > MAX_BUFFER
    {
        return Err(BackendError("device interface path size is invalid".into()));
    }
    let word_size = size_of::<usize>();
    let mut storage = vec![0_usize; required_usize.div_ceil(word_size)];
    let detail = storage
        .as_mut_ptr()
        .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    // SAFETY: usize storage provides sufficient alignment and capacity. Only cbSize is written.
    unsafe {
        (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    }
    devinfo = zeroed_devinfo_data();
    // SAFETY: all buffers are live and sized by the preceding SetupAPI call.
    let success = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set.0,
            interface,
            detail,
            required,
            null_mut(),
            &mut devinfo,
        )
    };
    if success == 0 {
        return Err(super::last_error("SetupDiGetDeviceInterfaceDetailW"));
    }
    // SAFETY: DevicePath lies within the validated detail buffer.
    let path_pointer = unsafe { (*detail).DevicePath.as_ptr() };
    let storage_start = storage.as_ptr() as usize;
    let start = path_pointer as usize - storage_start;
    if start >= required_usize || start % 2 != 0 {
        return Err(BackendError(
            "device interface path offset is invalid".into(),
        ));
    }
    let units = (required_usize - start) / 2;
    // SAFETY: the range is inside aligned storage and was produced by SetupAPI.
    let wide_path = unsafe { std::slice::from_raw_parts(path_pointer, units) };
    Ok((decode_nul_terminated(wide_path)?, devinfo))
}

fn storage_descriptor(handle: &OwnedHandle) -> Result<crate::raw::StorageDescriptor, BackendError> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut header = MaybeUninit::<STORAGE_DESCRIPTOR_HEADER>::zeroed();
    let mut returned = 0_u32;
    // SAFETY: query and header are valid fixed-size buffers for this IOCTL.
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&query as *const STORAGE_PROPERTY_QUERY).cast(),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            header.as_mut_ptr().cast(),
            size_of::<STORAGE_DESCRIPTOR_HEADER>() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 || returned < size_of::<STORAGE_DESCRIPTOR_HEADER>() as u32 {
        return Err(super::last_error("IOCTL_STORAGE_QUERY_PROPERTY header"));
    }
    // SAFETY: DeviceIoControl initialized the complete header as verified above.
    let header = unsafe { header.assume_init() };
    let size = header.Size as usize;
    if size < size_of::<STORAGE_DESCRIPTOR_HEADER>() || size > MAX_BUFFER {
        return Err(BackendError("storage descriptor size is invalid".into()));
    }
    let mut bytes = vec![0_u8; size];
    returned = 0;
    // SAFETY: all pointers reference valid buffers of the supplied sizes.
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&query as *const STORAGE_PROPERTY_QUERY).cast(),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            bytes.as_mut_ptr().cast(),
            bytes.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 || returned as usize > bytes.len() {
        return Err(super::last_error("IOCTL_STORAGE_QUERY_PROPERTY"));
    }
    parse_storage_descriptor(&bytes[..returned as usize])
        .map_err(|error| BackendError(error.to_string()))
}

fn partition_layout(handle: &OwnedHandle) -> Result<crate::raw::DriveLayout, BackendError> {
    let bytes = ioctl_buffer(handle, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, 64 * 1024)?;
    parse_drive_layout(&bytes)
        .map_err(|error| BackendError(format!("IOCTL_DISK_GET_DRIVE_LAYOUT_EX: {error}")))
}

fn disk_capacity(handle: &OwnedHandle) -> Result<u64, BackendError> {
    // GET_LENGTH_INFO requires read access. GEOMETRY_EX permits the existing
    // metadata-only handle and returns an exact size without elevation.
    let bytes = ioctl_buffer(handle, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, 1024)?;
    parse_disk_geometry_capacity(&bytes)
        .map_err(|error| BackendError(format!("IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: {error}")))
}

struct VolumeEnumeration {
    volumes: Vec<Volume>,
    diagnostics: ScanDiagnostics,
}

fn enumerate_volumes() -> Result<VolumeEnumeration, BackendError> {
    let mut name = vec![0_u16; 1024];
    // SAFETY: name is a writable buffer with the supplied element count.
    let raw_find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if raw_find == INVALID_HANDLE_VALUE {
        return Err(super::last_error("FindFirstVolumeW"));
    }
    let find = VolumeFindHandle(raw_find);
    let mut volumes = Vec::new();
    let mut diagnostics = ScanDiagnostics::default();
    loop {
        if let Some(volume_name) =
            diagnostics.capture("volume name", decode_nul_terminated(&name))
        {
            if let Some(volume) = diagnostics.capture(&volume_name, inspect_volume(&volume_name)) {
                volumes.push(volume);
            }
        }

        name.fill(0);
        // SAFETY: find is a live enumeration handle and name is writable.
        let more = unsafe { FindNextVolumeW(find.0, name.as_mut_ptr(), name.len() as u32) };
        if more == 0 {
            // SAFETY: GetLastError has no preconditions.
            let error = unsafe { GetLastError() };
            if error == ERROR_NO_MORE_FILES {
                break;
            }
            diagnostics.record(
                "volume enumeration",
                BackendError(format!("FindNextVolumeW failed with Win32 error {error}")),
            );
            break;
        }
    }
    Ok(VolumeEnumeration {
        volumes,
        diagnostics,
    })
}

fn inspect_volume(volume_name: &str) -> Result<Volume, BackendError> {
    let open_name = volume_name
        .strip_suffix('\\')
        .ok_or_else(|| BackendError("volume name lacks trailing slash".into()))?;
    let handle = OwnedHandle::open(&wide(open_name))?;
    let extent_bytes = ioctl_buffer(&handle, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, 64 * 1024)?;
    let extents = parse_disk_extents(&extent_bytes)
        .map_err(|error| BackendError(format!("IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: {error}")))?;
    let disk_numbers = extents
        .iter()
        .map(|extent| extent.disk_number)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Volume {
        id: volume_name.to_owned(),
        mount_paths: volume_paths(volume_name)?,
        capacity_bytes: volume_capacity(volume_name),
        filesystem: volume_filesystem(volume_name),
        disk_numbers,
        extents: extents
            .into_iter()
            .map(|extent| VolumeExtent {
                disk_number: extent.disk_number,
                starting_offset: extent.starting_offset,
                extent_length: extent.extent_length,
            })
            .collect(),
        mapping_complete: true,
    })
}

fn volume_capacity(volume_name: &str) -> Option<u64> {
    let volume = wide(volume_name);
    let mut available = 0_u64;
    let mut total = 0_u64;
    let mut free = 0_u64;
    // SAFETY: the input is NUL-terminated and all output pointers are valid.
    let ok = unsafe { GetDiskFreeSpaceExW(volume.as_ptr(), &mut available, &mut total, &mut free) };
    (ok != 0).then_some(total)
}

fn volume_filesystem(volume_name: &str) -> Option<String> {
    let volume = wide(volume_name);
    let mut filesystem = vec![0_u16; 64];
    // SAFETY: volume is NUL-terminated and the filesystem output buffer is writable.
    let ok = unsafe {
        GetVolumeInformationW(
            volume.as_ptr(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    };
    if ok == 0 {
        None
    } else {
        decode_nul_terminated(&filesystem).ok()
    }
}

fn volume_paths(volume_name: &str) -> Result<Vec<String>, BackendError> {
    let volume = wide(volume_name);
    let mut required = 0_u32;
    let mut buffer = vec![0_u16; 1024];
    // SAFETY: all pointers refer to valid NUL-terminated or writable buffers.
    let mut ok = unsafe {
        GetVolumePathNamesForVolumeNameW(
            volume.as_ptr(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut required,
        )
    };
    if ok == 0 {
        // SAFETY: GetLastError has no preconditions.
        let error = unsafe { GetLastError() };
        if error != ERROR_MORE_DATA || required == 0 || required as usize > MAX_BUFFER / 2 {
            return Err(BackendError(format!(
                "GetVolumePathNamesForVolumeNameW failed with Win32 error {error}"
            )));
        }
        buffer.resize(required as usize, 0);
        // SAFETY: buffers are valid and the requested size came from Windows.
        ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                volume.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut required,
            )
        };
    }
    if ok == 0 {
        return Err(super::last_error("GetVolumePathNamesForVolumeNameW"));
    }
    parse_multi_sz(&buffer).map_err(|error| BackendError(error.to_string()))
}

fn system_disk_numbers() -> Result<BTreeSet<u32>, BackendError> {
    let mut windows = vec![0_u16; 1024];
    // SAFETY: windows is a writable UTF-16 buffer.
    let length = unsafe { GetWindowsDirectoryW(windows.as_mut_ptr(), windows.len() as u32) };
    if length == 0 || length as usize >= windows.len() {
        return Err(super::last_error("GetWindowsDirectoryW"));
    }
    windows.truncate(length as usize);
    windows.push(0);

    let mut mount = vec![0_u16; 1024];
    // SAFETY: input is NUL-terminated and output is writable.
    let ok =
        unsafe { GetVolumePathNameW(windows.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) };
    if ok == 0 {
        return Err(super::last_error("GetVolumePathNameW"));
    }
    let mut volume = vec![0_u16; 1024];
    // SAFETY: mount is NUL-terminated by Windows and volume is writable.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(mount.as_ptr(), volume.as_mut_ptr(), volume.len() as u32)
    };
    if ok == 0 {
        return Err(super::last_error("GetVolumeNameForVolumeMountPointW"));
    }
    let volume_name = decode_nul_terminated(&volume)?;
    let open_name = volume_name
        .strip_suffix('\\')
        .ok_or_else(|| BackendError("system volume name lacks trailing slash".into()))?;
    let handle = OwnedHandle::open(&wide(open_name))?;
    let bytes = ioctl_buffer(&handle, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, 64 * 1024)?;
    Ok(parse_disk_extents(&bytes)
        .map_err(|error| BackendError(error.to_string()))?
        .into_iter()
        .map(|extent| extent.disk_number)
        .collect())
}

fn critical_storage_usage(volumes: &[Volume]) -> Result<(BTreeSet<u32>, bool), BackendError> {
    let mut disk_numbers = system_disk_numbers()?;
    let paths = match critical_registry_paths() {
        Ok(paths) => paths,
        Err(_) => return Ok((disk_numbers, false)),
    };
    for path in paths {
        let volume_name = match volume_name_for_path(&path) {
            Ok(name) => name,
            Err(_) => return Ok((disk_numbers, false)),
        };
        let Some(volume) = volumes
            .iter()
            .find(|volume| volume.id.eq_ignore_ascii_case(&volume_name))
        else {
            return Ok((disk_numbers, false));
        };
        disk_numbers.extend(volume.disk_numbers.iter().copied());
    }
    Ok((disk_numbers, true))
}

fn critical_registry_paths() -> Result<Vec<String>, BackendError> {
    const CRASH_CONTROL: &str = r"SYSTEM\CurrentControlSet\Control\CrashControl";
    let mut paths = enumerate_pagefiles()?;

    for (name, default_path, optional) in [
        ("DumpFile", Some(r"%SystemRoot%\MEMORY.DMP"), false),
        ("MinidumpDir", Some(r"%SystemRoot%\Minidump"), false),
        ("DedicatedDumpFile", None, true),
    ] {
        let value = read_registry_strings(CRASH_CONTROL, name, false)?;
        let raw = match value.and_then(|values| values.into_iter().next()) {
            Some(value) if !value.trim().is_empty() => Some(value),
            _ => default_path.map(str::to_owned),
        };
        match raw {
            Some(raw) => paths.push(expand_environment(&raw)?),
            None if optional => {}
            None => {
                return Err(BackendError(format!(
                    "critical storage registry value {name} is unavailable"
                )))
            }
        }
    }
    if paths.iter().any(|path| {
        let normalized = path.strip_prefix(r"\??\").unwrap_or(path);
        normalized.contains('*') || normalized.contains('?')
    }) {
        return Err(BackendError(
            "critical storage configuration contains an ambiguous path".into(),
        ));
    }
    Ok(paths)
}

struct PagefileContext {
    paths: Vec<String>,
    failed: bool,
}

unsafe extern "system" fn collect_pagefile(
    context: *mut c_void,
    _information: *mut ENUM_PAGE_FILE_INFORMATION,
    filename: *const u16,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if context.is_null() || filename.is_null() {
            return false;
        }
        let mut length = 0_usize;
        // SAFETY: Windows supplies a NUL-terminated name and the scan is bounded.
        while length < 32_768 && unsafe { *filename.add(length) } != 0 {
            length += 1;
        }
        if length == 32_768 {
            return false;
        }
        // SAFETY: the callback contract keeps filename valid for this call.
        let units = unsafe { std::slice::from_raw_parts(filename, length) };
        let Ok(path) = String::from_utf16(units) else {
            return false;
        };
        // SAFETY: context points to the live PagefileContext passed below.
        unsafe { &mut *context.cast::<PagefileContext>() }
            .paths
            .push(path);
        true
    }));
    match result {
        Ok(true) => 1,
        _ => {
            if !context.is_null() {
                // SAFETY: context is the same live PagefileContext described above.
                unsafe { &mut *context.cast::<PagefileContext>() }.failed = true;
            }
            0
        }
    }
}

fn enumerate_pagefiles() -> Result<Vec<String>, BackendError> {
    let mut context = PagefileContext {
        paths: Vec::new(),
        failed: false,
    };
    // SAFETY: callback catches panics and context remains live synchronously.
    let ok = unsafe {
        K32EnumPageFilesW(
            Some(collect_pagefile),
            (&mut context as *mut PagefileContext).cast(),
        )
    };
    if ok == 0 || context.failed {
        return Err(super::last_error("K32EnumPageFilesW"));
    }
    Ok(context.paths)
}

fn read_registry_strings(
    subkey: &str,
    value: &str,
    multi: bool,
) -> Result<Option<Vec<String>>, BackendError> {
    let subkey = wide(subkey);
    let value = wide(value);
    let flags = if multi {
        RRF_RT_REG_MULTI_SZ
    } else {
        RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND
    };
    let mut value_type = 0_u32;
    let mut size = 0_u32;
    // SAFETY: names are NUL-terminated and this is the documented sizing call.
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            flags,
            &mut value_type,
            null_mut(),
            &mut size,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if result != ERROR_SUCCESS || size == 0 || size as usize > MAX_BUFFER || size % 2 != 0 {
        return Err(BackendError(format!(
            "RegGetValueW sizing failed with Win32 error {result}"
        )));
    }
    let mut bytes = vec![0_u8; size as usize];
    // SAFETY: bytes is writable for the size supplied by the preceding call.
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            flags,
            &mut value_type,
            bytes.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != ERROR_SUCCESS || size as usize > bytes.len() || size % 2 != 0 {
        return Err(BackendError(format!(
            "RegGetValueW failed with Win32 error {result}"
        )));
    }
    bytes.truncate(size as usize);
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    match value_type {
        REG_MULTI_SZ if multi => parse_multi_sz(&units)
            .map(Some)
            .map_err(|error| BackendError(error.to_string())),
        REG_SZ | REG_EXPAND_SZ if !multi => {
            decode_nul_terminated(&units).map(|value| Some(vec![value]))
        }
        _ => Err(BackendError("registry value has an unexpected type".into())),
    }
}

fn expand_environment(value: &str) -> Result<String, BackendError> {
    let source = wide(value);
    // SAFETY: source is NUL-terminated; a null output requests the required size.
    let required = unsafe { ExpandEnvironmentStringsW(source.as_ptr(), null_mut(), 0) };
    if required == 0 || required as usize > MAX_BUFFER / 2 {
        return Err(super::last_error("ExpandEnvironmentStringsW sizing"));
    }
    let mut output = vec![0_u16; required as usize];
    // SAFETY: output is writable for the size returned above.
    let written = unsafe {
        ExpandEnvironmentStringsW(source.as_ptr(), output.as_mut_ptr(), output.len() as u32)
    };
    if written == 0 || written as usize > output.len() {
        return Err(super::last_error("ExpandEnvironmentStringsW"));
    }
    decode_nul_terminated(&output)
}

fn volume_name_for_path(path: &str) -> Result<String, BackendError> {
    let normalized = path.strip_prefix(r"\??\").unwrap_or(path);
    let path = wide(normalized);
    let mut mount = vec![0_u16; 1024];
    // SAFETY: path is NUL-terminated and mount is writable.
    let ok = unsafe { GetVolumePathNameW(path.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) };
    if ok == 0 {
        return Err(super::last_error("GetVolumePathNameW critical path"));
    }
    let mut volume = vec![0_u16; 1024];
    // SAFETY: mount is NUL-terminated by Windows and volume is writable.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(mount.as_ptr(), volume.as_mut_ptr(), volume.len() as u32)
    };
    if ok == 0 {
        return Err(super::last_error(
            "GetVolumeNameForVolumeMountPointW critical path",
        ));
    }
    decode_nul_terminated(&volume)
}

fn resolve_target(
    devinst: u32,
    disk_instance_id: &str,
) -> Result<Option<(Target, u32)>, BackendError> {
    resolve_target_with(
        devinst,
        disk_instance_id,
        ancestor_properties,
        parent_devinst,
        inspect_target,
    )
}

struct AncestorProperties {
    enumerator: String,
    service: String,
    capabilities: u32,
}

impl AncestorProperties {
    fn is_usb_hub(&self) -> bool {
        self.enumerator.eq_ignore_ascii_case("USB")
            && matches!(
                self.service.to_ascii_lowercase().as_str(),
                "usbhub" | "usbhub3"
            )
    }
}

fn ancestor_properties(devinst: u32) -> Result<AncestorProperties, BackendError> {
    Ok(AncestorProperties {
        enumerator: devnode_string(devinst, CM_DRP_ENUMERATOR_NAME)?,
        service: devnode_string(devinst, CM_DRP_SERVICE)?,
        capabilities: devnode_dword(devinst, CM_DRP_CAPABILITIES)?,
    })
}

fn parent_devinst(devinst: u32) -> Result<Option<u32>, BackendError> {
    let mut parent = 0_u32;
    // SAFETY: parent is writable and devinst is a local devnode obtained from SetupAPI/CM.
    let result = unsafe { CM_Get_Parent(&mut parent, devinst, 0) };
    match result {
        CR_SUCCESS => Ok(Some(parent)),
        CR_NO_SUCH_DEVNODE => Ok(None),
        _ => Err(BackendError(format!(
            "CM_Get_Parent failed with CONFIGRET {result}"
        ))),
    }
}

fn resolve_target_with<P, A, I>(
    devinst: u32,
    disk_instance_id: &str,
    mut properties: P,
    mut parent: A,
    mut inspect: I,
) -> Result<Option<(Target, u32)>, BackendError>
where
    P: FnMut(u32) -> Result<AncestorProperties, BackendError>,
    A: FnMut(u32) -> Result<Option<u32>, BackendError>,
    I: FnMut(u32, &str) -> Result<Target, BackendError>,
{
    let mut current = devinst;
    let mut path_complete = true;
    let mut hub_boundary_found = false;
    let mut candidates = Vec::new();
    for _ in 0..MAX_TOPOLOGY_DEPTH {
        match properties(current) {
            Ok(node) if node.is_usb_hub() => {
                // Stop before inspecting a hub as an eject candidate, regardless
                // of its removable bit. Devices beyond it are outside this target.
                hub_boundary_found = true;
                break;
            }
            Ok(node)
                if node.enumerator.eq_ignore_ascii_case("USB")
                    && node.capabilities & CM_DEVCAP_REMOVABLE != 0 =>
            {
                candidates.push((inspect(current, disk_instance_id)?, current));
            }
            Err(_) => path_complete = false,
            _ => {}
        }
        let Some(next) = parent(current)? else {
            break;
        };
        current = next;
    }
    let unique = candidates.len() == 1;
    Ok(candidates.into_iter().next().map(|(mut target, devinst)| {
        target.topology_complete &= path_complete && hub_boundary_found && unique;
        (target, devinst)
    }))
}

fn inspect_target(target: u32, disk_instance_id: &str) -> Result<Target, BackendError> {
    let target_instance_id = device_id(target)?;
    let service = devnode_string(target, CM_DRP_SERVICE).unwrap_or_default();
    let container_id = devnode_container_id(target).unwrap_or_default();
    let topology = inspect_subtree(target, &target_instance_id, disk_instance_id)?;
    let parent_is_valid_hub = parent_is_hub(target);
    let service_supported = is_storage_transport_service(&service);
    let name = devnode_string(target, CM_DRP_FRIENDLYNAME)
        .or_else(|_| devnode_string(target, CM_DRP_DEVICEDESC))
        .unwrap_or_else(|_| "USB storage device".into());
    Ok(Target {
        instance_id: target_instance_id,
        container_id: container_id.clone(),
        service: service.clone(),
        name,
        removable: true,
        topology_complete: topology.complete
            && topology.contains_disk
            && !container_id.is_empty()
            && !service.is_empty(),
        is_hub: topology.is_hub,
        has_non_storage_descendants: topology.has_non_storage || !service_supported,
        parent_is_valid_hub,
        descendant_disk_instance_ids: topology.disk_instance_ids,
    })
}

fn parent_is_hub(devinst: u32) -> bool {
    let Ok(Some(parent)) = parent_devinst(devinst) else {
        return false;
    };
    ancestor_properties(parent).is_ok_and(|node| node.is_usb_hub())
}

fn is_storage_transport_service(service: &str) -> bool {
    matches!(
        service.to_ascii_lowercase().as_str(),
        "usbstor" | "uaspstor"
    )
}

struct TopologyInspection {
    complete: bool,
    contains_disk: bool,
    is_hub: bool,
    has_non_storage: bool,
    disk_instance_ids: BTreeSet<String>,
}

fn inspect_subtree(
    target: u32,
    target_instance_id: &str,
    disk_instance_id: &str,
) -> Result<TopologyInspection, BackendError> {
    let (service, mut complete) = match devnode_string(target, CM_DRP_SERVICE) {
        Ok(service) => (service, true),
        Err(_) => (String::new(), false),
    };
    let target_id_lower = target_instance_id.to_ascii_lowercase();
    let is_hub = service.to_ascii_lowercase().contains("hub")
        || target_id_lower.contains("root_hub")
        || target_id_lower.contains("\\hub");
    let mut nodes = Vec::new();
    collect_descendants(target, 0, &mut nodes, &mut complete)?;
    if nodes.len() > MAX_TOPOLOGY_NODES {
        complete = false;
    }
    let mut contains_disk = false;
    let mut has_non_storage = false;
    let mut disk_instance_ids = BTreeSet::new();
    for node in nodes.into_iter().take(MAX_TOPOLOGY_NODES) {
        let node_id = match device_id(node) {
            Ok(value) => Some(value),
            Err(_) => {
                complete = false;
                None
            }
        };
        let id_matches = node_id
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(disk_instance_id));
        if id_matches {
            contains_disk = true;
        }
        let class = devnode_string(node, CM_DRP_CLASS);
        let service = devnode_string(node, CM_DRP_SERVICE);
        match (class, service) {
            (Ok(class), Ok(service)) if is_known_storage_node(&class, &service) => {
                if class.eq_ignore_ascii_case("DiskDrive") {
                    if let Some(node_id) = node_id {
                        disk_instance_ids.insert(node_id);
                    } else {
                        complete = false;
                    }
                }
            }
            (Ok(_), Ok(_)) => has_non_storage = true,
            _ => complete = false,
        }
    }
    Ok(TopologyInspection {
        complete,
        contains_disk,
        is_hub,
        has_non_storage,
        disk_instance_ids,
    })
}

fn collect_descendants(
    parent: u32,
    depth: usize,
    nodes: &mut Vec<u32>,
    complete: &mut bool,
) -> Result<(), BackendError> {
    if depth >= MAX_TOPOLOGY_DEPTH || nodes.len() >= MAX_TOPOLOGY_NODES {
        *complete = false;
        return Ok(());
    }
    let mut child = 0_u32;
    // SAFETY: child is writable and parent is a local devnode.
    let result = unsafe { CM_Get_Child(&mut child, parent, 0) };
    if result == CR_NO_SUCH_DEVNODE {
        return Ok(());
    }
    if result != CR_SUCCESS {
        *complete = false;
        return Ok(());
    }
    loop {
        if nodes.len() >= MAX_TOPOLOGY_NODES {
            *complete = false;
            return Ok(());
        }
        nodes.push(child);
        collect_descendants(child, depth + 1, nodes, complete)?;
        let mut sibling = 0_u32;
        // SAFETY: sibling is writable and child is a local devnode.
        let result = unsafe { CM_Get_Sibling(&mut sibling, child, 0) };
        if result == CR_NO_SUCH_DEVNODE {
            break;
        }
        if result != CR_SUCCESS {
            *complete = false;
            break;
        }
        child = sibling;
    }
    Ok(())
}

fn is_known_storage_node(class: &str, service: &str) -> bool {
    matches!(
        (
            class.to_ascii_lowercase().as_str(),
            service.to_ascii_lowercase().as_str()
        ),
        ("diskdrive", "disk")
            | ("scsiadapter", "uaspstor")
            | ("scsiadapter", "usbstor")
            | ("volume", "volmgr")
            | ("volume", "volume")
            | ("storagevolume", "volmgr")
            | ("storagevolume", "volume")
            | ("storage", "partmgr")
            | ("storage", "volmgr")
    )
}

fn devnode_string(devinst: u32, property: u32) -> Result<String, BackendError> {
    let bytes = devnode_property(devinst, property)?;
    if bytes.len() % 2 != 0 {
        return Err(BackendError("devnode string byte count is invalid".into()));
    }
    // SAFETY: u8 buffer is interpreted as UTF-16 with checked even length; unaligned reads are copied.
    let values = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    decode_nul_terminated(&values)
}

fn devnode_dword(devinst: u32, property: u32) -> Result<u32, BackendError> {
    let bytes = devnode_property(devinst, property)?;
    if bytes.len() != 4 {
        return Err(BackendError("devnode DWORD size is invalid".into()));
    }
    Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
}

fn devnode_container_id(devinst: u32) -> Result<String, BackendError> {
    let mut property_type = 0_u32;
    let mut size = 0_u32;
    // SAFETY: this is the documented sizing call and all output pointers are valid.
    let result = unsafe {
        CM_Get_DevNode_PropertyW(
            devinst,
            &DEVPKEY_Device_ContainerId,
            &mut property_type,
            null_mut(),
            &mut size,
            0,
        )
    };
    if result != CR_BUFFER_SMALL || size as usize != size_of::<windows_sys::core::GUID>() {
        return Err(BackendError(
            "device container ID is unavailable or malformed".into(),
        ));
    }
    let mut guid = MaybeUninit::<windows_sys::core::GUID>::zeroed();
    // SAFETY: guid is writable for the exact size requested by the sizing call.
    let result = unsafe {
        CM_Get_DevNode_PropertyW(
            devinst,
            &DEVPKEY_Device_ContainerId,
            &mut property_type,
            guid.as_mut_ptr().cast::<u8>(),
            &mut size,
            0,
        )
    };
    if result != CR_SUCCESS
        || property_type != DEVPROP_TYPE_GUID
        || size as usize != size_of::<windows_sys::core::GUID>()
    {
        return Err(BackendError("device container ID query failed".into()));
    }
    // SAFETY: the successful query initialized the complete GUID.
    let guid = unsafe { guid.assume_init() };
    Ok(format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7]
    ))
}

fn devnode_property(devinst: u32, property: u32) -> Result<Vec<u8>, BackendError> {
    let mut length = 0_u32;
    let mut data_type = 0_u32;
    // SAFETY: this is the documented sizing call with valid output pointers.
    let result = unsafe {
        CM_Get_DevNode_Registry_PropertyW(
            devinst,
            property,
            &mut data_type,
            null_mut(),
            &mut length,
            0,
        )
    };
    if result != CR_BUFFER_SMALL && !(result == CR_SUCCESS && length == 0) {
        return Err(BackendError(format!(
            "devnode property {property} sizing failed with CONFIGRET {result}"
        )));
    }
    if length == 0 || length as usize > MAX_BUFFER {
        return Err(BackendError("devnode property size is invalid".into()));
    }
    let mut bytes = vec![0_u8; length as usize];
    // SAFETY: bytes is writable for length bytes and all output pointers are valid.
    let result = unsafe {
        CM_Get_DevNode_Registry_PropertyW(
            devinst,
            property,
            &mut data_type,
            bytes.as_mut_ptr().cast::<c_void>(),
            &mut length,
            0,
        )
    };
    if result != CR_SUCCESS || length as usize > bytes.len() {
        return Err(BackendError(format!(
            "devnode property {property} failed with CONFIGRET {result}"
        )));
    }
    bytes.truncate(length as usize);
    Ok(bytes)
}

fn device_id(devinst: u32) -> Result<String, BackendError> {
    let mut buffer = vec![0_u16; MAX_DEVICE_ID_LEN as usize + 1];
    // SAFETY: buffer is writable and devinst is local.
    let result = unsafe { CM_Get_Device_IDW(devinst, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
    if result != CR_SUCCESS {
        return Err(BackendError(format!(
            "CM_Get_Device_IDW failed with CONFIGRET {result}"
        )));
    }
    decode_nul_terminated(&buffer)
}

fn ioctl_fixed<T>(handle: &OwnedHandle, code: u32) -> Result<T, BackendError> {
    let mut output = MaybeUninit::<T>::zeroed();
    let mut returned = 0_u32;
    // SAFETY: output points to a valid fixed-size T buffer and no input is required.
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw(),
            code,
            null(),
            0,
            output.as_mut_ptr().cast(),
            size_of::<T>() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: read immediately after the failed synchronous API call.
        let error = unsafe { GetLastError() };
        return Err(BackendError(format!(
            "DeviceIoControl 0x{code:08x} failed with Win32 error {error}"
        )));
    }
    if returned < size_of::<T>() as u32 {
        return Err(BackendError(format!(
            "DeviceIoControl 0x{code:08x} returned a truncated fixed output ({returned} bytes)"
        )));
    }
    // SAFETY: DeviceIoControl initialized at least size_of::<T>() bytes.
    Ok(unsafe { output.assume_init() })
}

fn ioctl_buffer(handle: &OwnedHandle, code: u32, capacity: usize) -> Result<Vec<u8>, BackendError> {
    if capacity == 0 || capacity > MAX_BUFFER {
        return Err(BackendError("IOCTL buffer capacity is invalid".into()));
    }
    let mut bytes = vec![0_u8; capacity];
    let mut returned = 0_u32;
    // SAFETY: bytes is writable for capacity bytes and no input is required.
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw(),
            code,
            null(),
            0,
            bytes.as_mut_ptr().cast(),
            bytes.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: GetLastError has no preconditions.
        let error = unsafe { GetLastError() };
        if error == ERROR_MORE_DATA || error == ERROR_INSUFFICIENT_BUFFER {
            return Err(BackendError(format!(
                "DeviceIoControl 0x{code:08x} output exceeded the {capacity}-byte safety limit"
            )));
        }
        return Err(BackendError(format!(
            "DeviceIoControl 0x{code:08x} failed with Win32 error {error}"
        )));
    }
    if returned as usize > bytes.len() {
        return Err(BackendError(format!(
            "DeviceIoControl 0x{code:08x} returned an invalid byte count ({returned})"
        )));
    }
    bytes.truncate(returned as usize);
    Ok(bytes)
}

fn zeroed_interface_data() -> SP_DEVICE_INTERFACE_DATA {
    // SAFETY: this Win32 POD structure permits zero initialization; cbSize is then set.
    let mut value: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
    value.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
    value
}

fn zeroed_devinfo_data() -> SP_DEVINFO_DATA {
    // SAFETY: this Win32 POD structure permits zero initialization; cbSize is then set.
    let mut value: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
    value.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
    value
}

fn decode_nul_terminated(values: &[u16]) -> Result<String, BackendError> {
    let end = values
        .iter()
        .position(|value| *value == 0)
        .ok_or_else(|| BackendError("UTF-16 buffer is not NUL-terminated".into()))?;
    String::from_utf16(&values[..end])
        .map_err(|_| BackendError("Windows returned invalid UTF-16".into()))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        assemble_snapshot, is_storage_transport_service, resolve_target_with, AncestorProperties,
        DiskEnumeration, ScanDiagnostics, VolumeEnumeration, MAX_TOPOLOGY_DEPTH,
    };
    use crate::{
        backend::BackendError,
        model::{Disk, Partition, Volume, VolumeExtent},
        policy::{evaluate_inventory, BlockReason, BusType, DiskCandidate, Inventory, Target},
    };
    use std::collections::BTreeSet;

    fn disks() -> DiskEnumeration {
        let candidates = [4, 7]
            .into_iter()
            .map(|number| DiskCandidate {
                disk: Disk {
                    instance_id: format!("USBSTOR\\test-{number}"),
                    number,
                    name: format!("Test USB {number}"),
                    serial: String::new(),
                    capacity_bytes: 128_035_676_160,
                    partition_count: 0,
                    partitions: Vec::new(),
                    partition_table_supported: false,
                },
                bus_type: BusType::Usb,
                target: None,
            })
            .collect();
        DiskEnumeration {
            candidates,
            virtual_dependency_scan_complete: true,
            virtual_host_volume_ids: BTreeSet::new(),
            diagnostics: ScanDiagnostics::default(),
        }
    }

    fn volumes() -> VolumeEnumeration {
        VolumeEnumeration {
            volumes: Vec::new(),
            diagnostics: ScanDiagnostics::default(),
        }
    }

    #[test]
    fn incomplete_disk_volume_or_critical_scan_preserves_rows_but_blocks_all() {
        for failed_scan in 0..5 {
            let mut disks = disks();
            let mut volumes = volumes();
            let mut critical = Ok((BTreeSet::new(), true));
            let failure = || BackendError("simulated access denied".into());
            match failed_scan {
                0 => disks.diagnostics.record("unrelated disk", failure()),
                1 => volumes.diagnostics.record("unrelated volume", failure()),
                2 => critical = Err(failure()),
                3 => critical = Ok((BTreeSet::new(), false)),
                _ => disks.virtual_dependency_scan_complete = false,
            }
            let snapshot = assemble_snapshot(disks, volumes, critical).unwrap();
            assert_eq!(snapshot.listed.len(), 2);
            for item in snapshot.listed {
                assert_eq!(item.eligibility, Err(BlockReason::InventoryIncomplete));
            }
        }
    }

    #[test]
    fn failed_scan_with_no_usb_rows_reports_the_original_operation() {
        let mut disks = disks();
        disks.candidates.clear();
        disks.diagnostics.record(
            "disk interface 2",
            BackendError("DeviceIoControl 0x000700a0 failed with Win32 error 5".into()),
        );
        let result = assemble_snapshot(disks, volumes(), Ok((BTreeSet::new(), true)));
        let problem = match result {
            Ok(_) => panic!("failed inventory must not be reported as an empty success"),
            Err(problem) => problem,
        };
        assert!(problem.0.contains("disk interface 2"));
        assert!(problem.0.contains("0x000700a0"));
        assert!(problem.0.contains("Win32 error 5"));
    }

    type TestAncestor<'a> = Option<(&'a str, &'a str, u32)>;

    fn test_properties(node: TestAncestor<'_>) -> Result<AncestorProperties, BackendError> {
        let (enumerator, service, capabilities) =
            node.ok_or_else(|| BackendError("simulated missing ancestor property".into()))?;
        Ok(AncestorProperties {
            enumerator: enumerator.into(),
            service: service.into(),
            capabilities,
        })
    }

    fn resolve_test_path(
        path: &[TestAncestor<'_>],
        start: u32,
        inspected: &mut Vec<u32>,
    ) -> Option<(Target, u32)> {
        resolve_target_with(
            start,
            r"USBSTOR\test-7",
            |node| test_properties(path[node as usize]),
            |node| Ok((node as usize + 1 < path.len()).then_some(node + 1)),
            |node, disk_id| {
                inspected.push(node);
                let properties = test_properties(path[node as usize])?;
                assert!(
                    !properties.is_usb_hub(),
                    "a hub must never be an eject candidate"
                );
                let parent_is_valid_hub =
                    path.get(node as usize + 1).copied().is_some_and(|parent| {
                        test_properties(parent).is_ok_and(|info| info.is_usb_hub())
                    });
                Ok(Target {
                    instance_id: format!(r"USB\target-{node}"),
                    container_id: format!("{{container-{node}}}"),
                    name: "Test USB".into(),
                    removable: true,
                    topology_complete: true,
                    is_hub: false,
                    has_non_storage_descendants: !is_storage_transport_service(
                        &properties.service,
                    ),
                    parent_is_valid_hub,
                    descendant_disk_instance_ids: BTreeSet::from([disk_id.into()]),
                    service: properties.service,
                })
            },
        )
        .unwrap()
    }

    fn eligibility(target: Target) -> Result<(), BlockReason> {
        let inventory = Inventory {
            disks: vec![DiskCandidate {
                disk: Disk {
                    instance_id: r"USBSTOR\test-7".into(),
                    number: 7,
                    name: "Test USB disk".into(),
                    serial: "test-7".into(),
                    capacity_bytes: 8_000_000_000,
                    partition_count: 1,
                    partitions: vec![Partition {
                        number: 1,
                        starting_offset: 1_048_576,
                        length: 4_000_000,
                    }],
                    partition_table_supported: true,
                },
                bus_type: BusType::Usb,
                target: Some(target),
            }],
            volumes: vec![Volume {
                id: r"\\?\Volume{test-7}\".into(),
                mount_paths: vec![r"H:\".into()],
                capacity_bytes: Some(4_000_000),
                filesystem: Some("exFAT".into()),
                disk_numbers: vec![7],
                extents: vec![VolumeExtent {
                    disk_number: 7,
                    starting_offset: 1_048_576,
                    extent_length: 4_000_000,
                }],
                mapping_complete: true,
            }],
            critical_disk_numbers: BTreeSet::new(),
            critical_scan_complete: true,
            virtual_dependency_scan_complete: true,
            virtual_host_volume_ids: BTreeSet::new(),
            complete: true,
        };
        let mut listed = evaluate_inventory(inventory);
        assert_eq!(listed.len(), 1);
        listed.remove(0).eligibility
    }

    #[test]
    fn discovery_and_revalidation_stop_below_removable_usb_hubs() {
        for service in ["USBSTOR", "UASPStor"] {
            for hub_service in ["USBHUB", "USBHUB3"] {
                for hub_capabilities in [0x80, 0x84] {
                    let path = [
                        Some(("USBSTOR", "disk", 0x10)),
                        Some(("USB", service, 0x94)),
                        Some(("USB", hub_service, hub_capabilities)),
                        // Unknown nodes above the verified hub are not inspected.
                        None,
                    ];
                    let mut inspected = Vec::new();
                    let (discovered, node) = resolve_test_path(&path, 0, &mut inspected).unwrap();
                    assert_eq!(node, 1);
                    assert_eq!(inspected, vec![1]);
                    assert_eq!(eligibility(discovered.clone()), Ok(()));

                    inspected.clear();
                    let (revalidated, node) = resolve_test_path(&path, 1, &mut inspected).unwrap();
                    assert_eq!(node, 1);
                    assert_eq!(inspected, vec![1]);
                    assert_eq!(revalidated, discovered);
                }
            }
        }
    }

    #[test]
    fn composite_and_unknown_nodes_below_the_hub_remain_blocked() {
        for ancestor in [
            Some(("USB", "usbccgp", 0x84)),
            Some(("USB", "usbccgp", 0x80)),
            Some(("USB", "unknown", 0x80)),
            Some(("SWD", "USBHUB3", 0x84)),
            None,
        ] {
            let path = [
                Some(("USBSTOR", "disk", 0x10)),
                Some(("USB", "USBSTOR", 0x94)),
                ancestor,
                Some(("USB", "USBHUB3", 0x84)),
            ];
            let mut inspected = Vec::new();
            let (target, node) = resolve_test_path(&path, 0, &mut inspected).unwrap();
            assert_eq!(node, 1);
            assert!(!inspected.contains(&3));
            assert_eq!(eligibility(target), Err(BlockReason::TopologyIncomplete));
        }
    }

    #[test]
    fn missing_or_unreached_hub_boundary_never_approves_a_target() {
        let mut path = vec![
            Some(("USBSTOR", "disk", 0x10)),
            Some(("USB", "USBSTOR", 0x94)),
        ];
        let (target, _) = resolve_test_path(&path, 0, &mut Vec::new()).unwrap();
        assert_eq!(eligibility(target), Err(BlockReason::TopologyIncomplete));

        path.resize(MAX_TOPOLOGY_DEPTH + 1, Some(("OTHER", "unknown", 0)));
        path.push(Some(("USB", "USBHUB3", 0x84)));
        let (target, _) = resolve_test_path(&path, 0, &mut Vec::new()).unwrap();
        assert!(!target.topology_complete);
        assert_eq!(eligibility(target), Err(BlockReason::TopologyIncomplete));

        let only_hub = [Some(("USB", "USBHUB3", 0x84))];
        let mut inspected = Vec::new();
        assert!(resolve_test_path(&only_hub, 0, &mut inspected).is_none());
        assert!(inspected.is_empty());
    }
}
