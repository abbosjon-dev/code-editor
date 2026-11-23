//! Remote folders over SSH.
//!
//! A remote folder is mirrored into a local cache (streamed with `tar` over
//! `ssh`), so the explorer, search, language servers and Git status work
//! unchanged. Saves, deletes and renames are sent to the host right away;
//! terminals and AI agents run on the host, and files they change are pulled
//! back periodically (only files newer than the last sync).
//!
//! Uses the system `ssh` with the user's config and keys (`BatchMode`: no
//! password prompts; use keys or an agent).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteSpec {
    /// `user@host`, `host`, or an alias from ~/.ssh/config.
    pub host: String,
    pub port: Option<u16>,
    /// Folder on the host (absolute, or `~/...`).
    pub path: String,
}

impl RemoteSpec {
    /// Parse `user@host:/path`, `host:~/dir` or `ssh://user@host:2222/path`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("ssh://") {
            let (hostport, path) = rest.split_once('/')?;
            let (host, port) = match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p.parse().ok()?)),
                None => (hostport.to_string(), None),
            };
            let path = format!("/{path}");
            return (!host.is_empty()).then_some(Self { host, port, path });
        }
        let (host, path) = s.split_once(':')?;
        if host.is_empty() || path.is_empty() || host.contains(char::is_whitespace) {
            return None;
        }
        Some(Self { host: host.to_string(), port: None, path: path.to_string() })
    }

    pub fn label(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}:{}", self.host, self.path),
            None => format!("{}:{}", self.host, self.path),
        }
    }

    /// The folder as a shell word (`~/x` stays expandable).
    fn qpath(&self) -> String {
        match self.path.strip_prefix("~/") {
            Some(rest) => format!("\"$HOME\"/{}", sh_quote(rest)),
            None if self.path == "~" => "\"$HOME\"".into(),
            None => sh_quote(&self.path),
        }
    }

    /// A path inside the folder (relative, `/`-separated) as a shell word.
    fn qrel(&self, rel: &str) -> String {
        format!("{}/{}", self.qpath(), sh_quote(rel.trim_start_matches('/')))
    }
}

/// Quote for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@+,".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Local mirror of a remote folder.
pub fn cache_dir(config: &Path, spec: &RemoteSpec) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    spec.host.hash(&mut h);
    spec.port.hash(&mut h);
    spec.path.hash(&mut h);
    let name: String = spec
        .path
        .rsplit('/')
        .find(|p| !p.is_empty() && *p != "~")
        .unwrap_or("home")
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let host: String = spec.host.chars().map(|c| if c.is_alphanumeric() || c == '.' { c } else { '_' }).collect();
    // The last folder keeps the remote name (shown as the workspace name).
    config.join("remote").join(host).join(format!("{:08x}", h.finish() as u32)).join(name)
}

/// `ssh` with the options every call uses; `tty` for interactive sessions.
pub fn ssh_args(spec: &RemoteSpec, tty: bool) -> Vec<String> {
    let mut a: Vec<String> = vec![];
    if tty {
        a.push("-t".into());
    } else {
        a.extend(["-o".into(), "BatchMode=yes".into()]);
    }
    a.extend(["-o".into(), "ConnectTimeout=10".into()]);
    if cfg!(unix) {
        // Reuse one connection for the many small commands.
        a.extend([
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            "ControlPath=~/.ssh/zenith-%C".into(),
            "-o".into(),
            "ControlPersist=10m".into(),
        ]);
    }
    if let Some(p) = spec.port {
        a.extend(["-p".into(), p.to_string()]);
    }
    a.push(spec.host.clone());
    a
}

fn ssh(spec: &RemoteSpec, script: &str) -> Command {
    let mut c = Command::new("ssh");
    c.args(ssh_args(spec, false)).arg(script).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    c
}

