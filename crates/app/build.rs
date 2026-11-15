//! Windows only: embeds the icon and the version information into nit.exe. Elsewhere it does
//! nothing (Linux takes the icon from the desktop file, macOS from the app bundle).

fn main() {
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let icon =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/icons/nit.ico");
        println!("cargo:rerun-if-changed={}", icon.display());
        let mut res = winresource::WindowsResource::new();
        res.set_icon(&icon.to_string_lossy())
            .set("ProductName", "nit")
            .set("FileDescription", "nit")
            .set("InternalName", "nit")
            .set("OriginalFilename", "nit.exe")
            .set("LegalCopyright", "Copyright (c) 2026 The nit contributors");
        res.compile()
            .expect("embedding the Windows resources (icon, version)");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
