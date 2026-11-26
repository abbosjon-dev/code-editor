//! Device screen in the editor: an Android phone or emulator (through
//! `adb`: screenshots, taps, swipes, keys, text) or an iOS simulator
//! (`xcrun simctl`: screenshots only — Apple has no input API). Works with
//! real devices too, and on the SSH host when the device is attached there.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use crate::remote::RemoteSpec;
use crate::theme::Palette;

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Android { serial: String },
    Ios { udid: String },
}

/// Something to do on the device (device pixels).
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Tap(u32, u32),
    Swipe(u32, u32, u32, u32, u32),
    Key(&'static str),
    Text(String),
}

/// The `adb` arguments for an input (after `-s serial`).
pub fn adb_input_args(i: &Input) -> Vec<String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match i {
        Input::Tap(x, y) => s(&["shell", "input", "tap", &x.to_string(), &y.to_string()]),
        Input::Swipe(a, b, c, d, ms) => s(&[
            "shell",
            "input",
            "swipe",
            &a.to_string(),
            &b.to_string(),
            &c.to_string(),
            &d.to_string(),
            &ms.to_string(),
        ]),
        Input::Key(k) => s(&["shell", "input", "keyevent", k]),
        // `input text` needs spaces as %s and shell characters escaped.
        Input::Text(t) => {
            let mut e = String::new();
            for c in t.chars() {
                match c {
                    ' ' => e.push_str("%s"),
                    c if c.is_ascii_alphanumeric() || ".,-_@/:".contains(c) => e.push(c),
                    c => {
                        e.push('\\');
                        e.push(c);
                    }
                }
            }
            vec!["shell".into(), "input".into(), "text".into(), e]
        }
    }
}

/// Run a program (locally or on the SSH host) and return its raw output.
fn run_bytes(remote: Option<&RemoteSpec>, program: &str, args: &[String]) -> Result<Vec<u8>, String> {
    let mut cmd = match remote {
        Some(r) => {
            let line: Vec<String> = std::iter::once(program.to_string())
                .chain(args.iter().cloned())
                .map(|a| crate::remote::sh_quote(&a))
                .collect();
            let mut c = std::process::Command::new("ssh");
            c.args(crate::remote::ssh_args(r, false));
            c.arg(format!("exec \"${{SHELL:-sh}}\" -lc {}", crate::remote::sh_quote(&line.join(" "))));
            c
        }
        None => {
            let mut c = std::process::Command::new(program);
            c.args(args);
            c
        }
    };
    cmd.stdin(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
    let out = cmd.output().map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().last().unwrap_or("failed").trim().to_string());
    }
    Ok(out.stdout)
}

/// One screenshot of the device.
fn capture(target: &Target, remote: Option<&RemoteSpec>, raw_ok: &AtomicBool) -> Result<egui::ColorImage, String> {
    match target {
        Target::Android { serial } => {
            if raw_ok.load(Ordering::Relaxed) {
                let data = run_bytes(remote, "adb", &["-s", serial, "exec-out", "screencap"].map(String::from))?;
                match parse_raw(&data) {
                    Some(img) => return Ok(img),
                    None => raw_ok.store(false, Ordering::Relaxed),
                }
            }
            let png = run_bytes(remote, "adb", &["-s", serial, "exec-out", "screencap", "-p"].map(String::from))?;
            decode_png(&png)
        }
        Target::Ios { udid } => {
            let png = run_bytes(
                remote,
                "xcrun",
                &["simctl", "io", udid, "screenshot", "--type=png", "/dev/stdout"].map(String::from),
            )?;
            decode_png(&png)
        }
    }
}

fn decode_png(png: &[u8]) -> Result<egui::ColorImage, String> {
    let img = image::load_from_memory(png).map_err(|e| format!("screenshot: {e}"))?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}