fn run(mut c: Command) -> Result<Vec<u8>, String> {
    let out = c.output().map_err(|e| format!("ssh: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if e.is_empty() { "ssh failed".into() } else { e })
    }
}

/// Folders not worth copying (build output, dependencies).
const SKIP: &[&str] = &["target", "node_modules", ".venv", "__pycache__", "dist", "build"];

/// The host's clock (Unix seconds), used to ask for newer files later.
fn remote_now(spec: &RemoteSpec) -> Result<i64, String> {
    let out = run(ssh(spec, "date +%s"))?;
    String::from_utf8_lossy(&out).trim().parse().map_err(|_| "can't read the host's clock".into())
}

/// Unpack a tar.gz stream into `dest`; returns the extracted paths.
fn untar(data: &[u8], dest: &Path) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let mut c = Command::new("tar");
    c.args(["-xzvf", "-", "-C"]).arg(dest).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("tar: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("tar: no stdin")?;
    let data = data.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(format!("tar: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    // GNU tar lists names on stdout, bsdtar on stderr ("x name").
    let names = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    Ok(names
        .lines()
        .map(|l| l.trim().trim_start_matches("x ").trim_start_matches("./"))
        .filter(|l| !l.is_empty() && !l.ends_with('/'))
        .map(|l| dest.join(l))
        .collect())
}

fn excludes() -> String {
    SKIP.iter().map(|d| format!("--exclude=./{d}")).collect::<Vec<_>>().join(" ")
}

/// Copy the whole folder into `dest`; returns the host time it started.
pub fn download(spec: &RemoteSpec, dest: &Path) -> Result<i64, String> {
    let since = remote_now(spec)?;
    let script = format!("cd {} && tar -czf - {} .", spec.qpath(), excludes());
    let mut c = ssh(spec, &script);
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("ssh: {e}"))?;
    let mut data = vec![];
    child.stdout.take().ok_or("no output")?.read_to_end(&mut data).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    untar(&data, dest)?;
    Ok(since)
}

/// Files changed on the host since `since`: copied into `dest`, returned.
pub fn pull_changes(spec: &RemoteSpec, dest: &Path, since: i64) -> Result<(i64, Vec<PathBuf>), String> {
    let now = remote_now(spec)?;
    let prune = SKIP.iter().map(|d| format!("-path ./{d}")).collect::<Vec<_>>().join(" -o ");
    // One second of overlap so nothing written during the last sync is missed.
    let script = format!(
        "cd {} && find . \\( {prune} \\) -prune -o -type f -newermt @{} -print0 | tar --null -T - -czf -",
        spec.qpath(),
        since - 1
    );
    let data = run(ssh(spec, &script))?;
    if data.is_empty() {
        return Ok((now, vec![]));
    }
    Ok((now, untar(&data, dest)?))
}

/// Write `local` to `rel` on the host.
pub fn upload(spec: &RemoteSpec, rel: &str, local: &Path) -> Result<(), String> {
    let target = spec.qrel(rel);
    let dir = rel.rsplit_once('/').map(|(d, _)| spec.qrel(d)).unwrap_or_else(|| spec.qpath());
    let mut c = ssh(spec, &format!("mkdir -p {dir} && cat > {target}"));
    let file = std::fs::File::open(local).map_err(|e| e.to_string())?;
    c.stdin(Stdio::from(file));
    run(c).map(|_| ())
}

pub fn remove(spec: &RemoteSpec, rel: &str) -> Result<(), String> {
    run(ssh(spec, &format!("rm -rf {}", spec.qrel(rel)))).map(|_| ())
}

pub fn rename(spec: &RemoteSpec, from: &str, to: &str) -> Result<(), String> {
    run(ssh(spec, &format!("mv {} {}", spec.qrel(from), spec.qrel(to)))).map(|_| ())
}

pub fn mkdir(spec: &RemoteSpec, rel: &str) -> Result<(), String> {
    run(ssh(spec, &format!("mkdir -p {}", spec.qrel(rel)))).map(|_| ())
}

/// `ssh` arguments for an interactive shell in the folder.
pub fn shell_args(spec: &RemoteSpec) -> Vec<String> {
    let mut a = ssh_args(spec, true);
    a.push(format!("cd {} && exec \"${{SHELL:-sh}}\" -l", spec.qpath()));
    a
}

/// `ssh` arguments to run a program (an agent CLI) in the folder, with the
/// login shell's PATH.
pub fn program_args(spec: &RemoteSpec, program: &str, args: &[String]) -> Vec<String> {
    let cmd: Vec<String> =
        std::iter::once(program.to_string()).chain(args.iter().cloned()).map(|a| sh_quote(&a)).collect();
    let mut a = ssh_args(spec, true);
    a.push(format!("cd {} && exec \"${{SHELL:-sh}}\" -lc {}", spec.qpath(), sh_quote(&cmd.join(" "))));
    a
}

// ------------------------------------------------------------------ session

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

enum Op {
    Upload(String, Vec<u8>),
    Remove(String),
    Rename(String, String),
    Mkdir(String),
}

/// How often files changed on the host are fetched.
pub const PULL_EVERY: Duration = Duration::from_secs(4);

/// Result of a background pull: new `since` and the files that changed.
type PullResult = Result<(i64, Vec<PathBuf>), String>;
/// A connection in progress: what was asked for, its cache and the outcome.
pub type Connecting = (RemoteSpec, PathBuf, Receiver<Result<i64, String>>);

/// An open remote folder: its local mirror and the work going to / coming
/// from the host.
pub struct Session {
    pub spec: RemoteSpec,
    pub cache: PathBuf,
    pub since: i64,
    ops: Sender<Op>,
    errors: Receiver<String>,
    pull: Option<Receiver<PullResult>>,
    last_pull: Instant,
    waker: Arc<dyn Fn() + Send + Sync>,
    pub last_error: Option<String>,
}

/// Copy files from `incoming` into `cache` when their content differs;
/// returns the changed paths (in `cache`).
fn merge_incoming(incoming: &Path, cache: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let mut changed = vec![];
    for f in files {
        let Ok(rel) = f.strip_prefix(incoming) else { continue };
        let dst = cache.join(rel);
        let Ok(new) = std::fs::read(f) else { continue };
        if std::fs::read(&dst).is_ok_and(|old| old == new) {
            continue;
        }
        if let Some(p) = dst.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if std::fs::write(&dst, &new).is_ok() {
            changed.push(dst);
        }
    }
    let _ = std::fs::remove_dir_all(incoming);
    changed
}

impl Session {
    pub fn new(spec: RemoteSpec, cache: PathBuf, since: i64, waker: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (ops, rx) = channel::<Op>();
        let (etx, errors) = channel();
        let worker_spec = spec.clone();
        let w = waker.clone();
        // One worker: operations reach the host in the order they were made.
        std::thread::spawn(move || {
            for op in rx {
                let r = match &op {
                    Op::Upload(rel, data) => {
                        let tmp = std::env::temp_dir().join(format!("zenith-up-{}", std::process::id()));
                        std::fs::write(&tmp, data)
                            .map_err(|e| e.to_string())
                            .and_then(|_| upload(&worker_spec, rel, &tmp))
                            .map_err(|e| format!("saving {rel} on the host failed: {e}"))
                    }
                    Op::Remove(rel) => remove(&worker_spec, rel).map_err(|e| format!("delete {rel}: {e}")),
                    Op::Rename(a, b) => rename(&worker_spec, a, b).map_err(|e| format!("rename {a}: {e}")),
                    Op::Mkdir(rel) => mkdir(&worker_spec, rel).map_err(|e| format!("mkdir {rel}: {e}")),
                };
                if let Err(e) = r {
                    let _ = etx.send(e);
                    w();
                }
            }
        });
        Self { spec, cache, since, ops, errors, pull: None, last_pull: Instant::now(), waker, last_error: None }
    }

    /// Path relative to the remote folder (`/`-separated), if inside it.
    pub fn rel(&self, p: &Path) -> Option<String> {
        let r = p.strip_prefix(&self.cache).ok()?;
        let s = r.to_string_lossy().replace('\\', "/");
        (!s.is_empty()).then_some(s)
    }

    pub fn upload(&self, p: &Path) {
        if let (Some(rel), Ok(data)) = (self.rel(p), std::fs::read(p)) {
            let _ = self.ops.send(Op::Upload(rel, data));
        }
    }

    pub fn remove(&self, p: &Path) {
        if let Some(rel) = self.rel(p) {
            let _ = self.ops.send(Op::Remove(rel));
        }
    }

    pub fn rename(&self, from: &Path, to: &Path) {
        if let (Some(a), Some(b)) = (self.rel(from), self.rel(to)) {
            let _ = self.ops.send(Op::Rename(a, b));
        }
    }

    pub fn mkdir(&self, p: &Path) {
        if let Some(rel) = self.rel(p) {
            let _ = self.ops.send(Op::Mkdir(rel));
        }
    }

    /// Fetch host changes now.
    pub fn pull_now(&mut self) {
        if self.pull.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let (spec, cache, since, w) = (self.spec.clone(), self.cache.clone(), self.since, self.waker.clone());
        std::thread::spawn(move || {
            let incoming = cache.with_extension("incoming");
            let _ = std::fs::remove_dir_all(&incoming);
            let r = pull_changes(&spec, &incoming, since)
                .map(|(now, files)| (now, merge_incoming(&incoming, &cache, &files)));
            let _ = tx.send(r);
            w();
        });
        self.pull = Some(rx);
        self.last_pull = Instant::now();
    }

    /// Collect results; starts a periodic pull. Returns changed files and errors.
    pub fn poll(&mut self) -> (Vec<PathBuf>, Vec<String>) {
        let mut errors: Vec<String> = self.errors.try_iter().collect();
        let mut changed = vec![];
        if let Some(rx) = &self.pull {
            if let Ok(r) = rx.try_recv() {
                self.pull = None;
                match r {
                    Ok((now, files)) => {
                        self.since = now;
                        changed = files;
                        self.last_error = None;
                    }
                    Err(e) => {
                        // Report a lost connection once, not every few seconds.
                        if self.last_error.as_deref() != Some(e.as_str()) {
                            errors.push(format!("{}: {e}", self.spec.host));
                        }
                        self.last_error = Some(e);
                    }
                }
            }
        }
        if self.pull.is_none() && self.last_pull.elapsed() >= PULL_EVERY {
            self.pull_now();
        }
        (changed, errors)
    }
}

/// Connect: mirror `spec` into its cache in the background.
pub fn connect(spec: RemoteSpec, cache: PathBuf, waker: impl Fn() + Send + 'static) -> Receiver<Result<i64, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(&cache);
        let _ = tx.send(download(&spec, &cache));
        waker();
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        let s = RemoteSpec::parse("me@box:/srv/app").unwrap();
        assert_eq!((s.host.as_str(), s.port, s.path.as_str()), ("me@box", None, "/srv/app"));
        let s = RemoteSpec::parse("ssh://me@box:2222/srv/app").unwrap();
        assert_eq!((s.host.as_str(), s.port, s.path.as_str()), ("me@box", Some(2222), "/srv/app"));
        let s = RemoteSpec::parse("devbox:~/code").unwrap();
        assert_eq!(s.qpath(), "\"$HOME\"/code");
        assert_eq!(s.qrel("src/a b.rs"), "\"$HOME\"/code/'src/a b.rs'");
        assert!(RemoteSpec::parse("nohost").is_none());
        assert!(RemoteSpec::parse(":/x").is_none());
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("plain/path.rs"), "plain/path.rs");
        let c1 = cache_dir(Path::new("/c"), &RemoteSpec::parse("a@h:/srv/app").unwrap());
        let c2 = cache_dir(Path::new("/c"), &RemoteSpec::parse("a@h:/srv/other/app").unwrap());
        assert_ne!(c1, c2);
        assert!(c1.ends_with("app"));
    }

    /// Against a real SSH server: set ZENITH_TEST_SSH=host (key auth).
    #[test]
    #[ignore]
    fn roundtrip_over_ssh() {
        let Ok(host) = std::env::var("ZENITH_TEST_SSH") else { return };
        let base = std::env::temp_dir().join(format!("zenith-remote-{}", std::process::id()));
        let remote_dir = base.join("remote");
        std::fs::create_dir_all(remote_dir.join("src")).unwrap();
        std::fs::create_dir_all(remote_dir.join("target")).unwrap();
        std::fs::write(remote_dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(remote_dir.join("target/big"), "skip me").unwrap();
        let spec = RemoteSpec { host, port: None, path: remote_dir.to_string_lossy().into_owned() };
        let cache = base.join("cache");
        let since = download(&spec, &cache).unwrap();
        assert!(cache.join("src/main.rs").exists());
        assert!(!cache.join("target/big").exists());
        // Save goes up.
        std::fs::write(cache.join("src/main.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();
        upload(&spec, "src/main.rs", &cache.join("src/main.rs")).unwrap();
        assert!(std::fs::read_to_string(remote_dir.join("src/main.rs")).unwrap().contains("hi"));
        upload(&spec, "new dir/x.txt", &cache.join("src/main.rs")).unwrap();
        assert!(remote_dir.join("new dir/x.txt").exists());
        // Changes on the host come back.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(remote_dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let (_, changed) = pull_changes(&spec, &cache, since + 1).unwrap();
        assert!(changed.iter().any(|p| p.ends_with("src/lib.rs")), "{changed:?}");
        assert!(cache.join("src/lib.rs").exists());
        rename(&spec, "src/lib.rs", "src/lib2.rs").unwrap();
        assert!(remote_dir.join("src/lib2.rs").exists());
        remove(&spec, "new dir").unwrap();
        assert!(!remote_dir.join("new dir").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
