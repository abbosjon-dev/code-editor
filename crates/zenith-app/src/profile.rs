//! Profiles: where a program spends its time. Run configurations are
//! profiled with py-spy (Python) or perf (everything native); the samples
//! (collapsed stacks, or `perf script` output) become a flame graph —
//! callers above callees, width = time — plus the functions and lines that
//! took the most time themselves. Clicking a frame opens its code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, RichText, Sense};

use crate::theme::Palette;

/// One stack frame: a function, and where (when the profiler knows).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Frame {
    pub name: String,
    pub file: Option<String>,
    pub line: Option<u32>,
}

impl Frame {
    /// `func (file.py:12)` (py-spy) or a bare name (perf, inferno).
    pub fn parse(s: &str) -> Frame {
        let s = s.trim();
        if let Some(open) = s.rfind(" (") {
            let inner = s[open + 2..].trim_end_matches(')');
            if let Some((file, line)) = inner.rsplit_once(':') {
                if let Ok(line) = line.parse() {
                    return Frame { name: s[..open].to_string(), file: Some(file.to_string()), line: Some(line) };
                }
            }
        }
        Frame { name: s.to_string(), file: None, line: None }
    }

    /// Frames of one function merge in the graph (lines differ).
    fn key(&self) -> (String, Option<String>) {
        (self.name.clone(), self.file.clone())
    }
}

/// Collapsed stacks: `root;caller;callee count` per line.
pub fn parse_collapsed(text: &str) -> Vec<(Vec<Frame>, u64)> {
    text.lines()
        .filter_map(|l| {
            let (stack, n) = l.trim_end().rsplit_once(' ')?;
            let n: u64 = n.parse().ok()?;
            Some((stack.split(';').filter(|f| !f.is_empty()).map(Frame::parse).collect(), n))
        })
        .collect()
}

/// `perf script` output: per sample a header line, then frames (leaf
/// first) like `\t 55d0c3 my::func+0x12 (/path/bin)`, then a blank line.
pub fn parse_perf_script(text: &str) -> Vec<(Vec<Frame>, u64)> {
    let mut counts: HashMap<Vec<String>, u64> = HashMap::new();
    let mut comm = String::new();
    let mut frames: Vec<String> = vec![];
    let flush = |comm: &str, frames: &mut Vec<String>, counts: &mut HashMap<Vec<String>, u64>| {
        if !comm.is_empty() || !frames.is_empty() {
            let mut stack: Vec<String> = vec![comm.to_string()];
            stack.extend(frames.drain(..).rev());
            *counts.entry(stack).or_insert(0) += 1;
        }
    };
    for l in text.lines() {
        if l.trim().is_empty() {
            flush(&comm, &mut frames, &mut counts);
            comm.clear();
            continue;
        }
        if l.starts_with([' ', '\t']) {
            let mut parts = l.split_whitespace();
            parts.next(); // address
            let rest: Vec<&str> = parts.collect();
            let rest = rest.join(" ");
            // "func+0x12 (/lib.so)" → "func"
            let sym = rest.rsplit_once(" (").map_or(rest.as_str(), |(s, _)| s);
            let sym = sym.rsplit_once("+0x").map_or(sym, |(s, _)| s);
            if !sym.is_empty() && sym != "[unknown]" {
                frames.push(sym.to_string());
            }
        } else {
            comm = l.split_whitespace().next().unwrap_or("").to_string();
        }
    }
    flush(&comm, &mut frames, &mut counts);
    counts.into_iter().map(|(s, n)| (s.iter().map(|f| Frame::parse(f)).collect(), n)).collect()
}

/// A node of the call tree.
#[derive(Clone, Debug)]
pub struct Node {
    pub frame: Frame,
    pub total: u64,
    pub self_time: u64,
    pub children: Vec<usize>,
    pub depth: usize,
}

pub struct Profile {
    pub nodes: Vec<Node>,
    pub total: u64,
    /// Self time per function, most first.
    pub hot_functions: Vec<(Frame, u64)>,
    /// Self time per (file, line), most first.
    pub hot_lines: Vec<((String, u32), u64)>,
}

