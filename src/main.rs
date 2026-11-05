// Hide the console window in release builds on Windows.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod app;
mod compose;
mod config;
mod drafts;
mod model;
mod net;
mod notify;
mod oauth;
mod secrets;
mod smtp;
mod store;
mod view;
mod watcher;
mod worker;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Mail")
            .with_app_id("mail-rs")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([760.0, 440.0])
            .with_drag_and_drop(true)
            .with_icon(app::icon()),
        ..Default::default()
    };
    eframe::run_native("Mail", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
