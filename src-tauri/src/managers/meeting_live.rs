//! Live meeting transcript: each channel is cut into utterances by the speech
//! detector, and every utterance is transcribed with the dictation model while
//! the meeting goes on. Results are saved as `live` segments and sent to the
//! UI as they arrive. The after-meeting pass (with speaker labels) replaces
//! them with `final` segments.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use log::{debug, error, warn};
use rusqlite::params;
use serde::Serialize;
use specta::Type;
use tauri::{AppHandle, Emitter, Manager};

use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use crate::managers::history::HistoryManager;
use crate::managers::meeting_process::is_echo_text;
use crate::managers::transcription::TranscriptionManager;

/// Event carrying one [`LiveSegment`].
pub const SEGMENT_EVENT: &str = "meeting-segment";

/// Speech kept from before the detector fires, so first syllables survive.
const PREFILL_MS: u64 = 300;
/// An utterance ends after this much silence.
const END_SILENCE_MS: u64 = 700;
/// Long monologues are cut so text keeps flowing.
const MAX_UTTERANCE_MS: u64 = 15_000;
/// Utterances with less voiced audio than this are noise (coughs, clicks).
const MIN_VOICED_MS: u64 = 250;
/// Trailing silence left on an utterance.
const KEEP_TAIL_MS: u64 = 200;

const fn samples(ms: u64) -> usize {
    (ms * WHISPER_SAMPLE_RATE as u64 / 1000) as usize
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum MeetingChannel {
    /// Your microphone.
    Mic,
    /// The PC's audio output: the other people.
    System,
}

impl MeetingChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
        }
    }

    /// Speaker key before speaker separation has run.
    pub fn default_speaker(self) -> &'static str {
        match self {
            Self::Mic => "you",
            Self::System => "others",
        }
    }
}

/// A finished utterance: audio plus its position in the meeting.
#[derive(Debug)]
pub struct Utterance {
    pub channel: MeetingChannel,
    pub t0_ms: i64,
    pub t1_ms: i64,
    pub samples: Vec<f32>,
}

/// One live transcript line, as stored and as sent to the UI.
#[derive(Debug, Clone, Serialize, Type)]
pub struct LiveSegment {
    pub meeting_id: i64,
    pub channel: MeetingChannel,
    pub speaker_key: String,
    pub t0_ms: i64,
    pub t1_ms: i64,
    pub text: String,
}

/// Cuts a channel's detector frames into utterances.
pub struct UtteranceCutter {
    channel: MeetingChannel,
    /// Samples consumed so far (the channel's timeline).
    pos: u64,
    prefill: VecDeque<f32>,
    current: Vec<f32>,
    start: u64,
    voiced: usize,
    silence: usize,
}

impl UtteranceCutter {
    pub fn new(channel: MeetingChannel) -> Self {
        Self {
            channel,
            pos: 0,
            prefill: VecDeque::with_capacity(samples(PREFILL_MS)),
            current: Vec::new(),
            start: 0,
            voiced: 0,
            silence: 0,
        }
    }

    /// Feed one detector frame and whether it was speech. Returns an
    /// utterance when one just ended.
    pub fn push_frame(&mut self, frame: &[f32], voiced: bool) -> Option<Utterance> {
        let frame_start = self.pos;
        self.pos += frame.len() as u64;

        if self.current.is_empty() {
            if !voiced {
                self.prefill.extend(frame);
                let excess = self.prefill.len().saturating_sub(samples(PREFILL_MS));
                self.prefill.drain(..excess);
                return None;
            }
            self.start = frame_start - self.prefill.len() as u64;
            self.current.extend(self.prefill.drain(..));
        }

        self.current.extend_from_slice(frame);
        if voiced {
            self.voiced += frame.len();
            self.silence = 0;
        } else {
            self.silence += frame.len();
        }

        if self.silence >= samples(END_SILENCE_MS)
            || self.current.len() >= samples(MAX_UTTERANCE_MS)
        {
            return self.take();
        }
        None
    }

    /// End of the recording: whatever is in progress.
    pub fn flush(&mut self) -> Option<Utterance> {
        self.take()
    }

    fn take(&mut self) -> Option<Utterance> {
        if self.current.is_empty() {
            return None;
        }
        let trim = self.silence.saturating_sub(samples(KEEP_TAIL_MS));
        let keep = self.current.len() - trim.min(self.current.len());
        let mut audio = std::mem::take(&mut self.current);
        audio.truncate(keep);
        let voiced = std::mem::replace(&mut self.voiced, 0);
        self.silence = 0;
        if voiced < samples(MIN_VOICED_MS) {
            return None;
        }
        let rate = WHISPER_SAMPLE_RATE as i64;
        let t0_ms = self.start as i64 * 1000 / rate;
        Some(Utterance {
            channel: self.channel,
            t0_ms,
            t1_ms: t0_ms + audio.len() as i64 * 1000 / rate,
            samples: audio,
        })
    }
}

/// Mic lines wait this long for the system line they might be echoing.
const ECHO_HOLD: Duration = Duration::from_secs(3);
/// System lines kept to compare new mic lines against.
const RECENT_SYSTEM: usize = 20;