impl Profile {
    pub fn build(stacks: &[(Vec<Frame>, u64)]) -> Profile {
        let mut nodes = vec![Node {
            frame: Frame { name: "all".into(), file: None, line: None },
            total: 0,
            self_time: 0,
            children: vec![],
            depth: 0,
        }];
        let mut index: HashMap<(usize, (String, Option<String>)), usize> = HashMap::new();
        let mut self_fn: HashMap<(String, Option<String>), (Frame, u64)> = HashMap::new();
        let mut self_line: HashMap<(String, u32), u64> = HashMap::new();
        for (stack, n) in stacks {
            nodes[0].total += n;
            let mut at = 0;
            for f in stack {
                let key = (at, f.key());
                let next = match index.get(&key) {
                    Some(&i) => i,
                    None => {
                        let depth = nodes[at].depth + 1;
                        nodes.push(Node { frame: f.clone(), total: 0, self_time: 0, children: vec![], depth });
                        let i = nodes.len() - 1;
                        nodes[at].children.push(i);
                        index.insert(key, i);
                        i
                    }
                };
                nodes[next].total += n;
                // The first line seen points into the function.
                if nodes[next].frame.line.is_none() {
                    nodes[next].frame.line = f.line;
                }
                at = next;
            }
            nodes[at].self_time += n;
            if let Some(leaf) = stack.last() {
                self_fn.entry(leaf.key()).or_insert_with(|| (leaf.clone(), 0)).1 += n;
                if let (Some(file), Some(line)) = (&leaf.file, leaf.line) {
                    *self_line.entry((file.clone(), line)).or_insert(0) += n;
                }
            }
        }
        // Wider first, like most flame graphs.
        let totals: Vec<u64> = nodes.iter().map(|n| n.total).collect();
        for n in &mut nodes {
            n.children.sort_by_key(|c| std::cmp::Reverse(totals[*c]));
        }
        let mut hot_functions: Vec<(Frame, u64)> = self_fn.into_values().collect();
        hot_functions.sort_by_key(|f| std::cmp::Reverse(f.1));
        let mut hot_lines: Vec<((String, u32), u64)> = self_line.into_iter().collect();
        hot_lines.sort_by_key(|l| std::cmp::Reverse(l.1));
        let total = nodes[0].total;
        Profile { nodes, total, hot_functions, hot_lines }
    }

    pub fn load(path: &Path) -> Result<Profile, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let looks_perf = text.lines().find(|l| !l.trim().is_empty()).is_some_and(|l| {
            // perf script headers have a pid and a timestamp ("1234 12.345:").
            l.split_whitespace().any(|w| w.ends_with(':') && w.trim_end_matches(':').parse::<f64>().is_ok())
        });
        let stacks = if looks_perf { parse_perf_script(&text) } else { parse_collapsed(&text) };
        if stacks.is_empty() {
            return Err(format!("{}: no samples", path.display()));
        }
        Ok(Profile::build(&stacks))
    }
}

/// How a profile is shown.
#[derive(Debug, PartialEq)]
pub enum Output {
    /// A file the editor reads into its flame graph (relative to the run's folder).
    File(String),
    /// samply shows it in the Firefox Profiler in the browser.
    Browser,
}

/// The profiling command for a run command line: py-spy for Python (any
/// OS); perf on Linux (in-editor flame graph); samply elsewhere, or when
/// perf is missing (macOS, Windows, Linux — opens in the browser).
pub fn wrap(kind: crate::run::Kind, command: &str, out_stem: &str) -> Result<(String, Output), String> {
    let has = |t: &str| which::which(t).is_ok();
    pick(kind, command, out_stem, has("py-spy"), has("perf") && cfg!(target_os = "linux"), has("samply"))
}

