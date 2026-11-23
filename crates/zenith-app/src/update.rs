//! Updates: asks GitHub for the latest release, tells the user when it is
//! newer, and on request downloads the right package for how Zenith was
//! installed, verifies it and installs it (or starts the installer).
//!
//! Uses the system `curl` (shipped with Windows 10+, macOS and Linux), so no
//! TLS stack is compiled into the editor.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};

pub const LATEST_API: &str = "https://api.github.com/repos/abbosjon-dev/code-editor/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/abbosjon-dev/code-editor/releases";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub assets: Vec<Asset>,
}

/// A downloadable file of a release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Hex SHA-256 when GitHub provides it (`digest: "sha256:…"`).
    pub sha256: Option<String>,
}

/// "v1.2.3", "1.2", "1.2.3-beta" → (1, 2, 3).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>());
    let major = it.next()?.ok()?;
    let minor = it.next().unwrap_or(Ok(0)).ok()?;
    let patch = it.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

/// Parse the `releases/latest` answer.
pub fn parse_release(json: &str) -> Option<Release> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let assets = v["assets"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(Asset {
                        name: x["name"].as_str()?.to_string(),
                        url: x["browser_download_url"].as_str()?.to_string(),
                        size: x["size"].as_u64().unwrap_or(0),
                        sha256: x["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Release {
        version: v["tag_name"].as_str()?.trim_start_matches('v').to_string(),
        url: v["html_url"].as_str().unwrap_or(RELEASES_PAGE).to_string(),
        assets,
    })
}

/// The latest published release (None when there is none yet).
pub fn fetch_latest() -> Result<Option<Release>, String> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sSL",
        "--max-time",
        "10",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: zenith-editor",
        "-w",
        "\n%{http_code}",
    ]);
    // ZENITH_UPDATE_URL: a mirror or a local release description (tests).
    let url = std::env::var("ZENITH_UPDATE_URL").unwrap_or_else(|_| LATEST_API.to_string());
    let local = url.starts_with("file://");
    cmd.arg(&url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    match code.trim() {
        "200" => Ok(parse_release(body)),
        "000" if local => Ok(parse_release(body)),
        "404" => Ok(None), // no releases published
        c => Err(format!("GitHub answered HTTP {c}")),
    }
}

/// Result of an update check.
pub type CheckResult = Result<Option<Release>, String>;

/// Run [`fetch_latest`] in the background.
pub fn check_async(waker: impl Fn() + Send + 'static) -> Receiver<Result<Option<Release>, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(fetch_latest());
        waker();
    });
    rx
}

// ------------------------------------------------------------------ installing

/// How this copy of Zenith was installed (decides what to download).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// Linux AppImage at this path (`$APPIMAGE`).
    AppImage(PathBuf),
    /// macOS bundle (`…/Zenith.app`).
    MacApp(PathBuf),
    /// Windows MSI install (Program Files).
    Msi,
    /// Debian package (`/usr/bin/zenith`): needs `sudo apt install`.
    Deb,
    /// A plain executable (tar.gz / zip / built from source) at this path.
    Binary(PathBuf),
}

pub fn detect_install() -> Install {
    let exe = std::env::current_exe().unwrap_or_default();
    if let Some(app) = std::env::var_os("APPIMAGE") {
        return Install::AppImage(PathBuf::from(app));
    }
    let s = exe.to_string_lossy().to_string();
    if cfg!(target_os = "macos") {
        if let Some(i) = s.find(".app/Contents/MacOS/") {
            return Install::MacApp(PathBuf::from(&s[..i + 4]));
        }
    }
    if cfg!(windows) && s.to_lowercase().contains("\\program files") {
        return Install::Msi;
    }
    if cfg!(target_os = "linux") && exe.starts_with("/usr/bin") {
        return Install::Deb;
    }
    Install::Binary(exe)
}

fn arch_aliases() -> &'static [&'static str] {
    match std::env::consts::ARCH {
        "x86_64" => &["x86_64", "amd64"],
        "aarch64" => &["aarch64", "arm64"],
        _ => &[],
    }
}

/// The release file for this install type, OS and CPU.
pub fn pick_asset<'a>(assets: &'a [Asset], install: &Install, os: &str, arches: &[&str]) -> Option<&'a Asset> {
    let has_arch = |n: &str| arches.iter().any(|a| n.contains(a));
    let find = |pred: &dyn Fn(&str) -> bool| assets.iter().find(|a| pred(&a.name.to_lowercase()));
    match (install, os) {
        (Install::AppImage(_), _) => find(&|n| n.ends_with(".appimage") && has_arch(n)),
        (Install::Deb, _) => find(&|n| n.ends_with(".deb") && has_arch(n)),
        (Install::MacApp(_), _) => find(&|n| n.ends_with(".dmg") && has_arch(n)),
        (Install::Msi, _) => find(&|n| n.ends_with(".msi") && has_arch(n)),
        (Install::Binary(_), "linux") => find(&|n| n.ends_with(".tar.gz") && n.contains("linux") && has_arch(n)),
        (Install::Binary(_), "windows") => find(&|n| n.ends_with(".zip") && n.contains("windows") && has_arch(n)),
        (Install::Binary(_), "macos") => find(&|n| n.ends_with(".dmg") && has_arch(n)),
        _ => None,
    }
}

