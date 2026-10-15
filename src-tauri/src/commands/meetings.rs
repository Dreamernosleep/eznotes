use crate::managers::meeting::{MeetingManager, MeetingStatus, StopReason};
use std::sync::Arc;
use tauri::State;

/// Start recording a meeting. Returns the new meeting's id.
#[tauri::command]
#[specta::specta]
pub async fn start_meeting_recording(
    meeting_manager: State<'_, Arc<MeetingManager>>,
    source_app: Option<String>,
) -> Result<i64, String> {
    let manager = meeting_manager.inner().clone();
    // Opening the audio devices can block for a moment.
    tauri::async_runtime::spawn_blocking(move || manager.start(source_app))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Stop the current meeting. Returns its id, or `None` if nothing was
/// recording or the recording was discarded (too short or no speech).
#[tauri::command]
#[specta::specta]
pub async fn stop_meeting_recording(
    meeting_manager: State<'_, Arc<MeetingManager>>,
) -> Result<Option<i64>, String> {
    let manager = meeting_manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.stop(StopReason::Manual))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn get_meeting_status(meeting_manager: State<'_, Arc<MeetingManager>>) -> MeetingStatus {
    meeting_manager.status()
}