fn pick(
    kind: crate::run::Kind,
    command: &str,
    out_stem: &str,
    py_spy: bool,
    perf: bool,
    samply: bool,
) -> Result<(String, Output), String> {
    let q = crate::remote::sh_quote;
    if kind == crate::run::Kind::Python {
        if !py_spy {
            return Err("Profiling Python needs py-spy: pip install py-spy".into());
        }
        let out = format!("{out_stem}.folded");
        // py-spy starts the program itself: its words follow `--`.
        return Ok((format!("py-spy record -r 200 -f raw -o {} -- {command}", q(&out)), Output::File(out)));
    }
    if perf {
        let data = format!("{out_stem}.data");
        let out = format!("{out_stem}.perf");
        return Ok((
            format!(
                "perf record -F 999 -g -o {} -- sh -c {} ; perf script -i {} > {}",
                q(&data),
                q(command),
                q(&data),
                q(&out)
            ),
            Output::File(out),
        ));
    }
    if samply {
        // samply records the program (and what it starts), then serves the
        // profile to the Firefox Profiler with symbols.
        return Ok((format!("samply record -- {command}"), Output::Browser));
    }
    Err("Profiling needs samply (cargo install samply — macOS, Windows, Linux) or perf on Linux; py-spy for Python"
        .into())
}

pub enum Action {
    Open(PathBuf, u32),
}

/// The profile window.
pub struct View {
    pub title: String,
    profile: Profile,
    /// Folder that relative frame paths are in.
    root: PathBuf,
    /// The node the graph is zoomed to.
    zoom: usize,
    search: String,
}

impl View {
    pub fn new(title: String, profile: Profile, root: PathBuf) -> Self {
        Self { title, profile, root, zoom: 0, search: String::new() }
    }

    fn path_of(&self, f: &Frame) -> Option<(PathBuf, u32)> {
        let file = f.file.as_ref()?;
        let p = Path::new(file);
        let full = if p.is_absolute() { p.to_path_buf() } else { self.root.join(p) };
        full.is_file().then(|| (full, f.line.unwrap_or(1)))
    }

