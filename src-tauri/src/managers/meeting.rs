//! Meeting recording: your microphone and the system audio ("the others"),
//! recorded side by side as two 16 kHz mono files.
//!
//! The microphone gets its own stream, separate from dictation's, so
//! dictation keeps working during a meeting. Both channels are written as raw
//! 16-bit PCM while recording (a crash loses at most what is buffered) and
//! converted to WAV when the meeting ends.
//!
//! A meeting stops on request, at the configured maximum length, or after a
//! long silence once someone has spoken. Recordings that are too short or
//! never contained speech are discarded.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::Local;
use handy_recorder::{AudioChunk, Channels, Recorder, RecorderConfig, Sink};
use log::{error, info, warn};
use rusqlite::params;
use serde::Serialize;
use specta::Type;
use tauri::{AppHandle, Emitter, Manager};

use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use crate::audio_toolkit::{EarshotVad, SileroVad, VoiceActivityDetector};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::history::HistoryManager;
use crate::managers::meeting_live::{
    spawn_transcriber, MeetingChannel, Utterance, UtteranceCutter,
};
use crate::managers::meeting_process::spawn_processing;
use crate::managers::meeting_store;
use crate::settings::get_settings;

/// Folder under the app data dir that holds one sub-folder per meeting.
const MEETINGS_DIR: &str = "meetings";
const MIC_PCM: &str = "mic.pcm";
const SYSTEM_PCM: &str = "system.pcm";
pub const MIC_WAV: &str = "mic.wav";
pub const SYSTEM_WAV: &str = "system.wav";

/// Shorter recordings are treated as accidental and discarded.
const MIN_MEETING: Duration = Duration::from_secs(10);
/// Stop after this much silence, once someone has spoken.
const SILENCE_STOP: Duration = Duration::from_secs(10 * 60);
/// Warn this long before the maximum length is reached.
const MAX_LENGTH_WARNING: Duration = Duration::from_secs(2 * 60);
const WATCHDOG_TICK: Duration = Duration::from_secs(1);
/// How often the meeting window is read for who is speaking.
const SCREEN_POLL: Duration = Duration::from_secs(1);
const SILERO_THRESHOLD: f32 = 0.3;
const EARSHOT_THRESHOLD: f32 = 0.5;

/// Event with the current [`MeetingStatus`], sent on every start and stop.
pub const STATUS_EVENT: &str = "meeting-status";
/// Event sent [`MAX_LENGTH_WARNING`] before the maximum length stops a meeting.
pub const WARNING_EVENT: &str = "meeting-length-warning";

#[derive(Debug, Clone, Serialize, Type)]
pub struct MeetingStatus {
    pub recording: bool,
    pub meeting_id: Option<i64>,
    /// Start time in ms since the Unix epoch.
    pub started_at: Option<i64>,
    /// Whether the other people's audio is being captured too.
    pub system_audio: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Manual,
    MaxLength,
    /// The meeting app released the microphone (auto-detected meetings).
    AppClosed,
    Silence,
}

/// Speech bookkeeping shared by both channels and the watchdog.
struct Activity {
    started: Instant,
    /// Ms since start of the last speech frame, plus one (0 = none yet).
    last_speech: AtomicU64,
}

impl Activity {
    fn mark_speech(&self) {
        let ms = self.started.elapsed().as_millis() as u64 + 1;
        self.last_speech.fetch_max(ms, Ordering::Relaxed);
    }

    fn heard_speech(&self) -> bool {
        self.last_speech.load(Ordering::Relaxed) > 0
    }

    fn silence(&self) -> Option<Duration> {
        let last = self.last_speech.load(Ordering::Relaxed);
        (last > 0)
            .then(|| Duration::from_millis(self.started.elapsed().as_millis() as u64 + 1 - last))
    }
}

/// Writes one channel to raw 16-bit PCM, tracks whether it contains speech,
/// and hands finished utterances to the live transcriber.
struct ChannelWriter {
    file: BufWriter<File>,
    vad: Option<Box<dyn VoiceActivityDetector>>,
    frame: Vec<f32>,
    activity: Arc<Activity>,
    cutter: UtteranceCutter,
    live: mpsc::Sender<Utterance>,
    failed: bool,
}