/// What happened after installing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The new version is in place: restart this executable.
    Restart(PathBuf),
    /// An installer was started (Zenith should quit).
    InstallerStarted,
    /// The user has to finish it (message says how).
    Manual(String),
}

fn command(program: &str) -> Command {
    #[allow(unused_mut)] // mutated on Windows
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    c
}

fn run(mut c: Command) -> Result<(), String> {
    let out = c.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Download `asset` into `dir` and check its size and checksum.
pub fn download(asset: &Asset, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(&asset.name);
    let mut c = command("curl");
    c.args(["-fsSL", "--max-time", "600", "-H", "User-Agent: zenith-editor", "-o"]).arg(&path).arg(&asset.url);
    run(c).map_err(|e| format!("download failed: {e}"))?;
    verify(&path, asset)?;
    Ok(path)
}

pub fn verify(path: &Path, asset: &Asset) -> Result<(), String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if asset.size > 0 && size != asset.size {
        return Err(format!("downloaded {size} bytes, expected {}", asset.size));
    }
    if let Some(want) = &asset.sha256 {
        let got = sha256_file(path).map_err(|e| e.to_string())?;
        if &got != want {
            return Err("checksum mismatch: the download is corrupt or was tampered with".into());
        }
    }
    Ok(())
}

/// Swap `new` in for `target` (the running file is moved aside first, which
/// also works for a running executable on Windows).
pub fn replace_file(new: &Path, target: &Path) -> Result<(), String> {
    let old = target.with_extension("old");
    let _ = std::fs::remove_file(&old);
    if target.exists() {
        std::fs::rename(target, &old).map_err(|e| format!("can't replace {}: {e}", target.display()))?;
    }
    if let Err(e) = std::fs::rename(new, target).or_else(|_| std::fs::copy(new, target).map(|_| ())) {
        let _ = std::fs::rename(&old, target); // put the old one back
        return Err(format!("can't write {}: {e}", target.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Find the editor binary inside an extracted archive.
fn find_binary(dir: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) { "zenith.exe" } else { "zenith" };
    ignore::WalkBuilder::new(dir)
        .hidden(false)
        .build()
        .flatten()
        .find(|e| e.file_name() == name && e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
}

/// Install `file` (already verified) for this install type.
pub fn install_file(file: &Path, install: &Install, work: &Path) -> Result<Outcome, String> {
    match install {
        Install::AppImage(target) => {
            replace_file(file, target)?;
            Ok(Outcome::Restart(target.clone()))
        }
        Install::Binary(exe) => {
            let out = work.join("extracted");
            let _ = std::fs::remove_dir_all(&out);
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            // `tar` reads .tar.gz everywhere and .zip on Windows 10+ (bsdtar).
            let mut c = command("tar");
            c.arg("-xf").arg(file).arg("-C").arg(&out);
            run(c).map_err(|e| format!("can't unpack: {e}"))?;
            let bin = find_binary(&out).ok_or("the archive has no zenith binary")?;
            replace_file(&bin, exe)?;
            Ok(Outcome::Restart(exe.clone()))
        }
        Install::MacApp(app) => {
            let mount = work.join("mnt");
            let _ = std::fs::create_dir_all(&mount);
            let mut c = command("hdiutil");
            c.args(["attach", "-nobrowse", "-quiet", "-mountpoint"]).arg(&mount).arg(file);
            run(c).map_err(|e| format!("can't open the disk image: {e}"))?;
            let src = mount.join("Zenith.app");
            let staged = app.with_extension("new");
            let _ = std::fs::remove_dir_all(&staged);
            let mut c = command("ditto");
            c.arg(&src).arg(&staged);
            let copied = run(c);
            let mut d = command("hdiutil");
            d.args(["detach", "-quiet"]).arg(&mount);
            let _ = run(d);
            copied.map_err(|e| format!("can't copy the app: {e}"))?;
            let old = app.with_extension("old");
            let _ = std::fs::remove_dir_all(&old);
            std::fs::rename(app, &old).map_err(|e| format!("can't replace {}: {e}", app.display()))?;
            if let Err(e) = std::fs::rename(&staged, app) {
                let _ = std::fs::rename(&old, app);
                return Err(e.to_string());
            }
            let _ = std::fs::remove_dir_all(&old);
            Ok(Outcome::Restart(app.join("Contents/MacOS/zenith")))
        }
        Install::Msi => {
            command("msiexec").arg("/i").arg(file).spawn().map_err(|e| format!("can't start the installer: {e}"))?;
            Ok(Outcome::InstallerStarted)
        }
        Install::Deb => Ok(Outcome::Manual(format!(
            "Downloaded {}. Install it with:  sudo apt install {}",
            file.display(),
            file.display()
        ))),
    }
}

/// Download and install `release` (runs in the background).
pub fn install_async(release: Release, waker: impl Fn() + Send + 'static) -> Receiver<Result<Outcome, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let r = (|| {
            let install = detect_install();
            let asset = pick_asset(&release.assets, &install, std::env::consts::OS, arch_aliases())
                .ok_or("this release has no package for your system: download it from the release page")?;
            let work = std::env::temp_dir().join(format!("zenith-update-{}", release.version));
            let file = download(asset, &work)?;
            install_file(&file, &install, &work)
        })();
        let _ = tx.send(r);
        waker();
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.2"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.0.0-beta.1"), Some((1, 0, 0)));
        assert_eq!(parse_version("nightly"), None);
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.12"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn release_json() {
        let r = parse_release(
            r#"{"tag_name": "v0.3.0", "html_url": "https://x/r/v0.3.0", "draft": false, "prerelease": false}"#,
        );
        assert_eq!(r, Some(Release { version: "0.3.0".into(), url: "https://x/r/v0.3.0".into(), assets: vec![] }));
        assert_eq!(parse_release(r#"{"tag_name": "v1", "prerelease": true}"#), None);
        assert_eq!(parse_release("not json"), None);
    }

    fn assets() -> Vec<Asset> {
        [
            "zenith-0.3.0-linux-x86_64.tar.gz",
            "zenith_0.3.0_amd64.deb",
            "Zenith-0.3.0-x86_64.AppImage",
            "Zenith-0.3.0-macos-aarch64.dmg",
            "Zenith-0.3.0-macos-x86_64.dmg",
            "Zenith-0.3.0-x86_64.msi",
            "zenith-0.3.0-windows-x86_64.zip",
        ]
        .iter()
        .map(|n| Asset { name: n.to_string(), url: format!("https://x/{n}"), size: 1, sha256: None })
        .collect()
    }

    #[test]
    fn picks_the_right_package() {
        let a = assets();
        let x86 = &["x86_64", "amd64"][..];
        let arm = &["aarch64", "arm64"][..];
        let name = |i: &Install, os: &str, arch: &[&str]| pick_asset(&a, i, os, arch).map(|x| x.name.clone());
        let bin = Install::Binary(PathBuf::from("/x/zenith"));
        assert_eq!(name(&bin, "linux", x86).as_deref(), Some("zenith-0.3.0-linux-x86_64.tar.gz"));
        assert_eq!(name(&Install::Deb, "linux", x86).as_deref(), Some("zenith_0.3.0_amd64.deb"));
        assert_eq!(
            name(&Install::AppImage("/a".into()), "linux", x86).as_deref(),
            Some("Zenith-0.3.0-x86_64.AppImage")
        );
        assert_eq!(
            name(&Install::MacApp("/A.app".into()), "macos", arm).as_deref(),
            Some("Zenith-0.3.0-macos-aarch64.dmg")
        );
        assert_eq!(name(&Install::Msi, "windows", x86).as_deref(), Some("Zenith-0.3.0-x86_64.msi"));
        assert_eq!(name(&bin, "windows", x86).as_deref(), Some("zenith-0.3.0-windows-x86_64.zip"));
        assert_eq!(name(&bin, "linux", arm), None);
    }

    #[test]
    fn parses_assets_with_digest() {
        let r = parse_release(
            r#"{"tag_name":"v1.0.0","html_url":"u","assets":[{"name":"a.tar.gz","browser_download_url":"https://d/a","size":3,"digest":"sha256:ABCD"}]}"#,
        )
        .unwrap();
        assert_eq!(
            r.assets[0],
            Asset { name: "a.tar.gz".into(), url: "https://d/a".into(), size: 3, sha256: Some("abcd".into()) }
        );
    }

    /// A tar.gz "release" replaces a fake installed binary after verification.
    #[cfg(unix)]
    #[test]
    fn verifies_and_installs_a_binary_archive() {
        let dir = std::env::temp_dir().join(format!("zenith-upd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("pkg/zenith-9.9.9");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("zenith"), "NEW BINARY").unwrap();
        let tgz = dir.join("zenith-9.9.9-linux-x86_64.tar.gz");
        let ok = Command::new("tar").arg("-czf").arg(&tgz).arg("-C").arg(dir.join("pkg")).arg("zenith-9.9.9").status();
        if !ok.is_ok_and(|s| s.success()) {
            return;
        }
        let installed = dir.join("bin/zenith");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "OLD BINARY").unwrap();

        let size = std::fs::metadata(&tgz).unwrap().len();
        let good = Asset { name: "x".into(), url: String::new(), size, sha256: Some(sha256_file(&tgz).unwrap()) };
        verify(&tgz, &good).unwrap();
        let bad = Asset { sha256: Some("00".repeat(32)), ..good.clone() };
        assert!(verify(&tgz, &bad).unwrap_err().contains("checksum"));
        assert!(verify(&tgz, &Asset { size: 1, ..good }).is_err());

        let out = install_file(&tgz, &Install::Binary(installed.clone()), &dir.join("work")).unwrap();
        assert_eq!(out, Outcome::Restart(installed.clone()));
        assert_eq!(std::fs::read_to_string(&installed).unwrap(), "NEW BINARY");
        assert_eq!(std::fs::read_to_string(installed.with_extension("old")).unwrap(), "OLD BINARY");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