    pub fn ui(&mut self, ctx: &egui::Context, p: &Palette, open: &mut bool) -> Option<Action> {
        let mut action = None;
        let screen = ctx.screen_rect();
        egui::Window::new(format!("Profile  {}", self.title))
            .id(egui::Id::new("zenith-profile"))
            .open(open)
            .default_size(screen.size() * egui::vec2(0.85, 0.75))
            .default_pos(screen.min + screen.size() * egui::vec2(0.075, 0.1))
            .collapsible(false)
            .show(ctx, |ui| {
                let total = self.profile.total.max(1);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} samples", self.profile.total)).color(p.text_dim));
                    if self.zoom != 0 && ui.small_button("⟲ Reset zoom").clicked() {
                        self.zoom = 0;
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("Highlight functions…")
                            .desired_width(220.0),
                    );
                });
                ui.separator();
                let avail = ui.available_size();
                let side_w = (avail.x * 0.3).clamp(220.0, 420.0);
                ui.horizontal_top(|ui| {
                    // --- flame graph ---
                    let top_down = egui::Layout::top_down(egui::Align::Min);
                    ui.allocate_ui_with_layout(egui::vec2(avail.x - side_w - 12.0, avail.y), top_down, |ui| {
                        egui::ScrollArea::vertical().id_salt("flame").auto_shrink([false, false]).show(ui, |ui| {
                            if let Some(a) = self.flame_ui(ui, p) {
                                action = Some(a);
                            }
                        });
                    });
                    ui.separator();
                    // --- hottest ---
                    ui.allocate_ui_with_layout(egui::vec2(side_w, avail.y), top_down, |ui| {
                        ui.set_max_width(side_w);
                        egui::ScrollArea::vertical().id_salt("hot").auto_shrink([false, false]).show(ui, |ui| {
                            ui.label(RichText::new("Most time in the function itself").small().strong());
                            for (f, n) in self.profile.hot_functions.iter().take(25) {
                                let pct = *n as f64 * 100.0 / total as f64;
                                let r = ui.add(
                                    egui::Label::new(
                                        RichText::new(format!("{pct:5.1}%  {}", f.name))
                                            .small()
                                            .monospace()
                                            .color(p.text),
                                    )
                                    .truncate()
                                    .sense(Sense::click()),
                                );
                                if r.on_hover_text(f.file.clone().unwrap_or_default()).clicked() {
                                    if let Some((path, line)) = self.path_of(f) {
                                        action = Some(Action::Open(path, line));
                                    }
                                }
                            }
                            if !self.profile.hot_lines.is_empty() {
                                ui.add_space(8.0);
                                ui.label(RichText::new("Hottest lines").small().strong());
                                for ((file, line), n) in self.profile.hot_lines.iter().take(25) {
                                    let pct = *n as f64 * 100.0 / total as f64;
                                    let name = file.rsplit(['/', '\\']).next().unwrap_or(file);
                                    let r = ui.add(
                                        egui::Label::new(
                                            RichText::new(format!("{pct:5.1}%  {name}:{line}"))
                                                .small()
                                                .monospace()
                                                .color(p.accent),
                                        )
                                        .sense(Sense::click()),
                                    );
                                    if r.clicked() {
                                        let f =
                                            Frame { name: String::new(), file: Some(file.clone()), line: Some(*line) };
                                        if let Some(target) = self.path_of(&f) {
                                            action = Some(Action::Open(target.0, target.1));
                                        }
                                    }
                                }
                            }
                        });
                    });
                });
            });
        action
    }

    /// Icicle graph (callers on top): the zoomed node spans the width.
    fn flame_ui(&mut self, ui: &mut egui::Ui, p: &Palette) -> Option<Action> {
        let mut action = None;
        let row_h = 18.0;
        let width = ui.available_width();
        let root = self.zoom;
        let root_depth = self.profile.nodes[root].depth;
        let root_total = self.profile.nodes[root].total.max(1) as f32;
        // Deepest level shown, for the height.
        let mut max_depth = 0;
        let mut stack = vec![root];
        while let Some(i) = stack.pop() {
            max_depth = max_depth.max(self.profile.nodes[i].depth - root_depth);
            stack.extend(self.profile.nodes[i].children.iter().copied());
        }
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, (max_depth + 1) as f32 * row_h), Sense::hover());
        let painter = ui.painter_at(rect);
        let search = self.search.to_lowercase();
        let font = egui::FontId::proportional(11.5);
        let mut todo = vec![(root, rect.min.x)];
        let mut zoom_to = None;
        while let Some((i, x)) = todo.pop() {
            let n = &self.profile.nodes[i];
            let w = n.total as f32 / root_total * width;
            if w < 1.0 {
                continue;
            }
            let y = rect.min.y + (n.depth - root_depth) as f32 * row_h;
            let r = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w - 1.0, row_h - 1.0));
            let matched = !search.is_empty() && n.frame.name.to_lowercase().contains(&search);
            let color = if matched { p.accent } else { frame_color(&n.frame.name, i == 0) };
            let resp = ui.interact(r, ui.id().with(("flame", i)), Sense::click());
            painter.rect_filled(r, 2.0, if resp.hovered() { color.gamma_multiply(1.25) } else { color });
            if w > 30.0 {
                let label = fit(&n.frame.name, ((w - 6.0) / 6.5) as usize);
                painter.text(
                    r.left_center() + egui::vec2(3.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    label,
                    font.clone(),
                    Color32::from_gray(20),
                );
            }
            let pct = n.total as f64 * 100.0 / self.profile.total.max(1) as f64;
            let self_pct = n.self_time as f64 * 100.0 / self.profile.total.max(1) as f64;
            let resp = resp.on_hover_text(format!(
                "{}\n{}{} samples · {pct:.1}% (itself {self_pct:.1}%)\nClick: zoom · double-click: open the code",
                n.frame.name,
                n.frame.file.as_ref().map(|f| format!("{f}:{}\n", n.frame.line.unwrap_or(0))).unwrap_or_default(),
                n.total
            ));
            if resp.double_clicked() {
                if let Some((path, line)) = self.path_of(&n.frame) {
                    action = Some(Action::Open(path, line));
                }
            } else if resp.clicked() {
                zoom_to = Some(i);
            }
            let mut cx = x;
            for &c in &n.children {
                todo.push((c, cx));
                cx += self.profile.nodes[c].total as f32 / root_total * width;
            }
        }
        if let Some(z) = zoom_to {
            self.zoom = z;
        }
        action
    }
}