/// `screencap` without `-p`: width, height, pixel format (u32 LE) and, on
/// newer Androids, a color space — then the pixels.
pub fn parse_raw(data: &[u8]) -> Option<egui::ColorImage> {
    let u = |i: usize| -> Option<u32> { Some(u32::from_le_bytes(data.get(i..i + 4)?.try_into().ok()?)) };
    let (w, h, format) = (u(0)? as usize, u(4)? as usize, u(8)?);
    if w == 0 || h == 0 || w > 10_000 || h > 10_000 {
        return None;
    }
    let px = w * h * 4;
    let header = [16, 12].into_iter().find(|hd| data.len() == hd + px)?;
    let pixels = &data[header..];
    let rgba: Vec<u8> = match format {
        // RGBA_8888, RGBX_8888 (alpha unused)
        1 | 2 => pixels.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        // BGRA_8888
        5 => pixels.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect(),
        _ => return None,
    };
    Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

/// `scrcpy` (a fast, full-frame-rate mirror with audio) for this device,
/// when it is installed here.
pub fn scrcpy_available() -> bool {
    which::which("scrcpy").is_ok()
}

pub struct Mirror {
    pub target: Target,
    pub name: String,
    frames: Receiver<(Instant, Result<egui::ColorImage, String>)>,
    /// When the frame on screen was taken.
    last_taken: Option<Instant>,
    /// The device is attached to the SSH host (scrcpy would need it here).
    remote: bool,
    input: Sender<Input>,
    stop: Arc<AtomicBool>,
    texture: Option<egui::TextureHandle>,
    /// Device pixels.
    size: [usize; 2],
    error: Option<String>,
    text: String,
    /// Frames shown in the last second (for the fps label).
    times: Vec<Instant>,
    drag_from: Option<(egui::Pos2, Instant)>,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Mirror {
    pub fn start(target: Target, name: String, remote: Option<RemoteSpec>, ctx: egui::Context) -> Self {
        let is_remote = remote.is_some();
        // Two captures in flight (a device answers one while the other
        // travels); frames carry when they were taken so an older one that
        // arrives late is dropped.
        let (ftx, frx) = sync_channel::<(Instant, Result<egui::ColorImage, String>)>(2);
        let (itx, irx) = channel::<Input>();
        let stop = Arc::new(AtomicBool::new(false));
        // Raw screenshots skip PNG compression on the phone (much faster);
        // devices that don't support them fall back to PNG for good.
        let raw_ok = Arc::new(AtomicBool::new(matches!(target, Target::Android { .. })));
        for n in 0..2u64 {
            let (target, remote, stop, ftx, ctx, raw_ok) =
                (target.clone(), remote.clone(), stop.clone(), ftx.clone(), ctx.clone(), raw_ok.clone());
            std::thread::spawn(move || {
                // Stagger the second worker by half a frame.
                std::thread::sleep(Duration::from_millis(n * 60));
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    let frame = capture(&target, remote.as_ref(), &raw_ok);
                    let failed = frame.is_err();
                    if ftx.send((started, frame)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                    // Up to ~15 fps per worker; back off while it fails
                    // (unplugged, still booting).
                    let min = if failed { Duration::from_secs(2) } else { Duration::from_millis(66) };
                    if let Some(rest) = min.checked_sub(started.elapsed()) {
                        std::thread::sleep(rest);
                    }
                }
            });
        }
        if let Target::Android { serial } = &target {
            let (serial, remote) = (serial.clone(), remote);
            std::thread::spawn(move || {
                for i in irx {
                    let mut args = vec!["-s".to_string(), serial.clone()];
                    args.extend(adb_input_args(&i));
                    if let Err(e) = run_bytes(remote.as_ref(), "adb", &args) {
                        log::warn!("adb input: {e}");
                    }
                }
            });
        }
        Self {
            target,
            name,
            frames: frx,
            input: itx,
            stop,
            texture: None,
            size: [0, 0],
            error: None,
            text: String::new(),
            last_taken: None,
            remote: is_remote,
            times: vec![],
            drag_from: None,
        }
    }

    fn interactive(&self) -> bool {
        matches!(self.target, Target::Android { .. })
    }

    fn send(&self, i: Input) {
        let _ = self.input.send(i);
    }

    /// Toolbar and the screen; returns true when closed.
    pub fn ui(&mut self, ui: &mut egui::Ui, p: &Palette) -> bool {
        let mut close = false;
        while let Ok((taken, f)) = self.frames.try_recv() {
            if self.last_taken.is_some_and(|t| taken < t) {
                continue;
            }
            self.last_taken = Some(taken);
            match f {
                Ok(img) => {
                    self.size = img.size;
                    self.error = None;
                    self.times.push(Instant::now());
                    match &mut self.texture {
                        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                        None => {
                            self.texture =
                                Some(ui.ctx().load_texture("device-mirror", img, egui::TextureOptions::LINEAR))
                        }
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
        self.times.retain(|t| t.elapsed() < Duration::from_secs(1));
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("📱 {}", self.name)).strong());
            ui.label(RichText::new(format!("{} fps", self.times.len())).small().color(p.text_faint));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Close").clicked() {
                    close = true;
                }
                if let (Target::Android { serial }, true, false) = (&self.target, scrcpy_available(), self.remote) {
                    if ui
                        .small_button("↗ scrcpy")
                        .on_hover_text("Open in scrcpy: full frame rate, with sound")
                        .clicked()
                    {
                        let _ = std::process::Command::new("scrcpy").args(["-s", serial]).spawn();
                    }
                }
            });
        });
        if self.interactive() {
            ui.horizontal(|ui| {
                for (label, key, tip) in [
                    ("◁", "KEYCODE_BACK", "Back"),
                    ("○", "KEYCODE_HOME", "Home"),
                    ("▢", "KEYCODE_APP_SWITCH", "Recent apps"),
                    ("⏻", "KEYCODE_POWER", "Power"),
                    ("🔉", "KEYCODE_VOLUME_DOWN", "Volume down"),
                    ("🔊", "KEYCODE_VOLUME_UP", "Volume up"),
                ] {
                    if ui.small_button(label).on_hover_text(tip).clicked() {
                        self.send(Input::Key(key));
                    }
                }
            });
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.text)
                        .hint_text("Type into the device, Enter to send")
                        .desired_width(ui.available_width()),
                );
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.text.is_empty() {
                    let t = std::mem::take(&mut self.text);
                    self.send(Input::Text(t));
                }
            });
        } else {
            ui.label(RichText::new("View only — interact in the Simulator window").small().color(p.text_faint));
        }
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).small().color(p.warning));
        }
        let avail = ui.available_rect_before_wrap();
        let Some(tex) = &self.texture else {
            ui.scope_builder(egui::UiBuilder::new().max_rect(avail), |ui| {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                });
            });
            return close;
        };
        let (w, h) = (self.size[0] as f32, self.size[1] as f32);
        let scale = (avail.width() / w).min(avail.height() / h);
        let rect = egui::Rect::from_center_size(avail.center(), egui::vec2(w, h) * scale);
        let resp = ui.allocate_rect(avail, egui::Sense::click_and_drag());
        ui.painter().image(
            tex.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        if !self.interactive() {
            return close;
        }
        let to_device = |pos: egui::Pos2| -> (u32, u32) {
            let x = ((pos.x - rect.min.x) / scale).clamp(0.0, w - 1.0);
            let y = ((pos.y - rect.min.y) / scale).clamp(0.0, h - 1.0);
            (x as u32, y as u32)
        };
        if resp.drag_started() {
            self.drag_from = resp.interact_pointer_pos().filter(|p| rect.contains(*p)).map(|p| (p, Instant::now()));
        }
        if resp.drag_stopped() {
            if let (Some((from, t)), Some(to)) = (self.drag_from.take(), resp.interact_pointer_pos()) {
                let ((a, b), (c, d)) = (to_device(from), to_device(to));
                let ms = t.elapsed().as_millis().clamp(50, 2000) as u32;
                if from.distance(to) < 6.0 {
                    self.send(Input::Tap(a, b));
                } else {
                    self.send(Input::Swipe(a, b, c, d, ms));
                }
            }
        } else if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos().filter(|p| rect.contains(*p)) {
                let (x, y) = to_device(pos);
                self.send(Input::Tap(x, y));
            }
        }
        // Mouse wheel scrolls the page (a short swipe).
        if resp.hovered() {
            let dy = ui.input(|i| i.raw_scroll_delta.y);
            if dy.abs() > 1.0 {
                if let Some(pos) = ui.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p)) {
                    let (x, y) = to_device(pos);
                    let dist = (dy / scale * 2.0).clamp(-h / 2.0, h / 2.0);
                    let y2 = (y as f32 + dist).clamp(0.0, h - 1.0) as u32;
                    self.send(Input::Swipe(x, y, x, y2, 150));
                }
            }
        }
        close
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_screencap() {
        let mut data = vec![];
        for v in [2u32, 1, 1, 0] {
            data.extend(v.to_le_bytes()); // 2×1, RGBA, color space
        }
        data.extend([255, 0, 0, 7, 0, 0, 255, 9]);
        let img = parse_raw(&data).unwrap();
        assert_eq!(img.size, [2, 1]);
        assert_eq!(img.pixels[0], egui::Color32::from_rgb(255, 0, 0));
        assert_eq!(img.pixels[1], egui::Color32::from_rgb(0, 0, 255));
        // Old 12-byte header, BGRA.
        let mut old = vec![];
        for v in [1u32, 1, 5] {
            old.extend(v.to_le_bytes());
        }
        old.extend([255, 0, 0, 255]);
        assert_eq!(parse_raw(&old).unwrap().pixels[0], egui::Color32::from_rgb(0, 0, 255));
        // PNG data isn't raw.
        assert!(parse_raw(b"\x89PNG\r\n\x1a\nxxxxxxxxxxxx").is_none());
    }

    #[test]
    fn adb_input_commands() {
        assert_eq!(adb_input_args(&Input::Tap(10, 20)), ["shell", "input", "tap", "10", "20"]);
        assert_eq!(
            adb_input_args(&Input::Swipe(1, 2, 3, 4, 300)),
            ["shell", "input", "swipe", "1", "2", "3", "4", "300"]
        );
        assert_eq!(adb_input_args(&Input::Key("KEYCODE_BACK")), ["shell", "input", "keyevent", "KEYCODE_BACK"]);
        assert_eq!(adb_input_args(&Input::Text("hi there & co".into()))[3], "hi%sthere%s\\&%sco");
    }
}
