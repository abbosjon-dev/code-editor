// Hide the console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod changeset;
mod commands;
mod conflicts;
mod debug;
mod diff_view;
mod document;
mod editor_view;
mod file_tree;
mod folding;
mod git;
mod highlight;
mod keymap;
mod lsp;
mod mcp_ui;
mod mention;
mod palette;
mod review;
mod sample_plugin;
mod settings;
mod settings_ui;
mod terminal_view;
mod testing;
mod theme;
mod treesitter;
mod update;
mod vim;
mod workspace;

use eframe::egui;

/// App icon drawn procedurally: violet gradient rounded square with a diamond.
fn icon() -> egui::IconData {
    let size = 128u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let r = 26.0f32;
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let s = size as f32;
            // Rounded-rect mask.
            let dx = (r - fx).max(fx - (s - r)).max(0.0);
            let dy = (r - fy).max(fy - (s - r)).max(0.0);
            let inside = (dx * dx + dy * dy).sqrt() <= r;
            if !inside {
                continue;
            }
            let t = (fx + fy) / (2.0 * s);
            let (r0, g0, b0) = (0x8b as f32, 0x7c as f32, 0xf6 as f32);
            let (r1, g1, b1) = (0x3d as f32, 0xdb as f32, 0xd9 as f32);
            let mut c = [r0 + (r1 - r0) * t, g0 + (g1 - g0) * t, b0 + (b1 - b0) * t];
            // Diamond in the middle.
            let d = (fx - s / 2.0).abs() + (fy - s / 2.0).abs();
            if d < s * 0.28 {
                c = [255.0, 255.0, 255.0];
            } else if d < s * 0.31 {
                c = [c[0] * 0.6, c[1] * 0.6, c[2] * 0.6];
            }
            let i = ((y * size + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&[c[0] as u8, c[1] as u8, c[2] as u8, 255]);
        }
    }
    egui::IconData { rgba, width: size, height: size }
}

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("zenith {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Zenith {} — fast, extensible, AI-native code editor\n\nUSAGE:\n    zenith [PATH]\n\nPATH may be a folder (opened as workspace) or a file.", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Zenith")
            .with_app_id("zenith")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(icon()),
        vsync: true,
        ..Default::default()
    };
    eframe::run_native("Zenith", options, Box::new(move |cc| Ok(Box::new(app::ZenithApp::new(cc, args)))))
}