/// Warm colors, stable per function (like the classic flame graph).
fn frame_color(name: &str, root: bool) -> Color32 {
    if root {
        return Color32::from_rgb(0x9a, 0x9a, 0xb0);
    }
    let mut h: u32 = 2166136261;
    for b in name.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    let r = 205 + (h % 50) as u8;
    let g = 90 + ((h >> 8) % 120) as u8;
    let b = 40 + ((h >> 16) % 50) as u8;
    Color32::from_rgb(r, g, b)
}

fn fit(s: &str, chars: usize) -> String {
    if s.chars().count() <= chars {
        s.to_string()
    } else if chars > 2 {
        format!("{}…", s.chars().take(chars - 1).collect::<String>())
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PYSPY: &str = "<module> (main.py:25);main (main.py:20);parse_numbers (main.py:12) 42\n\
<module> (main.py:25);main (main.py:19);slow_sum (main.py:6) 81\n\
<module> (main.py:25);main (main.py:20);parse_numbers (main.py:13) 126\n\
<module> (main.py:25);main (main.py:19);slow_sum (main.py:7) 209\n";

    #[test]
    fn builds_a_tree_from_py_spy() {
        let s = parse_collapsed(PYSPY);
        assert_eq!(s[0].0[2], Frame { name: "parse_numbers".into(), file: Some("main.py".into()), line: Some(12) });
        let p = Profile::build(&s);
        assert_eq!(p.total, 458);
        // <module> → main → {slow_sum, parse_numbers}: lines merged per function.
        let main = p.nodes.iter().find(|n| n.frame.name == "main").unwrap();
        assert_eq!(main.total, 458);
        assert_eq!(main.children.len(), 2);
        assert_eq!(p.nodes[main.children[0]].frame.name, "slow_sum"); // widest first
        assert_eq!(p.hot_functions[0], (p.nodes[main.children[0]].frame.clone(), 290));
        assert_eq!(p.hot_lines[0], (("main.py".into(), 7), 209));
    }

    #[test]
    fn parses_perf_script() {
        let text = "myapp  1234 100.000001:     1001 cycles:\n\
\t    55d0c3 work::hot+0x12 (/w/target/release/myapp)\n\
\t    55d0a0 main+0x30 (/w/target/release/myapp)\n\
\t    7f00aa __libc_start_main+0xf0 (/usr/lib/libc.so.6)\n\
\n\
myapp  1234 100.001001:     1001 cycles:\n\
\t    55d0c3 work::hot+0x12 (/w/target/release/myapp)\n\
\t    55d0a0 main+0x30 (/w/target/release/myapp)\n\
\t    7f00aa __libc_start_main+0xf0 (/usr/lib/libc.so.6)\n\
\n";
        let s = parse_perf_script(text);
        assert_eq!(s.len(), 1);
        let names: Vec<&str> = s[0].0.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["myapp", "__libc_start_main", "main", "work::hot"]);
        assert_eq!(s[0].1, 2);
    }

    #[test]
    fn picks_a_profiler() {
        use crate::run::Kind;
        let (cmd, out) = pick(Kind::Python, "python3 main.py", ".zenith/p", true, false, false).unwrap();
        assert_eq!(cmd, "py-spy record -r 200 -f raw -o .zenith/p.folded -- python3 main.py");
        assert_eq!(out, Output::File(".zenith/p.folded".into()));
        assert!(pick(Kind::Python, "python3 main.py", "p", false, true, true).is_err());
        let (cmd, out) = pick(Kind::Cargo, "cargo run --release", "p", false, true, true).unwrap();
        assert!(cmd.starts_with("perf record") && out == Output::File("p.perf".into()));
        let (cmd, out) = pick(Kind::Cargo, "cargo run --release", "p", false, false, true).unwrap();
        assert_eq!((cmd.as_str(), out), ("samply record -- cargo run --release", Output::Browser));
        assert!(pick(Kind::Go, "go run .", "p", false, false, false).unwrap_err().contains("samply"));
    }
}
