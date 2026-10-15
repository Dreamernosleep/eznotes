//! Speaker diarization ("who spoke when") for meetings.
//!
//! The diarizer (Sortformer, from the catalog's `diarization` role) runs in its
//! own [`EngineSupervisor`] worker, so it never evicts the dictation model. It
//! is loaded only for a run and unloaded right after: meetings are occasional
//! and the model is not worth keeping in memory.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use log::info;
use serde::Serialize;
use tauri::AppHandle;
use transcribe_cpp::{
    DiarizeExtension, DiarizeOptions, SortformerDiarizeOptions, SortformerPreset,
};

use crate::engine_supervisor::{EngineSupervisor, LoadSpec};
use crate::managers::model::ModelManager;
use crate::managers::transcription::{
    transcribe_gpu_disabled_for_host, transcribe_load_target, TimedText,
};

/// One speaker turn: speaker `speaker` (1-based) talks from `t0_ms` to `t1_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SpeakerTurn {
    pub speaker: u32,
    pub t0_ms: i64,
    pub t1_ms: i64,
}

/// Transcript text attributed to a speaker. `speaker` is `None` when no turn
/// overlaps the text (shown as "Unknown").
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LabelledSegment {
    pub speaker: Option<u32>,
    pub t0_ms: i64,
    pub t1_ms: i64,
    pub text: String,
}

pub struct SpeakerManager {
    engine: EngineSupervisor,
    model_manager: Arc<ModelManager>,
    app_handle: AppHandle,
    /// Runs are serialised: one diarizer load at a time.
    run_lock: Mutex<()>,
}

impl SpeakerManager {
    pub fn new(app_handle: &AppHandle, model_manager: Arc<ModelManager>) -> Self {
        Self {
            engine: EngineSupervisor::new(!transcribe_gpu_disabled_for_host()),
            model_manager,
            app_handle: app_handle.clone(),
            run_lock: Mutex::new(()),
        }
    }

    /// Download the diarization model if it isn't on disk yet.
    pub async fn ensure_model(&self) -> Result<PathBuf> {
        self.model_manager.download_diarizer().await
    }

    /// Speaker turns for 16 kHz mono PCM, in time order. `device_index` is
    /// the --device-index override; `None` follows the accelerator setting.
    pub fn diarize(&self, pcm: Vec<f32>, device_index: Option<usize>) -> Result<Vec<SpeakerTurn>> {
        let _guard = self.run_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self
            .model_manager
            .diarizer_path()
            .ok_or_else(|| anyhow::anyhow!("The speaker model is not downloaded yet"))?;

        let (backend, device) = transcribe_load_target(&self.app_handle, device_index);
        let loaded = self
            .engine
            .load(LoadSpec {
                path,
                backend,
                device,
            })
            .map_err(|e| anyhow::anyhow!("Failed to load the speaker model: {}", e))?;
        info!(
            "Speaker model loaded on {} ({})",
            loaded.device, loaded.backend
        );

        // The offline operating point: the whole recording is available.
        let options = DiarizeOptions {
            family: Some(DiarizeExtension::Sortformer(SortformerDiarizeOptions {
                preset: Some(SortformerPreset::VeryHighLatency),
            })),
        };
        let result = self.engine.diarize(pcm, options);
        self.engine.unload();

        let turns = result.map_err(|e| anyhow::anyhow!("Speaker detection failed: {}", e))?;
        Ok(turns
            .into_iter()
            .filter(|t| t.speaker_id > 0 && t.t1_ms > t.t0_ms)
            .map(|t| SpeakerTurn {
                speaker: t.speaker_id as u32,
                t0_ms: t.t0_ms,
                t1_ms: t.t1_ms,
            })
            .collect())
    }
}

/// Items up to this long are treated as words: attributed by where they
/// start, because models stretch a word's end across the pause after it.
const WORD_MAX_MS: i64 = 2_000;
/// A word starting this close to a turn (in a gap between turns) still
/// belongs to it.
const NEAR_TURN_MS: i64 = 500;
/// A pause longer than this starts a new paragraph, even for the same speaker.
const PARAGRAPH_GAP_MS: i64 = 1_500;

/// Attribute each transcript item to a speaker, then merge consecutive items
/// of the same speaker into one turn. Short items (words) take the speaker
/// talking where they start; longer segments take the speaker whose turns
/// overlap them the most. Items no turn explains get `None`.
pub fn label_segments(segments: &[TimedText], turns: &[SpeakerTurn]) -> Vec<LabelledSegment> {
    let mut out: Vec<LabelledSegment> = Vec::new();
    for segment in segments {
        let speaker = if segment.t1_ms - segment.t0_ms <= WORD_MAX_MS {
            speaker_at(segment.t0_ms, turns)
                .or_else(|| dominant_speaker(segment.t0_ms, segment.t1_ms, turns))
        } else {
            dominant_speaker(segment.t0_ms, segment.t1_ms, turns)
        };
        let text = segment.text.trim();
        if text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last)
                if last.speaker == speaker && segment.t0_ms - last.t1_ms <= PARAGRAPH_GAP_MS =>
            {
                last.t1_ms = last.t1_ms.max(segment.t1_ms);
                // Punctuation tokens attach to the previous word.
                if text.starts_with(|c: char| c.is_alphanumeric()) {
                    last.text.push(' ');
                }
                last.text.push_str(text);
            }
            _ => out.push(LabelledSegment {
                speaker,
                t0_ms: segment.t0_ms,
                t1_ms: segment.t1_ms,
                text: text.to_string(),
            }),
        }
    }
    out
}

