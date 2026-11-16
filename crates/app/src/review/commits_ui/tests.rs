use super::*;
use chrono::{TimeZone, Utc};

fn commit(id: char, hour: u32) -> CommitInfo {
    CommitInfo {
        id: id.to_string().repeat(40),
        author: "Ada".into(),
        date: Utc.with_ymd_and_hms(2026, 11, 16, hour, 0, 0).unwrap(),
        subject: format!("commit {id}"),
    }
}

fn mark(hour: u32) -> ReviewMark {
    ReviewMark {
        at: Utc.with_ymd_and_hms(2026, 11, 16, hour, 30, 0).unwrap(),
        commit: None,
    }
}

#[test]
fn nothing_is_marked_until_a_review_is_known() {
    let mut c = Commits::default();
    assert_eq!(c.new_flags(), Vec::<bool>::new());
    assert_eq!(c.note(), None, "nothing is said before the forge answered");
    c.set_list(vec![commit('a', 8), commit('b', 10)]);
    assert_eq!(c.new_flags(), [false, false]);
    assert_eq!(c.note(), None);
    c.set_mark(None);
    assert_eq!(c.new_flags(), [false, false]);
    assert_eq!(c.note().unwrap(), "You have not reviewed this yet.");
}

#[test]
fn commits_after_my_review_are_new_whatever_arrives_first() {
    let mut c = Commits::default();
    c.set_mark(Some(mark(9)));
    c.set_list(vec![commit('a', 8), commit('b', 10), commit('c', 11)]);
    assert_eq!(c.new_flags(), [false, true, true]);
    assert_eq!(c.new_count(), 2);
    assert_eq!(
        c.note().unwrap(),
        "2 commits since your last review (2026-11-16 09:30 UTC)."
    );
    c.set_mark(Some(mark(12)));
    assert_eq!(
        c.note().unwrap(),
        "No commits since your last review (2026-11-16 12:30 UTC)."
    );
}

#[test]
fn caught_up_is_the_newest_commit_before_the_new_ones() {
    let mut c = Commits::default();
    assert_eq!(c.caught_up_head(), None);
    c.set_list(vec![commit('a', 8), commit('b', 10), commit('c', 11)]);
    assert_eq!(c.caught_up_head(), None, "the review is not known yet");
    c.set_mark(None);
    assert_eq!(
        c.caught_up_head(),
        None,
        "no review of mine: nothing to be caught up to"
    );
    c.set_mark(Some(mark(9)));
    assert_eq!(c.caught_up_head(), Some(commit('a', 8).id));
    c.set_mark(Some(mark(12)));
    assert_eq!(
        c.caught_up_head(),
        Some(commit('c', 11).id),
        "nothing new: the head"
    );
    c.set_mark(Some(mark(7)));
    assert_eq!(c.caught_up_head(), Some(String::new()), "everything new");
    c.set_list(vec![]);
    assert_eq!(c.caught_up_head(), None, "no commits of its own");
}

#[test]
fn a_failed_review_read_says_so_and_marks_nothing() {
    let mut c = Commits::default();
    c.set_list(vec![commit('a', 8)]);
    c.fail_mark("network error".into());
    assert_eq!(c.new_flags(), [false]);
    assert!(c.note().unwrap().contains("network error"));
    c.set_mark(Some(mark(7)));
    assert_eq!(c.new_flags(), [true], "a later answer replaces the failure");
}
