//! Databases: SQLite, PostgreSQL and MySQL / MariaDB through their own
//! command-line clients (sqlite3, psql, mysql), so no drivers are bundled
//! and it works on the SSH host too. Connections come from
//! `.zenith/databases.toml`, `DATABASE_URL` in the project's env files and
//! SQLite files in the folder. SQL is sent on stdin; results are read with
//! separators that can't appear in normal text.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::remote::RemoteSpec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Sqlite,
    Postgres,
    Mysql,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    pub name: String,
    /// `postgres://…`, `mysql://…`, `sqlite:path` or a path to a SQLite file.
    pub url: String,
}

impl Connection {
    pub fn kind(&self) -> Option<Kind> {
        let u = self.url.trim();
        if u.starts_with("postgres://") || u.starts_with("postgresql://") {
            Some(Kind::Postgres)
        } else if u.starts_with("mysql://") || u.starts_with("mariadb://") {
            Some(Kind::Mysql)
        } else if u.starts_with("sqlite:") || [".db", ".sqlite", ".sqlite3", ".db3"].iter().any(|e| u.ends_with(e)) {
            Some(Kind::Sqlite)
        } else {
            None
        }
    }

    /// The URL with its password hidden (for showing).
    pub fn display_url(&self) -> String {
        let u = &self.url;
        let Some((scheme, rest)) = u.split_once("://") else { return u.clone() };
        match rest.split_once('@') {
            Some((cred, host)) => match cred.split_once(':') {
                Some((user, _)) => format!("{scheme}://{user}:•••@{host}"),
                None => u.clone(),
            },
            None => u.clone(),
        }
    }
}

const FS: char = '\u{1f}';
const RS: char = '\u{1e}';

/// A client to run: program, arguments, environment.
pub type ClientCommand = (String, Vec<String>, Vec<(String, String)>);

/// The client command for a connection.
pub fn client(c: &Connection, root: &Path) -> Result<ClientCommand, String> {
    let fs = FS.to_string();
    let rs = RS.to_string();
    match c.kind() {
        Some(Kind::Sqlite) => {
            let p = c.url.trim().trim_start_matches("sqlite://").trim_start_matches("sqlite:");
            let path = if Path::new(p).is_absolute() { PathBuf::from(p) } else { root.join(p) };
            Ok((
                "sqlite3".into(),
                vec![
                    "-batch".into(),
                    "-bail".into(),
                    "-header".into(),
                    "-separator".into(),
                    fs,
                    "-newline".into(),
                    rs,
                    "-nullvalue".into(),
                    "NULL".into(),
                    path.to_string_lossy().into_owned(),
                ],
                vec![],
            ))
        }
        Some(Kind::Postgres) => Ok((
            "psql".into(),
            vec![
                c.url.clone(),
                "-X".into(),
                "-q".into(),
                "-A".into(),
                "-F".into(),
                fs,
                "-R".into(),
                rs,
                "-P".into(),
                "footer=off".into(),
                "-P".into(),
                "null=NULL".into(),
                "-v".into(),
                "ON_ERROR_STOP=1".into(),
                "-f".into(),
                "-".into(),
            ],
            vec![],
        )),
        Some(Kind::Mysql) => {
            let rest = c.url.split_once("://").map_or("", |(_, r)| r);
            let (cred, hostdb) = rest.rsplit_once('@').unwrap_or(("", rest));
            let (user, pass) = cred.split_once(':').unwrap_or((cred, ""));
            let (hostport, db) = hostdb.split_once('/').unwrap_or((hostdb, ""));
            let db = db.split('?').next().unwrap_or("");
            let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "3306"));
            let mut args =
                vec!["--batch".into(), "--column-names".into(), "-h".into(), host.into(), "-P".into(), port.into()];
            if !user.is_empty() {
                args.extend(["-u".into(), percent_decode(user)]);
            }
            if !db.is_empty() {
                args.push(db.into());
            }
            // The password goes in the environment, not on the command line.
            let env = if pass.is_empty() { vec![] } else { vec![("MYSQL_PWD".into(), percent_decode(pass))] };
            Ok(("mysql".into(), args, env))
        }
        None => Err(format!("Unknown database URL: {}", c.display_url())),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// SQL that lists the tables and views.
