//! Git history: the commit log (with a lane graph), a commit's changed
//! files, and file contents at a commit, read with the `git` CLI through
//! the repository's runner (so it works on SSH hosts too).

use crate::git::Runner;

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub parents: Vec<String>,
    pub author: String,
    /// Unix seconds.
    pub time: i64,
    pub subject: String,
    /// Branch / tag names pointing here ("HEAD -> main, origin/main").
    pub refs: String,
}

const FORMAT: &str = "--format=%H%x1f%h%x1f%P%x1f%an%x1f%at%x1f%s%x1f%D%x1e";

pub fn parse_log(out: &str) -> Vec<Commit> {
    out.split('\x1e')
        .filter_map(|rec| {
            let f: Vec<&str> = rec.trim_matches('\n').split('\x1f').collect();
            if f.len() < 7 || f[0].is_empty() {
                return None;
            }
            Some(Commit {
                hash: f[0].to_string(),
                short: f[1].to_string(),
                parents: f[2].split_whitespace().map(str::to_string).collect(),
                author: f[3].to_string(),
                time: f[4].parse().unwrap_or(0),
                subject: f[5].to_string(),
                refs: f[6].to_string(),
            })
        })
        .collect()
}

/// The newest `limit` commits (of all branches; of `file` when given,
/// following renames).
pub fn log(run: &Runner, file: Option<&str>, limit: usize) -> Result<Vec<Commit>, String> {
    let n = format!("-n{limit}");
    let mut args = vec!["log", FORMAT, n.as_str(), "--date-order"];
    match file {
        Some(f) => args.extend(["--follow", "--", f]),
        None => args.push("--all"),
    }
    run.git(&args).map(|o| parse_log(&o))
}

/// Files changed by `hash` (against its first parent), or between two
/// commits: (status letter, path, old path for renames).
pub fn changed_files(run: &Runner, from: Option<&str>, to: &str) -> Result<Vec<(char, String, String)>, String> {
    let out = match from {
        Some(a) => run.git(&["diff", "--name-status", "-M", a, to])?,
        // `--root` lists the first commit's files too.
        None => run.git(&["show", "--name-status", "-M", "--format=", "--root", to])?,
    };
    Ok(parse_name_status(&out))
}

pub fn parse_name_status(out: &str) -> Vec<(char, String, String)> {
    out.lines()
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let st = parts.next()?.chars().next()?;
            let a = parts.next()?.to_string();
            match parts.next() {
                // Renames / copies: old, new.
                Some(b) => Some((st, b.to_string(), a)),
                None => Some((st, a.clone(), a)),
            }
        })
        .collect()
}

/// A file's text at a commit (empty if it didn't exist there).
pub fn file_at(run: &Runner, rev: &str, path: &str) -> String {
    run.git(&["show", &format!("{rev}:{path}")]).unwrap_or_default().replace("\r\n", "\n")
}

/// The commit's full message and author line.
pub fn message(run: &Runner, hash: &str) -> String {
    run.git(&["show", "-s", "--format=%B%n— %an <%ae>, %ad", "--date=local", hash]).unwrap_or_default()
}

/// How lines `from..=to` (1-based) of `file` changed: newest commit first,
/// each with the part of its diff that touched them (`git log -L`).
pub fn line_history(
    run: &Runner,
    file: &str,
    from: usize,
    to: usize,
    limit: usize,
) -> Result<Vec<(Commit, String)>, String> {
    let range = format!("-L{from},{to}:{file}");
    let n = format!("-n{limit}");
    let out = run.git(&["log", "--format=%x1e%H%x1f%h%x1f%P%x1f%an%x1f%at%x1f%s%x1f%D", n.as_str(), range.as_str()])?;
    Ok(parse_line_history(&out))
}

pub fn parse_line_history(out: &str) -> Vec<(Commit, String)> {
    out.split('\x1e')
        .filter_map(|rec| {
            let (head, patch) = rec.split_once('\n').unwrap_or((rec, ""));
            let mut c = parse_log(&format!("{head}\x1e"));
            let c = c.pop()?;
            // Keep the hunks (from "@@"), not the diff --git / index lines.
            let diff: Vec<&str> = patch.lines().skip_while(|l| !l.starts_with("@@")).collect();
            Some((c, diff.join("\n").trim_end().to_string()))
        })
        .collect()
}

/// One row of the graph: the commit's lane, the lanes passing through,
/// and the lanes its parents continue in (drawn to the next row).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub lane: usize,
    /// Lanes with a line through this row (besides the commit's own).
    pub through: Vec<usize>,
    /// Lanes of the parents in the next row.
    pub to_parents: Vec<usize>,
    /// Lanes this commit merges in from above (lines ending at its dot).
    pub from_above: Vec<usize>,
}

