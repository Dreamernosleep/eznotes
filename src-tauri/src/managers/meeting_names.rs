//! Participant names from the meeting app's window.
//!
//! Meeting apps don't expose who is talking, but their accessibility labels
//! do name the people in the call ("More options for Joe Esper", "Mute Joe
//! Esper's microphone"). While a detected meeting records, those names are
//! collected so a speaker can be named in one click.

use anyhow::Result;
use rusqlite::{params, Connection};

/// Label shapes that carry a participant's name (Google Meet, English UI).
/// Some only appear while the mouse is over a tile, so the window is read
/// repeatedly during the call.
const PREFIXES: &[&str] = &["More options for ", "Pin ", "Mute "];
const SUFFIXES: &[&str] = &[" to your main screen", "'s microphone"];

/// The participant named by an accessibility label, if it names one.
pub fn participant_from_label(label: &str) -> Option<String> {
    let label = label.trim();
    let name = if let Some(rest) = label.strip_prefix("More options for ") {
        rest
    } else {
        let rest = PREFIXES[1..].iter().find_map(|p| label.strip_prefix(p))?;
        SUFFIXES.iter().find_map(|s| rest.strip_suffix(s))?
    }
    .trim();
    let looks_like_name = !name.is_empty()
        && name.len() <= 60
        && name.chars().next().is_some_and(|c| c.is_alphabetic())
        && !name.contains(" presentation");
    looks_like_name.then(|| name.to_string())
}

pub fn save_participant(conn: &Connection, meeting_id: i64, name: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO meeting_participants (meeting_id, name) VALUES (?1, ?2)",
        params![meeting_id, name],
    )?;
    Ok(())
}

pub fn participants(conn: &Connection, meeting_id: i64) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT name FROM meeting_participants WHERE meeting_id = ?1 ORDER BY name")?;
    let names = stmt
        .query_map(params![meeting_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_meet_labels() {
        // Labels read from a real Google Meet call.
        let p = participant_from_label;
        assert_eq!(
            p("More options for Joe Esper").as_deref(),
            Some("Joe Esper")
        );
        assert_eq!(
            p("Mute Donovan Rood's microphone").as_deref(),
            Some("Donovan Rood")
        );
        assert_eq!(
            p("Pin Joe Esper to your main screen").as_deref(),
            Some("Joe Esper")
        );
        assert_eq!(
            p("Unpin Donovan Rood's presentation from your main screen"),
            None
        );
        assert_eq!(p("You can't unmute someone else's presentation"), None);
        assert_eq!(p("Mute tab"), None);
        assert_eq!(p("Turn off microphone"), None);
    }

    #[test]
    fn participants_are_stored_once() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meeting_participants (meeting_id INTEGER NOT NULL, name TEXT NOT NULL,
               PRIMARY KEY (meeting_id, name));",
        )
        .unwrap();
        for name in ["Joe Esper", "Donovan Rood", "Joe Esper"] {
            save_participant(&conn, 1, name).unwrap();
        }
        assert_eq!(
            participants(&conn, 1).unwrap(),
            vec!["Donovan Rood", "Joe Esper"]
        );
    }
}
