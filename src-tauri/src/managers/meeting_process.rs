//! The after-meeting pass: a clean transcript with speaker labels.
//!
//! 1. Transcribe both channels with word timing (in chunks cut at pauses, so
//!    long meetings never exceed a model's input limit).
//! 2. Separate the other people's voices (Sortformer) and label their words.
//! 3. Drop mic lines that are just the speakers' sound picked up again by the
//!    microphone (no echo cancellation without headphones).
//! 4. Replace the live lines with these `final` segments and mark the meeting
//!    complete.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Result};
use log::{error, info};
use rusqlite::params;
use tauri::{AppHandle, Emitter, Manager};

use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use crate::managers::history::HistoryManager;
use crate::managers::meeting::{MIC_WAV, SYSTEM_WAV};
use crate::managers::meeting_names;
use crate::managers::speakers::{label_segments, LabelledSegment, SpeakerManager};
use crate::managers::transcription::{TimedText, TranscriptionManager};

/// Event sent with the meeting id when processing finished (or failed).
pub const PROCESSED_EVENT: &str = "meeting-processed";

/// Longest audio handed to the transcription model in one call.
const CHUNK_MS: usize = 30_000;
/// Chunk boundaries are moved to the quietest spot in this final stretch.
const CUT_SEARCH_MS: usize = 5_000;
const CUT_WINDOW_MS: usize = 100;
/// A mic line counts as echo when this share of its words was also heard,
/// at about the same time, on the system channel.
const ECHO_WORD_SHARE: f32 = 0.6;
const ECHO_TIME_SLACK_MS: i64 = 1_500;

const fn ms_to_samples(ms: usize) -> usize {
    ms * WHISPER_SAMPLE_RATE as usize / 1000
}

/// Process meeting `id` on a background thread, after `wait_for` (the live
/// transcriber) has finished so the two don't compete for the model.
pub fn spawn_processing(app: AppHandle, id: i64, wait_for: Option<JoinHandle<()>>) {
    thread::spawn(move || {
        if let Some(live) = wait_for {
            let _ = live.join();
        }
        let history = app.state::<Arc<HistoryManager>>();
        match process(&app, id) {
            Ok(()) => info!("Meeting {id} processed"),
            Err(e) => {
                error!("Meeting {id} processing failed: {e}");
                if let Ok(conn) = history.get_connection() {
                    let _ = conn.execute(
                        "UPDATE meetings SET status = 'failed', error = ?2 WHERE id = ?1",
                        params![id, e.to_string()],
                    );
                }
            }
        }
        let _ = app.emit(PROCESSED_EVENT, id);
    });
}

/// Restart processing for meetings a quit interrupted. Call once at startup,
/// after interrupted recordings were recovered.
pub fn resume_pending(app: &AppHandle) {
    let ids: Vec<i64> = app
        .state::<Arc<HistoryManager>>()
        .get_connection()
        .and_then(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM meetings WHERE status = 'processing'")?;
            let ids = stmt
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok(ids)
        })
        .unwrap_or_default();
    for id in ids {
        info!("Resuming processing of meeting {id}");
        spawn_processing(app.clone(), id, None);
    }
}

