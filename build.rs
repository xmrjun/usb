use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
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
