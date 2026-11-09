fn main() {
    // icon and version info (Task Manager, Start menu, installer)
    embed_resource::compile("docksy.rc", embed_resource::NONE).manifest_optional().unwrap();
    println!("cargo:rerun-if-changed=docksy.rc");
    println!("cargo:rerun-if-changed=assets/docksy.ico");
    // Common Controls v6 (modern tooltips and menus), embedded in the exe.
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
    // GUI app: no console window.
    println!("cargo:rustc-link-arg-bins=/SUBSYSTEM:WINDOWS");
    println!("cargo:rustc-link-arg-bins=/ENTRY:mainCRTStartup");
}
