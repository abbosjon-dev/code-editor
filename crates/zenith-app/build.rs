fn main() {
    // Embed the app icon into zenith.exe on Windows.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/icon.ico");
        res.set("FileDescription", "Zenith code editor");
        res.set("ProductName", "Zenith");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed icon: {e}");
        }
    }
    println!("cargo:rerun-if-changed=../../packaging/icon.ico");
    // `cfg(webview)`: an embedded web view is built in (always on macOS and
    // Windows; on Linux with `--features webview`).
    println!("cargo::rustc-check-cfg=cfg(webview)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "macos" || os == "windows" || std::env::var_os("CARGO_FEATURE_WEBVIEW").is_some() {
        println!("cargo::rustc-cfg=webview");
    }
}
