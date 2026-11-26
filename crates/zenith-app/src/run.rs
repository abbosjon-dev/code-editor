//! Run: start the project (a dev server, an app, a Flutter / Expo build…)
//! in its own terminal tab. Configurations come from `.zenith/run.toml` and
//! from what the folder looks like (Cargo, npm / pnpm / yarn / bun, Flutter,
//! Expo, Python, Go, Docker Compose). The output is watched for local URLs
//! (a dev server's "Local: http://localhost:5173") to preview them.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use zenith_terminal::CommandSpec;

use crate::remote::RemoteSpec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    Cargo,
    Node,
    Flutter,
    Expo,
    Python,
    Go,
    Compose,
    #[default]
    Custom,
}

impl Kind {
    fn parse(s: &str) -> Kind {
        match s.to_lowercase().as_str() {
            "cargo" | "rust" => Kind::Cargo,
            "node" | "npm" | "web" => Kind::Node,
            "flutter" | "dart" => Kind::Flutter,
            "expo" | "react-native" => Kind::Expo,
            "python" => Kind::Python,
            "go" => Kind::Go,
            "compose" | "docker" => Kind::Compose,
            _ => Kind::Custom,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Kind::Cargo => "⚙",
            Kind::Node => "●",
            Kind::Flutter => "◆",
            Kind::Expo => "▲",
            Kind::Python => "▣",
            Kind::Go => "◇",
            Kind::Compose => "■",
            Kind::Custom => "▶",
        }
    }

    /// Keys the running process understands (label, bytes, tooltip).
    pub fn keys(self) -> &'static [(&'static str, &'static str, &'static str)] {
        match self {
            Kind::Flutter => &[
                ("⟳ Hot reload", "r", "Reload code changes, keep the app state (r)"),
                ("↻ Hot restart", "R", "Restart the app (R)"),
            ],
            Kind::Expo => &[
                ("⟳ Reload", "r", "Reload the app on connected devices (r)"),
                ("Android", "a", "Open on an Android device / emulator (a)"),
                ("iOS", "i", "Open on the iOS simulator (i)"),
                ("Web", "w", "Show the web version (in the preview, or the browser)"),
            ],
            _ => &[],
        }
    }
}

/// One way to run the project.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunConfig {
    pub name: String,
    /// A shell command line.
    pub command: String,
    /// Folder relative to the workspace root (empty = the root).
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub kind: Kind,
    /// Page to preview (otherwise the first local URL printed).
    pub url: Option<String>,
    /// From run.toml (not detected).
    pub from_file: bool,
    /// A build (its card lists what it produced).
    pub build: bool,
}

#[derive(Deserialize)]
struct FileConfig {
    #[serde(default)]
    run: Vec<FileEntry>,
    /// Also list the detected configurations (default true).
    #[serde(default = "yes")]
    detect: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct FileEntry {
    name: String,
    command: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    kind: String,
    url: Option<String>,
}

pub fn config_path(root: &Path) -> PathBuf {
    root.join(".zenith").join("run.toml")
}

pub const TEMPLATE: &str = r#"# Run configurations for Zenith (Run and Debug panel, Ctrl+F5).
# Detected ones (Cargo, npm, Flutter, Expo, Python, Go, Compose) are listed
# too unless `detect = false`.

# [[run]]
# name = "Web: dev server"
# command = "npm run dev"
# cwd = "frontend"              # relative to the folder
# kind = "node"                 # cargo | node | flutter | expo | python | go | compose
# url = "http://localhost:5173" # page to preview (default: first URL printed)
# [run.env]
# PORT = "5173"
"#;

/// Configurations for the folder: run.toml's first, then detected ones.
pub fn load(root: &Path) -> (Vec<RunConfig>, Option<String>) {
    let mut out = vec![];
    let mut err = None;
    let mut detect_too = true;
    if let Ok(text) = std::fs::read_to_string(config_path(root)) {
        match toml::from_str::<FileConfig>(&text) {
            Ok(f) => {
                detect_too = f.detect;
                out.extend(f.run.into_iter().map(|e| RunConfig {
                    kind: if e.kind.is_empty() { guess_kind(&e.command) } else { Kind::parse(&e.kind) },
                    name: e.name,
                    command: e.command,
                    cwd: e.cwd,
                    env: e.env.into_iter().collect(),
                    url: e.url,
                    from_file: true,
                    build: false,
                }));
            }
            Err(e) => err = Some(format!(".zenith/run.toml: {e}")),
        }
    }
    if detect_too {
        for d in detect(root) {
            if !out.iter().any(|c| c.command == d.command && c.cwd == d.cwd) {
                out.push(d);
            }
        }
    }
    (out, err)
}

fn guess_kind(command: &str) -> Kind {
    let c = command.trim_start();
    let first = c.split_whitespace().next().unwrap_or("");
    if first == "flutter" {
        Kind::Flutter
    } else if c.contains("expo ") || c.ends_with("expo") {
        Kind::Expo
    } else if first == "cargo" {
        Kind::Cargo
    } else if ["npm", "pnpm", "yarn", "bun", "npx", "node", "deno"].contains(&first) {
        Kind::Node
    } else if first.starts_with("python") || first == "uv" || first == "poetry" {
        Kind::Python
    } else if first == "go" {
        Kind::Go
    } else if c.starts_with("docker compose") || first == "docker-compose" {
        Kind::Compose
    } else {
        Kind::Custom
    }
}

/// What the folder (and its direct subfolders, for monorepos) can run.
pub fn detect(root: &Path) -> Vec<RunConfig> {
    let mut out = detect_in(root, "");
    // One level down: `frontend/`, `mobile/`, `server/`…
    if let Ok(rd) = std::fs::read_dir(root) {
        let mut subs: Vec<_> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| {
                !n.starts_with('.') && !["node_modules", "target", "build", "dist", "vendor"].contains(&n.as_str())
            })
            .collect();
        subs.sort();
        for s in subs.into_iter().take(40) {
            out.extend(detect_in(&root.join(&s), &s));
        }
    }
    out
}

