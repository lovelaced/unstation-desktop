//! Desktop Tauri shell for Unstation.
//!
//! The command/event layer lives in the shared [`unstation_app`] crate (also used by the
//! Android shell); this binary is intentionally thin — it supplies the desktop
//! `tauri.conf.json` context (via `generate_context!`) and runs the shared builder with
//! the ffmpeg-based `publish` feature enabled.

pub fn run() {
    unstation_app::init_logging();
    unstation_app::builder()
        // Desktop-only: collapse second launches into the running window. With the
        // `deep-link` feature, any unstation:// URL in the second instance's argv is
        // forwarded to the deep-link plugin (registered in the shared builder), so the
        // frontend's onOpenUrl handler receives it; here we just surface the window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            use tauri::Manager;
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
        .run(tauri::generate_context!())
        .expect("error while running Unstation");
}
