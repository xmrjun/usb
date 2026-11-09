#[cfg(windows)]
fn main() -> std::process::ExitCode {
    let mut backend = usb_disk_ejector::windows::WindowsBackend::new();
    let code = usb_disk_ejector::cli::run(
        std::env::args(),
        &mut backend,
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        std::io::stderr().lock(),
    );
    std::process::ExitCode::from(code.clamp(0, 255) as u8)
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("usb-ejector is supported only on 64-bit Windows.");
    std::process::ExitCode::FAILURE
}
