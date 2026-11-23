//! Plugin marketplace: a JSON index of plugins that live in Git
//! repositories. Installing clones the plugin (only its folder when it sits
//! inside a bigger repository) into the plugins folder.
//!
//! Index format (`registry/plugins.json` in the Zenith repository by default,
//! or any URL / file set in Settings → Advanced):
//!
//! ```json
//! { "plugins": [ { "id": "todo-tree", "name": "TODO Tree", "version": "0.1.0",
//!   "description": "…", "author": "…", "repository": "https://github.com/o/r.git",
//!   "path": "plugins/todo-tree", "rev": "v0.1.0", "tags": ["productivity"] } ] }
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

pub const DEFAULT_REGISTRY: &str =
    "https://raw.githubusercontent.com/abbosjon-dev/code-editor/main/registry/plugins.json";

/// Written into an installed plugin's folder: where it came from.
const SOURCE_FILE: &str = ".zenith-source.json";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    /// Git URL (or a local path) of the repository.
    pub repository: String,
    /// Folder of the plugin inside the repository ("" = the root).
    pub path: String,
    /// Branch or tag to install ("" = the default branch).
    pub rev: String,
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
struct Index {
    plugins: Vec<Entry>,
}

pub fn parse_index(text: &str) -> Result<Vec<Entry>, String> {
    let idx: Index = serde_json::from_str(text).map_err(|e| format!("bad plugin index: {e}"))?;
    let mut v: Vec<Entry> = idx.plugins.into_iter().filter(|e| valid_id(&e.id) && !e.repository.is_empty()).collect();
    v.sort_by_key(|e| e.name.to_lowercase());
    Ok(v)
}

/// Ids become folder names: keep them plain.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[allow(unused_mut)] // changed on Windows
fn command(program: &str) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    c
}

