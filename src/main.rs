// Esconde o console no Windows em builds de release (mantem no debug para logs).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod download;
mod emoji;
mod hostkey;
mod instance;
mod osinfo;
mod paste;
mod pty;
mod remember;
mod sftp;
mod ssh;
mod terminal;
mod upload;
mod vault;
mod viewer;
mod vtfix;

use app::App;

/// Nome do app no eframe: define a pasta de dados (`%APPDATA%\SaguTerm\data`).
pub const APP_NAME: &str = "SaguTerm";

/// Carrega o mini-logo como icone da janela (embutido no binario).
fn load_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/logo_mini.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

fn main() -> eframe::Result<()> {
    // Ja aberto: traz a janela existente para a frente e sai sem abrir outra.
    let _instancia = match instance::start() {
        instance::Start::Activated => return Ok(()),
        instance::Start::Primary(slot) => Some(slot),
        instance::Start::Unchecked => None,
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1000.0, 680.0])
        .with_min_inner_size([640.0, 420.0])
        // Titulo sem a versao (ela aparece no topo da ajuda); instance.rs procura a janela por ele.
        .with_title(instance::WINDOW_TITLE);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
