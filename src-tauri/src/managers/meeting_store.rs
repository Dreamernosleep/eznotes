//! Reading and editing saved meetings: the list, one meeting's transcript,
//! notes, titles, speaker names, Markdown export and audio retention.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use chrono::{Local, TimeZone};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use specta::Type;

#[derive(Debug, Clone, Serialize, Type)]
pub struct MeetingSummary {
    pub id: i64,
    pub title: String,
    pub source_app: Option<String>,
    /// Ms since the Unix epoch.
    pub started_at: i64,
    pub duration_ms: i64,
    /// recording | processing | complete | failed
    pub status: String,
    pub error: Option<String>,
    /// Display names of the speakers, "You" first.
    pub speakers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct MeetingSegment {
    pub channel: String,
    pub speaker_key: String,
    pub t0_ms: i64,
    pub t1_ms: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct MeetingSpeaker {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct MeetingDetail {
    pub summary: MeetingSummary,
    pub notes: String,
    /// `final` once processed, otherwise the `live` lines.
    pub transcript_kind: String,
    pub segments: Vec<MeetingSegment>,
    pub speakers: Vec<MeetingSpeaker>,
    /// People seen in the meeting app, offered when naming a speaker.
    pub participants: Vec<String>,
}

fn speaker_order(key: &str) -> (u8, String) {
    match key {
        "you" => (0, String::new()),
        "others" => (2, String::new()),
        k => (1, format!("{:>6}", k.trim_start_matches('s'))),
    }
}

fn speakers(conn: &Connection, id: i64) -> Result<Vec<MeetingSpeaker>> {
    let mut stmt = conn
        .prepare("SELECT speaker_key, display_name FROM meeting_speakers WHERE meeting_id = ?1")?;
    let mut list = stmt
        .query_map(params![id], |row| {
            Ok(MeetingSpeaker {
                key: row.get(0)?,
                name: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    list.sort_by_key(|s| speaker_order(&s.key));
    Ok(list)
}

fn summary_from_row(conn: &Connection, row: &rusqlite::Row<'_>) -> Result<MeetingSummary> {
    let id: i64 = row.get("id")?;
    Ok(MeetingSummary {
        id,
        title: row.get("title")?,
        source_app: row.get("source_app")?,
        started_at: row.get("started_at")?,
        duration_ms: row.get("duration_ms")?,
        status: row.get("status")?,
        error: row.get("error")?,
        speakers: speakers(conn, id)?.into_iter().map(|s| s.name).collect(),
    })
}

const SUMMARY_COLUMNS: &str = "id, title, source_app, started_at, duration_ms, status, error";

/// All meetings, newest first.
pub fn list(conn: &Connection) -> Result<Vec<MeetingSummary>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SUMMARY_COLUMNS} FROM meetings ORDER BY started_at DESC"
    ))?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(summary_from_row(conn, row)?);
    }
    Ok(out)
}

pub fn detail(conn: &Connection, id: i64) -> Result<MeetingDetail> {
    let (summary, notes) = conn
        .query_row(
            &format!("SELECT {SUMMARY_COLUMNS}, notes_md FROM meetings WHERE id = ?1"),
            params![id],
            |row| {
                Ok((
                    summary_from_row(conn, row),
                    row.get::<_, String>("notes_md")?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| anyhow!("Meeting {id} not found"))?;
    let summary = summary?;

    let has_final: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM meeting_segments WHERE meeting_id = ?1 AND kind = 'final')",
        params![id],
        |row| row.get(0),
    )?;
    let kind = if has_final { "final" } else { "live" };
    let mut stmt = conn.prepare(
        "SELECT channel, speaker_key, t0_ms, t1_ms, text FROM meeting_segments
         WHERE meeting_id = ?1 AND kind = ?2 ORDER BY t0_ms, id",
    )?;
    let segments = stmt
        .query_map(params![id, kind], |row| {
            Ok(MeetingSegment {
                channel: row.get(0)?,
                speaker_key: row.get(1)?,
                t0_ms: row.get(2)?,
                t1_ms: row.get(3)?,
                text: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(MeetingDetail {
        summary,
        notes,
        transcript_kind: kind.to_string(),
        segments,
        speakers: speakers(conn, id)?,
        participants: crate::managers::meeting_names::participants(conn, id).unwrap_or_default(),
    })
}

pub fn set_notes(conn: &Connection, id: i64, notes: &str) -> Result<()> {
    conn.execute(
        "UPDATE meetings SET notes_md = ?2 WHERE id = ?1",
        params![id, notes],
    )?;
    Ok(())
}

pub fn set_title(conn: &Connection, id: i64, title: &str) -> Result<()> {
    let title = title.trim();
    if title.is_empty() {
        return Err(anyhow!("A meeting title can't be empty"));
    }
    conn.execute(
        "UPDATE meetings SET title = ?2 WHERE id = ?1",
        params![id, title],
    )?;
    Ok(())
}

/// Rename a speaker everywhere in the meeting. Manual names survive
/// re-processing.
pub fn rename_speaker(conn: &Connection, id: i64, key: &str, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("A speaker name can't be empty"));
    }
    conn.execute(
        "INSERT INTO meeting_speakers (meeting_id, speaker_key, display_name, source)
         VALUES (?1, ?2, ?3, 'manual')
         ON CONFLICT(meeting_id, speaker_key)
         DO UPDATE SET display_name = excluded.display_name, source = 'manual'",
        params![id, key, name],
    )?;
    Ok(())
}

/// The meeting's folder (relative to the app data dir), for deletion or
/// opening.
pub fn folder(conn: &Connection, id: i64) -> Result<String> {
    conn.query_row(
        "SELECT folder FROM meetings WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| anyhow!("Meeting {id} not found"))
}

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM meetings WHERE id = ?1", params![id])?;
    Ok(())
}

/// Folders of finished meetings that started before `cutoff_ms`, whose
/// audio may be deleted (the transcript stays).
pub fn audio_expired_folders(conn: &Connection, cutoff_ms: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT folder FROM meetings WHERE started_at < ?1 AND status IN ('complete', 'failed')",
    )?;
    let folders = stmt
        .query_map(params![cutoff_ms], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(folders)
}

fn clock(ms: i64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

/// The meeting as Markdown: info, your notes, then the transcript.
pub fn markdown(detail: &MeetingDetail) -> String {
    let names: HashMap<&str, &str> = detail
        .speakers
        .iter()
        .map(|s| (s.key.as_str(), s.name.as_str()))
        .collect();
    let s = &detail.summary;
    let when = Local
        .timestamp_millis_opt(s.started_at)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();

    let mut md = format!("# {}\n\n", s.title);
    md.push_str(&format!("- **Date:** {when}\n"));
    md.push_str(&format!("- **Length:** {}\n", clock(s.duration_ms)));
    if let Some(app) = &s.source_app {
        md.push_str(&format!("- **App:** {app}\n"));
    }
    if !s.speakers.is_empty() {
        md.push_str(&format!("- **Speakers:** {}\n", s.speakers.join(", ")));
    }
    if !detail.notes.trim().is_empty() {
        md.push_str("\n## Notes\n\n");
        md.push_str(detail.notes.trim());
        md.push('\n');
    }
    md.push_str("\n## Transcript\n\n");
    for seg in &detail.segments {
        let who = names.get(seg.speaker_key.as_str()).copied().unwrap_or(
            match seg.speaker_key.as_str() {
                "you" => "You",
                _ => "Others",
            },
        );
        md.push_str(&format!(
            "**{who}** [{}]: {}\n\n",
            clock(seg.t0_ms),
            seg.text.trim()
        ));
    }
    md.trim_end().to_string() + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.execute_batch(
            "CREATE TABLE meetings (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL,
               source_app TEXT, started_at INTEGER NOT NULL, ended_at INTEGER,
               duration_ms INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL,
               notes_md TEXT NOT NULL DEFAULT '', folder TEXT NOT NULL, error TEXT);
             CREATE TABLE meeting_segments (id INTEGER PRIMARY KEY AUTOINCREMENT,
               meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
               kind TEXT NOT NULL, channel TEXT NOT NULL, speaker_key TEXT NOT NULL,
               t0_ms INTEGER NOT NULL, t1_ms INTEGER NOT NULL, text TEXT NOT NULL);
             CREATE TABLE meeting_speakers (
               meeting_id INTEGER NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
               speaker_key TEXT NOT NULL, display_name TEXT NOT NULL, source TEXT NOT NULL,
               PRIMARY KEY (meeting_id, speaker_key));
             INSERT INTO meetings (title, source_app, started_at, duration_ms, status, folder)
               VALUES ('Zoom Oct 15, 14:00', 'Zoom', 1792000000000, 65000, 'complete', 'meetings/a');
             INSERT INTO meeting_segments (meeting_id, kind, channel, speaker_key, t0_ms, t1_ms, text) VALUES
               (1, 'live', 'mic', 'you', 0, 1000, 'live line'),
               (1, 'final', 'system', 's1', 3000, 5000, 'Hi there.'),
               (1, 'final', 'mic', 'you', 6000, 8000, 'Hello!');
             INSERT INTO meeting_speakers VALUES (1, 's1', 'Speaker 1', 'auto'), (1, 'you', 'You', 'auto');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn detail_prefers_final_lines_and_orders_speakers() {
        let conn = db();
        let d = detail(&conn, 1).unwrap();
        assert_eq!(d.transcript_kind, "final");
        assert_eq!(d.segments.len(), 2);
        assert_eq!(d.summary.speakers, vec!["You", "Speaker 1"]);
    }

    #[test]
    fn rename_applies_everywhere_and_markdown_uses_it() {
        let conn = db();
        rename_speaker(&conn, 1, "s1", "Alex").unwrap();
        set_notes(&conn, 1, "- ship Friday").unwrap();
        let md = markdown(&detail(&conn, 1).unwrap());
        assert!(md.starts_with("# Zoom Oct 15, 14:00\n"));
        assert!(md.contains("- **Length:** 01:05"));
        assert!(md.contains("## Notes\n\n- ship Friday"));
        assert!(md.contains("**Alex** [00:03]: Hi there."));
        assert!(md.contains("**You** [00:06]: Hello!"));
        assert!(rename_speaker(&conn, 1, "s1", "  ").is_err());
    }

    #[test]
    fn delete_cascades_and_expiry_selects_old_finished_meetings() {
        let conn = db();
        assert_eq!(
            audio_expired_folders(&conn, 1792000000001).unwrap(),
            vec!["meetings/a"]
        );
        assert!(audio_expired_folders(&conn, 1).unwrap().is_empty());
        delete(&conn, 1).unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM meeting_segments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }
}