impl ChannelWriter {
    fn create(
        path: &Path,
        channel: MeetingChannel,
        vad: Option<Box<dyn VoiceActivityDetector>>,
        activity: Arc<Activity>,
        live: mpsc::Sender<Utterance>,
    ) -> Result<Self> {
        let frame_len = vad.as_ref().map_or(0, |v| v.frame_samples());
        Ok(Self {
            file: BufWriter::new(File::create(path)?),
            vad,
            frame: Vec::with_capacity(frame_len),
            activity,
            cutter: UtteranceCutter::new(channel),
            live,
            failed: false,
        })
    }

    fn push(&mut self, samples: &[f32]) {
        if !self.failed {
            let bytes: Vec<u8> = samples
                .iter()
                .flat_map(|s| ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes())
                .collect();
            if let Err(e) = self.file.write_all(&bytes) {
                error!("Meeting audio write failed: {e}");
                self.failed = true;
            }
        }

        let Some(vad) = self.vad.as_mut() else {
            return;
        };
        let frame_len = vad.frame_samples();
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (frame_len - self.frame.len()).min(rest.len());
            self.frame.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.frame.len() == frame_len {
                let voiced = matches!(vad.is_voice(&self.frame), Ok(true));
                if voiced {
                    self.activity.mark_speech();
                }
                if let Some(utterance) = self.cutter.push_frame(&self.frame, voiced) {
                    let _ = self.live.send(utterance);
                }
                self.frame.clear();
            }
        }
    }

    fn finish(mut self) -> io::Result<()> {
        if let Some(utterance) = self.cutter.flush() {
            let _ = self.live.send(utterance);
        }
        self.file.flush()
    }
}

/// The meeting microphone's sink: everything goes to the mic channel.
struct MicSink(ChannelWriter);

impl Sink for MicSink {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
        let real = chunk.valid_frames * chunk.channels as usize;
        self.0.push(&chunk.samples[..real]);
    }
}

struct ActiveMeeting {
    id: i64,
    folder: PathBuf,
    started_at: i64,
    activity: Arc<Activity>,
    mic: Recorder<MicSink>,
    #[cfg(windows)]
    loopback: Option<crate::audio_toolkit::audio::LoopbackCapture>,
    system: Arc<Mutex<Option<ChannelWriter>>>,
    watchdog_stop: Arc<AtomicBool>,
    /// Live transcription; ends once both channel writers are finished.
    live: JoinHandle<()>,
    /// Stops the meeting-window reader (speaker names), if one runs.
    screen_stop: Arc<AtomicBool>,
}

pub struct MeetingManager {
    app_handle: AppHandle,
    history: Arc<HistoryManager>,
    audio: Arc<AudioRecordingManager>,
    meetings_dir: PathBuf,
    active: Mutex<Option<ActiveMeeting>>,
}

impl MeetingManager {
    pub fn new(
        app_handle: &AppHandle,
        history: Arc<HistoryManager>,
        audio: Arc<AudioRecordingManager>,
    ) -> Result<Self> {
        let meetings_dir = crate::portable::app_data_dir(app_handle)?.join(MEETINGS_DIR);
        fs::create_dir_all(&meetings_dir)?;
        Ok(Self {
            app_handle: app_handle.clone(),
            history,
            audio,
            meetings_dir,
            active: Mutex::new(None),
        })
    }

    pub fn status(&self) -> MeetingStatus {
        let active = self.active.lock().unwrap();
        match active.as_ref() {
            Some(m) => MeetingStatus {
                recording: true,
                meeting_id: Some(m.id),
                started_at: Some(m.started_at),
                system_audio: m.has_system_audio(),
            },
            None => MeetingStatus {
                recording: false,
                meeting_id: None,
                started_at: None,
                system_audio: false,
            },
        }
    }

