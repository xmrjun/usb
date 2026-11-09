use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-env-changed=USB_EJECTOR_BUILD_ID");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    if let Some(build_id) = std::env::var("USB_EJECTOR_BUILD_ID")
        .ok()
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .filter(|value| {
            (7..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        println!("cargo:rustc-env=USB_EJECTOR_BUILD_ID={build_id}");
    }
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        let manifest =
            PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("app.manifest");
        let argument = format!("/MANIFESTINPUT:{}", manifest.display());
        for binary in ["usb-ejector", "usb-ejector-gui"] {
            println!("cargo:rustc-link-arg-bin={binary}=/MANIFEST:EMBED");
            println!("cargo:rustc-link-arg-bin={binary}={argument}");
        }
    }
}
