use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::{
    Foundation::{GetLastError, ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION},
    Storage::FileSystem::{FlushFileBuffers, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS},
    System::{
        Ioctl::{FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME},
        IO::DeviceIoControl,
    },
};

use crate::{
    backend::{BackendError, Cancellation},
    model::{Device, Volume},
    raw::{parse_disk_extents, DiskExtent},
    volume_prepare::VolumePreparation,
};

use super::{
    eject::trace,
    handles::OwnedHandle,
    inventory::{confirm_target_identity, locate_revalidated_target},
};

const MAX_EXTENT_BUFFER: usize = 64 * 1024;
const LOCK_ATTEMPTS: u8 = 3;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(500);

pub(super) struct NativePreparation<'a> {
    expected: &'a Device,
    cancellation: &'a Cancellation,
    diagnostics: &'a mut String,
    started: Instant,
}

impl<'a> NativePreparation<'a> {
    pub(super) fn new(
        expected: &'a Device,
        cancellation: &'a Cancellation,
        diagnostics: &'a mut String,
        started: Instant,
    ) -> Self {
        Self {
            expected,
            cancellation,
            diagnostics,
            started,
        }
    }

    fn check_cancellation(&self) -> Result<(), BackendError> {
        if self.cancellation.is_cancelled() {
            Err(BackendError(
                "已取消安全卷准备；将关闭已打开的卷句柄。".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn record<T>(
        &mut self,
        operation: &str,
        volume: &Volume,
        began: Instant,
        result: &Result<T, BackendError>,
    ) {
        let outcome = match result {
            Ok(_) => "成功".to_owned(),
            Err(problem) => format!("失败：{problem}"),
        };
        trace(
            self.diagnostics,
            self.started,
            &format!(
                "{operation}; volume={}; duration_ms={}; {outcome}",
                volume.id,
                began.elapsed().as_millis()
            ),
        );
    }
}

impl VolumePreparation for NativePreparation<'_> {
    type Handle = OwnedHandle;

    fn open(&mut self, volume: &Volume) -> Result<Self::Handle, BackendError> {
        self.check_cancellation()?;
        let began = Instant::now();
        let result = volume_open_path(&volume.id).and_then(|path| {
            let path = path
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            OwnedHandle::open_read_write(&path)
        });
        self.record(
            "CreateFileW(GENERIC_READ|GENERIC_WRITE, SHARE_READ|SHARE_WRITE)",
            volume,
            began,
            &result,
        );
        result
    }

    fn verify(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError> {
        self.check_cancellation()?;
        let began = Instant::now();
        let result = read_extents(handle).and_then(|actual| {
            if extents_match(&actual, volume) {
                Ok(())
            } else {
                Err(BackendError(format!(
                    "同一句柄返回的逻辑卷磁盘范围已变化；expected={:?}; actual={actual:?}",
                    volume.extents
                )))
            }
        });
        self.record(
            "IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS / verify",
            volume,
            began,
            &result,
        );
        result
    }

    fn lock(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError> {
        let mut attempt = 0;
        loop {
            self.check_cancellation()?;
            attempt += 1;
            let began = Instant::now();
            let raw_result = control(handle, FSCTL_LOCK_VOLUME);
            let result = raw_result.map_err(|code| win32_error("FSCTL_LOCK_VOLUME", code));
            self.record(
                &format!("FSCTL_LOCK_VOLUME attempt={attempt}"),
                volume,
                began,
                &result,
            );
            match raw_result {
                Ok(()) => return Ok(()),
                Err(code) if attempt < LOCK_ATTEMPTS && retryable_lock_error(code) => {
                    self.check_cancellation()?;
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(_) => return result,
            }
        }
    }

    fn flush(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError> {
        self.check_cancellation()?;
        let began = Instant::now();
        // SAFETY: the live volume handle has GENERIC_WRITE access and is locked by this process.
        let flushed = unsafe { FlushFileBuffers(handle.as_raw()) };
        let result = if flushed == 0 {
            // SAFETY: capture the error immediately after the failed synchronous call.
            let code = unsafe { GetLastError() };
            Err(win32_error("FlushFileBuffers", code))
        } else {
            Ok(())
        };
        self.record("FlushFileBuffers", volume, began, &result);
        result
    }

    fn dismount(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError> {
        self.check_cancellation()?;
        let began = Instant::now();
        // prepare_volumes calls this only after every volume is verified, locked
        // and flushed, and after the physical target has been revalidated.
        let result = control(handle, FSCTL_DISMOUNT_VOLUME)
            .map_err(|code| win32_error("FSCTL_DISMOUNT_VOLUME", code));
        self.record(
            "FSCTL_DISMOUNT_VOLUME (previously locked)",
            volume,
            began,
            &result,
        );
        result
    }

    fn close(&mut self, handle: Self::Handle, volume: &Volume) -> Result<(), BackendError> {
        // Cleanup must run even when cancellation is set or an earlier close failed.
        let began = Instant::now();
        let result = handle.close_checked();
        self.record("CloseHandle", volume, began, &result);
        result
    }

    fn validate_target(&mut self) -> Result<(), BackendError> {
        self.check_cancellation()?;
        let began = Instant::now();
        let result = locate_revalidated_target(self.expected).and_then(|devinst| {
            confirm_target_identity(devinst, &self.expected.target_instance_id)
        });
        trace(
            self.diagnostics,
            self.started,
            &format!(
                "卷准备目标身份/拓扑复验; target={}; duration_ms={}; result={result:?}",
                self.expected.target_instance_id,
                began.elapsed().as_millis()
            ),
        );
        result
    }
}

fn volume_open_path(id: &str) -> Result<&str, BackendError> {
    const PREFIX: &str = r"\\?\Volume{";
    let invalid = || BackendError("卷准备只接受规范的 Volume GUID 路径；不使用盘符替代。".into());
    let path = id.strip_suffix('\\').ok_or_else(invalid)?;
    if path.len() != PREFIX.len() + 36 + 1
        || !path
            .get(..PREFIX.len())
            .is_some_and(|value| value.eq_ignore_ascii_case(PREFIX))
        || !path.ends_with('}')
    {
        return Err(invalid());
    }
    let guid = path.get(PREFIX.len()..path.len() - 1).ok_or_else(invalid)?;
    if !guid.bytes().enumerate().all(|(index, value)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            value == b'-'
        } else {
            value.is_ascii_hexdigit()
        }
    }) {
        return Err(invalid());
    }
    Ok(path)
}

fn extents_match(actual: &[DiskExtent], expected: &Volume) -> bool {
    if !expected.mapping_complete || expected.extents.is_empty() {
        return false;
    }
    let mut actual = actual
        .iter()
        .map(|extent| (extent.disk_number, extent.starting_offset, extent.extent_length))
        .collect::<Vec<_>>();
    let mut expected = expected
        .extents
        .iter()
        .map(|extent| (extent.disk_number, extent.starting_offset, extent.extent_length))
        .collect::<Vec<_>>();
    actual.sort_unstable();
    expected.sort_unstable();
    actual == expected
}

fn read_extents(handle: &OwnedHandle) -> Result<Vec<DiskExtent>, BackendError> {
    let mut bytes = vec![0_u8; MAX_EXTENT_BUFFER];
    let mut returned = 0_u32;
    // SAFETY: the handle is live and bytes is a writable buffer of the advertised size.
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw(),
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            null(),
            0,
            bytes.as_mut_ptr().cast(),
            bytes.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: capture the error before any subsequent Windows API call.
        let code = unsafe { GetLastError() };
        return Err(win32_error("IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS", code));
    }
    if returned as usize > bytes.len() {
        return Err(BackendError(
            "逻辑卷范围查询返回了无效的缓冲区长度。".into(),
        ));
    }
    bytes.truncate(returned as usize);
    parse_disk_extents(&bytes).map_err(|problem| BackendError(problem.to_string()))
}

fn control(handle: &OwnedHandle, code: u32) -> Result<(), u32> {
    let mut returned = 0_u32;
    // SAFETY: the handle is live; these synchronous volume controls use no input/output buffers.
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw(),
            code,
            null(),
            0,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: capture the failure code immediately after DeviceIoControl.
        Err(unsafe { GetLastError() })
    } else {
        Ok(())
    }
}

fn win32_error(operation: &str, code: u32) -> BackendError {
    BackendError(format!("{operation} 失败（Win32 {code}, 0x{code:08x}）"))
}

fn retryable_lock_error(code: u32) -> bool {
    matches!(
        code,
        ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION
    )
}

#[cfg(test)]
mod tests {
    use super::{extents_match, retryable_lock_error, volume_open_path};
    use crate::{
        model::{Volume, VolumeExtent},
        raw::DiskExtent,
    };

    #[test]
    fn preparation_accepts_only_canonical_volume_guid_paths() {
        let valid = r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\";
        assert_eq!(
            volume_open_path(valid).unwrap(),
            valid.trim_end_matches('\\')
        );
        for invalid in [
            r"\\.\G:",
            r"G:\",
            r"\\server\share\",
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}",
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\file\",
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdeg}\",
        ] {
            assert!(volume_open_path(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn extent_verification_rejects_disk_offset_length_and_count_changes() {
        let expected = Volume {
            id: String::new(),
            mount_paths: Vec::new(),
            capacity_bytes: None,
            filesystem: None,
            disk_numbers: vec![2],
            mapping_complete: true,
            extents: vec![VolumeExtent {
                disk_number: 2,
                starting_offset: 1_048_576,
                extent_length: 8192,
            }],
        };
        let valid = DiskExtent {
            disk_number: 2,
            starting_offset: 1_048_576,
            extent_length: 8192,
        };
        assert!(extents_match(std::slice::from_ref(&valid), &expected));
        for changed in [
            DiskExtent {
                disk_number: 3,
                ..valid.clone()
            },
            DiskExtent {
                starting_offset: 0,
                ..valid.clone()
            },
            DiskExtent {
                extent_length: 4096,
                ..valid.clone()
            },
        ] {
            assert!(!extents_match(&[changed], &expected));
        }
        assert!(!extents_match(&[], &expected));
        assert!(!extents_match(&[valid.clone(), valid], &expected));
    }

    #[test]
    fn lock_retries_only_documented_contention_errors() {
        for code in [5, 32, 33] {
            assert!(retryable_lock_error(code));
        }
        for code in [0, 1, 6, 21, 87, 1117] {
            assert!(!retryable_lock_error(code));
        }
    }
}
