use crate::managers::history::HistoryManager;
use crate::managers::meeting::{MeetingManager, MeetingStatus, StopReason};
use crate::managers::meeting_detect::{DetectedMeeting, MeetingDetector};
use crate::managers::meeting_store::{self, MeetingDetail, MeetingSummary};
use crate::settings::{get_settings, write_settings};
use std::sync::Arc;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

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

#[tauri::command]
#[specta::specta]
pub fn list_meetings(
    history: State<'_, Arc<HistoryManager>>,
) -> Result<Vec<MeetingSummary>, String> {
    let conn = history.get_connection().map_err(|e| e.to_string())?;
    meeting_store::list(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn get_meeting(
    history: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<MeetingDetail, String> {
    let conn = history.get_connection().map_err(|e| e.to_string())?;
    meeting_store::detail(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn get_meeting_markdown(
    history: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<String, String> {
    let conn = history.get_connection().map_err(|e| e.to_string())?;
    let detail = meeting_store::detail(&conn, id).map_err(|e| e.to_string())?;
    Ok(meeting_store::markdown(&detail))
}

#[tauri::command]
#[specta::specta]
pub fn update_meeting_notes(
    history: State<'_, Arc<HistoryManager>>,
    id: i64,
    notes: String,
) -> Result<(), String> {
    let conn = history.get_connection().map_err(|e| e.to_string())?;
    meeting_store::set_notes(&conn, id, &notes).map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn rename_meeting(
    history: State<'_, Arc<HistoryManager>>,
    id: i64,
    title: String,
) -> Result<(), String> {
    let conn = history.get_connection().map_err(|e| e.to_string())?;
    meeting_store::set_title(&conn, id, &title).map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn rename_meeting_speaker(
    history: State<'_, Arc<HistoryManager>>,
    id: i64,
    speaker_key: String,
    name: String,
) -> Result<(), String> {
    let conn = history.get_connection().map_err(|e| e.to_string())?;
    meeting_store::rename_speaker(&conn, id, &speaker_key, &name).map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn delete_meeting(
    app: AppHandle,
    history: State<'_, Arc<HistoryManager>>,
    meeting_manager: State<'_, Arc<MeetingManager>>,
    id: i64,
) -> Result<(), String> {
    if meeting_manager.status().meeting_id == Some(id) {
        return Err("Stop the recording before deleting it".to_string());
    }
    let conn = history.get_connection().map_err(|e| e.to_string())?;
    let folder = meeting_store::folder(&conn, id).map_err(|e| e.to_string())?;
    meeting_store::delete(&conn, id).map_err(|e| e.to_string())?;
    let dir = crate::portable::app_data_dir(&app)
        .map_err(|e| e.to_string())?
        .join(folder);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        log::warn!("Meeting {id}: could not remove {}: {e}", dir.display());
    }
    Ok(())
}

/// Run the after-meeting pass again (e.g. after it failed).
#[tauri::command]
#[specta::specta]
pub fn retry_meeting_processing(
    app: AppHandle,
    history: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    let conn = history.get_connection().map_err(|e| e.to_string())?;
    let updated = conn
        .execute(
            "UPDATE meetings SET status = 'processing', error = NULL
             WHERE id = ?1 AND status IN ('complete', 'failed')",
            rusqlite::params![id],
        )
        .map_err(|e| e.to_string())?;
    if updated == 0 {
        return Err("This meeting is still recording or processing".to_string());
    }
    crate::managers::meeting_process::spawn_processing(app, id, None);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn open_meeting_folder(
    app: AppHandle,
    history: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    let conn = history.get_connection().map_err(|e| e.to_string())?;
    let folder = meeting_store::folder(&conn, id).map_err(|e| e.to_string())?;
    let dir = crate::portable::app_data_dir(&app)
        .map_err(|e| e.to_string())?
        .join(folder);
    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<String>)
        .map_err(|e| format!("Failed to open the meeting folder: {e}"))
}

#[tauri::command]
#[specta::specta]
pub fn change_meeting_max_minutes_setting(app: AppHandle, minutes: u32) -> Result<(), String> {
    if minutes == 0 {
        return Err("The maximum length must be at least one minute".to_string());
    }
    let mut settings = get_settings(&app);
    settings.meeting_max_minutes = minutes;
    write_settings(&app, settings);
    Ok(())
}

/// Days to keep meeting audio (0 = forever). Transcripts are always kept.
#[tauri::command]
#[specta::specta]
pub fn change_meeting_audio_retention_days_setting(
    app: AppHandle,
    meeting_manager: State<'_, Arc<MeetingManager>>,
    days: u32,
) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.meeting_audio_retention_days = days;
    write_settings(&app, settings);
    meeting_manager.cleanup_old_audio();
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_meeting_auto_detect_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.meeting_auto_detect = enabled;
    write_settings(&app, settings);
    Ok(())
}

/// Replace the list of apps never offered for recording.
#[tauri::command]
#[specta::specta]
pub fn change_meeting_ignored_apps_setting(
    app: AppHandle,
    apps: Vec<String>,
) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.meeting_ignored_apps = apps;
    write_settings(&app, settings);
    Ok(())
}

/// The meeting the prompt card is offering, if any.
#[tauri::command]
#[specta::specta]
pub fn meeting_prompt_current(
    detector: State<'_, Arc<MeetingDetector>>,
) -> Option<DetectedMeeting> {
    detector.current()
}

#[tauri::command]
#[specta::specta]
pub async fn meeting_prompt_record(
    detector: State<'_, Arc<MeetingDetector>>,
) -> Result<i64, String> {
    let detector = detector.inner().clone();
    tauri::async_runtime::spawn_blocking(move || detector.accept())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn meeting_prompt_dismiss(detector: State<'_, Arc<MeetingDetector>>) {
    detector.dismiss();
}

#[tauri::command]
#[specta::specta]
pub fn meeting_prompt_never(detector: State<'_, Arc<MeetingDetector>>) {
    detector.ignore_app();
}

#[tauri::command]
#[specta::specta]
pub fn change_meeting_screen_names_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.meeting_screen_names = enabled;
    write_settings(&app, settings);
    Ok(())
}
