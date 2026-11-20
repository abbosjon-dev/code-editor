//! Scaffolds the bundled example plugin into the user's plugins folder.

use std::path::{Path, PathBuf};

const MANIFEST: &str = include_str!("../../../plugins/examples/text-tools/plugin.toml");
const MAIN: &str = include_str!("../../../plugins/examples/text-tools/main.py");

pub fn scaffold(plugins_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = plugins_dir.join("text-tools");
    std::fs::create_dir_all(&dir)?;
    let mut manifest = MANIFEST.to_string();
    // Windows usually ships `python`, not `python3`.
    if cfg!(windows) {
        manifest = manifest.replace("\"python3\"", "\"python\"");
    }
    std::fs::write(dir.join("plugin.toml"), manifest)?;
    std::fs::write(dir.join("main.py"), MAIN)?;
    Ok(dir)
}