/// Transcribe utterances as they arrive, until every sender is dropped (the
/// meeting stopped). Join the handle before the after-meeting pass.
///
/// Without headphones the mic also hears the speakers, so a mic line is held
/// for [`ECHO_HOLD`] and dropped if the system channel said the same thing.
pub fn spawn_transcriber(
    app: AppHandle,
    meeting_id: i64,
    utterances: mpsc::Receiver<Utterance>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut held_mic: Vec<(Instant, LiveSegment)> = Vec::new();
        let mut recent_system: VecDeque<LiveSegment> = VecDeque::new();
        let mut open = true;
        while open {
            match utterances.recv_timeout(Duration::from_millis(500)) {
                Ok(u) => {
                    if let Some(segment) = transcribe(&app, meeting_id, u) {
                        match segment.channel {
                            MeetingChannel::System => {
                                held_mic.retain(|(_, mic)| !echoes(mic, [&segment]));
                                publish(&app, &segment);
                                if recent_system.len() == RECENT_SYSTEM {
                                    recent_system.pop_front();
                                }
                                recent_system.push_back(segment);
                            }
                            MeetingChannel::Mic => {
                                if !echoes(&segment, recent_system.iter()) {
                                    held_mic.push((Instant::now(), segment));
                                }
                            }
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => open = false,
            }
            let now = Instant::now();
            held_mic.retain(|(at, mic)| {
                let due = !open || now.duration_since(*at) >= ECHO_HOLD;
                if due {
                    publish(&app, mic);
                }
                !due
            });
        }
    })
}

fn echoes<'a>(mic: &LiveSegment, system: impl IntoIterator<Item = &'a LiveSegment>) -> bool {
    is_echo_text(
        (mic.t0_ms, mic.t1_ms, &mic.text),
        system
            .into_iter()
            .map(|s| (s.t0_ms, s.t1_ms, s.text.as_str())),
    )
}

fn transcribe(app: &AppHandle, meeting_id: i64, u: Utterance) -> Option<LiveSegment> {
    let tm = app.state::<Arc<TranscriptionManager>>();
    // Waits for an in-progress load; starts one if the model was unloaded
    // (e.g. after the idle timeout).
    tm.initiate_model_load();
    let text = match tm.transcribe(u.samples) {
        Ok(text) => text.trim().to_string(),
        Err(e) => {
            warn!("Meeting {meeting_id}: live transcription failed: {e}");
            return None;
        }
    };
    (!text.is_empty()).then(|| LiveSegment {
        meeting_id,
        channel: u.channel,
        speaker_key: u.channel.default_speaker().to_string(),
        t0_ms: u.t0_ms,
        t1_ms: u.t1_ms,
        text,
    })
}

fn publish(app: &AppHandle, segment: &LiveSegment) {
    if let Err(e) = save_live_segment(app, segment) {
        error!(
            "Meeting {}: saving live segment failed: {e}",
            segment.meeting_id
        );
    }
    debug!(
        "Meeting {} [{}] {}",
        segment.meeting_id,
        segment.channel.as_str(),
        crate::utils::redact_text(&segment.text)
    );
    let _ = app.emit(SEGMENT_EVENT, segment);
}

fn save_live_segment(app: &AppHandle, s: &LiveSegment) -> anyhow::Result<()> {
    app.state::<Arc<HistoryManager>>()
        .get_connection()?
        .execute(
        "INSERT INTO meeting_segments (meeting_id, kind, channel, speaker_key, t0_ms, t1_ms, text)
         VALUES (?1, 'live', ?2, ?3, ?4, ?5, ?6)",
        params![
            s.meeting_id,
            s.channel.as_str(),
            s.speaker_key,
            s.t0_ms,
            s.t1_ms,
            s.text
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 512; // 32 ms

    fn feed(cutter: &mut UtteranceCutter, ms: u64, voiced: bool) -> Vec<Utterance> {
        let frames = samples(ms) / FRAME;
        let frame = vec![0.1f32; FRAME];
        (0..frames)
            .filter_map(|_| cutter.push_frame(&frame, voiced))
            .collect()
    }

    #[test]
    fn utterance_ends_after_silence_with_prefill() {
        let mut c = UtteranceCutter::new(MeetingChannel::Mic);
        assert!(feed(&mut c, 1_000, false).is_empty());
        assert!(feed(&mut c, 1_000, true).is_empty());
        let out = feed(&mut c, 1_000, false);
        assert_eq!(out.len(), 1);
        let u = &out[0];
        // Starts ~300 ms before the speech (prefill), at ~700 ms into the audio.
        assert!((680..=710).contains(&u.t0_ms), "t0 {}", u.t0_ms);
        // Prefill + 1 s of speech + ~200 ms of kept tail.
        let len = u.t1_ms - u.t0_ms;
        assert!((1_450..=1_560).contains(&len), "len {len}");
    }

    #[test]
    fn short_noise_bursts_are_dropped() {
        let mut c = UtteranceCutter::new(MeetingChannel::System);
        assert!(feed(&mut c, 100, true).is_empty());
        assert!(feed(&mut c, 1_000, false).is_empty());
    }

    #[test]
    fn long_speech_is_cut_at_the_maximum() {
        let mut c = UtteranceCutter::new(MeetingChannel::System);
        let out = feed(&mut c, 32_000, true);
        assert_eq!(out.len(), 2);
        assert!(out[0].t1_ms - out[0].t0_ms >= 14_900);
        assert!(c.flush().is_some());
    }
}