fn detect_in(dir: &Path, rel: &str) -> Vec<RunConfig> {
    let mut out = vec![];
    let at = |name: &str| if rel.is_empty() { name.to_string() } else { format!("{rel}: {name}") };
    let mk = |name: String, command: &str, kind: Kind| RunConfig {
        name,
        command: command.to_string(),
        cwd: rel.to_string(),
        kind,
        ..Default::default()
    };
    // Flutter.
    if let Ok(p) = std::fs::read_to_string(dir.join("pubspec.yaml")) {
        if p.contains("flutter:") {
            out.push(mk(at("Flutter: run"), "flutter run", Kind::Flutter));
        } else if dir.join("bin").is_dir() {
            out.push(mk(at("Dart: run"), "dart run", Kind::Custom));
        }
    }
    // Node: Expo, or package.json scripts.
    if let Ok(text) = std::fs::read_to_string(dir.join("package.json")) {
        let pkg: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let has_dep = |d: &str| ["dependencies", "devDependencies"].iter().any(|k| pkg[k].get(d).is_some());
        let pm = if dir.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if dir.join("yarn.lock").exists() {
            "yarn"
        } else if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            "bun"
        } else {
            "npm"
        };
        if has_dep("expo") {
            out.push(mk(at("Expo: start"), "npx expo start", Kind::Expo));
        }
        if let Some(scripts) = pkg["scripts"].as_object() {
            for s in ["dev", "start", "serve", "preview", "web", "android", "ios"] {
                if !scripts.contains_key(s) || (has_dep("expo") && s == "start") {
                    continue;
                }
                let cmd = match (pm, s) {
                    ("npm", "start") => "npm start".to_string(),
                    ("npm", _) => format!("npm run {s}"),
                    (pm, _) => format!("{pm} {s}"),
                };
                let kind = if has_dep("expo") || has_dep("react-native") { Kind::Expo } else { Kind::Node };
                out.push(mk(at(&format!("{pm}: {s}")), &cmd, kind));
            }
        }
    }
    // Rust.
    if let Ok(t) = std::fs::read_to_string(dir.join("Cargo.toml")) {
        if dir.join("src/main.rs").exists() || t.contains("[[bin]]") {
            out.push(mk(at("Cargo: run"), "cargo run", Kind::Cargo));
        }
    }
    // Go.
    if dir.join("go.mod").exists() && (dir.join("main.go").exists() || dir.join("cmd").is_dir()) {
        out.push(mk(at("Go: run"), "go run .", Kind::Go));
    }
    // Python.
    let py = if cfg!(windows) { "python" } else { "python3" };
    if dir.join("manage.py").exists() {
        out.push(mk(at("Django: runserver"), &format!("{py} manage.py runserver"), Kind::Python));
    } else {
        for f in ["main.py", "app.py"] {
            if dir.join(f).exists() {
                out.push(mk(at(&format!("Python: {f}")), &format!("{py} {f}"), Kind::Python));
                break;
            }
        }
    }
    // Docker Compose.
    if ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"].iter().any(|f| dir.join(f).exists())
    {
        out.push(mk(at("Docker Compose: up"), "docker compose up", Kind::Compose));
    }
    out
}

