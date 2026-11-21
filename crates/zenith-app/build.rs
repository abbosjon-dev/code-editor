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
}
