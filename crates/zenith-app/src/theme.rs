//! Visual theme: UI palette, fonts and egui style.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, TextStyle, Visuals,
};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub bg_sidebar: Color32,
    pub bg_panel: Color32,
    pub bg_elevated: Color32,
    pub bg_hover: Color32,
    pub bg_active: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub selection: Color32,
    pub current_line: Color32,
    pub find_match: Color32,
    pub cursor: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
    pub indent_guide: Color32,
    /// ANSI 16-color palette for the terminal.
    pub ansi: [Color32; 16],
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

const fn hexa(rgb: u32, a: u8) -> Color32 {
    Color32::from_rgba_premultiplied(
        (((rgb >> 16) & 0xff) * a as u32 / 255) as u8,
        (((rgb >> 8) & 0xff) * a as u32 / 255) as u8,
        ((rgb & 0xff) * a as u32 / 255) as u8,
        a,
    )
}

impl Palette {
    /// "Zenith Midnight" — the default dark theme.
    pub fn midnight() -> Self {
        Self {
            dark: true,
            bg: hex(0x15171e),
            bg_sidebar: hex(0x111318),
            bg_panel: hex(0x111318),
            bg_elevated: hex(0x1d2029),
            bg_hover: hex(0x232733),
            bg_active: hex(0x2a2f3d),
            border: hex(0x252a36),
            text: hex(0xd5d9e3),
            text_dim: hex(0x8a92a6),
            text_faint: hex(0x4d5466),
            accent: hex(0x8b7cf6),
            accent_soft: hexa(0x8b7cf6, 60),
            selection: hexa(0x5b6ee1, 85),
            current_line: hexa(0xffffff, 8),
            find_match: hexa(0xe8b04a, 70),
            cursor: hex(0xa79bff),
            success: hex(0x4fd1a5),
            warning: hex(0xe8b04a),
            error: hex(0xf2667a),
            indent_guide: hexa(0xffffff, 14),
            ansi: [
                hex(0x1d2029),
                hex(0xf2667a),
                hex(0x4fd1a5),
                hex(0xe8b04a),
                hex(0x6aa5ff),
                hex(0xb48cff),
                hex(0x3ddbd9),
                hex(0xc8cdd8),
                hex(0x4d5466),
                hex(0xff8597),
                hex(0x7ae5c0),
                hex(0xf5c977),
                hex(0x94bfff),
                hex(0xcfb2ff),
                hex(0x7ff0ee),
                hex(0xffffff),
            ],
        }
    }

    /// "Zenith Daylight" — light theme.
    pub fn daylight() -> Self {
        Self {
            dark: false,
            bg: hex(0xfbfbfd),
            bg_sidebar: hex(0xf1f2f6),
            bg_panel: hex(0xf1f2f6),
            bg_elevated: hex(0xffffff),
            bg_hover: hex(0xe6e8ef),
            bg_active: hex(0xdcdfe9),
            border: hex(0xdfe2ea),
            text: hex(0x24283b),
            text_dim: hex(0x5f6680),
            text_faint: hex(0xa3a9bb),
            accent: hex(0x6a57e8),
            accent_soft: hexa(0x6a57e8, 40),
            selection: hexa(0x6a8cff, 70),
            current_line: hexa(0x000000, 8),
            find_match: hexa(0xf0a020, 80),
            cursor: hex(0x6a57e8),
            success: hex(0x1a9b6f),
            warning: hex(0xb7791f),
            error: hex(0xd23c56),
            indent_guide: hexa(0x000000, 20),
            ansi: [
                hex(0x24283b),
                hex(0xd23c56),
                hex(0x1a9b6f),
                hex(0xb7791f),
                hex(0x3466d6),
                hex(0x8a4fd8),
                hex(0x138a96),
                hex(0x6b7088),
                hex(0x8a90a6),
                hex(0xe85a72),
                hex(0x27b585),
                hex(0xd2952e),
                hex(0x5585f0),
                hex(0xa36ef0),
                hex(0x22a7b4),
                hex(0x24283b),
            ],
        }
    }

    pub fn by_name(name: &str) -> Self {
        match name {
            "zenith-daylight" | "light" => Self::daylight(),
            _ => Self::midnight(),
        }
    }

    /// Map a 256-color index to RGB.
    pub fn ansi_256(&self, idx: u8) -> Color32 {
        match idx {
            0..=15 => self.ansi[idx as usize],
            16..=231 => {
                let i = idx - 16;
                let c = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                Color32::from_rgb(c(i / 36), c((i / 6) % 6), c(i % 6))
            }
            _ => {
                let g = 8 + (idx - 232) * 10;
                Color32::from_rgb(g, g, g)
            }
        }
    }
}