/// Build commands for a configuration's project (label, command line).
pub fn build_tasks(cfg: &RunConfig, root: &Path) -> Vec<(String, String)> {
    let dir = if cfg.cwd.is_empty() { root.to_path_buf() } else { root.join(&cfg.cwd) };
    let t = |l: &str, c: &str| (l.to_string(), c.to_string());
    let mut out = vec![];
    match cfg.kind {
        Kind::Flutter => {
            out.push(t("Android APK (release)", "flutter build apk --release"));
            out.push(t("Android App Bundle (Play Store)", "flutter build appbundle --release"));
            if cfg!(target_os = "macos") {
                out.push(t("iOS IPA (App Store)", "flutter build ipa --release"));
                out.push(t("macOS app", "flutter build macos --release"));
            }
            out.push(t("Web", "flutter build web --release"));
            if cfg!(target_os = "linux") {
                out.push(t("Linux app", "flutter build linux --release"));
            }
            if cfg!(windows) {
                out.push(t("Windows app", "flutter build windows --release"));
            }
        }
        Kind::Expo => {
            out.push(t("EAS cloud build: Android", "npx --yes eas-cli build --platform android"));
            out.push(t("EAS cloud build: iOS", "npx --yes eas-cli build --platform ios"));
            out.push(t("Build and run on Android (local)", "npx expo run:android"));
            if cfg!(target_os = "macos") {
                out.push(t("Build and run on iOS (local)", "npx expo run:ios"));
            }
            out.push(t("Export web", "npx expo export --platform web"));
        }
        Kind::Cargo => out.push(t("Release build", "cargo build --release")),
        Kind::Go => out.push(t("Build", "go build ./...")),
        Kind::Compose => out.push(t("Build images", "docker compose build")),
        Kind::Node => {
            let pkg: serde_json::Value = std::fs::read_to_string(dir.join("package.json"))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            if pkg["scripts"].get("build").is_some() {
                let pm = cfg
                    .command
                    .split_whitespace()
                    .next()
                    .filter(|p| ["pnpm", "yarn", "bun"].contains(p))
                    .unwrap_or("npm");
                out.push(t("Production build", &format!("{pm} run build")));
            }
        }
        Kind::Python | Kind::Custom => {}
    }
    out
}

/// Files a build of this kind produces (relative to its folder), newest
/// builds first: APKs, bundles, IPAs, web output, release binaries.
pub fn artifacts(kind: Kind, dir: &Path) -> Vec<PathBuf> {
    let rel: &[&str] = match kind {
        Kind::Flutter => &[
            "build/app/outputs/flutter-apk/app-release.apk",
            "build/app/outputs/flutter-apk/app-debug.apk",
            "build/app/outputs/bundle/release/app-release.aab",
            "build/web/index.html",
            "build/linux/x64/release/bundle",
            "build/macos/Build/Products/Release",
            "build/windows/x64/runner/Release",
        ],
        Kind::Expo => &[
            "dist/index.html",
            "android/app/build/outputs/apk/debug/app-debug.apk",
            "android/app/build/outputs/apk/release/app-release.apk",
        ],
        Kind::Node => &["dist/index.html", "build/index.html", ".next/BUILD_ID", "out/index.html"],
        Kind::Cargo => &["target/release"],
        _ => &[],
    };
    let mut out: Vec<PathBuf> = rel.iter().map(|r| dir.join(r)).filter(|p| p.exists()).collect();
    // IPAs carry the app's name: look in their folder.
    if kind == Kind::Flutter {
        if let Ok(rd) = std::fs::read_dir(dir.join("build/ios/ipa")) {
            out.extend(rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "ipa")));
        }
    }
    out
}

/// A Flutter app's Android release signing isn't set up (release builds
/// then use the debug key, which stores reject).
pub fn missing_android_signing(kind: Kind, dir: &Path) -> bool {
    kind == Kind::Flutter && dir.join("android").is_dir() && !dir.join("android/key.properties").exists()
}