    /// Time since the last speech in the current meeting, if any was heard.
    pub fn silence(&self) -> Option<Duration> {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|m| m.activity.silence())
    }

    pub fn is_recording(&self) -> bool {
        self.active.lock().unwrap().is_some()
    }

    /// Start recording a meeting. `source_app` (e.g. "Zoom") only names it.
    pub fn start(&self, source_app: Option<String>) -> Result<i64> {
        self.start_for_app(source_app, None)
    }

    /// Start recording; with `app_pid`, the other side is captured from that
    /// app only (and its child processes), not from everything the PC plays.
    pub fn start_for_app(&self, source_app: Option<String>, app_pid: Option<u32>) -> Result<i64> {
        let mut active = self.active.lock().unwrap();
        if active.is_some() {
            return Err(anyhow!("A meeting is already being recorded"));
        }

        let now = Local::now();
        let folder_name = now.format("%Y-%m-%d_%H%M%S").to_string();
        let folder = self.meetings_dir.join(&folder_name);
        fs::create_dir_all(&folder)?;
        let title = format!(
            "{} {}",
            source_app.as_deref().unwrap_or("Meeting"),
            now.format("%b %-d, %H:%M")
        );
        let started_at = now.timestamp_millis();

        let conn = self.history.get_connection()?;
        conn.execute(
            "INSERT INTO meetings (title, source_app, started_at, status, folder)
             VALUES (?1, ?2, ?3, 'recording', ?4)",
            params![
                title,
                source_app,
                started_at,
                format!("{MEETINGS_DIR}/{folder_name}")
            ],
        )?;
        let id = conn.last_insert_rowid();

        match self.open_capture(id, &folder, started_at, app_pid) {
            Ok(meeting) => {
                info!(
                    "Meeting {} recording started (system audio: {})",
                    id,
                    meeting.has_system_audio()
                );
                *active = Some(meeting);
            }
            Err(e) => {
                let _ = conn.execute("DELETE FROM meetings WHERE id = ?1", params![id]);
                let _ = fs::remove_dir_all(&folder);
                return Err(e);
            }
        }
        drop(active);
        self.spawn_watchdog();
        self.notify();
        Ok(id)
    }

    fn open_capture(
        &self,
        id: i64,
        folder: &Path,
        started_at: i64,
        app_pid: Option<u32>,
    ) -> Result<ActiveMeeting> {
        let activity = Arc::new(Activity {
            started: Instant::now(),
            last_speech: AtomicU64::new(0),
        });

        let activity_for_screen = activity.clone();
        let (live_tx, live_rx) = mpsc::channel();
        let live = spawn_transcriber(self.app_handle.clone(), id, live_rx);

        let (device, channel) = self.audio.meeting_microphone();
        let mic_writer = ChannelWriter::create(
            &folder.join(MIC_PCM),
            MeetingChannel::Mic,
            self.create_vad(),
            activity.clone(),
            live_tx.clone(),
        )?;
        let mic = Recorder::open(RecorderConfig {
            device,
            channels: channel.map_or(Channels::MixToMono, Channels::Only),
            ..RecorderConfig::speech()
        })
        .map_err(|e| anyhow!("Cannot open the microphone: {e}"))?;
        mic.start(MicSink(mic_writer))
            .map_err(|e| anyhow!("Cannot start the microphone: {}", e.error))?;

        let system = Arc::new(Mutex::new(Some(ChannelWriter::create(
            &folder.join(SYSTEM_PCM),
            MeetingChannel::System,
            self.create_vad(),
            activity.clone(),
            live_tx,
        )?)));

        // Without system audio the meeting still records your side (e.g. an
        // in-person meeting), so a capture failure is not fatal.
        // The meeting app's own audio when known; everything the PC plays if
        // that isn't available (unknown app, or Windows older than 10 2004).
        #[cfg(windows)]
        let loopback = {
            use crate::audio_toolkit::audio::LoopbackCapture;
            let open = |target: Option<u32>| {
                let system = system.clone();
                LoopbackCapture::start(target, move |samples| {
                    if let Some(writer) = system.lock().unwrap().as_mut() {
                        writer.push(samples);
                    }
                })
            };
            let capture = match app_pid {
                Some(pid) => open(Some(pid)).or_else(|e| {
                    warn!("Meeting {id}: app-only audio unavailable ({e}); using all system audio");
                    open(None)
                }),
                None => open(None),
            };
            match capture {
                Ok(capture) => Some(capture),
                Err(e) => {
                    warn!("Meeting {id}: recording without system audio: {e}");
                    None
                }
            }
        };

        Ok(ActiveMeeting {
            id,
            folder: folder.to_path_buf(),
            started_at,
            activity,
            mic,
            #[cfg(windows)]
            loopback,
            system,
            watchdog_stop: Arc::new(AtomicBool::new(false)),
            live,
            screen_stop: self.spawn_screen_reader(id, app_pid, activity_for_screen),
        })
    }

    /// While a meeting from a known app records, read its window every second
    /// for who it shows as speaking. One name at a time is a clear signal;
    /// several (or none) is skipped.
    fn spawn_screen_reader(
        &self,
        id: i64,
        app_pid: Option<u32>,
        activity: Arc<Activity>,
    ) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        #[cfg(windows)]
        if let Some(pid) = app_pid.filter(|_| get_settings(&self.app_handle).meeting_screen_names) {
            let stop = stop.clone();
            let history = self.history.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let names = crate::managers::meeting_screen::speaking_names(pid);
                    if let [name] = names.as_slice() {
                        let t_ms = activity.started.elapsed().as_millis() as i64;
                        if let Ok(conn) = history.get_connection() {
                            let _ = crate::managers::meeting_names::save_highlight(
                                &conn, id, t_ms, name,
                            );
                        }
                    }
                    thread::sleep(SCREEN_POLL);
                }
            });
        }
        #[cfg(not(windows))]
        let _ = (id, app_pid, activity);
        stop
    }

    /// Silero (the dictation default), or Earshot if Silero can't load. Only
    /// used to notice speech for the stop rules, so either is good enough.
    fn create_vad(&self) -> Option<Box<dyn VoiceActivityDetector>> {
        let silero = self
            .app_handle
            .path()
            .resolve(
                "resources/models/silero-vad-v6.2-F32.gguf",
                tauri::path::BaseDirectory::Resource,
            )
            .map_err(anyhow::Error::from)
            .and_then(|path| SileroVad::new(path, SILERO_THRESHOLD));
        match silero {
            Ok(vad) => Some(Box::new(vad)),
            Err(e) => {
                warn!("Meeting VAD: Silero unavailable ({e}); using Earshot");
                EarshotVad::new(EARSHOT_THRESHOLD)
                    .ok()
                    .map(|v| Box::new(v) as Box<dyn VoiceActivityDetector>)
            }
        }
    }

    /// Stop the current meeting. Returns its id, or `None` when nothing was
    /// recording or the recording was discarded (too short / no speech).
    pub fn stop(&self, reason: StopReason) -> Result<Option<i64>> {
        let Some(meeting) = self.active.lock().unwrap().take() else {
            return Ok(None);
        };
        meeting.watchdog_stop.store(true, Ordering::Release);
        meeting.screen_stop.store(true, Ordering::Release);
        let duration = meeting.activity.started.elapsed();
        let heard_speech = meeting.activity.heard_speech();
        let id = meeting.id;
        let folder = meeting.folder.clone();

        match meeting.mic.stop() {
            Ok(stopped) => {
                if stopped.dropped_frames > 0 {
                    warn!(
                        "Meeting {id}: {} mic frames dropped",
                        stopped.dropped_frames
                    );
                }
                if let Err(e) = stopped.sink.0.finish() {
                    error!("Meeting {id}: mic audio flush failed: {e}");
                }
            }
            Err(e) => error!("Meeting {id}: stopping the microphone failed: {e}"),
        }
        #[cfg(windows)]
        if let Some(loopback) = meeting.loopback {
            loopback.stop();
        }
        if let Some(writer) = meeting.system.lock().unwrap().take() {
            if let Err(e) = writer.finish() {
                error!("Meeting {id}: system audio flush failed: {e}");
            }
        }
        info!(
            "Meeting {id} stopped ({reason:?}) after {}s, speech heard: {heard_speech}",
            duration.as_secs()
        );

        let result = if duration < MIN_MEETING || !heard_speech {
            info!("Meeting {id} discarded (too short or no speech)");
            // Let queued live lines land before the row disappears.
            let _ = meeting.live.join();
            self.discard(id, &folder).map(|_| None)
        } else {
            self.finalize(id, &folder, meeting.started_at, duration)
                .map(|_| {
                    spawn_processing(self.app_handle.clone(), id, Some(meeting.live));
                    Some(id)
                })
        };
        self.notify();
        result
    }

    /// Turn the raw PCM into WAV files and mark the meeting ready to process.
    fn finalize(&self, id: i64, folder: &Path, started_at: i64, duration: Duration) -> Result<()> {
        pcm_to_wav(&folder.join(MIC_PCM), &folder.join(MIC_WAV))?;
        let system_pcm = folder.join(SYSTEM_PCM);
        if system_pcm.exists() {
            pcm_to_wav(&system_pcm, &folder.join(SYSTEM_WAV))?;
        }
        let duration_ms = duration.as_millis() as i64;
        self.history.get_connection()?.execute(
            "UPDATE meetings SET status = 'processing', ended_at = ?2, duration_ms = ?3
             WHERE id = ?1",
            params![id, started_at + duration_ms, duration_ms],
        )?;
        Ok(())
    }

    fn discard(&self, id: i64, folder: &Path) -> Result<()> {
        self.history
            .get_connection()?
            .execute("DELETE FROM meetings WHERE id = ?1", params![id])?;
        if let Err(e) = fs::remove_dir_all(folder) {
            warn!("Meeting {id}: could not remove {}: {e}", folder.display());
        }
        Ok(())
    }

    /// Finish meetings left in `recording` by a crash or forced quit, from the
    /// PCM files they wrote. Call once at startup.
    pub fn recover_interrupted(&self) {
        let rows: Vec<(i64, String, i64)> = match self.history.get_connection().and_then(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, folder, started_at FROM meetings WHERE status = 'recording'",
            )?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }) {
            Ok(rows) => rows,
            Err(e) => {
                error!("Meeting recovery: cannot read meetings: {e}");
                return;
            }
        };

        for (id, folder, started_at) in rows {
            let Ok(data_dir) = crate::portable::app_data_dir(&self.app_handle) else {
                return;
            };
            let folder = data_dir.join(folder);
            let mic_bytes = fs::metadata(folder.join(MIC_PCM)).map_or(0, |m| m.len());
            let duration = Duration::from_millis(mic_bytes / 2 * 1000 / WHISPER_SAMPLE_RATE as u64);
            let result = if duration < MIN_MEETING {
                self.discard(id, &folder)
            } else {
                self.finalize(id, &folder, started_at, duration)
            };
            match result {
                Ok(()) => info!(
                    "Recovered interrupted meeting {id} ({}s)",
                    duration.as_secs()
                ),
                Err(e) => {
                    error!("Meeting recovery failed for {id}: {e}");
                    if let Ok(conn) = self.history.get_connection() {
                        let _ = conn.execute(
                            "UPDATE meetings SET status = 'failed', error = ?2 WHERE id = ?1",
                            params![id, e.to_string()],
                        );
                    }
                }
            }
        }
    }

    /// Delete audio of finished meetings older than the retention setting
    /// (transcripts and notes are kept).
    pub fn cleanup_old_audio(&self) {
        let days = get_settings(&self.app_handle).meeting_audio_retention_days;
        if days == 0 {
            return;
        }
        let cutoff = Local::now().timestamp_millis() - days as i64 * 24 * 3600 * 1000;
        let folders = match self
            .history
            .get_connection()
            .and_then(|conn| meeting_store::audio_expired_folders(&conn, cutoff))
        {
            Ok(folders) => folders,
            Err(e) => {
                error!("Meeting audio cleanup: {e}");
                return;
            }
        };
        let Ok(data_dir) = crate::portable::app_data_dir(&self.app_handle) else {
            return;
        };
        for folder in folders {
            for name in [MIC_WAV, SYSTEM_WAV] {
                let path = data_dir.join(&folder).join(name);
                if path.exists() {
                    match fs::remove_file(&path) {
                        Ok(()) => info!("Removed old meeting audio {}", path.display()),
                        Err(e) => warn!("Could not remove {}: {e}", path.display()),
                    }
                }
            }
        }
    }

    /// Enforce the maximum length and the long-silence stop.
    fn spawn_watchdog(&self) {
        let (activity, stop) = {
            let active = self.active.lock().unwrap();
            let Some(m) = active.as_ref() else {
                return;
            };
            (m.activity.clone(), m.watchdog_stop.clone())
        };
        let app = self.app_handle.clone();
        let max = Duration::from_secs(get_settings(&app).meeting_max_minutes.max(1) as u64 * 60);
        thread::spawn(move || {
            let mut warned = false;
            while !stop.load(Ordering::Acquire) {
                thread::sleep(WATCHDOG_TICK);
                let elapsed = activity.started.elapsed();
                let reason = if elapsed >= max {
                    Some(StopReason::MaxLength)
                } else if activity.silence().is_some_and(|s| s >= SILENCE_STOP) {
                    Some(StopReason::Silence)
                } else {
                    None
                };
                if !warned && elapsed + MAX_LENGTH_WARNING >= max && reason.is_none() {
                    warned = true;
                    let _ = app.emit(WARNING_EVENT, MAX_LENGTH_WARNING.as_secs());
                }
                if let Some(reason) = reason {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Some(manager) = app.try_state::<Arc<MeetingManager>>() {
                        if let Err(e) = manager.stop(reason) {
                            error!("Automatic meeting stop failed: {e}");
                        }
                    }
                    break;
                }
            }
        });
    }

    /// Tell the UI and the tray that recording started or stopped.
    fn notify(&self) {
        let _ = self.app_handle.emit(STATUS_EVENT, self.status());
        crate::tray::sync_tray(&self.app_handle);
    }
}

