//! Desktop Tauri shell for Unstation.
//!
//! The command/event layer lives in the shared [`unstation_app`] crate (also used by the
//! Android shell); this binary is intentionally thin — it supplies the desktop
//! `tauri.conf.json` context (via `generate_context!`) and runs the shared builder with
//! the ffmpeg-based `publish` feature enabled.

pub fn run() {
    unstation_app::init_logging();
    unstation_app::builder()
        .run(tauri::generate_context!())
        .expect("error while running Unstation");
}
