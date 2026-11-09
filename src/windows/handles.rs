use windows_sys::Win32::{
    Devices::DeviceAndDriverInstallation::{SetupDiDestroyDeviceInfoList, HDEVINFO},
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FindVolumeClose, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    },
};

use crate::backend::BackendError;

pub(super) struct DeviceInfoSet(pub HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by SetupDiGetClassDevsW and is owned here.
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

pub(super) struct OwnedHandle(pub HANDLE);

impl OwnedHandle {
    pub fn open(path: &[u16]) -> Result<Self, BackendError> {
        // SAFETY: path is NUL-terminated, pointers are valid, and no security attributes are used.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(super::last_error("CreateFileW"));
        }
        Ok(Self(handle))
    }

    pub fn as_raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and uniquely owned by this wrapper.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub(super) struct VolumeFindHandle(pub HANDLE);

impl Drop for VolumeFindHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by FindFirstVolumeW and is owned here.
        unsafe {
            FindVolumeClose(self.0);
        }
    }
}