/// Event sent when starting or stopping a meeting fails, with the message.
pub const ERROR_EVENT: &str = "meeting-error";

/// Start a meeting if none is recording, otherwise stop it. Used by the tray,
/// the `--toggle-meeting` remote flag and (later) the shortcut. Runs off the
/// caller's thread: opening audio devices can take a moment.
pub fn toggle_meeting(app: &AppHandle) {
    let app = app.clone();
    thread::spawn(move || {
        let Some(manager) = app.try_state::<Arc<MeetingManager>>() else {
            return;
        };
        let result = if manager.is_recording() {
            manager.stop(StopReason::Manual).map(|_| ())
        } else {
            manager.start(None).map(|_| ())
        };
        if let Err(e) = result {
            error!("Meeting toggle failed: {e}");
            let _ = app.emit(ERROR_EVENT, e.to_string());
        }
    });
}

impl ActiveMeeting {
    fn has_system_audio(&self) -> bool {
        #[cfg(windows)]
        return self.loopback.is_some();
        #[cfg(not(windows))]
        return false;
    }
}

/// Wrap raw 16 kHz mono 16-bit PCM in a WAV file, then delete the PCM.
fn pcm_to_wav(pcm: &Path, wav: &Path) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: WHISPER_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut reader = BufReader::new(File::open(pcm)?);
    let mut writer = hound::WavWriter::create(wav, spec)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        // A crash can leave half a sample at the end; drop it.
        for pair in buf[..n - n % 2].chunks_exact(2) {
            writer.write_sample(i16::from_le_bytes([pair[0], pair[1]]))?;
        }
    }
    writer.finalize()?;
    fs::remove_file(pcm)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn pcm_converts_to_wav_and_is_removed() {
        let tmp = TempDir::new().unwrap();
        let pcm = tmp.path().join("a.pcm");
        let wav = tmp.path().join("a.wav");
        let samples: [i16; 4] = [0, 1000, -1000, i16::MAX];
        let mut bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        bytes.push(7); // half a sample left by a crash
        fs::write(&pcm, bytes).unwrap();

        pcm_to_wav(&pcm, &wav).unwrap();

        assert!(!pcm.exists());
        let read: Vec<i16> = hound::WavReader::open(&wav)
            .unwrap()
            .samples::<i16>()
            .map(|s| s.unwrap())
            .collect();
        assert_eq!(read, samples);
    }

    #[test]
    fn silence_is_measured_from_the_last_speech() {
        let activity = Activity {
            started: Instant::now() - Duration::from_secs(30),
            last_speech: AtomicU64::new(0),
        };
        assert!(!activity.heard_speech());
        assert_eq!(activity.silence(), None);
        activity.last_speech.store(10_001, Ordering::Relaxed);
        let silence = activity.silence().unwrap();
        assert!(silence >= Duration::from_secs(19) && silence <= Duration::from_secs(21));
    }
}
