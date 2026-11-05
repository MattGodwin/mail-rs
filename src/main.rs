// Hide the console window in release builds on Windows.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod app;
mod compose;
mod config;
mod drafts;
#[cfg(test)]
mod fixture_tests;
mod formats;
mod html;
mod http;
mod model;
mod net;
mod notify;
mod oauth;
mod secrets;
mod smtp;
mod store;
mod update;
mod view;
mod watcher;
mod worker;

use eframe::Renderer;

/// Where startup problems and crashes are recorded.
fn crash_log() -> std::path::PathBuf {
    config::cache_dir().join("crash.log")
}

fn log_problem(text: &str) {
    use std::io::Write;
    let path = crash_log();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] Mail {}: {text}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), env!("CARGO_PKG_VERSION"));
    }
}

fn error_dialog(title: &str, text: &str) {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(title)
        .set_description(text)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// A crash should never look like "nothing happened": log it, and tell the
/// user if the UI thread is the one that crashed.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let bt = std::backtrace::Backtrace::force_capture();
        log_problem(&format!("panic in thread '{thread}': {info}\n{bt}"));
        if thread == "main" {
            error_dialog(
                "Mail has stopped",
                &format!("Mail ran into a problem and has to close.\n\n{info}\n\nDetails were saved to {}", crash_log().display()),
            );
        }
        default(info);
    }));
}

const RENDERER_ENV: &str = "MAIL_RENDERER";

fn renderer_file() -> std::path::PathBuf {
    config::cache_dir().join("renderer")
}

/// Direct3D/Metal/Vulkan via wgpu by default; OpenGL if that failed before
/// on this computer (or MAIL_RENDERER=glow).
fn choose_renderer() -> Renderer {
    let pref = std::env::var(RENDERER_ENV).ok().or_else(|| std::fs::read_to_string(renderer_file()).ok());
    match pref.as_deref().map(str::trim) {
        Some("glow") => Renderer::Glow,
        _ => Renderer::Wgpu,
    }
}

fn run(renderer: Renderer) -> eframe::Result {
    let options = eframe::NativeOptions {
        renderer,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Mail")
            .with_app_id("mail-rs")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([760.0, 440.0])
            .with_drag_and_drop(true)
            .with_icon(app::icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Mail",
        options,
        Box::new(move |cc| {
            let app = app::App::new(cc);
            // Remember the fallback only after it successfully created the app.
            if renderer == Renderer::Glow {
                let _ = std::fs::create_dir_all(config::cache_dir());
                let _ = std::fs::write(renderer_file(), "glow");
            }
            log_problem(&format!("window initialized with the {renderer} renderer"));
            Ok(Box::new(app))
        }),
    )
}

fn main() {
    install_panic_hook();
    update::cleanup();
    let renderer = choose_renderer();
    log_problem(&format!("starting with the {renderer} renderer"));
    let Err(e) = run(renderer) else { return };
    log_problem(&format!("could not start with the {renderer} renderer: {e}"));
    // A window system can only be started once per process, so retry the
    // other renderer in a fresh process, and remember what worked.
    if renderer == Renderer::Wgpu && std::env::var(RENDERER_ENV).is_err() {
        if let Ok(exe) = std::env::current_exe() {
            if std::process::Command::new(exe).env(RENDERER_ENV, "glow").spawn().is_ok() {
                return;
            }
        }
    }
    let _ = std::fs::remove_file(renderer_file());
    error_dialog(
        "Mail couldn't start",
        &format!("Mail couldn't open its window on this computer.\n\n{e}\n\nDetails were saved to {}", crash_log().display()),
    );
    std::process::exit(1);
}
