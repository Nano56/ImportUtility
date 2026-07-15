#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

mod app;
mod importer;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([600.0, 700.0])
            .with_title("Video/Audio Import Utility"),
        ..Default::default()
    };
    eframe::run_native(
        "Import Utility",
        options,
        Box::new(|cc| {
            Box::new(app::ImportUtilityApp::new(cc))
        }),
    )
}
