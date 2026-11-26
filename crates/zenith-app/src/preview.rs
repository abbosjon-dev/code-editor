//! Web preview: a page (usually a dev server started with Run) shown in a
//! panel next to the code, in the system WebView (WKWebView on macOS,
//! WebView2 on Windows, WebKitGTK on Linux builds with `--features webview`).
//! Device presets size the page like a phone or a tablet. Where no WebView
//! is available the page opens in the browser instead.

use eframe::egui::{self, RichText};

use crate::theme::Palette;

/// Device presets: name, width, height (CSS pixels; 0 = fill the panel).
pub const DEVICES: &[(&str, f32, f32)] = &[
    ("Responsive", 0.0, 0.0),
    ("iPhone 15", 393.0, 852.0),
    ("iPhone SE", 375.0, 667.0),
    ("Pixel 8", 412.0, 915.0),
    ("Galaxy S23", 360.0, 780.0),
    ("iPad", 820.0, 1180.0),
    ("Laptop", 1280.0, 800.0),
];

/// Whether this build can show pages inside the editor.
pub fn embedded_supported() -> bool {
    cfg!(webview)
}

pub enum Action {
    OpenInBrowser(String),
    Close,
}

#[cfg_attr(not(webview), allow(dead_code))]
enum Op {
    Load(String),
    Reload,
    Script(&'static str),
}

pub struct Preview {
    pub url: String,
    address: String,
    pub device: usize,
    /// Reload when a file is saved (for servers without hot reload).
    pub reload_on_save: bool,
    /// Where the page goes this frame (points); `None` = hidden.
    rect: Option<egui::Rect>,
    /// Page zoom: a device scaled down to fit keeps its CSS width.
    zoom: f32,
    #[cfg(webview)]
    applied_zoom: f32,
    ops: Vec<Op>,
    pub error: Option<String>,
    #[cfg(webview)]
    view: Option<wry::WebView>,
}

impl Preview {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            address: url.to_string(),
            device: 0,
            reload_on_save: false,
            rect: None,
            zoom: 1.0,
            #[cfg(webview)]
            applied_zoom: 1.0,
            ops: vec![],
            error: None,
            #[cfg(webview)]
            view: None,
        }
    }

    pub fn load(&mut self, url: &str) {
        self.url = url.to_string();
        self.address = url.to_string();
        self.ops.push(Op::Load(url.to_string()));
    }

    pub fn reload(&mut self) {
        self.ops.push(Op::Reload);
    }

    /// The toolbar, and room for the page below it.
    pub fn ui(&mut self, ui: &mut egui::Ui, p: &Palette) -> Option<Action> {
        let mut action = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if ui.small_button("←").on_hover_text("Back").clicked() {
                self.ops.push(Op::Script("history.back()"));
            }
            if ui.small_button("→").on_hover_text("Forward").clicked() {
                self.ops.push(Op::Script("history.forward()"));
            }
            if ui.small_button("⟳").on_hover_text("Reload").clicked() {
                self.reload();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Close the preview").clicked() {
                    action = Some(Action::Close);
                }
                if ui.small_button("↗").on_hover_text("Open in the browser").clicked() {
                    action = Some(Action::OpenInBrowser(self.url.clone()));
                }
                let r = ui.add(egui::TextEdit::singleline(&mut self.address).desired_width(ui.available_width()));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let mut u = self.address.trim().to_string();
                    if !u.contains("://") {
                        u = format!("http://{u}");
                    }
                    self.load(&u);
                }
            });
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("preview-device").selected_text(DEVICES[self.device].0).show_ui(ui, |ui| {
                for (i, (name, w, h)) in DEVICES.iter().enumerate() {
                    let label = if *w > 0.0 { format!("{name}  {w}×{h}") } else { name.to_string() };
                    ui.selectable_value(&mut self.device, i, label);
                }
            });
            ui.checkbox(&mut self.reload_on_save, RichText::new("Reload on save").small())
                .on_hover_text("Reload the page when a file is saved (for servers without hot reload)");
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(p.warning).small());
        }
        let avail = ui.available_rect_before_wrap();
        let (w, h) = (DEVICES[self.device].1, DEVICES[self.device].2);
        self.zoom = 1.0;
        let rect = if w > 0.0 {
            // A device: its size, scaled down to fit, centered, with a frame.
            let scale = (avail.width() / (w + 16.0)).min(avail.height() / (h + 16.0)).min(1.0);
            let size = egui::vec2(w, h) * scale;
            let r = egui::Rect::from_center_size(avail.center(), size);
            self.zoom = scale;
            ui.painter().rect_stroke(
                r.expand(6.0),
                14.0,
                egui::Stroke::new(2.0_f32, p.border),
                egui::StrokeKind::Outside,
            );
            if scale < 1.0 {
                ui.painter().text(
                    egui::pos2(r.center().x, r.max.y + 10.0),
                    egui::Align2::CENTER_TOP,
                    format!("{:.0}%", scale * 100.0),
                    egui::FontId::proportional(10.0),
                    p.text_faint,
                );
            }
            r
        } else {
            avail
        };
        if !embedded_supported() {
            ui.painter().text(
                avail.center(),
                egui::Align2::CENTER_CENTER,
                "This build has no embedded web view — ↗ opens the page in the browser",
                egui::FontId::proportional(12.0),
                p.text_dim,
            );
        }
        ui.allocate_rect(avail, egui::Sense::hover());
        self.rect = Some(rect);
        action
    }

    /// Hide the page this frame (an egui popup or dialog would be covered
    /// by the native view).
    pub fn hide(&mut self) {
        self.rect = None;
    }

    /// Create / move / drive the native web view. Call once per frame after
    /// the UI, then [`Self::hide`] is implied until the next `ui`.
    #[allow(unused_variables)]
    pub fn sync(&mut self, frame: &eframe::Frame, pixels_per_point: f32) {
        let rect = self.rect.take();
        #[cfg(webview)]
        {
            #[cfg(target_os = "linux")]
            {
                // WebKitGTK runs on GTK's loop: advance it a little each frame.
                static GTK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                if !*GTK.get_or_init(|| gtk::init().is_ok()) {
                    self.error = Some("GTK could not be initialized: no embedded preview".into());
                    return;
                }
                for _ in 0..50 {
                    if !gtk::events_pending() {
                        break;
                    }
                    gtk::main_iteration_do(false);
                }
            }
            let bounds = |r: egui::Rect| wry::Rect {
                position: wry::dpi::PhysicalPosition::new(r.min.x * pixels_per_point, r.min.y * pixels_per_point)
                    .into(),
                size: wry::dpi::PhysicalSize::new(
                    (r.width() * pixels_per_point).max(1.0) as u32,
                    (r.height() * pixels_per_point).max(1.0) as u32,
                )
                .into(),
            };
            if self.view.is_none() {
                let Some(r) = rect else { return };
                match wry::WebViewBuilder::new()
                    .with_url(&self.url)
                    .with_bounds(bounds(r))
                    .with_devtools(true)
                    .build_as_child(frame)
                {
                    Ok(v) => {
                        self.view = Some(v);
                        self.error = None;
                        self.ops.retain(|o| !matches!(o, Op::Load(_)));
                    }
                    Err(e) => {
                        self.error = Some(format!("Embedded preview unavailable ({e}); use ↗ to open in the browser"));
                        return;
                    }
                }
            }
            let Some(v) = &self.view else { return };
            match rect {
                Some(r) => {
                    let _ = v.set_bounds(bounds(r));
                    let _ = v.set_visible(true);
                }
                None => {
                    let _ = v.set_visible(false);
                }
            }
            if (self.zoom - self.applied_zoom).abs() > 0.001 {
                let _ = v.zoom(self.zoom as f64);
                self.applied_zoom = self.zoom;
            }
            for op in self.ops.drain(..) {
                let _ = match op {
                    Op::Load(u) => v.load_url(&u),
                    Op::Reload => v.reload(),
                    Op::Script(s) => v.evaluate_script(s),
                };
            }
            if let Ok(u) = v.url() {
                if u != self.url && !u.is_empty() && u != "about:blank" {
                    self.url = u.clone();
                    self.address = u;
                }
            }
        }
    }
}
