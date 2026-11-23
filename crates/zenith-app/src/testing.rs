//! Test explorer: discovers and runs Rust (libtest) and Python (pytest)
//! tests in the background and reports per-test results.
//!
//! Rust: `cargo test --no-run --message-format=json` builds the test
//! binaries; each binary lists its tests (`--list`) and runs them directly
//! (all, or one with `--exact`) from its package directory.
//! Python: `pytest --collect-only -q`, and `-rA` for per-test results.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Unknown,
    Running,
    Passed,
    Failed,
    Ignored,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A libtest binary: (package, target, executable, working dir).
    Cargo {
        package: String,
        target: String,
        exe: PathBuf,
        cwd: PathBuf,
    },
    Pytest,
}

#[derive(Clone, Debug)]
pub struct TestItem {
    /// Group shown in the tree ("zenith-core · lib", "tests/test_x.py").
    pub group: String,
    /// Test name as the runner knows it ("buffer::tests::undo", "tests/a.py::test_b").
    pub name: String,
    pub kind: Kind,
    pub status: Status,
    /// Failure message / captured output.
    pub output: String,
}

impl TestItem {
    pub fn id(&self) -> String {
        format!("{}#{}", self.group, self.name)
    }

    /// Short name shown in the tree.
    pub fn short(&self) -> &str {
        match self.kind {
            Kind::Cargo { .. } => &self.name,
            Kind::Pytest => self.name.split_once("::").map_or(&self.name, |(_, t)| t),
        }
    }
}

pub enum Event {
    Log(String),
    Discovered(Vec<TestItem>),
    Status(String, Status, String),
    Done(Result<(), String>),
}

// ------------------------------------------------------------------ parsing

/// Test executables from `cargo test --no-run --message-format=json`.
pub fn parse_cargo_artifacts(out: &str) -> Vec<(String, String, PathBuf, PathBuf)> {
    let mut v = vec![];
    for line in out.lines() {
        let Ok(j) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if j["reason"] != "compiler-artifact" || j["profile"]["test"] != true {
            continue;
        }
        let Some(exe) = j["executable"].as_str() else { continue };
        let package = j["package_id"]
            .as_str()
            .and_then(package_name_from_id)
            .unwrap_or_else(|| j["target"]["name"].as_str().unwrap_or("?").to_string());
        let kind = j["target"]["kind"][0].as_str().unwrap_or("lib");
        let target = match kind {
            "lib" | "proc-macro" => "lib".to_string(),
            "bin" => format!("bin {}", j["target"]["name"].as_str().unwrap_or("")),
            k => format!("{k} {}", j["target"]["name"].as_str().unwrap_or("")),
        };
        let cwd =
            j["manifest_path"].as_str().and_then(|m| Path::new(m).parent().map(Path::to_path_buf)).unwrap_or_default();
        v.push((package, target, PathBuf::from(exe), cwd));
    }
    v
}

/// "path+file:///w/crates/zenith-core#0.1.0" or "zenith-core 0.1.0 (path+…)" → "zenith-core".
fn package_name_from_id(id: &str) -> Option<String> {
    if let Some((_, rest)) = id.rsplit_once('#') {
        // New format: …/name#version or …#name@version
        if let Some((name, _)) = rest.split_once('@') {
            return Some(name.to_string());
        }
        let path = id.split('#').next()?;
        return path.rsplit('/').next().map(str::to_string);
    }
    id.split_whitespace().next().map(str::to_string)
}

/// Names from `<test-binary> --list`.
pub fn parse_libtest_list(out: &str) -> Vec<String> {
    out.lines().filter_map(|l| l.strip_suffix(": test")).map(str::to_string).collect()
}

/// Per-test results and failure output from a libtest run.
pub fn parse_libtest_results(out: &str) -> (Vec<(String, Status)>, HashMap<String, String>) {
    let mut results = vec![];
    let mut failures: HashMap<String, String> = HashMap::new();
    let mut current: Option<String> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("test ") {
            if let Some((name, outcome)) = rest.rsplit_once(" ... ") {
                let status = match outcome.trim() {
                    "ok" => Status::Passed,
                    "FAILED" => Status::Failed,
                    s if s.starts_with("ignored") => Status::Ignored,
                    _ => continue,
                };
                results.push((name.trim().to_string(), status));
                continue;
            }
        }
        if let Some(name) = line.strip_prefix("---- ").and_then(|l| l.strip_suffix(" stdout ----")) {
            current = Some(name.to_string());
            continue;
        }
        if line == "failures:" || line.starts_with("test result:") {
            current = None;
            continue;
        }
        if let Some(n) = &current {
            let o = failures.entry(n.clone()).or_default();
            o.push_str(line);
            o.push('\n');
        }
    }
    (results, failures)
}

