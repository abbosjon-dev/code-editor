//! Problems in Run output: compiler and runtime errors printed by the
//! running program (rustc, gcc / clang, Go, Dart / Flutter, TypeScript,
//! Python tracebacks) become entries of the Problems panel. A success line
//! printed later ("Reloaded", "Compiled successfully", "Found 0 errors")
//! clears the earlier ones, so watch modes don't keep fixed errors.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use zenith_lsp::{Diagnostic, LspPos, LspRange, Severity};

/// A problem found in the output.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub path: PathBuf,
    /// 0-based.
    pub line: u32,
    pub col: u32,
    pub severity: Severity,
    pub message: String,
}

impl Problem {
    pub fn diagnostic(&self, source: &str) -> Diagnostic {
        let pos = LspPos { line: self.line, character: self.col };
        Diagnostic {
            range: LspRange { start: pos, end: pos },
            severity: self.severity,
            message: self.message.clone(),
            source: Some(source.to_string()),
            raw: serde_json::Value::Null,
        }
    }
}

fn re(s: &str) -> Regex {
    Regex::new(s).unwrap()
}

// `path:line:col: error: message` (gcc, clang, Go, Dart, Swift, tsc --pretty false…)
static LOCATED: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^\s*(?P<path>[^\s:()][^:()]*?\.[A-Za-z0-9]+):(?P<line>\d+):(?P<col>\d+):?\s*(?:-\s*)?(?P<sev>[Ee]rror|[Ww]arning|ERROR|WARNING|fatal error)?(?:\s*(?:TS\d+|\[[^\]]+\]))?:?\s*(?P<msg>.*)$",
    )
});
// `path(line,col): error TS2322: message` (tsc classic)
static PARENS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^\s*(?P<path>[^\s(][^(]*?\.[A-Za-z0-9]+)\((?P<line>\d+),(?P<col>\d+)\):\s*(?P<sev>error|warning)\s*(?:TS\d+)?:?\s*(?P<msg>.*)$",
    )
});
// rustc / cargo: `error[E0425]: message` then `  --> src/main.rs:3:5`
static RUST_HEAD: LazyLock<Regex> = LazyLock::new(|| re(r"^(?P<sev>error|warning)(?:\[\w+\])?:\s*(?P<msg>.+)$"));
static RUST_AT: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*-->\s*(?P<path>[^:]+):(?P<line>\d+):(?P<col>\d+)"));
// Python: `  File "app.py", line 10, in main`
static PY_FRAME: LazyLock<Regex> = LazyLock::new(|| re(r#"^\s*File "(?P<path>[^"]+)", line (?P<line>\d+)"#));
static PY_ERROR: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(?P<msg>(?:[A-Za-z_][\w.]*)?(?:Error|Exception|Interrupt|Exit)\b.*)$"));

/// Lines meaning "it builds now": problems before them are stale.
fn is_success(l: &str) -> bool {
    let l = l.trim();
    l.starts_with("Reloaded ")
        || l.starts_with("Restarted application")
        || l.contains("Compiled successfully")
        || l.contains("compiled successfully")
        || l.contains("Found 0 errors")
        || l.starts_with("Finished `") && !l.contains("error")
        || l.contains("Web Bundled")
        || l.contains("Android Bundled")
        || l.contains("iOS Bundled")
}

/// Problems in `lines`; relative paths are resolved against `cwd` and only
/// files that exist are kept (so ordinary output isn't mistaken for errors).
pub fn parse(lines: &[String], cwd: &Path) -> Vec<Problem> {
    let resolve = |p: &str| -> Option<PathBuf> {
        let p = p.trim().trim_start_matches("./");
        if p.is_empty() || p.contains("://") {
            return None;
        }
        let path = Path::new(p);
        let full = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
        full.is_file().then_some(full)
    };
    let sev = |s: Option<&str>| match s.map(str::to_lowercase).as_deref() {
        Some("warning") => Severity::Warning,
        _ => Severity::Error,
    };
    let mut out: Vec<Problem> = vec![];
    let mut rust_head: Option<(Severity, String)> = None;
    let mut py_frame: Option<(PathBuf, u32)> = None;
    for raw in lines {
        let l = raw.trim_end();
        if is_success(l) {
            out.clear();
            rust_head = None;
            continue;
        }
        if let Some(c) = RUST_HEAD.captures(l) {
            // "warning: `x` generated 3 warnings" summaries aren't problems.
            if !c["msg"].contains("generated") && !c["msg"].starts_with("could not compile") {
                rust_head = Some((sev(Some(&c["sev"])), c["msg"].to_string()));
            }
            continue;
        }
        if let Some(c) = RUST_AT.captures(l) {
            if let (Some((s, msg)), Some(path)) = (rust_head.take(), resolve(&c["path"])) {
                push(
                    &mut out,
                    Problem {
                        path,
                        line: c["line"].parse::<u32>().unwrap_or(1).saturating_sub(1),
                        col: c["col"].parse::<u32>().unwrap_or(1).saturating_sub(1),
                        severity: s,
                        message: msg,
                    },
                );
            }
            continue;
        }
        if let Some(c) = PY_FRAME.captures(l) {
            if let Some(path) = resolve(&c["path"]) {
                // The innermost frame in the project is where to look.
                py_frame = Some((path, c["line"].parse::<u32>().unwrap_or(1).saturating_sub(1)));
            }
            continue;
        }
        if let Some((path, line)) = py_frame.as_ref() {
            if let Some(c) = PY_ERROR.captures(l) {
                push(
                    &mut out,
                    Problem {
                        path: path.clone(),
                        line: *line,
                        col: 0,
                        severity: Severity::Error,
                        message: c["msg"].to_string(),
                    },
                );
                py_frame = None;
                continue;
            }
        }
        for re in [&*PARENS, &*LOCATED] {
            if let Some(c) = re.captures(l) {
                let msg = c.name("msg").map_or("", |m| m.as_str()).trim();
                // A bare location with nothing to say is a stack line, not a problem.
                if msg.is_empty() && c.name("sev").is_none() {
                    break;
                }
                if let Some(path) = resolve(&c["path"]) {
                    push(
                        &mut out,
                        Problem {
                            path,
                            line: c["line"].parse::<u32>().unwrap_or(1).saturating_sub(1),
                            col: c["col"].parse::<u32>().unwrap_or(1).saturating_sub(1),
                            severity: sev(c.name("sev").map(|m| m.as_str())),
                            message: if msg.is_empty() { "error".into() } else { msg.to_string() },
                        },
                    );
                }
                break;
            }
        }
    }
    out
}

fn push(out: &mut Vec<Problem>, p: Problem) {
    if !out.iter().any(|q| q.path == p.path && q.line == p.line && q.message == p.message) {
        out.push(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> PathBuf {
        let d = std::env::temp_dir().join(format!("zenith-runp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for f in ["src/main.rs", "lib/main.dart", "src/app.ts", "main.go", "app.py", "hello.c"] {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        d
    }

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn finds_compiler_errors() {
        let d = setup();
        let out = lines(
            "   Compiling x v0.1.0\n\
             error[E0425]: cannot find value `y` in this scope\n\
             \x20 --> src/main.rs:3:13\n\
             warning: unused variable: `z`\n\
             \x20 --> src/main.rs:2:9\n\
             warning: `x` (bin \"x\") generated 1 warning\n\
             lib/main.dart:12:5: Error: The getter 'foo' isn't defined.\n\
             src/app.ts(4,7): error TS2322: Type 'string' is not assignable to type 'number'.\n\
             src/app.ts:9:1 - error TS1005: ';' expected.\n\
             ./main.go:5:2: undefined: fmtx\n\
             hello.c:7:3: warning: implicit declaration of function 'foo'\n\
             at Object.<anonymous> (nowhere.js:1:1)\n\
             see http://localhost:8080/x:1:2: not a file\n",
        );
        let p = parse(&out, &d);
        let got: Vec<(String, u32, Severity)> = p
            .iter()
            .map(|p| (p.path.strip_prefix(&d).unwrap().to_string_lossy().into_owned(), p.line, p.severity))
            .collect();
        assert_eq!(
            got,
            [
                ("src/main.rs".into(), 2, Severity::Error),
                ("src/main.rs".into(), 1, Severity::Warning),
                ("lib/main.dart".into(), 11, Severity::Error),
                ("src/app.ts".into(), 3, Severity::Error),
                ("src/app.ts".into(), 8, Severity::Error),
                ("main.go".into(), 4, Severity::Error),
                ("hello.c".into(), 6, Severity::Warning),
            ]
        );
        assert_eq!(p[0].message, "cannot find value `y` in this scope");
        assert_eq!(p[3].message, "Type 'string' is not assignable to type 'number'.");
        assert_eq!(p[5].message, "undefined: fmtx");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn python_traceback_and_success_clears() {
        let d = setup();
        let py = d.join("app.py").to_string_lossy().into_owned();
        let out = lines(&format!(
            "Traceback (most recent call last):\n  File \"{py}\", line 10, in <module>\n    main()\n  File \"/usr/lib/python3/x.py\", line 3, in f\nNameError: name 'x' is not defined\n"
        ));
        let p = parse(&out, &d);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].line, p[0].message.as_str()), (9, "NameError: name 'x' is not defined"));
        // Fixed and hot reloaded: nothing left.
        let mut more = lines("lib/main.dart:12:5: Error: bad\n");
        more.push("Reloaded 1 of 650 libraries in 300ms.".into());
        assert!(parse(&more, &d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