pub const MONO_BOLD: &str = "mono-bold";
pub const MONO_ITALIC: &str = "mono-italic";
pub const UI_BOLD: &str = "ui-bold";

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    add(&mut fonts, "JetBrainsMono", include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf"));
    add(&mut fonts, "JetBrainsMono-Bold", include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf"));
    add(&mut fonts, "JetBrainsMono-Italic", include_bytes!("../../../assets/fonts/JetBrainsMono-Italic.ttf"));
    add(&mut fonts, "Inter", include_bytes!("../../../assets/fonts/Inter-Regular.ttf"));
    add(&mut fonts, "Inter-SemiBold", include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf"));

    // Keep egui's bundled fonts as fallbacks (emoji / symbols / CJK coverage).
    let fallbacks_prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let fallbacks_mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();

    // JetBrains Mono second: it covers many UI symbols (✕ ▾ ◧ ≡) that Inter lacks.
    let mut prop = vec!["Inter".to_owned(), "JetBrainsMono".to_owned()];
    prop.extend(fallbacks_prop.iter().cloned());
    let mut mono = vec!["JetBrainsMono".to_owned()];
    mono.extend(fallbacks_mono.iter().cloned());
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(FontFamily::Monospace, mono);

    let with = |first: &str, rest: &[String]| {
        let mut v = vec![first.to_owned()];
        v.extend(rest.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Name(MONO_BOLD.into()), with("JetBrainsMono-Bold", &fallbacks_mono));
    fonts.families.insert(FontFamily::Name(MONO_ITALIC.into()), with("JetBrainsMono-Italic", &fallbacks_mono));
    let mut bold_fallbacks = vec!["JetBrainsMono".to_owned()];
    bold_fallbacks.extend(fallbacks_prop.iter().cloned());
    fonts.families.insert(FontFamily::Name(UI_BOLD.into()), with("Inter-SemiBold", &bold_fallbacks));
    ctx.set_fonts(fonts);
}

pub fn apply_style(ctx: &egui::Context, p: &Palette, ui_font_size: f32) {
    let mut visuals = if p.dark { Visuals::dark() } else { Visuals::light() };
    visuals.override_text_color = Some(p.text);
    visuals.panel_fill = p.bg_panel;
    visuals.window_fill = p.bg_elevated;
    visuals.extreme_bg_color = p.bg;
    visuals.faint_bg_color = p.bg_hover;
    visuals.code_bg_color = p.bg_elevated;
    visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals.window_shadow =
        egui::epaint::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(110) };
    visuals.popup_shadow =
        egui::epaint::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(90) };
    visuals.selection.bg_fill = p.selection;
    visuals.selection.stroke = Stroke::new(1.0_f32, p.text);
    visuals.hyperlink_color = p.accent;
    visuals.text_cursor.stroke = Stroke::new(2.0_f32, p.cursor);

    let radius = CornerRadius::same(6);
    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = p.bg_panel;
    w.noninteractive.weak_bg_fill = p.bg_panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text_dim);
    w.noninteractive.corner_radius = radius;

    w.inactive.bg_fill = p.bg_elevated;
    w.inactive.weak_bg_fill = p.bg_elevated;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.inactive.corner_radius = radius;

    w.hovered.bg_fill = p.bg_hover;
    w.hovered.weak_bg_fill = p.bg_hover;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent_soft);
    w.hovered.fg_stroke = Stroke::new(1.5_f32, p.text);
    w.hovered.corner_radius = radius;

    w.active.bg_fill = p.bg_active;
    w.active.weak_bg_fill = p.bg_active;
    w.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
    w.active.fg_stroke = Stroke::new(2.0_f32, p.text);
    w.active.corner_radius = radius;

    w.open = w.active;

    let mut style = (*ctx.style()).clone();
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.menu_margin = Margin::same(6);
    style.spacing.window_margin = Margin::same(12);
    style.spacing.interact_size.y = 26.0;
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(ui_font_size - 2.0)),
        (TextStyle::Body, FontId::proportional(ui_font_size)),
        (TextStyle::Button, FontId::proportional(ui_font_size)),
        (TextStyle::Heading, FontId::new(ui_font_size + 6.0, FontFamily::Name(UI_BOLD.into()))),
        (TextStyle::Monospace, FontId::monospace(ui_font_size)),
    ]
    .into();
    ctx.set_style(style);
}

/// Color + glyph for a file, based on its extension.
pub fn file_icon(name: &str) -> (&'static str, Color32) {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "cargo.toml" | "cargo.lock" => return ("⚙", hex(0xdea584)),
        "dockerfile" => return ("🐳", hex(0x2496ed)),
        "makefile" => return ("⚙", hex(0x6d8086)),
        ".gitignore" | ".gitattributes" => return ("⎇", hex(0xf05033)),
        _ => {}
    }
    match ext.as_str() {
        "rs" => ("●", hex(0xdea584)),
        "py" => ("●", hex(0x4b8bbe)),
        "js" | "mjs" | "cjs" => ("●", hex(0xf1e05a)),
        "ts" | "tsx" => ("●", hex(0x3178c6)),
        "jsx" => ("●", hex(0x61dafb)),
        "go" => ("●", hex(0x00add8)),
        "c" | "h" => ("●", hex(0x8b9cff)),
        "cpp" | "cc" | "hpp" | "cxx" => ("●", hex(0xf34b7d)),
        "java" | "kt" => ("●", hex(0xb07219)),
        "swift" => ("●", hex(0xf05138)),
        "rb" => ("●", hex(0xcc342d)),
        "php" => ("●", hex(0x777bb4)),
        "cs" => ("●", hex(0x178600)),
        "html" | "htm" => ("◇", hex(0xe34c26)),
        "css" | "scss" | "sass" | "less" => ("◇", hex(0x563d7c)),
        "json" | "jsonc" => ("{}", hex(0xcbcb41)),
        "toml" | "yaml" | "yml" | "ini" | "cfg" => ("≡", hex(0x9aa5b8)),
        "md" | "markdown" | "txt" | "rst" => ("¶", hex(0x8ab4f8)),
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" => ("$", hex(0x89e051)),
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "ico" | "webp" => ("▣", hex(0xa074c4)),
        "lock" => ("🔒", hex(0x8a92a6)),
        _ => ("○", hex(0x8a92a6)),
    }
}

/// Advance width of one monospace cell and row height, measured from real
/// layout (glyph positions may be pixel-snapped, so a single glyph's width can
/// drift from the effective advance over a long line).
pub fn mono_metrics(ui: &egui::Ui, font: &FontId) -> (f32, f32) {
    const N: usize = 200;
    ui.fonts(|f| {
        let galley = f.layout_no_wrap("0".repeat(N), font.clone(), Color32::WHITE);
        (galley.size().x / N as f32, f.row_height(font))
    })
}