/// Node ids from `pytest --collect-only -q`.
pub fn parse_pytest_collect(out: &str) -> Vec<String> {
    out.lines().map(str::trim).filter(|l| l.contains("::") && !l.contains(' ')).map(str::to_string).collect()
}

/// Results from the `-rA` short summary: (node id, status, message).
pub fn parse_pytest_results(out: &str) -> Vec<(String, Status, String)> {
    out.lines()
        .filter_map(|l| {
            let (word, rest) = l.split_once(' ')?;
            let status = match word {
                "PASSED" | "XPASS" => Status::Passed,
                "FAILED" | "ERROR" => Status::Failed,
                "SKIPPED" | "XFAIL" => Status::Ignored,
                _ => return None,
            };
            let (id, msg) = rest.split_once(" - ").unwrap_or((rest, ""));
            // SKIPPED lines look like "SKIPPED [1] path:3: reason".
            let id = id.trim();
            if !id.contains("::") {
                return None;
            }
            Some((id.to_string(), status, msg.to_string()))
        })
        .collect()
}

// ------------------------------------------------------------------ running

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

/// Run a command, streaming stderr lines to the log; returns stdout and success.
fn run_logged(mut cmd: Command, tx: &Sender<Event>) -> Result<(String, bool), String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stderr = child.stderr.take();
    let tx2 = tx.clone();
    let t = std::thread::spawn(move || {
        let mut all = String::new();
        if let Some(e) = stderr {
            for l in BufReader::new(e).lines().map_while(Result::ok) {
                all.push_str(&l);
                all.push('\n');
                let _ = tx2.send(Event::Log(l));
            }
        }
        all
    });
    let mut out = String::new();
    if let Some(o) = child.stdout.take() {
        for l in BufReader::new(o).lines().map_while(Result::ok) {
            out.push_str(&l);
            out.push('\n');
        }
    }
    let err = t.join().unwrap_or_default();
    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    out.push_str(&err);
    Ok((out, ok))
}

pub fn has_cargo(root: &Path) -> bool {
    root.join("Cargo.toml").exists()
}

pub fn has_pytest(root: &Path) -> bool {
    let markers = ["pytest.ini", "conftest.py", "tox.ini", "setup.cfg"];
    if markers.iter().any(|m| root.join(m).exists()) {
        return true;
    }
    if std::fs::read_to_string(root.join("pyproject.toml")).is_ok_and(|t| t.contains("pytest")) {
        return true;
    }
    // Any test_*.py near the top.
    ignore::WalkBuilder::new(root).max_depth(Some(3)).build().flatten().any(|e| {
        let n = e.file_name().to_string_lossy();
        n.starts_with("test_") && n.ends_with(".py")
    })
}

fn python() -> &'static str {
    if which::which("python3").is_ok() {
        "python3"
    } else {
        "python"
    }
}

/// Build Rust test binaries and list every test.
fn discover_cargo(root: &Path, tx: &Sender<Event>) -> Result<Vec<TestItem>, String> {
    let _ = tx.send(Event::Log("$ cargo test --workspace --no-run".into()));
    let mut c = command("cargo");
    c.args(["test", "--workspace", "--no-run", "--message-format=json"]).current_dir(root);
    let (out, ok) = run_logged(c, tx)?;
    if !ok {
        return Err("cargo failed to build the tests (see the output)".into());
    }
    let mut items = vec![];
    for (package, target, exe, cwd) in parse_cargo_artifacts(&out) {
        let mut c = command(&exe.to_string_lossy());
        c.args(["--list", "--format", "terse"]).current_dir(&cwd);
        let Ok((list, _)) = run_logged(c, tx) else { continue };
        let group = format!("{package} · {target}");
        for name in parse_libtest_list(&list) {
            items.push(TestItem {
                group: group.clone(),
                name,
                kind: Kind::Cargo {
                    package: package.clone(),
                    target: target.clone(),
                    exe: exe.clone(),
                    cwd: cwd.clone(),
                },
                status: Status::Unknown,
                output: String::new(),
            });
        }
    }
    Ok(items)
}