/// What to spawn in the terminal: the command line in a login shell (so
/// PATH has flutter, node…), locally or on the SSH host.
pub fn spec(cfg: &RunConfig, root: &Path, remote: Option<&RemoteSpec>, extra_args: &str) -> CommandSpec {
    let mut line = if extra_args.is_empty() { cfg.command.clone() } else { format!("{} {extra_args}", cfg.command) };
    // Flutter wraps its messages (and URLs) at the terminal width itself;
    // without that the terminal wraps them and they can be found whole.
    if let Some(rest) = line.strip_prefix("flutter ").filter(|r| !r.contains("-wrap")) {
        line = format!("flutter --no-wrap {rest}");
    }
    let mut spec = match remote {
        Some(r) => {
            let env: String = cfg.env.iter().map(|(k, v)| format!("{k}={} ", crate::remote::sh_quote(v))).collect();
            let dir = if cfg.cwd.is_empty() {
                String::new()
            } else {
                format!(" && cd {}", crate::remote::sh_quote(&cfg.cwd))
            };
            let mut a = crate::remote::ssh_args(r, true);
            a.push(format!(
                "cd {}{dir} && {env}exec \"${{SHELL:-sh}}\" -lc {}",
                r.qpath(),
                crate::remote::sh_quote(&line)
            ));
            CommandSpec::program("ssh", a)
        }
        None => {
            let mut s = if cfg!(windows) {
                CommandSpec::program("cmd", vec!["/C".into(), line])
            } else {
                let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                CommandSpec::program(sh, vec!["-lc".into(), line])
            };
            s.env = cfg.env.clone();
            s
        }
    };
    let dir = if cfg.cwd.is_empty() { root.to_path_buf() } else { root.join(&cfg.cwd) };
    spec.cwd = Some(dir);
    spec
}

/// Local URLs printed by a dev server, in order of appearance.
pub fn find_urls(lines: &[String]) -> Vec<String> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"https?://(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\]|(?:\d{1,3}\.){3}\d{1,3})(?::\d+)?(?:/[^\s'\x22<>)\]]*)?")
            .unwrap()
    });
    let mut out: Vec<String> = vec![];
    for l in lines {
        for m in RE.find_iter(l) {
            let u = m
                .as_str()
                .trim_end_matches(['.', ',', ';'])
                .replace("0.0.0.0", "localhost")
                .replace("[::]", "localhost");
            if !out.contains(&u) {
                out.push(u);
            }
        }
    }
    out
}

/// Expo's app URLs (`exp://192.168.1.5:8081`) to open in Expo Go.
pub fn find_expo_urls(lines: &[String]) -> Vec<String> {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"exps?://[^\s'\x22<>)\]]+").unwrap());
    let mut out: Vec<String> = vec![];
    for l in lines {
        for m in RE.find_iter(l) {
            let u = m.as_str().trim_end_matches(['.', ',']).to_string();
            if !out.contains(&u) {
                out.push(u);
            }
        }
    }
    out
}

/// A URL another device on the network can open (not localhost).
pub fn is_lan_url(u: &str) -> bool {
    !(u.contains("://localhost") || u.contains("://127.") || u.contains("://[::"))
}

/// Join lines that a program broke itself at the terminal width (Flutter
/// wraps its messages): a full-width line followed by one that doesn't start
/// with a space continues it.
pub fn unwrap_lines(lines: &[String], cols: usize) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut join = false;
    for l in lines {
        let t = l.trim_end();
        match out.last_mut() {
            Some(last) if join && !t.is_empty() && !t.starts_with(char::is_whitespace) && !starts_sentence(t) => {
                last.push_str(t)
            }
            _ => out.push(t.to_string()),
        }
        // Too narrow a pane breaks everything: don't guess there.
        join = cols >= 30 && t.chars().count() >= cols.saturating_sub(1);
    }
    out
}

/// Add newly found URLs to the known ones: a cut-off copy of a known URL
/// is ignored, a longer one replaces its prefix.
pub fn merge_urls(mut known: Vec<String>, found: Vec<String>) -> Vec<String> {
    for u in found {
        if known.iter().any(|k| k.starts_with(&u)) {
            continue;
        }
        match known.iter_mut().find(|k| u.starts_with(k.as_str())) {
            Some(k) => *k = u,
            None => known.push(u),
        }
    }
    known
}

/// "The Flutter DevTools…": a new message, not the rest of a cut line.
fn starts_sentence(t: &str) -> bool {
    let mut c = t.chars();
    matches!((c.next(), c.next()), (Some(a), Some(b)) if a.is_uppercase() && b.is_lowercase())
}