/// Download the index: an http(s) URL, a `file://` URL or a path.
pub fn fetch_index(url: &str) -> Result<Vec<Entry>, String> {
    let url = url.trim();
    let text = if url.starts_with("http://") || url.starts_with("https://") {
        let out = command("curl")
            .args(["-fsSL", "--max-time", "15", "-H", "User-Agent: zenith-editor", url])
            .output()
            .map_err(|e| format!("curl: {e}"))?;
        if !out.status.success() {
            return Err(format!("couldn't download the plugin index: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        let path = url.strip_prefix("file://").unwrap_or(url);
        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?
    };
    parse_index(&text)
}

fn git(dir: &Path, args: &[&str]) -> Result<(), String> {
    let out = command("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Clone `e.repository` into `dest`: shallow, and only `e.path` when the
/// plugin is a folder of a bigger repository.
fn fetch(e: &Entry, dest: &Path) -> Result<(), String> {
    let parent = dest.parent().ok_or("bad folder")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let name = dest.file_name().ok_or("bad folder")?.to_string_lossy().into_owned();
    let mut args = vec!["clone", "--quiet", "--depth", "1"];
    if !e.rev.is_empty() {
        args.extend(["--branch", e.rev.as_str()]);
    }
    let path = e.path.trim_matches('/');
    if path.is_empty() {
        args.extend([e.repository.as_str(), name.as_str()]);
        return git(parent, &args);
    }
    // Sparse: fetch only the plugin's folder.
    let mut sparse = args.clone();
    sparse.extend(["--filter=blob:none", "--sparse", e.repository.as_str(), name.as_str()]);
    if git(parent, &sparse).and_then(|_| git(dest, &["sparse-checkout", "set", path])).is_ok() {
        return Ok(());
    }
    // Older git or a server without partial clone: a full shallow clone.
    let _ = std::fs::remove_dir_all(dest);
    args.extend([e.repository.as_str(), name.as_str()]);
    git(parent, &args)
}

/// Install (or update) a plugin into `plugins_dir/<id>`.
pub fn install(e: &Entry, plugins_dir: &Path) -> Result<String, String> {
    if !valid_id(&e.id) {
        return Err(format!("invalid plugin id `{}`", e.id));
    }
    let work = plugins_dir.with_file_name(format!(".plugin-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let checkout = work.join("src");
    let r = (|| {
        fetch(e, &checkout)?;
        let src = checkout.join(e.path.trim_matches('/'));
        let manifest = zenith_plugin::load_manifest(&src.join("plugin.toml"))
            .map_err(|err| format!("{} has no valid plugin.toml: {err:#}", e.name))?;
        if manifest.id != e.id {
            return Err(format!("the plugin's id is `{}`, the index says `{}`", manifest.id, e.id));
        }
        let _ = std::fs::remove_dir_all(src.join(".git"));
        let source = serde_json::to_string_pretty(e).unwrap_or_default();
        std::fs::write(src.join(SOURCE_FILE), source).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(plugins_dir).map_err(|e| e.to_string())?;
        let dest = plugins_dir.join(&e.id);
        // Keep the old version until the new one is in place.
        let old = work.join("old");
        if dest.exists() {
            std::fs::rename(&dest, &old).map_err(|e| format!("couldn't replace the old version: {e}"))?;
        }
        if let Err(err) = std::fs::rename(&src, &dest) {
            if old.exists() {
                let _ = std::fs::rename(&old, &dest);
            }
            return Err(format!("couldn't install: {err}"));
        }
        Ok(format!("{} {} installed", manifest.name, manifest.version))
    })();
    let _ = std::fs::remove_dir_all(&work);
    r
}

pub fn uninstall(id: &str, plugins_dir: &Path) -> Result<String, String> {
    if !valid_id(id) {
        return Err(format!("invalid plugin id `{id}`"));
    }
    let dir = plugins_dir.join(id);
    std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(format!("{id} uninstalled"))
}

/// Version of the plugin installed in `plugins_dir/<id>`.
pub fn installed_version(plugins_dir: &Path, id: &str) -> Option<String> {
    zenith_plugin::load_manifest(&plugins_dir.join(id).join("plugin.toml")).ok().map(|m| m.version)
}

/// `a` is a newer version than `b` (dotted numbers; text parts compare as text).
pub fn is_newer(a: &str, b: &str) -> bool {
    let parts = |s: &str| -> Vec<(u64, String)> {
        s.trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|p| (p.parse().unwrap_or(0), if p.parse::<u64>().is_ok() { String::new() } else { p.to_string() }))
            .collect()
    };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).cloned().unwrap_or_default();
        let y = b.get(i).cloned().unwrap_or_default();
        if x != y {
            return x > y;
        }
    }
    false
}

// ------------------------------------------------------------------ UI state

/// What the marketplace asks the app to do.
pub enum Outcome {
    /// Plugins changed on disk: reload them.
    Reload(String),
    Error(String),
}

enum Job {
    Index(Receiver<Result<Vec<Entry>, String>>),
    Install(String, Receiver<Result<String, String>>),
}

#[derive(Default)]
pub struct Market {
    pub entries: Vec<Entry>,
    pub error: Option<String>,
    pub search: String,
    loaded_from: Option<String>,
    jobs: Vec<Job>,
}

impl Market {
    pub fn busy_with(&self, id: &str) -> bool {
        self.jobs.iter().any(|j| matches!(j, Job::Install(i, _) if i == id))
    }

    pub fn loading(&self) -> bool {
        self.jobs.iter().any(|j| matches!(j, Job::Index(_)))
    }

    /// Load the index (once per URL unless `force`).
    pub fn load(&mut self, url: &str, force: bool, waker: Arc<dyn Fn() + Send + Sync>) {
        if self.loading() || (!force && self.loaded_from.as_deref() == Some(url)) {
            return;
        }
        self.loaded_from = Some(url.to_string());
        self.error = None;
        let (tx, rx) = channel();
        let url = url.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(fetch_index(&url));
            waker();
        });
        self.jobs.push(Job::Index(rx));
    }

    pub fn install(&mut self, e: Entry, plugins_dir: PathBuf, waker: Arc<dyn Fn() + Send + Sync>) {
        if self.busy_with(&e.id) {
            return;
        }
        let (tx, rx) = channel();
        let id = e.id.clone();
        std::thread::spawn(move || {
            let _ = tx.send(install(&e, &plugins_dir));
            waker();
        });
        self.jobs.push(Job::Install(id, rx));
    }

    pub fn poll(&mut self) -> Vec<Outcome> {
        let mut out = vec![];
        let mut keep = vec![];
        for job in self.jobs.drain(..) {
            match &job {
                Job::Index(rx) => match rx.try_recv() {
                    Ok(Ok(v)) => self.entries = v,
                    Ok(Err(e)) => self.error = Some(e),
                    Err(std::sync::mpsc::TryRecvError::Empty) => keep.push(job),
                    Err(_) => {}
                },
                Job::Install(_, rx) => match rx.try_recv() {
                    Ok(Ok(msg)) => out.push(Outcome::Reload(msg)),
                    Ok(Err(e)) => out.push(Outcome::Error(e)),
                    Err(std::sync::mpsc::TryRecvError::Empty) => keep.push(job),
                    Err(_) => {}
                },
            }
        }
        self.jobs = keep;
        out
    }

    /// Entries matching the search box.
    pub fn visible(&self) -> Vec<&Entry> {
        let q = self.search.trim().to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                q.is_empty()
                    || e.name.to_lowercase().contains(&q)
                    || e.id.contains(&q)
                    || e.description.to_lowercase().contains(&q)
                    || e.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_versions() {
        let v = parse_index(
            r#"{"plugins":[
                {"id":"b","name":"Beta","repository":"r"},
                {"id":"a","name":"alpha","repository":"r","tags":["x"]},
                {"id":"../evil","name":"E","repository":"r"},
                {"id":"c","name":"No repo"}]}"#,
        )
        .unwrap();
        assert_eq!(v.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.10", "1.9"));
        assert!(is_newer("v1.0.1", "1.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    /// Install a plugin from a folder of a local repository, update, uninstall.
    #[test]
    fn install_from_git_repository() {
        let root = std::env::temp_dir().join(format!("zenith-market-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        let pdir = repo.join("plugins/hello");
        std::fs::create_dir_all(&pdir).unwrap();
        let manifest = |v: &str| {
            format!("id = \"hello\"\nname = \"Hello\"\nversion = \"{v}\"\nrun = [\"python3\", \"main.py\"]\n")
        };
        std::fs::write(pdir.join("plugin.toml"), manifest("0.1.0")).unwrap();
        std::fs::write(pdir.join("main.py"), "print('hi')\n").unwrap();
        std::fs::write(repo.join("README.md"), "other files\n").unwrap();
        let commit = |msg: &str| {
            git(&repo, &["add", "-A"]).unwrap();
            git(&repo, &["-c", "user.name=t", "-c", "user.email=t@e", "commit", "-qm", msg]).unwrap();
        };
        if git(&repo, &["init", "-q"]).is_err() {
            return; // no git
        }
        commit("one");
        let path = repo.to_string_lossy().replace('\\', "/");
        // file:///C:/… on Windows, file:///tmp/… elsewhere.
        let url = format!("file://{}{path}", if path.starts_with('/') { "" } else { "/" });
        let e = Entry {
            id: "hello".into(),
            name: "Hello".into(),
            version: "0.1.0".into(),
            repository: url,
            path: "plugins/hello".into(),
            ..Default::default()
        };
        let plugins = root.join("config/plugins");
        install(&e, &plugins).unwrap();
        assert!(plugins.join("hello/main.py").exists());
        assert!(!plugins.join("hello/README.md").exists());
        assert!(!plugins.join("hello/.git").exists());
        assert!(plugins.join("hello").join(SOURCE_FILE).exists());
        assert_eq!(installed_version(&plugins, "hello").as_deref(), Some("0.1.0"));

        std::fs::write(pdir.join("plugin.toml"), manifest("0.2.0")).unwrap();
        commit("two");
        install(&e, &plugins).unwrap();
        assert_eq!(installed_version(&plugins, "hello").as_deref(), Some("0.2.0"));
        // Nothing left behind next to the plugins folder.
        assert_eq!(std::fs::read_dir(root.join("config")).unwrap().count(), 1);

        // A wrong id is refused and the installed version stays.
        let bad = Entry { id: "other".into(), ..e.clone() };
        assert!(install(&bad, &plugins).unwrap_err().contains("id"));
        assert!(!plugins.join("other").exists());

        uninstall("hello", &plugins).unwrap();
        assert!(!plugins.join("hello").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