fn discover_pytest(root: &Path, tx: &Sender<Event>) -> Result<Vec<TestItem>, String> {
    let _ = tx.send(Event::Log("$ pytest --collect-only -q".into()));
    let mut c = command(python());
    c.args(["-m", "pytest", "--collect-only", "-q"]).current_dir(root);
    let (out, _) = run_logged(c, tx)?;
    Ok(parse_pytest_collect(&out)
        .into_iter()
        .map(|id| TestItem {
            group: id.split("::").next().unwrap_or("").to_string(),
            name: id,
            kind: Kind::Pytest,
            status: Status::Unknown,
            output: String::new(),
        })
        .collect())
}

/// Discover all tests in the background.
pub fn discover(root: PathBuf, waker: Arc<dyn Fn() + Send + Sync>) -> Receiver<Event> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut items = vec![];
        let mut errors = vec![];
        if has_cargo(&root) {
            match discover_cargo(&root, &tx) {
                Ok(i) => items.extend(i),
                Err(e) => errors.push(e),
            }
        }
        if has_pytest(&root) {
            match discover_pytest(&root, &tx) {
                Ok(i) => items.extend(i),
                Err(e) => errors.push(e),
            }
        }
        let _ = tx.send(Event::Discovered(items));
        let _ = tx.send(Event::Done(if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }));
        waker();
    });
    rx
}

/// Run `tests` (all of one runner's tests when they're the whole set).
pub fn run(root: PathBuf, tests: Vec<TestItem>, waker: Arc<dyn Fn() + Send + Sync>) -> Receiver<Event> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let wake = || waker();
        for t in &tests {
            let _ = tx.send(Event::Status(t.id(), Status::Running, String::new()));
        }
        wake();
        let mut error = None;
        // Rust: rebuild once, then run per binary.
        let cargo: Vec<&TestItem> = tests.iter().filter(|t| matches!(t.kind, Kind::Cargo { .. })).collect();
        if !cargo.is_empty() {
            let _ = tx.send(Event::Log("$ cargo test --workspace --no-run".into()));
            let mut c = command("cargo");
            c.args(["test", "--workspace", "--no-run", "--message-format=json"]).current_dir(&root);
            match run_logged(c, &tx) {
                Ok((out, true)) => {
                    // Binaries may have new hashes after the rebuild.
                    let fresh = parse_cargo_artifacts(&out);
                    let mut by_group: HashMap<String, Vec<&TestItem>> = HashMap::new();
                    for t in &cargo {
                        by_group.entry(t.group.clone()).or_default().push(t);
                    }
                    for (group, items) in by_group {
                        let Kind::Cargo { package, target, cwd, .. } = &items[0].kind else { continue };
                        let exe = fresh
                            .iter()
                            .find(|(p, tg, _, _)| p == package && tg == target)
                            .map(|(_, _, e, _)| e.clone());
                        let Some(exe) = exe else { continue };
                        let mut c = command(&exe.to_string_lossy());
                        // Short panic messages (the full backtrace drowns the assertion).
                        c.current_dir(cwd).arg("--test-threads=4").env("RUST_BACKTRACE", "0");
                        if items.len() == 1 {
                            c.args([&items[0].name, "--exact"]);
                        }
                        let _ = tx.send(Event::Log(format!("▶ {group}")));
                        let Ok((out, _)) = run_logged(c, &tx) else { continue };
                        let (results, failures) = parse_libtest_results(&out);
                        for (name, status) in results {
                            let msg = failures.get(&name).cloned().unwrap_or_default();
                            let _ = tx.send(Event::Status(format!("{group}#{name}"), status, msg));
                        }
                        wake();
                    }
                }
                Ok((_, false)) => error = Some("cargo failed to build the tests".to_string()),
                Err(e) => error = Some(e),
            }
        }
        // Python.
        let py: Vec<&TestItem> = tests.iter().filter(|t| t.kind == Kind::Pytest).collect();
        if !py.is_empty() {
            let mut c = command(python());
            c.args(["-m", "pytest", "-q", "-rA", "--no-header"]).current_dir(&root);
            c.args(py.iter().map(|t| t.name.as_str()));
            let _ = tx.send(Event::Log("$ pytest".into()));
            match run_logged(c, &tx) {
                Ok((out, _)) => {
                    for l in out.lines().filter(|l| l.contains(" passed") || l.contains(" failed")) {
                        let _ = tx.send(Event::Log(l.to_string()));
                    }
                    for (id, status, msg) in parse_pytest_results(&out) {
                        let group = id.split("::").next().unwrap_or("").to_string();
                        let _ = tx.send(Event::Status(format!("{group}#{id}"), status, msg));
                    }
                }
                Err(e) => error = Some(e),
            }
        }
        let _ = tx.send(Event::Done(error.map_or(Ok(()), Err)));
        wake();
    });
    rx
}

