//! QR codes drawn with egui (e.g. an Expo URL to scan with Expo Go, or a
//! dev server's network URL to open on a phone).

use eframe::egui::{self, Color32, Rect, Sense};

/// The dark / light modules of `text`'s QR code, row by row (`None` if it
/// doesn't fit in a QR code).
pub fn modules(text: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let v: Vec<bool> = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
    let w = (v.len() as f64).sqrt() as usize;
    (w * w == v.len()).then_some((w, v))
}

/// Draw the QR code of `text`, `size` points wide, dark on white (with the
/// quiet zone scanners need).
pub fn ui(ui: &mut egui::Ui, text: &str, size: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), Sense::click());
    let Some((w, cells)) = modules(text) else { return resp };
    let quiet = 2.0;
    let cell = size / (w as f32 + quiet * 2.0);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::WHITE);
    for y in 0..w {
        for x in 0..w {
            if cells[y * w + x] {
                let min = rect.min + egui::vec2((x as f32 + quiet) * cell, (y as f32 + quiet) * cell);
                // A hair larger so neighbouring cells don't show seams.
                painter.rect_filled(Rect::from_min_size(min, egui::vec2(cell + 0.5, cell + 0.5)), 0.0, Color32::BLACK);
            }
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_urls() {
        let (w, cells) = super::modules("exp://192.168.1.20:8081").unwrap();
        assert!(w >= 21 && cells.len() == w * w);
        // Finder pattern: the top-left corner module is dark.
        assert!(cells[0]);
    }
}
