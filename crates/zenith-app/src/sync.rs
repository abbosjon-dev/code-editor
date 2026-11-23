//! Settings Sync: keeps settings.toml, keybindings.toml and plugins/ in a
//! Git repository of the user's choosing, so another machine can pull them.
//!
//! The repository is cloned into `<config>/sync-repo`. Push copies the files
//! in (adding and updating, never deleting), commits, rebases onto the
//! remote and pushes; Pull fetches, backs up
//! the current files to `<config>/sync-backup/<time>/` and copies the
//! repository's versions out.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Files and folders that are synced (relative to the config directory).
pub const SYNCED: &[&str] = &["settings.toml", "keybindings.toml", "plugins"];

pub fn repo_dir(config: &Path) -> PathBuf {
    config.join("sync-repo")
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir).env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("git {} failed", args.first().unwrap_or(&"")) } else { err })
    }
}

/// Clone `url` (an empty repository is fine) into the sync folder.
pub fn setup(config: &Path, url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("Enter the URL of a Git repository (e.g. a private GitHub repo)".into());
    }
    std::fs::create_dir_all(config).map_err(|e| e.to_string())?;
    let dir = repo_dir(config);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    git(config, &["clone", "--quiet", url, "sync-repo"])?;
    Ok(format!("Connected to {url}"))
}

pub fn is_set_up(config: &Path) -> bool {
    repo_dir(config).join(".git").exists()
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            let name = e.file_name();
            if name == ".git" || name == "__pycache__" || name == "node_modules" {
                continue;
            }
            copy_tree(&e.path(), &to.join(&name))?;
        }
        Ok(())
    } else {
        if let Some(p) = to.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(from, to).map(|_| ())
    }
}

fn remove(p: &Path) -> std::io::Result<()> {
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else if p.exists() {
        std::fs::remove_file(p)
    } else {
        Ok(())
    }
}

/// Mirror the synced items from `from` into `to` (missing ones are removed).
fn mirror(from: &Path, to: &Path) -> std::io::Result<()> {
    for item in SYNCED {
        let (src, dst) = (from.join(item), to.join(item));
        remove(&dst)?;
        if src.exists() {
            copy_tree(&src, &dst)?;
        }
    }
    Ok(())
}

/// Commit identity when the user has none configured.
fn identity(dir: &Path) -> Vec<String> {
    if git(dir, &["config", "user.email"]).is_ok_and(|e| !e.is_empty()) {
        vec![]
    } else {
        vec!["-c".into(), "user.name=Zenith".into(), "-c".into(), "user.email=zenith@localhost".into()]
    }
}

fn has_remote_branch(dir: &Path) -> Option<String> {
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    git(dir, &["rev-parse", "--verify", "--quiet", &format!("origin/{branch}")]).ok().map(|_| branch)
}

/// Upload this machine's settings.
pub fn push(config: &Path) -> Result<String, String> {
    let dir = repo_dir(config);
    if !is_set_up(config) {
        return Err("Settings Sync isn't set up".into());
    }
    let _ = git(&dir, &["fetch", "--quiet", "origin"]);
    // Additive: never delete what another machine added that this one lacks.
    for item in SYNCED {
        let src = config.join(item);
        if src.exists() {
            copy_tree(&src, &dir.join(item)).map_err(|e| e.to_string())?;
        }
    }
    git(&dir, &["add", "-A"])?;
    if git(&dir, &["status", "--porcelain"])?.is_empty() {
        return Ok("Settings are already up to date in the repository".into());
    }
    let mut args: Vec<String> = identity(&dir);
    args.extend(["commit".into(), "--quiet".into(), "-m".into(), "Update Zenith settings".into()]);
    git(&dir, &args.iter().map(String::as_str).collect::<Vec<_>>())?;
    if let Some(branch) = has_remote_branch(&dir) {
        // Someone pushed from another machine: put our commit on top.
        let mut args: Vec<String> = identity(&dir);
        args.extend(["rebase".into(), "--quiet".into(), format!("origin/{branch}")]);
        if let Err(e) = git(&dir, &args.iter().map(String::as_str).collect::<Vec<_>>()) {
            let _ = git(&dir, &["rebase", "--abort"]);
            let _ = git(&dir, &["reset", "--quiet", "--hard", &format!("origin/{branch}")]);
            return Err(format!("The repository has conflicting changes; Pull first, then Push again. ({e})"));
        }
    }
    git(&dir, &["push", "--quiet", "-u", "origin", "HEAD"])?;
    Ok("Settings pushed".into())
}

/// Download the repository's settings; returns where the old files were backed up.
pub fn pull(config: &Path) -> Result<String, String> {
    let dir = repo_dir(config);
    if !is_set_up(config) {
        return Err("Settings Sync isn't set up".into());
    }
    git(&dir, &["fetch", "--quiet", "origin"])?;
    let Some(branch) = has_remote_branch(&dir) else {
        return Ok("The repository is empty: Push from this machine first".into());
    };
    git(&dir, &["reset", "--quiet", "--hard", &format!("origin/{branch}")])?;
    if !SYNCED.iter().any(|i| dir.join(i).exists()) {
        return Ok("The repository has no settings yet".into());
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let backup = config.join("sync-backup").join(stamp.to_string());
    std::fs::create_dir_all(&backup).map_err(|e| e.to_string())?;
    for item in SYNCED {
        let src = config.join(item);
        if src.exists() {
            copy_tree(&src, &backup.join(item)).map_err(|e| e.to_string())?;
        }
    }
    mirror(&dir, config).map_err(|e| e.to_string())?;
    Ok(format!("Settings pulled (previous ones saved in {})", backup.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two "machines" share settings through a bare repository.
    #[test]
    fn push_and_pull_between_machines() {
        let root = std::env::temp_dir().join(format!("zenith-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let remote = root.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        if git(&remote, &["init", "--quiet", "--bare"]).is_err() {
            return; // no git
        }
        let url = remote.to_string_lossy().into_owned();
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(a.join("plugins/p1")).unwrap();
        std::fs::write(a.join("settings.toml"), "theme = \"zenith-daylight\"\n").unwrap();
        std::fs::write(a.join("plugins/p1/plugin.toml"), "id = \"p1\"\n").unwrap();
        std::fs::write(a.join("session.json"), "{}").unwrap(); // not synced

        setup(&a, &url).unwrap();
        assert_eq!(push(&a).unwrap(), "Settings pushed");
        assert!(push(&a).unwrap().contains("already up to date"));

        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("settings.toml"), "theme = \"old\"\n").unwrap();
        setup(&b, &url).unwrap();
        let msg = pull(&b).unwrap();
        assert!(msg.contains("pulled"), "{msg}");
        assert!(std::fs::read_to_string(b.join("settings.toml")).unwrap().contains("daylight"));
        assert!(b.join("plugins/p1/plugin.toml").exists());
        assert!(!b.join("session.json").exists());
        // The old file was backed up.
        let backups: Vec<_> = std::fs::read_dir(b.join("sync-backup")).unwrap().flatten().collect();
        assert!(std::fs::read_to_string(backups[0].path().join("settings.toml")).unwrap().contains("old"));

        // b changes and pushes; a pushes something else after rebasing.
        std::fs::write(b.join("keybindings.toml"), "[bindings]\n").unwrap();
        push(&b).unwrap();
        std::fs::write(a.join("plugins/p1/main.py"), "print(1)\n").unwrap();
        push(&a).unwrap();
        pull(&b).unwrap();
        assert!(b.join("plugins/p1/main.py").exists());
        assert!(b.join("keybindings.toml").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
