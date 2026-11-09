use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};

use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, SetLastError, ERROR_CANCELLED, HANDLE, HWND},
    Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
};

use crate::backend::BackendError;

struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: this guard owns the token returned by a successful OpenProcessToken.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub(crate) fn is_elevated() -> Result<bool, BackendError> {
    let mut token = null_mut();
    // SAFETY: the current-process pseudo-handle is valid and token is writable.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(super::last_error("读取当前进程权限"));
    }
    let token = TokenHandle(token);
    let mut information = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0;
    // SAFETY: the token is live and the output buffer matches TokenElevation.
    let succeeded = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut information as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    if succeeded == 0 {
        return Err(super::last_error("检查管理员权限"));
    }
    if returned < size_of::<TOKEN_ELEVATION>() as u32 {
        return Err(BackendError("管理员权限信息不完整。".into()));
    }
    Ok(information.TokenIsElevated != 0)
}

pub(crate) fn relaunch_elevated(hwnd: HWND) -> Result<(), BackendError> {
    let executable = std::env::current_exe()
        .map_err(|problem| BackendError(format!("无法确定当前程序路径：{problem}")))?;
    let directory = executable
        .parent()
        .ok_or_else(|| BackendError("无法确定当前程序所在目录。".into()))?;
    let executable = executable
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let directory = directory
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let operation = "runas\0".encode_utf16().collect::<Vec<_>>();
    // SAFETY: all strings live through ShellExecuteW. No arguments are forwarded,
    // so the new window starts without a selected device or an eject request.
    let (result, error) = unsafe {
        SetLastError(0);
        let result = ShellExecuteW(
            hwnd,
            operation.as_ptr(),
            executable.as_ptr(),
            null(),
            directory.as_ptr(),
            SW_SHOWNORMAL,
        ) as isize;
        (result, GetLastError())
    };
    if result > 32 {
        Ok(())
    } else if error == ERROR_CANCELLED {
        Err(BackendError(
            "已取消管理员权限请求，当前窗口仍以普通权限运行。".into(),
        ))
    } else {
        Err(BackendError(format!(
            "无法启动管理员模式（Shell 错误 {result}，Win32 错误 {error}）。"
        )))
    }
}
