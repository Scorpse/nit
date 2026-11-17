use super::*;
use prcore::diff::DiffFile;

fn commit(id: char, hour: u32) -> CommitInfo {
    CommitInfo {
        id: id.to_string().repeat(40),
        author: "Ada".into(),
        date: format!("2026-11-16T{hour:02}:00:00Z").parse().unwrap(),
        subject: format!("commit {id}"),
    }
}

fn mark(hour: u32) -> ReviewMark {
    ReviewMark {
        at: format!("2026-11-16T{hour:02}:30:00Z").parse().unwrap(),
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

fn file(path: &str) -> DiffFile {
    DiffFile {
        path: path.into(),
        old_path: None,
        status: prcore::diff::FileStatus::Modified,
        additions: 2,
        deletions: 1,
        binary: false,
        counts_known: true,
        new_oid: String::new(),
    }
}

#[test]
fn choosing_a_commit_asks_for_its_files_once() {
    let mut c = Commits::default();
    let r = commit_range(&commit('a', 8));
    assert_eq!(r.base, format!("{}^", "a".repeat(40)));
    c.select(r.clone());
    c.select(r.clone());
    assert_eq!(
        c.take_wants(),
        [Want::Files(r.clone())],
        "the same choice asks once"
    );
    c.set_files(r.clone(), Ok(vec![file("a.rs")]));
    let other = commit_range(&commit('b', 9));
    c.select(other);
    c.take_wants();
    c.select(r.clone());
    assert!(
        c.take_wants().is_empty(),
        "files already known are not asked again"
    );
}

#[test]
fn a_patch_for_a_file_no_longer_open_is_dropped_and_a_long_one_is_cut() {
    let mut c = Commits::default();
    let r = commit_range(&commit('a', 8));
    c.select(r.clone());
    c.open(&file("a.rs"));
    c.take_wants();
    c.set_patch(r.clone(), "b.rs".into(), Ok(Arc::new("+x\n".into())));
    assert!(c.patch.is_none(), "another file's answer");
    let long = "+x\n".repeat(MAX_PATCH_LINES + 5);
    c.set_patch(r.clone(), "a.rs".into(), Ok(Arc::new(long)));
    let p = c.patch.as_ref().unwrap();
    assert_eq!(p.lines.as_ref().unwrap().len(), MAX_PATCH_LINES);
    assert!(p.cut);
    c.select(commit_range(&commit('b', 9)));
    assert!(
        c.patch.is_none() && c.open_file.is_none(),
        "a new choice clears the file"
    );
}

#[test]
fn all_the_new_commits_are_one_range_from_where_i_was_caught_up() {
    let mut c = Commits {
        pr_base: "base".into(),
        ..Commits::default()
    };
    c.set_list(vec![commit('a', 8), commit('b', 10), commit('c', 11)]);
    assert_eq!(c.since_review(), None, "no review known");
    c.set_mark(Some(mark(9)));
    let r = c.since_review().unwrap();
    assert_eq!((r.base, r.head), (commit('a', 8).id, commit('c', 11).id));
    c.set_mark(Some(mark(7)));
    assert_eq!(
        c.since_review().unwrap().base,
        "base",
        "everything is new: from the PR's base"
    );
    c.set_mark(Some(mark(12)));
    assert_eq!(c.since_review(), None, "nothing new");
}