/// Assign lanes so each branch keeps a column (newest commit first).
pub fn lanes(commits: &[Commit]) -> Vec<Row> {
    // Which commit each lane is waiting for.
    let mut waiting: Vec<Option<String>> = vec![];
    let mut rows = vec![];
    for c in commits {
        // Lanes expecting this commit (several when branches meet here).
        let hits: Vec<usize> =
            waiting.iter().enumerate().filter(|(_, w)| w.as_deref() == Some(&c.hash)).map(|(i, _)| i).collect();
        let lane = match hits.first() {
            Some(&l) => l,
            None => waiting.iter().position(Option::is_none).unwrap_or_else(|| {
                waiting.push(None);
                waiting.len() - 1
            }),
        };
        let from_above: Vec<usize> = hits.iter().copied().filter(|&l| l != lane).collect();
        for &l in &from_above {
            waiting[l] = None;
        }
        let through: Vec<usize> =
            waiting.iter().enumerate().filter(|(i, w)| w.is_some() && *i != lane).map(|(i, _)| i).collect();
        let mut to_parents = vec![];
        for (k, p) in c.parents.iter().enumerate() {
            if k == 0 {
                waiting[lane] = Some(p.clone());
                to_parents.push(lane);
            } else if let Some(l) = waiting.iter().position(|w| w.as_deref() == Some(p)) {
                to_parents.push(l);
            } else {
                let l = waiting.iter().position(Option::is_none).unwrap_or_else(|| {
                    waiting.push(None);
                    waiting.len() - 1
                });
                waiting[l] = Some(p.clone());
                to_parents.push(l);
            }
        }
        if c.parents.is_empty() {
            waiting[lane] = None;
        }
        while waiting.last().is_some_and(Option::is_none) {
            waiting.pop();
        }
        rows.push(Row { lane, through, to_parents, from_above });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(hash: &str, parents: &[&str]) -> Commit {
        Commit {
            hash: hash.into(),
            short: hash.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author: "a".into(),
            time: 0,
            subject: hash.into(),
            refs: String::new(),
        }
    }

    #[test]
    fn parses_log_and_name_status() {
        let out = "abc\x1fab\x1fp1 p2\x1fAli\x1f1700000000\x1fMerge x\x1fHEAD -> main\x1e\ndef\x1fde\x1f\x1fVali\x1f1\x1fInit\x1f\x1e";
        let v = parse_log(out);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].parents, ["p1", "p2"]);
        assert_eq!(v[0].refs, "HEAD -> main");
        assert!(v[1].parents.is_empty());
        let ns = parse_name_status("M\tsrc/a.rs\nR100\told.rs\tnew.rs\nA\tb.py\n");
        assert_eq!(ns[1], ('R', "new.rs".into(), "old.rs".into()));
        assert_eq!(ns[2].0, 'A');
    }

    /// m merges b into a: m(a, b), a -> base, b -> base.
    #[test]
    fn lanes_for_a_merge() {
        let log = [c("m", &["a", "b"]), c("a", &["base"]), c("b", &["base"]), c("base", &[])];
        let r = lanes(&log);
        assert_eq!(r[0].lane, 0);
        assert_eq!(r[0].to_parents, [0, 1]);
        assert_eq!((r[1].lane, r[1].through.clone()), (0, vec![1]));
        assert_eq!((r[2].lane, r[2].through.clone()), (1, vec![0]));
        // Both lanes meet at base.
        assert_eq!(r[3].lane, 0);
        assert_eq!(r[3].from_above, [1]);
    }

    /// Against a real repository: log, files of a commit, file at a commit.
    #[test]
    fn reads_a_repository() {
        let dir = std::env::temp_dir().join(format!("zenith-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git").args(args).current_dir(&dir).output().map(|o| o.status.success())
        };
        if !git(&["init", "-q"]).unwrap_or(false) {
            return;
        }
        let id = ["-c", "user.name=T", "-c", "user.email=t@e"];
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]).unwrap();
        git(&[&id[..], &["commit", "-qm", "first"]].concat()).unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        git(&["add", "-A"]).unwrap();
        git(&[&id[..], &["commit", "-qm", "second"]].concat()).unwrap();
        let g = crate::git::Git::open(&dir).unwrap();
        let run = g.runner();
        let log = log(&run, None, 50).unwrap();
        assert_eq!(log.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), ["second", "first"]);
        let files = changed_files(&run, None, &log[0].hash).unwrap();
        assert_eq!(files.iter().map(|f| (f.0, f.1.as_str())).collect::<Vec<_>>(), [('M', "a.txt"), ('A', "b.txt")]);
        assert_eq!(file_at(&run, &log[1].hash, "a.txt"), "one\n");
        assert_eq!(file_at(&run, &log[1].hash, "b.txt"), "");
        let first_files = changed_files(&run, None, &log[1].hash).unwrap();
        assert_eq!(first_files.len(), 1);
        let lh = line_history(&run, "a.txt", 1, 1, 10).unwrap();
        assert_eq!(lh.iter().map(|(c, _)| c.subject.as_str()).collect::<Vec<_>>(), ["second", "first"]);
        assert!(lh[0].1.contains("-one") && lh[0].1.contains("+two"), "{}", lh[0].1);
        let hist = super::log(&run, Some("b.txt"), 50).unwrap();
        assert_eq!(hist.len(), 1);
        assert!(message(&run, &log[0].hash).starts_with("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
