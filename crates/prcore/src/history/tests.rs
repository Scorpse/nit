use super::*;
use chrono::TimeZone;

fn at(h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, h, 0, 0).unwrap()
}

fn commit(id: &str, h: u32) -> CommitInfo {
    CommitInfo {
        id: id.repeat(40)[..40].to_string(),
        author: "Ada".into(),
        date: at(h),
        subject: format!("commit {id}"),
    }
}

#[test]
fn log_entries_are_parsed_and_a_broken_one_is_skipped() {
    let raw = "aaaa\u{1f}Ada Lovelace\u{1f}2026-11-16T09:30:00+02:00\u{1f}Fix: a | b\u{1f}x\0\n\
               bbbb\u{1f}Bob\u{1f}not a date\u{1f}broken\0\
               cccc\u{1f}Cy\u{1f}2026-11-16T07:00:00Z\u{1f}\0";
    let got = parse_log(raw.as_bytes());
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].id, "aaaa");
    assert_eq!(got[0].author, "Ada Lovelace");
    assert_eq!(got[0].date, at(7) + chrono::Duration::minutes(30));
    assert_eq!(
        got[0].subject, "Fix: a | b\u{1f}x",
        "the subject keeps what follows"
    );
    assert_eq!(got[1].subject, "", "an empty subject is still a commit");
    assert!(parse_log(b"").is_empty());
}

#[test]
fn after_the_reviewed_commit_means_new() {
    let cs = [commit("a", 8), commit("b", 9), commit("c", 10)];
    let mark = ReviewMark {
        at: at(23),
        commit: Some(cs[1].id[..12].to_string()),
    };
    assert_eq!(
        new_since(&cs, &mark),
        [false, false, true],
        "the commit wins over the time, and an abbreviated id matches"
    );
}

#[test]
fn without_a_known_commit_the_review_time_decides() {
    let cs = [commit("a", 8), commit("b", 9), commit("c", 10)];
    let by_time = ReviewMark {
        at: at(9),
        commit: None,
    };
    assert_eq!(new_since(&cs, &by_time), [false, false, true]);
    let gone = ReviewMark {
        at: at(8),
        commit: Some("f".repeat(40)),
    };
    assert_eq!(
        new_since(&cs, &gone),
        [false, true, true],
        "a reviewed commit a force push removed falls back to the time"
    );
    assert!(new_since(&[], &gone).is_empty());
}
