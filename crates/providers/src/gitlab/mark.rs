//! My last review of an MR, from the newest note I wrote on it: a comment or reply, or the
//! system note GitLab adds for an approval. GitLab gives a time, not a commit.

use chrono::{DateTime, Utc};
use prcore::history::ReviewMark;
use serde_json::Value;

/// The mark of the newest of `notes` (as listed newest first) whose author is `me` (a numeric
/// user id as text).
pub(super) fn mark_of(notes: &Value, me: &str) -> Option<ReviewMark> {
    let mine = notes.as_array()?.iter().find(|n| {
        n["author"]["id"]
            .as_u64()
            .is_some_and(|id| id.to_string() == me)
    })?;
    let at = DateTime::parse_from_rfc3339(mine["created_at"].as_str()?)
        .ok()?
        .with_timezone(&Utc);
    Some(ReviewMark { at, commit: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_newest_note_of_mine_gives_the_time() {
        let notes = json!([
            {"author": {"id": 9}, "created_at": "2026-11-15T10:00:00.000Z"},
            {"author": {"id": 7}, "system": true, "created_at": "2026-11-14T12:00:00.500Z"},
            {"author": {"id": 7}, "created_at": "2026-11-13T12:00:00.000Z"}
        ]);
        let m = mark_of(&notes, "7").unwrap();
        assert_eq!(m.at.to_rfc3339(), "2026-11-14T12:00:00.500+00:00");
        assert_eq!(m.commit, None);
    }

    #[test]
    fn no_note_of_mine_or_a_bad_answer_gives_nothing() {
        let notes = json!([{"author": {"id": 9}, "created_at": "2026-11-15T10:00:00.000Z"}]);
        assert_eq!(mark_of(&notes, "7"), None);
        assert_eq!(mark_of(&json!([]), "7"), None);
        assert_eq!(mark_of(&json!({"message": "x"}), "7"), None);
        assert_eq!(
            mark_of(&json!([{"author": {"id": 7}, "created_at": "soon"}]), "7"),
            None
        );
    }
}
