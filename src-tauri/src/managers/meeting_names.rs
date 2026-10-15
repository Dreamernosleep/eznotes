//! Real names for separated voices, from the meeting app's own screen.
//!
//! While recording, the meeting window is read every second for the name of
//! whoever it shows as speaking ("highlights"). After the meeting, each
//! highlight is a vote for the voice talking at that moment; a voice takes a
//! name once it has enough votes and they mostly agree. Names set by hand
//! always win.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{params, Connection};

use crate::managers::speakers::SpeakerTurn;

/// Votes a voice needs before it takes a name.
const MIN_VOTES: usize = 3;
/// Share of a voice's votes the winning name needs.
const MIN_SHARE: f32 = 0.7;

/// The speaker (voice) talking at `t_ms`, if exactly one is.
fn voice_at(t_ms: i64, turns: &[SpeakerTurn]) -> Option<u32> {
    let mut active = turns
        .iter()
        .filter(|t| t.t0_ms <= t_ms && t_ms < t.t1_ms)
        .map(|t| t.speaker);
    let first = active.next()?;
    // Overlapping voices: the highlight could belong to either.
    active.all(|s| s == first).then_some(first)
}

/// Name each voice from `(time, name)` highlights.
pub fn assign_names(highlights: &[(i64, String)], turns: &[SpeakerTurn]) -> HashMap<u32, String> {
    let mut votes: HashMap<u32, HashMap<&str, usize>> = HashMap::new();
    for (t_ms, name) in highlights {
        if let Some(voice) = voice_at(*t_ms, turns) {
            *votes
                .entry(voice)
                .or_default()
                .entry(name.as_str())
                .or_default() += 1;
        }
    }
    votes
        .into_iter()
        .filter_map(|(voice, names)| {
            let total: usize = names.values().sum();
            let (best, count) = names.into_iter().max_by_key(|(_, n)| *n)?;
            (total >= MIN_VOTES && count as f32 / total as f32 >= MIN_SHARE)
                .then(|| (voice, best.to_string()))
        })
        .collect()
}

pub fn save_highlight(conn: &Connection, meeting_id: i64, t_ms: i64, name: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meeting_highlights (meeting_id, t_ms, name) VALUES (?1, ?2, ?3)",
        params![meeting_id, t_ms, name],
    )?;
    Ok(())
}

pub fn highlights(conn: &Connection, meeting_id: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn
        .prepare("SELECT t_ms, name FROM meeting_highlights WHERE meeting_id = ?1 ORDER BY t_ms")?;
    let rows = stmt
        .query_map(params![meeting_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Store screen names as speaker names, keeping any name set by hand.
pub fn save_screen_names(
    conn: &Connection,
    meeting_id: i64,
    names: &HashMap<u32, String>,
) -> Result<()> {
    for (voice, name) in names {
        conn.execute(
            "INSERT INTO meeting_speakers (meeting_id, speaker_key, display_name, source)
             VALUES (?1, ?2, ?3, 'screen')
             ON CONFLICT(meeting_id, speaker_key) DO UPDATE
               SET display_name = excluded.display_name, source = 'screen'
               WHERE meeting_speakers.source != 'manual'",
            params![meeting_id, format!("s{voice}"), name],
        )?;
    }
    Ok(())
}

/// Pull a speaker's name out of an accessibility label such as
/// "Alex Martin is speaking" or "Alex Martin, speaking". Returns `None` for
/// labels that don't announce a speaker.
pub fn speaker_from_label(label: &str) -> Option<String> {
    const SUFFIXES: &[&str] = &[" is speaking", ", speaking", " (speaking)", " is talking"];
    const PREFIXES: &[&str] = &["Speaking: ", "Talking: "];
    let label = label.trim();
    let name = SUFFIXES
        .iter()
        .find_map(|s| label.strip_suffix(s))
        .or_else(|| PREFIXES.iter().find_map(|p| label.strip_prefix(p)))?
        .trim();
    // "You are speaking" / "You" is the mic side, already labelled.
    (!name.is_empty() && name.len() <= 60 && !name.eq_ignore_ascii_case("you"))
        .then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(speaker: u32, t0: i64, t1: i64) -> SpeakerTurn {
        SpeakerTurn {
            speaker,
            t0_ms: t0,
            t1_ms: t1,
        }
    }

    fn hl(t: i64, name: &str) -> (i64, String) {
        (t, name.to_string())
    }

    #[test]
    fn a_voice_takes_a_name_with_enough_agreeing_votes() {
        let turns = [turn(1, 0, 10_000), turn(2, 10_000, 20_000)];
        let highlights = [
            hl(1_000, "Alex"),
            hl(4_000, "Alex"),
            hl(8_000, "Alex"),
            hl(9_000, "Marie"), // a late highlight: 3 of 4 still agree
            hl(12_000, "Marie"),
            hl(15_000, "Marie"),
        ];
        let names = assign_names(&highlights, &turns);
        assert_eq!(names.get(&1).map(String::as_str), Some("Alex"));
        // Speaker 2 has only 2 votes: no name yet.
        assert_eq!(names.get(&2), None);
    }

    #[test]
    fn disagreeing_votes_leave_the_voice_unnamed() {
        let turns = [turn(1, 0, 10_000)];
        let highlights = [
            hl(1_000, "Alex"),
            hl(2_000, "Marie"),
            hl(3_000, "Alex"),
            hl(4_000, "Marie"),
        ];
        assert!(assign_names(&highlights, &turns).is_empty());
    }

    #[test]
    fn overlapping_voices_cast_no_vote() {
        let turns = [turn(1, 0, 5_000), turn(2, 3_000, 8_000)];
        assert_eq!(voice_at(4_000, &turns), None);
        assert_eq!(voice_at(1_000, &turns), Some(1));
    }

    #[test]
    fn speaker_labels_are_parsed() {
        assert_eq!(
            speaker_from_label("Alex Martin is speaking").as_deref(),
            Some("Alex Martin")
        );
        assert_eq!(
            speaker_from_label("Speaking: Marie").as_deref(),
            Some("Marie")
        );
        assert_eq!(speaker_from_label("You is speaking"), None);
        assert_eq!(speaker_from_label("Mute microphone"), None);
    }

    #[test]
    fn manual_names_survive_screen_names() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meeting_speakers (meeting_id INTEGER NOT NULL, speaker_key TEXT NOT NULL,
               display_name TEXT NOT NULL, source TEXT NOT NULL, PRIMARY KEY (meeting_id, speaker_key));
             INSERT INTO meeting_speakers VALUES (1, 's1', 'Boss', 'manual'), (1, 's2', 'Speaker 2', 'auto');",
        )
        .unwrap();
        let names: HashMap<u32, String> =
            [(1, "Alex".to_string()), (2, "Marie".to_string())].into();
        save_screen_names(&conn, 1, &names).unwrap();
        let get = |k: &str| -> String {
            conn.query_row(
                "SELECT display_name FROM meeting_speakers WHERE speaker_key = ?1",
                params![k],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(get("s1"), "Boss");
        assert_eq!(get("s2"), "Marie");
    }
}
