//! Crash reports: a panic writes `<config>/crashes/crash-<time>.txt` with
//! the message, where it happened, a backtrace and the environment. The next
//! start shows the newest unseen report so it can be copied into an issue.

use std::path::{Path, PathBuf};

pub fn dir(config: &Path) -> PathBuf {
    config.join("crashes")
}

/// The text of a report.
pub fn report(message: &str, location: &str, thread: &str, backtrace: &str) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!(
        "Zenith {version} crashed\n\
         \n\
         Message:  {message}\n\
         Location: {location}\n\
         Thread:   {thread}\n\
         OS:       {os} ({arch})\n\
         Time:     {secs} (Unix)\n\
         \n\
         Backtrace:\n{backtrace}\n",
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
    )
}

/// Write a report for every panic (then run the default hook, which
/// prints to stderr).
pub fn install(config: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no message)".into());
        let location = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let text = report(&message, &location, &thread, &backtrace);
        let d = dir(&config);
        if std::fs::create_dir_all(&d).is_ok() {
            let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
            let _ = std::fs::write(d.join(format!("crash-{ms}.txt")), text);
        }
        previous(info);
    }));
}

/// The newest report not shown yet.
pub fn unseen(config: &Path) -> Option<(PathBuf, String)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir(config))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    files.sort();
    let newest = files.pop()?;
    let text = std::fs::read_to_string(&newest).ok()?;
    Some((newest, text))
}

/// Mark the reports as shown (kept as `.seen`; only the last 10 are kept).
pub fn mark_seen(path: &Path) {
    if let Some(d) = path.parent() {
        for p in std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()) {
            if p.extension().is_some_and(|e| e == "txt") {
                let _ = std::fs::rename(&p, p.with_extension("seen"));
            }
        }
        let mut old: Vec<PathBuf> = std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "seen"))
            .collect();
        old.sort();
        let n = old.len().saturating_sub(10);
        for p in &old[..n] {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// A GitHub "new issue" link with the report's first lines as the title.
pub fn issue_url(report: &str) -> String {
    let title = report.lines().nth(2).unwrap_or("Crash").trim().trim_start_matches("Message:").trim();
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    };
    format!(
        "https://github.com/abbosjon-dev/code-editor/issues/new?title={}&body={}",
        enc(&format!("Crash: {title}")),
        enc("Please paste the crash report here and describe what you were doing.")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_files() {
        let cfg = std::env::temp_dir().join(format!("zenith-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cfg);
        assert!(unseen(&cfg).is_none());
        std::fs::create_dir_all(dir(&cfg)).unwrap();
        let r = report("boom", "src/a.rs:3", "main", "0: x");
        assert!(r.contains("Message:  boom") && r.contains("Backtrace:\n0: x"));
        std::fs::write(dir(&cfg).join("crash-1.txt"), "old").unwrap();
        std::fs::write(dir(&cfg).join("crash-2.txt"), &r).unwrap();
        let (p, text) = unseen(&cfg).unwrap();
        assert!(p.ends_with("crash-2.txt") && text == r);
        mark_seen(&p);
        assert!(unseen(&cfg).is_none());
        assert!(dir(&cfg).join("crash-2.seen").exists());
        assert!(issue_url(&r).contains("title=Crash%3A%20boom"));
        let _ = std::fs::remove_dir_all(&cfg);
    }
}