fn process(app: &AppHandle, id: i64) -> Result<()> {
    let history = app.state::<Arc<HistoryManager>>();
    let folder: String = history.get_connection()?.query_row(
        "SELECT folder FROM meetings WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    let folder = crate::portable::app_data_dir(app)?.join(folder);
    let tm = app.state::<Arc<TranscriptionManager>>();
    let sm = app.state::<Arc<SpeakerManager>>();

    // Your side: one speaker, split into paragraphs at pauses.
    let mic_audio = read_wav(&folder.join(MIC_WAV))?;
    let mic_words = transcribe_chunked(&tm, &mic_audio)?;
    let mut mic = label_segments(&mic_words, &[]);

    // The other people: separate their voices, then label the words.
    let system_path = folder.join(SYSTEM_WAV);
    let mut turns = Vec::new();
    let system = if system_path.exists() {
        let audio = read_wav(&system_path)?;
        let words = transcribe_chunked(&tm, &audio)?;
        if !words.is_empty() {
            tauri::async_runtime::block_on(sm.ensure_model())?;
            turns = sm.diarize(audio, None)?;
        }
        label_segments(&words, &turns)
    } else {
        Vec::new()
    };

    let before = mic.len();
    mic.retain(|m| !is_echo(m, &system));
    if mic.len() < before {
        info!(
            "Meeting {id}: dropped {} echoed mic lines",
            before - mic.len()
        );
    }

    save_final(&history, id, &mic, &system)?;

    // Real names from the meeting window, where it showed who was speaking.
    let conn = history.get_connection()?;
    let names = meeting_names::assign_names(&meeting_names::highlights(&conn, id)?, &turns);
    if !names.is_empty() {
        info!(
            "Meeting {id}: {} speaker name(s) from the meeting window",
            names.len()
        );
        meeting_names::save_screen_names(&conn, id, &names)?;
    }
    Ok(())
}

/// Transcribe in chunks of at most [`CHUNK_MS`], cut at pauses, returning
/// timed words (or segments) on the whole recording's timeline.
fn transcribe_chunked(tm: &TranscriptionManager, audio: &[f32]) -> Result<Vec<TimedText>> {
    let mut out = Vec::new();
    for (start, end) in chunk_bounds(audio) {
        tm.initiate_model_load();
        let result = tm.transcribe_detailed(audio[start..end].to_vec())?;
        let offset = (start * 1000 / WHISPER_SAMPLE_RATE as usize) as i64;
        if result.segments.is_empty() {
            // A model without timestamps: the chunk becomes one segment.
            if !result.text.trim().is_empty() {
                out.push(TimedText {
                    t0_ms: offset,
                    t1_ms: offset + ((end - start) * 1000 / WHISPER_SAMPLE_RATE as usize) as i64,
                    text: result.text,
                });
            }
        } else {
            out.extend(result.segments.into_iter().map(|s| TimedText {
                t0_ms: s.t0_ms + offset,
                t1_ms: s.t1_ms + offset,
                text: s.text,
            }));
        }
    }
    Ok(out)
}

/// `[start, end)` sample ranges of at most [`CHUNK_MS`], each cut at the
/// quietest [`CUT_WINDOW_MS`] window of its last [`CUT_SEARCH_MS`].
fn chunk_bounds(audio: &[f32]) -> Vec<(usize, usize)> {
    let max = ms_to_samples(CHUNK_MS);
    let search = ms_to_samples(CUT_SEARCH_MS);
    let window = ms_to_samples(CUT_WINDOW_MS);
    let mut bounds = Vec::new();
    let mut start = 0;
    while start < audio.len() {
        let mut end = (start + max).min(audio.len());
        if end < audio.len() {
            let from = end - search;
            let quietest = (from..end - window)
                .step_by(window)
                .min_by(|&a, &b| {
                    energy(&audio[a..a + window]).total_cmp(&energy(&audio[b..b + window]))
                })
                .unwrap_or(end - window);
            end = quietest + window / 2;
        }
        bounds.push((start, end));
        start = end;
    }
    bounds
}

fn energy(samples: &[f32]) -> f32 {
    samples.iter().map(|s| s * s).sum()
}

/// A mic line is echo when most of its words were heard at about the same
/// time on the system channel.
fn is_echo(mic: &LabelledSegment, system: &[LabelledSegment]) -> bool {
    is_echo_text(
        (mic.t0_ms, mic.t1_ms, &mic.text),
        system.iter().map(|s| (s.t0_ms, s.t1_ms, s.text.as_str())),
    )
}

/// [`is_echo`] on plain `(t0_ms, t1_ms, text)` lines; the live transcript
/// uses it too.
pub(crate) fn is_echo_text<'a>(
    mic: (i64, i64, &str),
    system: impl IntoIterator<Item = (i64, i64, &'a str)>,
) -> bool {
    let (t0, t1, text) = mic;
    let mine = words(text);
    if mine.is_empty() {
        return false;
    }
    let mut heard: HashSet<String> = HashSet::new();
    for (s0, s1, s_text) in system {
        if s0 <= t1 + ECHO_TIME_SLACK_MS && t0 <= s1 + ECHO_TIME_SLACK_MS {
            heard.extend(words(s_text));
        }
    }
    let shared = mine.iter().filter(|w| heard.contains(*w)).count();
    shared as f32 / mine.len() as f32 >= ECHO_WORD_SHARE
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Speaker key stored for a labelled line: "you" for the mic, "s<N>" for a
/// separated voice, "others" when no voice could be attributed.
fn speaker_key(channel_is_mic: bool, speaker: Option<u32>) -> String {
    match (channel_is_mic, speaker) {
        (true, _) => "you".to_string(),
        (false, Some(n)) => format!("s{n}"),
        (false, None) => "others".to_string(),
    }
}

fn default_name(key: &str) -> String {
    match key {
        "you" => "You".to_string(),
        "others" => "Others".to_string(),
        k => format!("Speaker {}", k.trim_start_matches('s')),
    }
}

fn save_final(
    history: &HistoryManager,
    id: i64,
    mic: &[LabelledSegment],
    system: &[LabelledSegment],
) -> Result<()> {
    let mut conn = history.get_connection()?;
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM meeting_segments WHERE meeting_id = ?1 AND kind = 'final'",
        params![id],
    )?;
    let mut keys = HashSet::new();
    for (channel, rows) in [("mic", mic), ("system", system)] {
        for s in rows {
            let key = speaker_key(channel == "mic", s.speaker);
            tx.execute(
                "INSERT INTO meeting_segments
                 (meeting_id, kind, channel, speaker_key, t0_ms, t1_ms, text)
                 VALUES (?1, 'final', ?2, ?3, ?4, ?5, ?6)",
                params![id, channel, key, s.t0_ms, s.t1_ms, s.text],
            )?;
            keys.insert(key);
        }
    }
    // Names the user already set (manual renames) are kept.
    for key in keys {
        tx.execute(
            "INSERT OR IGNORE INTO meeting_speakers (meeting_id, speaker_key, display_name, source)
             VALUES (?1, ?2, ?3, 'auto')",
            params![id, key, default_name(&key)],
        )?;
    }
    let updated = tx.execute(
        "UPDATE meetings SET status = 'complete', error = NULL WHERE id = ?1",
        params![id],
    )?;
    if updated == 0 {
        return Err(anyhow!("Meeting {id} no longer exists"));
    }
    tx.commit()?;
    Ok(())
}

fn read_wav(path: &Path) -> Result<Vec<f32>> {
    crate::audio_toolkit::read_wav_samples(path)
        .map_err(|e| anyhow!("Cannot read {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(t0: i64, t1: i64, text: &str) -> LabelledSegment {
        LabelledSegment {
            speaker: None,
            t0_ms: t0,
            t1_ms: t1,
            text: text.to_string(),
        }
    }

    #[test]
    fn mic_line_repeating_the_speakers_is_echo() {
        let system = [line(10_000, 14_000, "Can we add the gift card option?")];
        assert!(is_echo(
            &line(10_200, 14_100, "can we add the gift card"),
            &system
        ));
        assert!(!is_echo(
            &line(10_200, 14_100, "Sure, next week works"),
            &system
        ));
        // Same words much later is a real reply, not echo.
        assert!(!is_echo(&line(40_000, 44_000, "gift card option"), &system));
    }

    #[test]
    fn chunks_cover_the_audio_and_cut_in_quiet_spots() {
        let rate = WHISPER_SAMPLE_RATE as usize;
        let mut audio = vec![0.5f32; rate * 70];
        // A silent gap at 28 s, inside the first chunk's search stretch.
        audio[28 * rate..28 * rate + rate / 5].fill(0.0);
        let bounds = chunk_bounds(&audio);
        assert_eq!(bounds.first().unwrap().0, 0);
        assert_eq!(bounds.last().unwrap().1, audio.len());
        assert!(bounds.windows(2).all(|w| w[0].1 == w[1].0));
        let first_cut = bounds[0].1 as f32 / rate as f32;
        assert!((28.0..28.2).contains(&first_cut), "cut at {first_cut}");
        assert!(bounds.iter().all(|(s, e)| e - s <= ms_to_samples(CHUNK_MS)));
    }

    #[test]
    fn speaker_keys_and_names() {
        assert_eq!(speaker_key(true, Some(2)), "you");
        assert_eq!(speaker_key(false, Some(2)), "s2");
        assert_eq!(speaker_key(false, None), "others");
        assert_eq!(default_name("s3"), "Speaker 3");
        assert_eq!(default_name("you"), "You");
    }
}
