#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    if let Err(problem) = usb_disk_ejector::gui::native::run() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        let text = wide(&format!("USB 安全弹出工具无法启动：\n\n{problem}"));
        let caption = wide("USB 安全弹出工具");
        // SAFETY: both strings are NUL-terminated and live for the synchronous call.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                caption.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("USB 安全弹出工具仅支持 64 位 Windows。");
}
