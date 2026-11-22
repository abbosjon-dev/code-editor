//! Scaffolds the bundled example plugins into the user's plugins folder.

use std::path::{Path, PathBuf};

const EXAMPLES: &[(&str, &str, &str)] = &[
    (
        "text-tools",
        include_str!("../../../plugins/examples/text-tools/plugin.toml"),
        include_str!("../../../plugins/examples/text-tools/main.py"),
    ),
    (
        "todo-tree",
        include_str!("../../../plugins/examples/todo-tree/plugin.toml"),
        include_str!("../../../plugins/examples/todo-tree/main.py"),
    ),
];

/// Write every example plugin; returns the first one's folder.
pub fn scaffold(plugins_dir: &Path) -> std::io::Result<PathBuf> {
    let mut first = None;
    for (name, manifest, main) in EXAMPLES {
        let dir = plugins_dir.join(name);
        std::fs::create_dir_all(&dir)?;
        let mut manifest = manifest.to_string();
        // Windows usually ships `python`, not `python3`.
        if cfg!(windows) {
            manifest = manifest.replace("\"python3\"", "\"python\"");
        }
        std::fs::write(dir.join("plugin.toml"), manifest)?;
        std::fs::write(dir.join("main.py"), main)?;
        first.get_or_insert(dir);
    }
    Ok(first.unwrap_or_else(|| plugins_dir.to_path_buf()))
}