pub fn tables_sql(k: Kind) -> &'static str {
    match k {
        Kind::Sqlite => "SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name;",
        Kind::Postgres => {
            "SELECT CASE WHEN table_schema = 'public' THEN table_name ELSE table_schema || '.' || table_name END FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog','information_schema') ORDER BY table_schema <> 'public', 1;"
        }
        Kind::Mysql => "SHOW TABLES;",
    }
}

/// A table name quoted for `k`.
pub fn quote_ident(k: Kind, name: &str) -> String {
    match k {
        Kind::Mysql => name.split('.').map(|p| format!("`{}`", p.replace('`', "``"))).collect::<Vec<_>>().join("."),
        _ => name.split('.').map(|p| format!("\"{}\"", p.replace('"', "\"\""))).collect::<Vec<_>>().join("."),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub error: Option<String>,
    pub millis: u128,
    pub sql: String,
    /// More rows came back than are kept.
    pub truncated: bool,
}

/// Most rows kept from one result.
pub const MAX_ROWS: usize = 2000;

/// Client output → columns and rows.
pub fn parse_output(k: Kind, out: &str) -> (Vec<String>, Vec<Vec<String>>, bool) {
    let mut records: Vec<Vec<String>> = match k {
        // mysql --batch: tab-separated lines, \t \n \\ escaped.
        Kind::Mysql => out
            .lines()
            .map(|l| {
                l.split('\t')
                    .map(|f| f.replace("\\n", "\n").replace("\\t", "\t").replace("\\0", "\0").replace("\\\\", "\\"))
                    .collect()
            })
            .collect(),
        _ => out
            .trim_end_matches(['\n', RS])
            .split(RS)
            .map(|r| r.trim_start_matches('\n'))
            .filter(|r| !r.is_empty())
            .map(|r| r.split(FS).map(str::to_string).collect())
            .collect(),
    };
    if records.is_empty() {
        return (vec![], vec![], false);
    }
    let columns = records.remove(0);
    let truncated = records.len() > MAX_ROWS;
    records.truncate(MAX_ROWS);
    (columns, records, truncated)
}

/// Run `sql` on `c` (locally or on the SSH host), on a thread.
pub fn query(c: &Connection, root: &Path, remote: Option<RemoteSpec>, sql: String) -> Receiver<QueryResult> {
    let (tx, rx) = channel();
    let (c, root) = (c.clone(), root.to_path_buf());
    std::thread::spawn(move || {
        let started = Instant::now();
        let mut r = QueryResult { sql: sql.clone(), ..Default::default() };
        match client(&c, &root).and_then(|cmd| run(cmd, remote.as_ref(), &root, &sql)) {
            Ok(out) => {
                let k = c.kind().unwrap_or(Kind::Sqlite);
                let (cols, rows, truncated) = parse_output(k, &out);
                r.columns = cols;
                r.rows = rows;
                r.truncated = truncated;
            }
            Err(e) => r.error = Some(e),
        }
        r.millis = started.elapsed().as_millis();
        let _ = tx.send(r);
    });
    rx
}

fn run(
    (program, args, env): ClientCommand,
    remote: Option<&RemoteSpec>,
    root: &Path,
    sql: &str,
) -> Result<String, String> {
    use std::io::Write;
    let mut cmd = match remote {
        Some(r) => {
            let envs: String = env.iter().map(|(k, v)| format!("{k}={} ", crate::remote::sh_quote(v))).collect();
            let line: Vec<String> = std::iter::once(program.clone())
                .chain(args.iter().cloned())
                .map(|a| crate::remote::sh_quote(&a))
                .collect();
            let mut c = std::process::Command::new("ssh");
            c.args(crate::remote::ssh_args(r, false));
            c.arg(format!(
                "cd {} && {envs}exec \"${{SHELL:-sh}}\" -lc {}",
                r.qpath(),
                crate::remote::sh_quote(&line.join(" "))
            ));
            c
        }
        None => {
            let mut c = std::process::Command::new(&program);
            c.args(&args).envs(env).current_dir(root);
            c
        }
    };
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("`{program}` was not found — install the {program} client")
        } else {
            format!("{program}: {e}")
        }
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(sql.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() || stderr.to_lowercase().contains("error") {
        return Err(if stderr.is_empty() { format!("{program} failed") } else { stderr });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Deserialize, Serialize, Default)]
struct File {
    #[serde(default)]
    connections: Vec<Connection>,
}

pub fn config_path(root: &Path) -> PathBuf {
    root.join(".zenith").join("databases.toml")
}

/// Saved connections, then `DATABASE_URL`s from env files, then SQLite
/// files in the folder (two levels deep).
pub fn connections(root: &Path) -> Vec<Connection> {
    let mut out: Vec<Connection> = std::fs::read_to_string(config_path(root))
        .ok()
        .and_then(|t| toml::from_str::<File>(&t).ok())
        .map(|f| f.connections)
        .unwrap_or_default();
    for f in crate::run::env_files(root) {
        let Ok(t) = std::fs::read_to_string(root.join(&f)) else { continue };
        for (k, v) in crate::run::parse_env_file(&t) {
            let c = Connection { name: format!("{k} ({f})"), url: v };
            if k.ends_with("DATABASE_URL") && c.kind().is_some() && !out.iter().any(|o| o.url == c.url) {
                out.push(c);
            }
        }
    }
    let mut files = vec![];
    let walk = ignore::WalkBuilder::new(root).max_depth(Some(3)).build();
    for e in walk.flatten() {
        let p = e.path();
        if p.is_file()
            && p.extension().is_some_and(|x| ["db", "sqlite", "sqlite3", "db3"].contains(&x.to_str().unwrap_or("")))
        {
            // SQLite files start with this header.
            let mut head = [0u8; 16];
            let ok = std::fs::File::open(p).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head)).is_ok();
            if ok && &head == b"SQLite format 3\0" {
                files.push(p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/"));
            }
        }
    }
    files.sort();
    for f in files {
        if !out.iter().any(|o| o.url.ends_with(&f)) {
            out.push(Connection { name: f.clone(), url: format!("sqlite:{f}") });
        }
    }
    out
}

