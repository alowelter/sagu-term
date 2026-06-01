// Esconde o console no Windows em builds de release (mantem no debug para logs).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod pty;
mod ssh;
mod terminal;
mod vault;

use app::App;

/// Carrega o mini-logo como icone da janela (assets/logo_mini.png).
fn load_icon() -> Option<egui::IconData> {
    let path = app::asset_path("logo_mini.png").or_else(|| app::asset_path("logo.png"))?;
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1000.0, 680.0])
        .with_min_inner_size([640.0, 420.0])
        .with_title("SaguTerm");
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "SaguTerm",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
