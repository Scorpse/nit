//! My last review of a PR, from the `participated_on` time of my own entry among the PR's
//! participants. Bitbucket gives a time, not a commit.

use jiff::Timestamp;
use prcore::history::ReviewMark;
use serde_json::Value;

/// The mark of the participant whose user is `me` (a uuid), if they took part and the time
/// is readable.
pub(super) fn mark_of(pr: &Value, me: &str) -> Option<ReviewMark> {
    let mine = pr
        .get("participants")?
        .as_array()?
        .iter()
        .find(|p| p["user"]["uuid"].as_str() == Some(me))?;
    let at = mine["participated_on"]
        .as_str()?
        .parse::<Timestamp>()
        .ok()?;
    Some(ReviewMark { at, commit: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pr() -> Value {
        json!({"participants": [
            {"user": {"uuid": "{a}"}, "participated_on": "2026-11-10T08:00:00.000000+00:00"},
            {"user": {"uuid": "{me}"}, "approved": true,
             "participated_on": "2026-11-12T09:30:00.123456+00:00"},
            {"user": {"uuid": "{c}"}, "participated_on": null}
        ]})
    }

    #[test]
    fn my_participation_time_is_the_mark() {
        let m = mark_of(&pr(), "{me}").unwrap();
        assert_eq!(m.at.to_string(), "2026-11-12T09:30:00.123456Z");
        assert_eq!(m.commit, None);
    }

    #[test]
    fn not_a_participant_or_no_readable_time_gives_nothing() {
        assert_eq!(mark_of(&pr(), "{nobody}"), None);
        assert_eq!(mark_of(&pr(), "{c}"), None);
        assert_eq!(mark_of(&json!({}), "{me}"), None);
        assert_eq!(mark_of(&json!({"participants": 3}), "{me}"), None);
    }
}