/// The speaker talking at `t`: the turn containing it, else the nearest turn
/// within [`NEAR_TURN_MS`]. With overlapping turns, the one that started last
/// (the speaker who just came in) wins.
fn speaker_at(t: i64, turns: &[SpeakerTurn]) -> Option<u32> {
    if let Some(turn) = turns
        .iter()
        .filter(|turn| turn.t0_ms <= t && t < turn.t1_ms)
        .max_by_key(|turn| turn.t0_ms)
    {
        return Some(turn.speaker);
    }
    turns
        .iter()
        .map(|turn| {
            let gap = if t < turn.t0_ms {
                turn.t0_ms - t
            } else {
                t - turn.t1_ms
            };
            (gap, turn.speaker)
        })
        .filter(|(gap, _)| *gap <= NEAR_TURN_MS)
        .min_by_key(|(gap, _)| *gap)
        .map(|(_, speaker)| speaker)
}

/// The speaker with the most total overlap with `[t0, t1)`, if any.
fn dominant_speaker(t0: i64, t1: i64, turns: &[SpeakerTurn]) -> Option<u32> {
    let mut totals: Vec<(u32, i64)> = Vec::new();
    for turn in turns {
        let overlap = t1.min(turn.t1_ms) - t0.max(turn.t0_ms);
        if overlap <= 0 {
            continue;
        }
        match totals.iter_mut().find(|(s, _)| *s == turn.speaker) {
            Some((_, total)) => *total += overlap,
            None => totals.push((turn.speaker, overlap)),
        }
    }
    // Ties go to the lower speaker id, so the result is deterministic.
    totals
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(speaker, _)| speaker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(t0: i64, t1: i64, text: &str) -> TimedText {
        TimedText {
            t0_ms: t0,
            t1_ms: t1,
            text: text.to_string(),
        }
    }

    fn turn(speaker: u32, t0: i64, t1: i64) -> SpeakerTurn {
        SpeakerTurn {
            speaker,
            t0_ms: t0,
            t1_ms: t1,
        }
    }

    #[test]
    fn picks_the_speaker_with_most_overlap() {
        let turns = [turn(1, 0, 1_000), turn(2, 1_000, 5_000)];
        let out = label_segments(&[seg(800, 3_000, "hello")], &turns);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speaker, Some(2));
    }

    #[test]
    fn segment_without_overlap_is_unknown() {
        let turns = [turn(1, 0, 1_000)];
        let out = label_segments(&[seg(2_000, 3_000, "later")], &turns);
        assert_eq!(out[0].speaker, None);
    }

    #[test]
    fn merges_consecutive_segments_of_the_same_speaker() {
        let turns = [turn(1, 0, 4_000), turn(2, 4_000, 8_000)];
        let segments = [
            seg(0, 1_500, "Hi there."),
            seg(1_500, 3_900, "How are you?"),
            seg(4_100, 6_000, "Good, thanks."),
            seg(6_000, 7_000, ""),
        ];
        let out = label_segments(&segments, &turns);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].speaker, Some(1));
        assert_eq!(out[0].text, "Hi there. How are you?");
        assert_eq!((out[0].t0_ms, out[0].t1_ms), (0, 3_900));
        assert_eq!(out[1].speaker, Some(2));
        assert_eq!(out[1].text, "Good, thanks.");
    }

    #[test]
    fn word_belongs_to_whoever_was_talking_when_it_started() {
        // "mobile" starts at the end of speaker 2's turn, but the model stretched
        // its end through the pause into speaker 1's turn.
        let turns = [turn(2, 10_480, 12_880), turn(1, 14_320, 15_120)];
        let words = [
            seg(12_100, 12_640, "on"),
            seg(12_640, 14_400, "mobile."),
            seg(14_400, 14_900, "Good"),
        ];
        let out = label_segments(&words, &turns);
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].speaker, out[0].text.as_str()),
            (Some(2), "on mobile.")
        );
        assert_eq!((out[1].speaker, out[1].text.as_str()), (Some(1), "Good"));
    }

    #[test]
    fn long_pause_starts_a_new_paragraph() {
        let words = [seg(0, 500, "Hello."), seg(4_000, 4_500, "Anyway")];
        let out = label_segments(&words, &[]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| s.speaker.is_none()));
    }

    #[test]
    fn joins_words_and_attaches_punctuation() {
        let turns = [turn(1, 0, 2_000)];
        let words = [
            seg(0, 300, "Hi"),
            seg(300, 400, ","),
            seg(400, 900, "Sarah"),
        ];
        let out = label_segments(&words, &turns);
        assert_eq!(out[0].text, "Hi, Sarah");
    }

    #[test]
    fn overlap_is_summed_per_speaker() {
        // Speaker 1 has two short turns that together beat speaker 2's one.
        let turns = [turn(1, 0, 400), turn(2, 400, 900), turn(1, 900, 1_400)];
        let out = label_segments(&[seg(0, 1_400, "x")], &turns);
        assert_eq!(out[0].speaker, Some(1));
    }
}
