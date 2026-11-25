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

/// Every file in the folder on the host (relative paths; skipped folders
/// and `.git` left out).
pub fn list_files(spec: &RemoteSpec) -> Result<std::collections::HashSet<String>, String> {
    let prune = SKIP.iter().chain([".git"].iter()).map(|d| format!("-path ./{d}")).collect::<Vec<_>>().join(" -o ");
    let script = format!("cd {} && find . \\( {prune} \\) -prune -o -type f -print0", spec.qpath());
    let out = run(ssh(spec, &script))?;
    Ok(String::from_utf8_lossy(&out)
        .split('\0')
        .map(|p| p.trim_start_matches("./").to_string())
        .filter(|p| !p.is_empty())
        .collect())
}

/// Delete files from the mirror that no longer exist on the host. Files
/// changed here since `keep_after` are kept (they may still be uploading).
fn prune_deleted(
    cache: &Path,
    present: &std::collections::HashSet<String>,
    keep_after: std::time::SystemTime,
) -> Vec<PathBuf> {
    fn walk(dir: &Path, cache: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                if dir == cache && (name == ".git" || SKIP.contains(&name.as_ref())) {
                    continue;
                }
                walk(&p, cache, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut files = vec![];
    walk(cache, cache, &mut files);
    let mut removed = vec![];
    for f in files {
        let Ok(rel) = f.strip_prefix(cache) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let recent = std::fs::metadata(&f).and_then(|m| m.modified()).is_ok_and(|t| t > keep_after);
        if !present.contains(&rel) && !recent && std::fs::remove_file(&f).is_ok() {
            removed.push(f);
        }
    }
    removed
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

/// Run `git <args>` in the folder on the host.
pub fn git(spec: &RemoteSpec, args: &[&str]) -> Result<String, String> {
    let words: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
    let script = format!("cd {} && GIT_TERMINAL_PROMPT=0 git {}", spec.qpath(), words.join(" "));
    run(ssh(spec, &script)).map(|o| String::from_utf8_lossy(&o).into_owned())
}

/// `git <args>` on the host with `input` on stdin.
pub fn git_input(spec: &RemoteSpec, args: &[&str], input: &[u8]) -> Result<String, String> {
    use std::io::Write;
    let words: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
    let mut c = ssh(spec, &format!("cd {} && git {}", spec.qpath(), words.join(" ")));
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("ssh: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Separator between the parts of [`git_state`]'s output.
const PART: &str = "\n--zenith-part--\n";

/// Everything the Source Control view needs, in one round trip:
/// (`status --porcelain -z`, branch, stash count, in-progress operation
/// markers found in the git dir, path prefix of the folder in the repo).
pub fn git_state(spec: &RemoteSpec) -> Result<(String, String, usize, Vec<String>, String), String> {
    let script = format!(
        "cd {} && git rev-parse --git-dir >/dev/null || exit 1; \
         git --no-optional-locks status --porcelain=v1 -z --untracked-files=all; printf '{PART}'; \
         git rev-parse --abbrev-ref HEAD; printf '{PART}'; \
         git --no-optional-locks stash list | wc -l; printf '{PART}'; \
         d=$(git rev-parse --git-dir); for f in rebase-merge rebase-apply MERGE_HEAD CHERRY_PICK_HEAD REVERT_HEAD; \
         do [ -e \"$d/$f\" ] && echo $f; done; printf '{PART}'; git rev-parse --show-prefix",
        spec.qpath()
    );
    let out = String::from_utf8_lossy(&run(ssh(spec, &script))?).into_owned();
    let parts: Vec<&str> = out.split("\n--zenith-part--\n").collect();
    if parts.len() < 5 {
        return Err("unexpected output from git on the host".into());
    }
    Ok((
        parts[0].to_string(),
        parts[1].trim().to_string(),
        parts[2].trim().parse().unwrap_or(0),
        parts[3].lines().map(str::to_string).filter(|l| !l.is_empty()).collect(),
        parts[4].trim().to_string(),
    ))
}

/// What runs on the host: its absolute folder path and which programs are
/// installed there (found through the login shell's PATH).
#[derive(Clone, Debug)]
pub struct HostInfo {
    pub spec: RemoteSpec,
    /// The local mirror.
    pub cache: PathBuf,
    /// The folder's absolute path on the host.
    pub root: String,
    /// Installed programs: name → path on the host.
    pub found: std::collections::HashMap<String, String>,
}

impl HostInfo {
    /// `local` (inside the mirror) as a path on the host.
    pub fn host_path(&self, local: &Path) -> Option<String> {
        let rel = local.strip_prefix(&self.cache).ok()?.to_string_lossy().replace('\\', "/");
        Some(if rel.is_empty() { self.root.clone() } else { format!("{}/{rel}", self.root.trim_end_matches('/')) })
    }

    /// Path mapping between the mirror folder `local` and the host.
    pub fn path_map(&self, local: &Path) -> Option<zenith_core::pathmap::PathMap> {
        let remote = self.host_path(local)?;
        let local_uri = zenith_lsp::text::path_to_uri(local);
        let remote_uri = format!("file://{}", uri_encode(&remote));
        Some(zenith_core::pathmap::PathMap::new(&local.to_string_lossy(), &remote, Some((&local_uri, &remote_uri))))
    }

    /// Command line that runs `program args` on the host in `local`'s folder
    /// (no terminal, so binary protocols on stdin/stdout stay intact).
    pub fn command(&self, local: &Path, program: &str, args: &[String]) -> Vec<String> {
        let dir = self.host_path(local).unwrap_or_else(|| self.root.clone());
        let exe = self.found.get(program).cloned().unwrap_or_else(|| program.to_string());
        let cmd: Vec<String> = std::iter::once(exe).chain(args.iter().cloned()).map(|a| sh_quote(&a)).collect();
        let mut a = vec!["ssh".to_string()];
        a.extend(ssh_args(&self.spec, false));
        a.push(format!("cd {} && exec \"${{SHELL:-sh}}\" -lc {}", sh_quote(&dir), sh_quote(&cmd.join(" "))));
        a
    }

    /// Run a shell command line on the host in `local`'s folder.
    pub fn shell(&self, local: &Path, line: &str) -> Vec<String> {
        let dir = self.host_path(local).unwrap_or_else(|| self.root.clone());
        let mut a = vec!["ssh".to_string()];
        a.extend(ssh_args(&self.spec, false));
        a.push(format!("cd {} && exec \"${{SHELL:-sh}}\" -lc {}", sh_quote(&dir), sh_quote(line)));
        a
    }
}

/// Percent-encode a POSIX path for a `file://` URI.
fn uri_encode(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Find the folder's absolute path and which of `programs` the host has.
pub fn probe(spec: &RemoteSpec, cache: &Path, programs: &[String]) -> Result<HostInfo, String> {
    let list: Vec<String> = programs.iter().map(|p| sh_quote(p)).collect();
    let inner = format!(
        "echo \"@root:$(pwd)\"; for c in {}; do p=$(command -v \"$c\" 2>/dev/null); \
         if [ -z \"$p\" ]; then for d in \"$HOME/.cargo/bin\" \"$HOME/.local/bin\" \"$HOME/go/bin\" /usr/local/bin; do \
         [ -x \"$d/$c\" ] && p=\"$d/$c\" && break; done; fi; [ -n \"$p\" ] && echo \"@has:$c=$p\"; done; true",
        list.join(" ")
    );
    let script = format!("cd {} && \"${{SHELL:-sh}}\" -lc {}", spec.qpath(), sh_quote(&inner));
    let out = String::from_utf8_lossy(&run(ssh(spec, &script))?).into_owned();
    let root = out.lines().find_map(|l| l.strip_prefix("@root:")).ok_or("couldn't read the folder's path")?;
    Ok(HostInfo {
        spec: spec.clone(),
        cache: cache.to_path_buf(),
        root: root.trim().to_string(),
        found: out
            .lines()
            .filter_map(|l| l.strip_prefix("@has:")?.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect(),
    })
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
/// How often deletions on the host are looked for (a full file listing).
const FULL_EVERY: Duration = Duration::from_secs(20);

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
    /// Last time deletions on the host were looked for.
    last_full: Instant,
    probe: Option<Receiver<Result<HostInfo, String>>>,
    /// The host's folder path and programs, once known.
    pub host: Option<HostInfo>,
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
        Self {
            spec,
            cache,
            since,
            ops,
            errors,
            pull: None,
            last_pull: Instant::now(),
            last_full: Instant::now(),
            probe: None,
            host: None,
            waker,
            last_error: None,
        }
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

    /// Look up the host's folder path and which of `programs` it has, in the
    /// background ([`poll`](Self::poll) reports when it's known).
    pub fn start_probe(&mut self, programs: Vec<String>) {
        let (tx, rx) = channel();
        let (spec, cache, w) = (self.spec.clone(), self.cache.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let _ = tx.send(probe(&spec, &cache, &programs));
            w();
        });
        self.probe = Some(rx);
    }

    /// The probe's result, once (the host info is then in `host`).
    pub fn take_probed(&mut self) -> Option<Result<(), String>> {
        let rx = self.probe.as_ref()?;
        let r = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(_) => Err("probe failed".into()),
        };
        self.probe = None;
        Some(r.map(|h| self.host = Some(h)))
    }

    /// Fetch host changes now.
    pub fn pull_now(&mut self) {
        self.pull_with(false);
    }

    /// Fetch host changes, including files deleted there (e.g. by git).
    pub fn pull_all(&mut self) {
        self.pull_with(true);
    }

    fn pull_with(&mut self, full: bool) {
        if self.pull.is_some() {
            if full {
                // Look for deletions with the next pull.
                self.last_full = Instant::now() - FULL_EVERY;
            }
            return;
        }
        let full = full || self.last_full.elapsed() >= FULL_EVERY;
        if full {
            self.last_full = Instant::now();
        }
        let (tx, rx) = channel();
        let (spec, cache, since, w) = (self.spec.clone(), self.cache.clone(), self.since, self.waker.clone());
        std::thread::spawn(move || {
            let started = std::time::SystemTime::now();
            let incoming = cache.with_extension("incoming");
            let _ = std::fs::remove_dir_all(&incoming);
            let r = pull_changes(&spec, &incoming, since).and_then(|(now, files)| {
                let mut changed = merge_incoming(&incoming, &cache, &files);
                if full {
                    let present = list_files(&spec)?;
                    changed.extend(prune_deleted(&cache, &present, started - Duration::from_secs(10)));
                }
                Ok((now, changed))
            });
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
        // Deleted on the host: gone from the mirror, unless just edited here.
        std::fs::write(cache.join("fresh.txt"), "new").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options().write(true).open(cache.join("src/lib.rs")).unwrap().set_modified(old).unwrap();
        let present = list_files(&spec).unwrap();
        assert!(present.contains("src/lib2.rs") && !present.contains("src/lib.rs"));
        let removed = prune_deleted(&cache, &present, std::time::SystemTime::now() - Duration::from_secs(5));
        assert!(removed.iter().any(|p| p.ends_with("src/lib.rs")), "{removed:?}");
        assert!(cache.join("fresh.txt").exists());
        // The host's path and programs.
        let info = probe(&spec, &cache, &["sh".into(), "no-such-program-zenith".into()]).unwrap();
        assert!(info.found.contains_key("sh") && !info.found.contains_key("no-such-program-zenith"));
        assert_eq!(std::path::Path::new(&info.root).canonicalize().unwrap(), remote_dir.canonicalize().unwrap());
        assert_eq!(info.host_path(&cache.join("src/a.rs")).unwrap(), format!("{}/src/a.rs", info.root));
        let out = std::process::Command::new("ssh")
            .args(&info.command(&cache.join("src"), "pwd", &[])[1..])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).trim().ends_with("/src"));
        // Git runs on the host.
        if git(&spec, &["init", "-q"]).is_ok() {
            let (status, _, stashes, ops, prefix) = git_state(&spec).unwrap();
            assert!(status.contains("src/main.rs"), "{status:?}");
            assert_eq!((stashes, ops.len(), prefix.as_str()), (0, 0, ""));
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