/// Add a connection to `.zenith/databases.toml`.
pub fn save_connection(root: &Path, c: Connection) -> std::io::Result<()> {
    let path = config_path(root);
    let mut f: File = std::fs::read_to_string(&path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default();
    f.connections.retain(|o| o.name != c.name);
    f.connections.push(c);
    std::fs::create_dir_all(path.parent().unwrap_or(root))?;
    std::fs::write(&path, toml::to_string_pretty(&f).unwrap_or_default())
}

/// Whether `s` has SQL besides comments and whitespace.
fn has_sql(s: &str) -> bool {
    s.lines().map(|l| l.split("--").next().unwrap_or("").trim()).any(|l| !l.is_empty() && l != ";")
}

/// The statement to run: the selection, else the one around the caret
/// (statements end with `;`; quotes are respected).
pub fn statement_at(text: &str, caret: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut bounds = vec![0];
    let mut quote: Option<char> = None;
    for (i, &c) in chars.iter().enumerate() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, ';') => bounds.push(i + 1),
            _ => {}
        }
    }
    bounds.push(chars.len());
    let caret = caret.min(chars.len());
    for w in bounds.windows(2) {
        let (a, b) = (w[0], w[1]);
        let s: String = chars[a..b].iter().collect();
        // The caret right after a `;` still means that statement.
        if caret <= b && (caret > a || a == 0) && has_sql(&s) {
            return s.trim().to_string();
        }
    }
    // Caret on an empty part (after the last `;`): the last statement.
    bounds
        .windows(2)
        .rev()
        .map(|w| chars[w[0]..w[1]].iter().collect::<String>())
        .find(|s| has_sql(s))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_clients() {
        let pg = Connection { name: "a".into(), url: "postgres://zen:s3cret@localhost:5432/shop".into() };
        assert_eq!(pg.kind(), Some(Kind::Postgres));
        assert_eq!(pg.display_url(), "postgres://zen:•••@localhost:5432/shop");
        let my = Connection { name: "b".into(), url: "mysql://root:p%40ss@db.local:3307/app".into() };
        let (prog, args, env) = client(&my, Path::new("/w")).unwrap();
        assert_eq!(prog, "mysql");
        assert!(args.windows(2).any(|w| w == ["-h", "db.local"]) && args.windows(2).any(|w| w == ["-P", "3307"]));
        assert_eq!(args.last().unwrap(), "app");
        assert_eq!(env, [("MYSQL_PWD".to_string(), "p@ss".to_string())]);
        assert!(!args.iter().any(|a| a.contains("ss")));
        let sq = Connection { name: "c".into(), url: "sqlite:data/app.db".into() };
        let expected = Path::new("/w").join("data/app.db").to_string_lossy().into_owned();
        assert_eq!(client(&sq, Path::new("/w")).unwrap().1.last().unwrap(), &expected);
        assert_eq!(quote_ident(Kind::Postgres, "audit.log\"x"), "\"audit\".\"log\"\"x\"");
        assert_eq!(quote_ident(Kind::Mysql, "users"), "`users`");
    }

    #[test]
    fn parses_outputs() {
        let out = format!("id{FS}name{FS}note{RS}1{FS}Olma{FS}NULL{RS}2{FS}Nok{FS}two\nlines{RS}");
        let (c, r, _) = parse_output(Kind::Postgres, &out);
        assert_eq!(c, ["id", "name", "note"]);
        assert_eq!(r[1], ["2", "Nok", "two\nlines"]);
        let (c, r, _) = parse_output(Kind::Mysql, "id\tname\n1\ta\\tb\n2\tc\\nd\n");
        assert_eq!(c, ["id", "name"]);
        assert_eq!(r[0][1], "a\tb");
        assert_eq!(r[1][1], "c\nd");
        assert_eq!(parse_output(Kind::Sqlite, ""), (vec![], vec![], false));
    }

    #[test]
    fn statements_under_the_caret() {
        let sql = "SELECT 1;\nSELECT ';' AS x;\n\nUPDATE t SET a = 2";
        assert_eq!(statement_at(sql, 3), "SELECT 1;");
        assert_eq!(statement_at(sql, 9), "SELECT 1;"); // right after the `;`
        assert_eq!(statement_at(sql, 14), "SELECT ';' AS x;");
        assert_eq!(statement_at(sql, sql.len()), "UPDATE t SET a = 2");
        assert_eq!(statement_at("SELECT 2;\n\n", 11), "SELECT 2;");
        assert_eq!(statement_at("-- db\n", 3), "");
        assert_eq!(statement_at("-- db\nSELECT 3;\n", 2), "-- db\nSELECT 3;");
    }

    #[test]
    fn sqlite_end_to_end() {
        if which::which("sqlite3").is_err() {
            return;
        }
        let d = std::env::temp_dir().join(format!("zenith-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let c = Connection { name: "t".into(), url: "sqlite:t.db".into() };
        let r = query(
            &c,
            &d,
            None,
            "CREATE TABLE p(id INTEGER, name TEXT); INSERT INTO p VALUES (1,'a|b'),(2,NULL);".into(),
        )
        .recv()
        .unwrap();
        assert_eq!(r.error, None);
        let r = query(&c, &d, None, "SELECT * FROM p ORDER BY id;".into()).recv().unwrap();
        assert_eq!(r.columns, ["id", "name"]);
        assert_eq!(r.rows, [vec!["1", "a|b"], vec!["2", "NULL"]]);
        let r = query(&c, &d, None, tables_sql(Kind::Sqlite).into()).recv().unwrap();
        assert_eq!(r.rows, [vec!["p"]]);
        let r = query(&c, &d, None, "SELECT * FROM nope;".into()).recv().unwrap();
        assert!(r.error.unwrap().contains("no such table"));
        assert_eq!(connections(&d).iter().map(|c| c.url.as_str()).collect::<Vec<_>>(), ["sqlite:t.db"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}

// ------------------------------------------------------------------ results panel

pub enum Action {
    Close,
    OpenText(String),
}

/// Query results next to the code.
#[derive(Default)]
pub struct Results {
    pub history: Vec<(String, QueryResult)>,
    pub shown: usize,
    pub pending: Option<(String, String, Receiver<QueryResult>)>,
}

impl Results {
    pub fn start(&mut self, conn: &Connection, root: &Path, remote: Option<RemoteSpec>, sql: String) {
        let rx = query(conn, root, remote, sql.clone());
        self.pending = Some((conn.name.clone(), sql, rx));
    }

    pub fn poll(&mut self) {
        let Some((name, _, rx)) = &self.pending else { return };
        if let Ok(r) = rx.try_recv() {
            self.history.insert(0, (name.clone(), r));
            self.history.truncate(20);
            self.shown = 0;
            self.pending = None;
        }
    }

    pub fn ui(&mut self, ui: &mut eframe::egui::Ui, p: &crate::theme::Palette, font_size: f32) -> Option<Action> {
        use eframe::egui::{self, RichText};
        let mut action = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("QUERY RESULTS").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").clicked() {
                    action = Some(Action::Close);
                }
                if self.history.len() > 1 {
                    let label = |(n, r): &(String, QueryResult)| {
                        let s: String = r.sql.split_whitespace().collect::<Vec<_>>().join(" ");
                        let s = if s.chars().count() > 40 {
                            format!("{}…", s.chars().take(39).collect::<String>())
                        } else {
                            s
                        };
                        format!("{n}: {s}")
                    };
                    egui::ComboBox::from_id_salt("db-history")
                        .width(ui.available_width().min(300.0))
                        .selected_text(self.history.get(self.shown).map(label).unwrap_or_default())
                        .show_ui(ui, |ui| {
                            for (i, h) in self.history.iter().enumerate() {
                                ui.selectable_value(&mut self.shown, i, label(h));
                            }
                        });
                }
            });
        });
        if let Some((name, sql, _)) = &self.pending {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new(format!("{name}: {}", sql.lines().next().unwrap_or_default()))
                        .small()
                        .color(p.text_dim),
                );
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let Some((_, r)) = self.history.get(self.shown) else {
            if self.pending.is_none() {
                ui.label(
                    RichText::new(
                        "Pick a table, or put the caret in a statement of a .sql file and press Ctrl+Alt+Enter.",
                    )
                    .color(p.text_dim),
                );
            }
            return action;
        };
        if let Some(e) = &r.error {
            ui.label(RichText::new(e).color(p.error).monospace());
            return action;
        }
        ui.horizontal(|ui| {
            let more = if r.truncated { format!(" (first {MAX_ROWS})") } else { String::new() };
            let what =
                if r.columns.is_empty() { "Done, no rows".to_string() } else { format!("{} rows{more}", r.rows.len()) };
            ui.label(RichText::new(what).color(p.success));
            ui.label(RichText::new(format!("{} ms", r.millis)).small().color(p.text_faint));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !r.columns.is_empty() {
                    if ui.small_button("Open as CSV").clicked() {
                        action = Some(Action::OpenText(to_csv(r)));
                    }
                    if ui.small_button("Copy").on_hover_text("Copy as tab-separated text").clicked() {
                        let mut t = r.columns.join("\t");
                        for row in &r.rows {
                            t.push('\n');
                            t.push_str(&row.join("\t"));
                        }
                        ui.ctx().copy_text(t);
                    }
                }
            });
        });
        ui.separator();
        if r.columns.is_empty() {
            return action;
        }
        let font = egui::FontId::monospace(font_size * 0.88);
        egui::ScrollArea::both().id_salt(("db-grid", self.shown)).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(("db-table", self.shown)).striped(true).spacing([14.0, 3.0]).show(ui, |ui| {
                for c in &r.columns {
                    ui.label(RichText::new(c).font(font.clone()).strong().color(p.accent));
                }
                ui.end_row();
                for row in &r.rows {
                    for v in row {
                        let one_line = v.replace('\n', "⏎");
                        let shown = if one_line.chars().count() > 60 {
                            format!("{}…", one_line.chars().take(59).collect::<String>())
                        } else {
                            one_line
                        };
                        let text = RichText::new(shown).font(font.clone());
                        let text = if v == "NULL" { text.color(p.text_faint).italics() } else { text };
                        let resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
                        if v.chars().count() > 60 || v.contains('\n') {
                            resp.clone().on_hover_text(v);
                        }
                        if resp.clicked() {
                            ui.ctx().copy_text(v.clone());
                        }
                    }
                    ui.end_row();
                }
            });
        });
        action
    }
}

fn to_csv(r: &QueryResult) -> String {
    let q = |v: &str| {
        if v.contains([',', '"', '\n']) {
            format!("\"{}\"", v.replace('"', "\"\""))
        } else {
            v.to_string()
        }
    };
    let mut out = r.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(",");
    for row in &r.rows {
        out.push('\n');
        out.push_str(&row.iter().map(|v| q(v)).collect::<Vec<_>>().join(","));
    }
    out
}