/// Where a test is defined: the file and 0-based line of `fn name` / `def name`.
pub fn locate(root: &Path, t: &TestItem) -> Option<(PathBuf, usize)> {
    match &t.kind {
        Kind::Pytest => {
            let file = root.join(t.name.split("::").next()?);
            let func = t.name.rsplit("::").next()?.split('[').next()?;
            let text = std::fs::read_to_string(&file).ok()?;
            let line = text.lines().position(|l| l.trim_start().starts_with(&format!("def {func}(")))?;
            Some((file, line))
        }
        Kind::Cargo { cwd, .. } => {
            let mut parts: Vec<&str> = t.name.split("::").collect();
            let func = parts.pop()?;
            let needle = format!("fn {func}(");
            let mut best: Option<(PathBuf, usize, usize)> = None;
            for e in ignore::WalkBuilder::new(cwd).build().flatten() {
                if e.path().extension().is_none_or(|x| x != "rs") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
                let Some(line) = text.lines().position(|l| l.contains(&needle)) else { continue };
                // Prefer files named after the test's module path.
                let p = e.path().to_string_lossy();
                let score =
                    parts.iter().filter(|m| p.contains(&format!("{m}.rs")) || p.contains(&format!("{m}/"))).count();
                if best.as_ref().is_none_or(|b| score > b.2) {
                    best = Some((e.path().to_path_buf(), line, score));
                }
            }
            best.map(|(p, l, _)| (p, l))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_artifacts_and_lists() {
        let json = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/crates/zenith-core#0.1.0","manifest_path":"/w/crates/zenith-core/Cargo.toml","target":{"kind":["lib"],"name":"zenith_core"},"profile":{"test":true},"executable":"/w/target/debug/deps/zenith_core-abc"}
{"reason":"compiler-artifact","package_id":"zenith-app 0.1.0 (path+file:///w/crates/zenith-app)","manifest_path":"/w/crates/zenith-app/Cargo.toml","target":{"kind":["bin"],"name":"zenith"},"profile":{"test":true},"executable":"/w/target/debug/deps/zenith-def"}
{"reason":"compiler-artifact","package_id":"x","target":{"kind":["lib"],"name":"dep"},"profile":{"test":false},"executable":null}
{"reason":"build-finished","success":true}"#;
        let a = parse_cargo_artifacts(json);
        assert_eq!(a.len(), 2);
        assert_eq!((a[0].0.as_str(), a[0].1.as_str()), ("zenith-core", "lib"));
        assert_eq!(a[0].3, PathBuf::from("/w/crates/zenith-core"));
        assert_eq!((a[1].0.as_str(), a[1].1.as_str()), ("zenith-app", "bin zenith"));
        assert_eq!(package_name_from_id("registry+https://x#serde@1.0.0").as_deref(), Some("serde"));
        assert_eq!(parse_libtest_list("a::b: test\nc: test\nbench_x: benchmark\n\n2 tests"), vec!["a::b", "c"]);
    }

    #[test]
    fn libtest_results_with_failures() {
        let out = "running 3 tests\ntest a::ok_one ... ok\ntest a::bad ... FAILED\ntest a::slow ... ignored, takes long\n\nfailures:\n\n---- a::bad stdout ----\nthread 'a::bad' panicked at src/a.rs:3:5:\nboom\n\n\nfailures:\n    a::bad\n\ntest result: FAILED. 1 passed; 1 failed; 1 ignored";
        let (r, f) = parse_libtest_results(out);
        assert_eq!(
            r,
            vec![
                ("a::ok_one".into(), Status::Passed),
                ("a::bad".into(), Status::Failed),
                ("a::slow".into(), Status::Ignored)
            ]
        );
        assert!(f["a::bad"].contains("boom"));
    }

    #[test]
    fn pytest_parsing() {
        let collect = "tests/test_a.py::test_one\ntests/test_a.py::TestX::test_two\ntests/test_b.py::test_p[1]\n\n3 tests collected in 0.01s\n";
        assert_eq!(parse_pytest_collect(collect).len(), 3);
        let out = "..F\n=== short test summary info ===\nPASSED tests/test_a.py::test_one\nFAILED tests/test_a.py::TestX::test_two - assert 1 == 2\nSKIPPED [1] tests/test_c.py:3: no db\n1 failed, 1 passed in 0.02s";
        let r = parse_pytest_results(out);
        assert_eq!(r.len(), 2);
        assert_eq!(r[1], ("tests/test_a.py::TestX::test_two".into(), Status::Failed, "assert 1 == 2".into()));
    }

    /// Real pytest end to end: discover, run, locate.
    #[test]
    fn pytest_end_to_end() {
        let ok = Command::new(python()).args(["-m", "pytest", "--version"]).output().is_ok_and(|o| o.status.success());
        if !ok {
            return;
        }
        let dir = std::env::temp_dir().join(format!("zenith-pytest-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(
            dir.join("tests/test_m.py"),
            "def test_good():\n    assert 1 + 1 == 2\n\n\ndef test_bad():\n    assert 2 == 3\n",
        )
        .unwrap();
        let rx = discover(dir.clone(), Arc::new(|| {}));
        let mut items = vec![];
        for ev in rx.iter() {
            if let Event::Discovered(i) = ev {
                items = i;
            }
        }
        assert_eq!(items.len(), 2, "{items:?}");
        let rx = run(dir.clone(), items.clone(), Arc::new(|| {}));
        let mut st = HashMap::new();
        for ev in rx.iter() {
            if let Event::Status(id, s, _) = ev {
                st.insert(id, s);
            }
        }
        assert_eq!(st[&items[0].id()], Status::Passed);
        assert_eq!(st[&items[1].id()], Status::Failed);
        assert_eq!(locate(&dir, &items[1]).map(|(_, l)| l), Some(4));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ------------------------------------------------------------------ panel

/// What the panel asks the app to do.
pub enum Action {
    Open(PathBuf, usize),
}

#[derive(Default)]
pub struct Panel {
    pub items: Vec<TestItem>,
    rx: Option<Receiver<Event>>,
    root: Option<PathBuf>,
    busy: bool,
    pub log: Vec<String>,
    error: Option<String>,
    selected: Option<String>,
    collapsed: std::collections::HashSet<String>,
    filter: String,
    discovered: bool,
}

impl Panel {
    pub fn refresh(&mut self, root: PathBuf, waker: Arc<dyn Fn() + Send + Sync>) {
        if self.busy {
            return;
        }
        self.log.clear();
        self.error = None;
        self.busy = true;
        self.discovered = true;
        self.root = Some(root.clone());
        self.rx = Some(discover(root, waker));
    }

    /// Run these tests (all when `None`).
    pub fn run(&mut self, which: Option<Vec<String>>, waker: Arc<dyn Fn() + Send + Sync>) {
        let Some(root) = self.root.clone() else { return };
        if self.busy {
            return;
        }
        let tests: Vec<TestItem> = match &which {
            None => self.items.clone(),
            Some(ids) => self.items.iter().filter(|t| ids.contains(&t.id())).cloned().collect(),
        };
        if tests.is_empty() {
            return;
        }
        self.log.clear();
        self.error = None;
        self.busy = true;
        self.rx = Some(run(root, tests, waker));
    }

    /// Ids of tests named `func` (last path segment).
    pub fn ids_named(&self, func: &str) -> Vec<String> {
        self.items
            .iter()
            .filter(|t| t.name.rsplit("::").next().and_then(|n| n.split('[').next()) == Some(func))
            .map(TestItem::id)
            .collect()
    }

    pub fn ensure_discovered(&mut self, root: Option<&Path>, waker: Arc<dyn Fn() + Send + Sync>) {
        if let Some(r) = root {
            if !self.discovered || self.root.as_deref() != Some(r) {
                self.items.clear();
                self.refresh(r.to_path_buf(), waker);
            }
        }
    }

    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        let mut changed = false;
        while let Ok(ev) = rx.try_recv() {
            changed = true;
            match ev {
                Event::Log(l) => {
                    self.log.push(l);
                    if self.log.len() > 3000 {
                        self.log.drain(..1000);
                    }
                }
                Event::Discovered(items) => {
                    // Keep earlier results for tests that still exist.
                    let old: HashMap<String, (Status, String)> =
                        self.items.drain(..).map(|t| (t.id(), (t.status, t.output))).collect();
                    self.items = items
                        .into_iter()
                        .map(|mut t| {
                            if let Some((s, o)) = old.get(&t.id()) {
                                t.status = *s;
                                t.output = o.clone();
                            }
                            t
                        })
                        .collect();
                }
                Event::Status(id, status, output) => {
                    if let Some(t) = self.items.iter_mut().find(|t| t.id() == id) {
                        t.status = status;
                        if status != Status::Running {
                            t.output = output;
                        }
                    }
                }
                Event::Done(r) => {
                    self.busy = false;
                    // Tests that never reported (e.g. build failed) are unknown again.
                    for t in &mut self.items {
                        if t.status == Status::Running {
                            t.status = Status::Unknown;
                        }
                    }
                    self.error = r.err();
                }
            }
        }
        if self.busy {
            return true;
        }
        changed
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let c = |s| self.items.iter().filter(|t| t.status == s).count();
        (c(Status::Passed), c(Status::Failed), self.items.len())
    }

    pub fn ui(
        &mut self,
        ui: &mut eframe::egui::Ui,
        p: &crate::theme::Palette,
        root: Option<&Path>,
        waker: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<Action> {
        use eframe::egui::{self, Align, Layout, RichText, Sense};
        let mut action = None;
        self.ensure_discovered(root, waker.clone());
        let mut run_ids: Option<Option<Vec<String>>> = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("TESTING").size(11.0).color(p.text_dim).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!self.busy, egui::Button::new("⟳").small()).on_hover_text("Discover tests").clicked()
                {
                    if let Some(r) = root {
                        self.refresh(r.to_path_buf(), waker.clone());
                    }
                }
                let failed: Vec<String> =
                    self.items.iter().filter(|t| t.status == Status::Failed).map(TestItem::id).collect();
                if !failed.is_empty()
                    && ui
                        .add_enabled(!self.busy, egui::Button::new(RichText::new("↻ Failed").color(p.error)).small())
                        .on_hover_text("Re-run failed tests")
                        .clicked()
                {
                    run_ids = Some(Some(failed));
                }
                if ui
                    .add_enabled(
                        !self.busy && !self.items.is_empty(),
                        egui::Button::new(RichText::new("▶ Run all").color(p.success)).small(),
                    )
                    .clicked()
                {
                    run_ids = Some(None);
                }
            });
        });
        let (pass, fail, total) = self.counts();
        ui.horizontal(|ui| {
            if self.busy {
                ui.spinner();
                ui.label(RichText::new(self.log.last().cloned().unwrap_or_default()).small().color(p.text_dim));
            } else if total == 0 {
                ui.label(
                    RichText::new(if root.is_some() { "No tests found (cargo / pytest)." } else { "Open a folder." })
                        .small()
                        .color(p.text_dim),
                );
            } else {
                ui.label(RichText::new(format!("{total} tests")).small().color(p.text_dim));
                if pass > 0 {
                    ui.label(RichText::new(format!("✓ {pass}")).small().color(p.success));
                }
                if fail > 0 {
                    ui.label(RichText::new(format!("✕ {fail}")).small().color(p.error));
                }
            }
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).small().color(p.error));
        }
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter tests").desired_width(f32::INFINITY));
        ui.add_space(4.0);

        let filter = self.filter.to_lowercase();
        let mut groups: Vec<(String, Vec<usize>)> = vec![];
        for (i, t) in self.items.iter().enumerate() {
            if !filter.is_empty()
                && !t.name.to_lowercase().contains(&filter)
                && !t.group.to_lowercase().contains(&filter)
            {
                continue;
            }
            match groups.iter_mut().find(|(g, _)| *g == t.group) {
                Some((_, v)) => v.push(i),
                None => groups.push((t.group.clone(), vec![i])),
            }
        }
        let icon = |s: Status| match s {
            Status::Passed => ("✓", p.success),
            Status::Failed => ("✕", p.error),
            Status::Ignored => ("○", p.text_faint),
            Status::Running => ("…", p.warning),
            Status::Unknown => ("●", p.text_faint),
        };
        let avail = ui.available_height();
        egui::ScrollArea::vertical().id_salt("tests").max_height(avail * 0.62).auto_shrink([false, true]).show(
            ui,
            |ui| {
                for (g, idx) in &groups {
                    let open = !self.collapsed.contains(g);
                    let gs = if idx.iter().any(|&i| self.items[i].status == Status::Failed) {
                        Status::Failed
                    } else if idx.iter().any(|&i| self.items[i].status == Status::Running) {
                        Status::Running
                    } else if idx.iter().all(|&i| matches!(self.items[i].status, Status::Passed | Status::Ignored)) {
                        Status::Passed
                    } else {
                        Status::Unknown
                    };
                    let (gi, gc) = icon(gs);
                    let r = ui.horizontal(|ui| {
                        ui.label(RichText::new(if open { "▾" } else { "▸" }).color(p.text_dim));
                        ui.label(RichText::new(gi).color(gc));
                        let r = ui.add(
                            egui::Label::new(RichText::new(g).strong().size(12.5)).sense(Sense::click()).truncate(),
                        );
                        ui.label(RichText::new(idx.len().to_string()).small().color(p.text_faint));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(!self.busy, egui::Button::new("▶").small().frame(false))
                                .on_hover_text("Run")
                                .clicked()
                            {
                                run_ids = Some(Some(idx.iter().map(|&i| self.items[i].id()).collect()));
                            }
                        });
                        r
                    });
                    if r.inner.clicked() {
                        if open {
                            self.collapsed.insert(g.clone());
                        } else {
                            self.collapsed.remove(g);
                        }
                    }
                    if !open {
                        continue;
                    }
                    for &i in idx {
                        let t = &self.items[i];
                        let id = t.id();
                        let (ic, col) = icon(t.status);
                        let selected = self.selected.as_ref() == Some(&id);
                        let row = ui.horizontal(|ui| {
                            ui.add_space(16.0);
                            ui.label(RichText::new(ic).color(col));
                            let r = ui.add(egui::SelectableLabel::new(selected, RichText::new(t.short()).size(12.5)));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add_enabled(!self.busy, egui::Button::new("▶").small().frame(false))
                                    .on_hover_text("Run this test")
                                    .clicked()
                                {
                                    run_ids = Some(Some(vec![id.clone()]));
                                }
                            });
                            r
                        });
                        if row.inner.clicked() {
                            self.selected = Some(id.clone());
                        }
                        if row.inner.double_clicked() {
                            if let Some((path, line)) = root.and_then(|r| locate(r, t)) {
                                action = Some(Action::Open(path, line));
                            }
                        }
                    }
                }
            },
        );
        ui.separator();
        // Output of the selected test (or the run log).
        let sel = self.selected.as_ref().and_then(|s| self.items.iter().find(|t| &t.id() == s));
        let (title, text) = match sel {
            Some(t) if !t.output.trim().is_empty() => (format!("OUTPUT · {}", t.short()), t.output.clone()),
            Some(t) => (
                format!("OUTPUT · {}", t.short()),
                match t.status {
                    Status::Passed => "Passed.".into(),
                    Status::Ignored => "Ignored.".into(),
                    Status::Unknown => "Not run yet. Double-click to open the test.".into(),
                    _ => String::new(),
                },
            ),
            None => ("LOG".into(), self.log.join("\n")),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).size(11.0).color(p.text_dim).strong());
            if sel.is_some() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Log").on_hover_text("Show the run log").clicked() {
                        self.selected = None;
                    }
                    if ui.small_button("Open").clicked() {
                        if let (Some(t), Some(r)) = (sel, root) {
                            if let Some((path, line)) = locate(r, t) {
                                action = Some(Action::Open(path, line));
                            }
                        }
                    }
                });
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("tests-out")
            .stick_to_bottom(sel.is_none())
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(text).monospace().size(11.5)).wrap());
            });
        if let Some(ids) = run_ids {
            self.run(ids, waker);
        }
        action
    }
}
