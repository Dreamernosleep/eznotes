//! The small "Record this meeting?" card, bottom-right of the primary screen.
//! Created on first use, then shown and hidden. It never takes focus, so the
//! meeting app keeps the keyboard.

use log::debug;
use tauri::{AppHandle, LogicalPosition, Manager, WebviewWindowBuilder};

const WIDTH: f64 = 340.0;
const HEIGHT: f64 = 132.0;
const MARGIN: f64 = 16.0;

/// Show the prompt window `label`, creating it if needed.
pub fn show(app: &AppHandle, label: &str) {
    let window = match app.get_webview_window(label) {
        Some(window) => window,
        None => {
            let mut builder = WebviewWindowBuilder::new(
                app,
                label,
                tauri::WebviewUrl::App("src/meeting-prompt/index.html".into()),
            )
            .title("Meeting")
            .inner_size(WIDTH, HEIGHT)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .decorations(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .transparent(true)
            .shadow(false)
            .accept_first_mouse(true)
            .focusable(false)
            .focused(false)
            .visible(false);
            if let Some(data_dir) = crate::portable::data_dir() {
                builder = builder.data_directory(data_dir.join("webview"));
            }
            match builder.build() {
                Ok(window) => window,
                Err(e) => {
                    debug!("Could not create the meeting prompt: {e}");
                    return;
                }
            }
        }
    };

    if let Ok(Some(monitor)) = window.primary_monitor() {
        let scale = monitor.scale_factor();
        let area = monitor.work_area();
        let right = (area.position.x as f64 + area.size.width as f64) / scale;
        let bottom = (area.position.y as f64 + area.size.height as f64) / scale;
        let _ = window.set_position(LogicalPosition::new(
            right - WIDTH - MARGIN,
            bottom - HEIGHT - MARGIN,
        ));
    }
    let _ = window.show();
}