/// The port of a local URL.
pub fn port_of(url: &str) -> Option<u16> {
    let rest = url.split("://").nth(1)?;
    let host = rest.split('/').next()?;
    host.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).or(Some(if url.starts_with("https") { 443 } else { 80 }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_projects() {
        let root = std::env::temp_dir().join(format!("zenith-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("app/pubspec.yaml"), "name: a\nflutter:\n  uses-material-design: true\n").unwrap();
        std::fs::write(root.join("web/package.json"), r#"{"scripts":{"dev":"vite","build":"vite build"}}"#).unwrap();
        std::fs::write(root.join("web/pnpm-lock.yaml"), "").unwrap();
        let (cfgs, err) = load(&root);
        assert!(err.is_none());
        let names: Vec<(&str, &str, &str)> =
            cfgs.iter().map(|c| (c.name.as_str(), c.command.as_str(), c.cwd.as_str())).collect();
        assert_eq!(
            names,
            [
                ("Cargo: run", "cargo run", ""),
                ("app: Flutter: run", "flutter run", "app"),
                ("web: pnpm: dev", "pnpm dev", "web")
            ]
        );
        assert_eq!(cfgs[1].kind, Kind::Flutter);
        let b = build_tasks(&cfgs[1], &root);
        assert!(b.iter().any(|(_, c)| c == "flutter build apk --release"));
        assert_eq!(build_tasks(&cfgs[2], &root), [("Production build".to_string(), "pnpm run build".to_string())]);
        std::fs::create_dir_all(root.join("app/build/app/outputs/flutter-apk")).unwrap();
        std::fs::write(root.join("app/build/app/outputs/flutter-apk/app-release.apk"), "").unwrap();
        assert_eq!(artifacts(Kind::Flutter, &root.join("app")).len(), 1);
        // run.toml comes first; `detect = false` hides the rest.
        std::fs::create_dir_all(root.join(".zenith")).unwrap();
        std::fs::write(
            config_path(&root),
            "detect = false\n[[run]]\nname = \"api\"\ncommand = \"npx expo start\"\n[run.env]\nA = \"1\"\n",
        )
        .unwrap();
        let (cfgs, _) = load(&root);
        assert_eq!(cfgs.len(), 1);
        assert_eq!((cfgs[0].kind, cfgs[0].env.clone()), (Kind::Expo, vec![("A".into(), "1".into())]));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn expo_package() {
        let root = std::env::temp_dir().join(format!("zenith-run-expo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"expo":"~51.0.0"},"scripts":{"start":"expo start","android":"expo start --android"}}"#,
        )
        .unwrap();
        let c = detect(&root);
        assert_eq!(c.iter().map(|c| c.command.as_str()).collect::<Vec<_>>(), ["npx expo start", "npm run android"]);
        assert!(c.iter().all(|c| c.kind == Kind::Expo));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn finds_urls() {
        let out = [
            "  VITE v5.0.0  ready in 300 ms".to_string(),
            "  ➜  Local:   http://localhost:5173/".to_string(),
            "  ➜  Network: http://192.168.1.5:5173/".to_string(),
            "Listening on http://0.0.0.0:8000.".to_string(),
            "again http://localhost:5173/".to_string(),
        ];
        assert_eq!(find_urls(&out), ["http://localhost:5173/", "http://192.168.1.5:5173/", "http://localhost:8000"]);
        assert_eq!(port_of("http://localhost:5173/"), Some(5173));
        let expo = [
            "› Metro waiting on exp://192.168.1.20:8081".to_string(),
            "› Web is waiting on http://localhost:8081".to_string(),
        ];
        assert_eq!(find_expo_urls(&expo), ["exp://192.168.1.20:8081"]);
        assert!(is_lan_url("http://192.168.1.5:5173/") && !is_lan_url("http://localhost:5173/"));
        let wrapped = [
            "The DevTools debugger is available at: http://127".to_string(),
            ".0.0.1:4233/x/".to_string(),
            "  next".to_string(),
        ];
        let joined = unwrap_lines(&wrapped, 50);
        assert_eq!(joined, ["The DevTools debugger is available at: http://127.0.0.1:4233/x/", "  next"]);
        assert_eq!(unwrap_lines(&wrapped[..2], 20).len(), 2); // narrow: no guessing
        let full =
            ["A Dart VM Service is available at: http://127.0.0.1:1/x=/".to_string(), "The DevTools…".to_string()];
        assert_eq!(unwrap_lines(&full, full[0].len()).len(), 2);
        let known = merge_urls(vec![], vec!["http://localhost:41705".into(), "http://127.0.0.1:4".into()]);
        let known = merge_urls(known, vec!["http://127.0.0.1:4233/x".into(), "http://localhost:4".into()]);
        assert_eq!(known, ["http://localhost:41705", "http://127.0.0.1:4233/x"]);
        assert_eq!(find_urls(&joined), ["http://127.0.0.1:4233/x/"]);
        assert_eq!(port_of("http://localhost/x"), Some(80));
    }
}
