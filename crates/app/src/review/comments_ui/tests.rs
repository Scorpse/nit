use super::*;
use prcore::comments::{Anchor, Comment};
use prcore::diff::RowKind;
use prcore::model::UserRef;
use std::sync::Arc;

use crate::review::diffview::{DiffView, ROW_H};

fn comment(id: u64, parent: Option<u64>, anchor: Option<Anchor>) -> Comment {
    let t = jiff::Timestamp::from_second(1_700_000_000 + id as i64).unwrap();
    Comment {
        id: id.into(),
        parent_id: parent.map(Into::into),
        author: UserRef {
            id: "{a}".into(),
            display_name: "Ana".into(),
            login: None,
        },
        body: format!("comment {id}"),
        created_on: t,
        updated_on: t,
        deleted: false,
        resolved: false,
        anchor,
        author_is_bot: false,
        thread: Some(prcore::comments::ThreadId::from(id)),
        can_resolve: true,
    }
}

fn at(path: &str, from: Option<u32>, to: Option<u32>) -> Option<Anchor> {
    Some(Anchor {
        path: path.into(),
        from,
        to,
        start: None,
        outdated: false,
        old_path: None,
        commit: None,
    })
}

fn thread(id: u64, anchor: Option<Anchor>, replies: usize) -> Thread {
    Thread {
        root: comment(id, None, anchor),
        replies: (0..replies)
            .map(|r| comment(id * 1000 + r as u64 + 1, Some(id), None))
            .collect(),
    }
}

/// Runs `f` with a painter of a real egui context (real fonts and text layout).
fn with_painter<R>(f: impl FnOnce(&Painter) -> R) -> R {
    let ctx = eframe::egui::Context::default();
    let (mut f, mut out) = (Some(f), None);
    let mut frame = ctx.run_ui(Default::default(), |ui| {
        if let Some(f) = f.take() {
            out = Some(f(ui.painter()));
        }
    });
    // the font atlas upload is not needed without a renderer
    frame.textures_delta.clear();
    out.expect("one pass ran")
}

#[test]
fn wrapped_body_height_follows_text_width_newlines_and_unbroken_tokens() {
    with_painter(|p| {
        let mut m = measure_with(p);
        let line = m("x", 400.0);
        assert!(line > 0.0);
        // longer text, more rows; narrower card, more rows
        let short = m(&"word ".repeat(20), 400.0);
        let long = m(&"word ".repeat(200), 400.0);
        assert!(long > short, "{long} vs {short}");
        assert!(m(&"word ".repeat(200), 200.0) > long, "narrower is taller");
        // newlines are rows of their own, blank lines included
        let five = m("a\nb\nc\n\nd", 400.0);
        assert!(five >= 5.0 * line - 0.5, "{five} vs {line}");
        assert_eq!(
            m("- one\n- two", 400.0),
            m("a\nb", 400.0),
            "bullets: one row each"
        );
        // a 120-char path with no spaces wraps by character, inside the wrap width
        let token = format!("build/{}", "d89e0f55".repeat(14));
        let g = body_galley(p, &token, 200.0, TEXT);
        assert!(g.rows.len() > 2, "{} rows", g.rows.len());
        assert!(g.size().x <= 200.0 + 0.5, "{} wide", g.size().x);
        let hash = "0123456789abcdef".repeat(10);
        let g = body_galley(p, &hash, 150.0, TEXT);
        assert!(g.rows.len() > 2 && g.size().x <= 150.5, "{:?}", g.size());
    });
}

#[test]
fn long_bodies_collapse_and_show_more_restores_them() {
    let mut t = thread(1, None, 1);
    let log: String = (0..100)
        .map(|i| format!("- step {i:02}: passed\n"))
        .collect();
    t.replies[0].body = log.repeat(2);
    assert!(is_long(&t.replies[0]) && !is_long(&t.root));
    let shown = shown_body(&t.replies[0], false);
    assert_eq!(shown.lines().count(), COLLAPSED_LINES, "{shown}");
    assert!(shown.ends_with('\u{2026}'));
    assert_eq!(shown_body(&t.replies[0], true), t.replies[0].body.as_str());
    // a long body without newlines is capped by chars
    let mut one = t.clone();
    one.replies[0].body = "x".repeat(LONG_BODY + 10);
    assert_eq!(
        shown_body(&one.replies[0], false).chars().count(),
        COLLAPSED_CHARS + 1
    );
    let mut c = comments_with(vec![t]);
    let w = 400.0;
    let collapsed = c.card(&c.threads[0], w, &mut measure);
    let b = &collapsed.comments[1];
    assert_eq!(b.more, Some(false));
    assert!(
        collapsed.comments[0].more.is_none(),
        "short bodies get no toggle"
    );
    let rects = collapsed.more_rects(Rect::from_min_size(Pos2::ZERO, vec2(w, 1.0)));
    assert_eq!(rects.len(), 1);
    assert_eq!(rects[0].0, c.threads[0].replies[0].id);
    assert_eq!(
        rects[0].1.top(),
        b.top + AUTHOR_H + b.body_h,
        "under the body"
    );
    let reply_id = c.threads[0].replies[0].id.clone();
    c.toggle_full(&reply_id);
    let full = c.card(&c.threads[0], w, &mut measure);
    assert_eq!(full.comments[1].more, Some(true));
    assert!(full.height > collapsed.height + 100.0);
    c.toggle_full(&reply_id);
    assert_eq!(c.card(&c.threads[0], w, &mut measure), collapsed);
}

#[test]
fn typed_comment_shows_a_badge_type_and_its_text_without_the_prefix() {
    let mut t = thread(1, None, 1);
    t.root.body = "**nit:** Rename it.".into();
    t.replies[0].body = "**Question:**\nWhy?".into();
    assert_eq!(comment_type(&t.root), Some(CommentType::Nit));
    assert_eq!(shown_body(&t.root, true), "Rename it.");
    assert_eq!(comment_type(&t.replies[0]), Some(CommentType::Question));
    assert_eq!(body_text(&t.replies[0]), "Why?");
    // measured as shown: the card reserves the height of the text without the prefix
    let lay = card_layout(&t, true, &|_| false, 300.0, &mut measure);
    assert_eq!(
        lay.comments[0].body_h,
        measure("Rename it.", body_width(300.0, false))
    );
    // not a prefix: shown as written
    t.root.body = "**nit**: as written".into();
    assert_eq!(
        (comment_type(&t.root), body_text(&t.root)),
        (None, "**nit**: as written")
    );
    t.root.body = "**nit:** x".into();
    t.root.deleted = true;
    assert_eq!((comment_type(&t.root), body_text(&t.root)), (None, DELETED));
}

#[test]
fn card_layout_stacks_comments_without_gaps_or_overlap() {
    let mut t = thread(1, None, 2);
    t.root.body = "a\n\nb\n- c\n- d".into();
    t.replies[1].body = "z".repeat(300);
    let lay = card_layout(&t, true, &|_| false, 300.0, &mut measure);
    let mut y = CARD_PAD + TITLE_H;
    for b in &lay.comments {
        y += COMMENT_GAP;
        assert_eq!(b.top, y);
        y += AUTHOR_H + b.body_h;
    }
    assert_eq!(lay.height, y + CARD_PAD);
    assert!(lay.comments[1].reply && !lay.comments[0].reply);
    // replies wrap at the indented width: the same text is at least as tall
    let reply_h = measure(&t.replies[1].body, body_width(300.0, true));
    assert_eq!(lay.comments[2].body_h, reply_h);
    assert_eq!(
        card_layout(&t, false, &|_| false, 300.0, &mut measure).height,
        COLLAPSED_H
    );
}

fn row(kind: RowKind, old: Option<u32>, new: Option<u32>) -> Row {
    Row {
        kind,
        old_line: old,
        new_line: new,
        text: "x".into(),
    }
}

/// @@ -10,3 +10,3 @@ / ctx 10 / del 11 / add 11 / ctx 12->12
fn rows() -> Vec<Row> {
    vec![
        row(RowKind::HunkHeader, None, None),
        row(RowKind::Context, Some(10), Some(10)),
        row(RowKind::Del, Some(11), None),
        row(RowKind::Add, None, Some(11)),
        row(RowKind::Context, Some(12), Some(12)),
    ]
}

/// One line of 16 px per started 100 px of text at 7 px a char.
fn measure(text: &str, width: f32) -> f32 {
    let per_line = (width / 7.0).floor().max(1.0) as usize;
    16.0 * text.chars().count().div_ceil(per_line).max(1) as f32
}

#[test]
fn thread_attaches_to_new_side_line() {
    let all = vec![thread(1, at("a.rs", None, Some(11)), 0)];
    let p = place(&rows(), &all, &[0]);
    assert_eq!(p.inline, vec![(3, vec![0])], "the added line 11");
    // context line: the new number decides, even when `from` is set too
    let all = vec![thread(1, at("a.rs", Some(99), Some(12)), 0)];
    assert_eq!(place(&rows(), &all, &[0]).inline, vec![(4, vec![0])]);
    assert!(place(&rows(), &all, &[0]).outdated.is_empty());
}

#[test]
fn thread_attaches_to_old_side_line() {
    let all = vec![
        thread(1, at("a.rs", Some(11), None), 0),
        thread(2, at("a.rs", Some(10), None), 0),
    ];
    let p = place(&rows(), &all, &[0, 1]);
    assert_eq!(p.inline, vec![(1, vec![1]), (2, vec![0])]);
}

#[test]
fn unmatched_thread_marked_outdated() {
    let all = vec![
        thread(1, at("a.rs", None, Some(500)), 0),
        thread(2, at("a.rs", Some(500), None), 0),
        thread(3, at("a.rs", None, None), 0),
        thread(4, at("a.rs", None, Some(10)), 0),
        thread(5, at("a.rs", Some(12), Some(12)), 0),
    ];
    let p = place(&rows(), &all, &[0, 1, 2, 3, 4]);
    assert_eq!(p.outdated, vec![0, 1], "never dropped");
    assert_eq!(p.file_level, vec![2]);
    assert_eq!(p.inline, vec![(1, vec![3]), (4, vec![4])]);
    // the hunk header carries no line: nothing attaches to it
    let all = vec![thread(1, at("a.rs", None, Some(0)), 0)];
    assert_eq!(place(&rows(), &all, &[0]).outdated, vec![0]);
}

#[test]
fn missing_new_line_falls_back_to_old_line_and_new_wins_when_both_exist() {
    // new line 500 is not in the patch, old line 11 is (the deleted row)
    let all = vec![
        thread(1, at("a.rs", Some(11), Some(500)), 0),
        // both found on different rows: the new side wins
        thread(2, at("a.rs", Some(10), Some(11)), 0),
        // neither found: outdated
        thread(3, at("a.rs", Some(500), Some(500)), 0),
    ];
    let p = place(&rows(), &all, &[0, 1, 2]);
    assert_eq!(p.inline, vec![(2, vec![0]), (3, vec![1])]);
    assert_eq!(p.outdated, vec![2]);
}

fn diff_file(path: &str, old: Option<&str>) -> prcore::diff::DiffFile {
    prcore::diff::DiffFile {
        path: path.into(),
        old_path: old.map(Into::into),
        status: prcore::diff::FileStatus::Modified,
        additions: 1,
        deletions: 1,
        binary: false,
        counts_known: true,
        new_oid: "o".into(),
    }
}

/// a.rs, and new.rs renamed from old.rs.
fn routed(threads: Vec<Thread>) -> Comments {
    let mut c = Comments::new();
    c.set_files(&[diff_file("a.rs", None), diff_file("new.rs", Some("old.rs"))]);
    c.set(threads, 0);
    c
}

#[test]
fn renamed_file_old_path_thread_is_outdated_on_the_renamed_file() {
    let c = routed(vec![
        thread(1, at("old.rs", None, Some(11)), 0),
        thread(2, at("old.rs", None, None), 0),
        thread(3, at("new.rs", None, Some(11)), 0),
    ]);
    let mut d = DiffView::default();
    d.set_patch_computed("new.rs", Arc::new(rows()), false);
    let v = d.view_mut("new.rs").unwrap();
    c.layout(v, 400.0, 0.0, &mut measure);
    let p = &v.comments.placement;
    assert_eq!(
        p.inline,
        vec![(3, vec![2])],
        "new-path threads attach to lines"
    );
    assert_eq!(p.outdated, vec![0, 1], "old-path threads are never dropped");
    assert!(outdated_note(&c.threads[0], "new.rs").contains("old.rs"));
    assert!(c.unplaced.is_empty());
    assert_eq!(c.unresolved_by_path().get("new.rs"), Some(&3));
}

fn flagged(path: &str, from: Option<u32>, to: Option<u32>) -> Option<Anchor> {
    at(path, from, to).map(|a| Anchor {
        outdated: true,
        ..a
    })
}

#[test]
fn flagged_line_thread_is_outdated_even_when_its_line_is_shown() {
    let all = vec![
        thread(1, flagged("a.rs", None, Some(11)), 0),
        // file-level: stays a file comment (with its badge)
        thread(2, flagged("a.rs", None, None), 0),
        thread(3, at("a.rs", None, Some(11)), 0),
        thread(4, at("a.rs", None, Some(500)), 0),
    ];
    let p = place(&rows(), &all, &[0, 1, 2, 3]);
    assert_eq!(p.outdated, vec![0, 3]);
    assert_eq!(p.file_level, vec![1]);
    assert_eq!(p.inline, vec![(3, vec![2])], "unflagged keeps its line");
    assert_eq!(outdated_reason(&all[0], "a.rs"), OutdatedReason::Flagged);
    assert_eq!(
        outdated_note(&all[0], "a.rs"),
        "Outdated: new line 11 changed since this comment"
    );
    assert_eq!(
        outdated_reason(&all[3], "a.rs"),
        OutdatedReason::LineNotInDiff
    );
    assert_eq!(
        outdated_note(&all[3], "a.rs"),
        "Outdated: new line 500 is not in this diff"
    );
}

#[test]
fn threads_on_files_not_in_the_diff_are_grouped_by_path_and_outdated() {
    let c = routed(vec![
        thread(1, at("docker-compose.yaml", None, Some(14)), 1),
        thread(2, at("build/m/modules.lock", None, None), 0),
        thread(3, at("docker-compose.yaml", Some(2), None), 0),
        thread(4, at("a.rs", None, Some(11)), 0),
        thread(5, at("old.rs", None, Some(1)), 0),
        thread(6, flagged("a.rs", None, Some(10)), 0),
        thread(7, None, 0),
        thread(8, flagged("a.rs", None, None), 0),
    ]);
    assert_eq!(
        c.ghost_paths(),
        vec!["build/m/modules.lock", "docker-compose.yaml"]
    );
    assert_eq!(c.on_ghost("docker-compose.yaml"), &[0, 2]);
    assert!(c.on_ghost("a.rs").is_empty());
    assert_eq!(c.unplaced, vec![0, 1, 2], "every ghost thread is unplaced");
    use OutdatedReason::*;
    let known: Vec<Option<OutdatedReason>> =
        (0..c.threads.len()).map(|i| c.known_outdated(i)).collect();
    assert_eq!(
        known,
        vec![
            Some(FileNotInDiff),
            Some(FileNotInDiff),
            Some(FileNotInDiff),
            None,
            Some(OldPath),
            Some(Flagged),
            None,
            None,
        ]
    );
    assert_eq!(
        reason_note(&c.threads[0], FileNotInDiff),
        "Outdated: file not in this diff"
    );
    // before the file index arrives nothing is a ghost
    let mut early = Comments::new();
    early.set(c.threads.clone(), 0);
    assert!(early.ghost_paths().is_empty());
    assert_eq!(early.known_outdated(0), None);
}

#[test]
fn thread_on_unknown_path_is_unplaced() {
    let c = routed(vec![
        thread(1, at("reverted.rs", None, Some(3)), 0),
        thread(2, at("a.rs", None, Some(11)), 0),
    ]);
    assert_eq!(c.unplaced, vec![0]);
    assert!(c.for_path("reverted.rs").is_empty());
    assert!(!c.unresolved_by_path().contains_key("reverted.rs"));
    assert_eq!(c.paths(), vec!["a.rs"], "only files of the diff are linked");
}

#[test]
fn unresolved_totals_are_consistent() {
    let mut resolved = thread(5, at("old.rs", None, Some(1)), 0);
    resolved.root.resolved = true;
    let c = routed(vec![
        thread(1, at("a.rs", None, Some(11)), 0),
        thread(2, at("a.rs", None, None), 0),
        thread(3, at("old.rs", None, Some(1)), 0),
        thread(4, at("new.rs", Some(2), None), 0),
        resolved,
        thread(6, None, 0),
        thread(7, at("gone.rs", None, Some(1)), 0),
    ]);
    let badges: usize = c.unresolved_by_path().values().sum();
    let pr = c
        .pr_level
        .iter()
        .filter(|&&i| is_open(&c.threads[i]))
        .count();
    let unplaced = c
        .unplaced
        .iter()
        .filter(|&&i| is_open(&c.threads[i]))
        .count();
    assert_eq!((badges, pr, unplaced), (4, 1, 1));
    assert_eq!(
        c.unresolved(),
        badges + pr + unplaced,
        "each thread counted once"
    );
    // files arriving after the threads route them the same way
    let mut late = Comments::new();
    late.set(c.threads.clone(), 0);
    late.set_files(&[diff_file("a.rs", None), diff_file("new.rs", Some("old.rs"))]);
    assert_eq!(late.unresolved_by_path(), c.unresolved_by_path());
    assert_eq!(late.unplaced, c.unplaced);
}

#[test]
fn height_change_above_the_viewport_keeps_the_top_row_in_place() {
    let mut c = comments_with(vec![
        thread(1, at("a.rs", None, Some(10)), 0),
        thread(2, at("a.rs", None, Some(12)), 0),
    ]);
    let mut d = view_of(rows());
    let v = d.view_mut("a.rs").unwrap();
    let first = c.layout(v, 400.0, 0.0, &mut measure);
    assert!(
        first.changed && first.scroll_y.is_none(),
        "first layout: no anchoring"
    );
    // viewport top 3 px into row 4 (below the card under row 1)
    let y = v.offsets.top(4) + 3.0;
    let unchanged = c.layout(v, 400.0, y, &mut measure);
    assert!(!unchanged.changed && unchanged.scroll_y.is_none());
    c.toggle(&1.into());
    let r = c.layout(v, 400.0, y, &mut measure);
    assert!(r.changed);
    let ny = r.scroll_y.expect("anchored");
    assert_eq!(ny, v.offsets.top(4) + 3.0, "same row, same offset into it");
    assert!(ny < y, "the card above collapsed");
}

#[test]
fn top_bar_count_is_hidden_while_loading_and_after_a_first_failure() {
    let mut c = Comments::new();
    assert_eq!(c.unresolved_label(), None, "loading");
    c.fail("Could not load comments (network error)".into());
    assert_eq!(
        c.unresolved_label(),
        None,
        "only the error, no 0 unresolved"
    );
    c.set(vec![thread(1, None, 0)], 0);
    assert_eq!(c.unresolved_label().as_deref(), Some("1 unresolved"));
    c.fail("again".into());
    assert_eq!(
        c.unresolved_label().as_deref(),
        Some("1 unresolved"),
        "a failed refresh keeps the threads already shown"
    );
}

fn comments_with(threads: Vec<Thread>) -> Comments {
    let mut c = Comments::new();
    c.set(threads, 0);
    c
}

fn view_of(rows: Vec<Row>) -> DiffView {
    let mut v = DiffView::default();
    v.set_patch_computed("a.rs", Arc::new(rows), false);
    v
}

#[test]
fn row_heights_include_expanded_thread() {
    let c = comments_with(vec![
        thread(1, at("a.rs", None, Some(11)), 2),
        thread(2, at("a.rs", None, Some(11)), 0),
        thread(3, at("b.rs", None, Some(11)), 0),
    ]);
    let mut d = view_of(rows());
    let v = d.view_mut("a.rs").unwrap();
    c.layout(v, 400.0, 0.0, &mut measure);
    let w = v.comments.card_w;
    assert!(w > 0.0 && w <= 400.0);
    let h1 = card_height(&c.threads[0], true, w, &mut measure);
    let h2 = card_height(&c.threads[1], true, w, &mut measure);
    assert!(h1 > h2, "replies add height: {h1} vs {h2}");
    assert_eq!(
        v.offsets.height(3),
        ROW_H + h1 + h2 + 3.0 * CARD_MARGIN,
        "both cards under the added line"
    );
    assert_eq!(v.offsets.height(2), ROW_H);
    assert_eq!(
        v.offsets.total(),
        5.0 * ROW_H + h1 + h2 + 3.0 * CARD_MARGIN,
        "b.rs threads are not on this file"
    );
    // a long body wraps into more lines and a taller card
    let mut long = thread(9, at("a.rs", None, Some(11)), 0);
    long.root.body = "word ".repeat(200);
    assert!(card_height(&long, true, w, &mut measure) > h2 + 100.0);
    // deleted bodies render as one placeholder line
    let mut del = thread(9, at("a.rs", None, Some(11)), 0);
    del.root.deleted = true;
    del.root.body = "word ".repeat(200);
    assert_eq!(card_height(&del, true, w, &mut measure), h2);
}

#[test]
fn resolved_threads_collapsed_by_default() {
    let mut resolved = thread(1, at("a.rs", None, Some(11)), 3);
    resolved.root.resolved = true;
    let mut c = comments_with(vec![resolved, thread(2, at("a.rs", None, Some(12)), 0)]);
    assert!(!c.is_expanded(&c.threads[0]));
    assert!(c.is_expanded(&c.threads[1]), "unresolved: expanded");
    let mut d = view_of(rows());
    let v = d.view_mut("a.rs").unwrap();
    c.layout(v, 400.0, 0.0, &mut measure);
    let w = v.comments.card_w;
    let collapsed = card_height(&c.threads[0], false, w, &mut measure);
    assert_eq!(collapsed, COLLAPSED_H, "one line");
    assert_eq!(v.offsets.height(3), ROW_H + collapsed + 2.0 * CARD_MARGIN);
    c.toggle(&1.into());
    assert!(c.is_expanded(&c.threads[0]));
    c.layout(v, 400.0, 0.0, &mut measure);
    let open = card_height(&c.threads[0], true, w, &mut measure);
    assert!(open > collapsed);
    assert_eq!(v.offsets.height(3), ROW_H + open + 2.0 * CARD_MARGIN);
    c.toggle(&1.into());
    c.layout(v, 400.0, 0.0, &mut measure);
    assert_eq!(v.offsets.height(3), ROW_H + collapsed + 2.0 * CARD_MARGIN);
}

#[test]
fn counts_and_scopes() {
    let mut resolved = thread(2, at("a.rs", None, Some(11)), 0);
    resolved.root.resolved = true;
    let mut lone_deleted = thread(5, at("a.rs", None, Some(12)), 0);
    lone_deleted.root.deleted = true;
    let c = comments_with(vec![
        thread(1, at("a.rs", None, Some(11)), 1),
        resolved,
        thread(3, at("a.rs", None, None), 0),
        thread(4, None, 2),
        lone_deleted,
        thread(6, at("b.rs", Some(3), None), 0),
    ]);
    assert_eq!(c.pr_level, vec![3]);
    assert_eq!(scope(&c.threads[3]), Scope::Pr);
    assert_eq!(scope(&c.threads[2]), Scope::File("a.rs"));
    let by = c.unresolved_by_path();
    assert_eq!(by.get("a.rs"), Some(&2), "inline + file-level, {by:?}");
    assert_eq!(by.get("b.rs"), Some(&1));
    assert_eq!(c.unresolved(), 4, "PR-level counts in the total");
}

/// Comment fetching through the real service (local demo repository).
mod service {
    use crate::demo::{review_comments, review_fixture_with, DemoReview};
    use crate::review::service::{CommentFetcher, ReviewEvent, ReviewService};
    use crate::review::ui::ReviewScreen;
    use eframe::egui;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};
    use store::Store;

    fn svc(f: &DemoReview, fetch: CommentFetcher) -> ReviewService {
        let head = f.head.clone();
        ReviewService::with_local(
            Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
            f.repo_path.clone(),
            egui::Context::default(),
            Arc::new(move |_, _| Ok(head.clone())),
            fetch,
        )
    }

    fn until(s: &ReviewService, done: impl Fn(&[ReviewEvent]) -> bool) -> Vec<ReviewEvent> {
        let start = Instant::now();
        let mut all = Vec::new();
        loop {
            all.extend(s.poll());
            if done(&all) {
                return all;
            }
            assert!(start.elapsed() < Duration::from_secs(30), "{all:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn has(evs: &[ReviewEvent], f: fn(&ReviewEvent) -> bool) -> bool {
        evs.iter().any(f)
    }

    #[test]
    fn comments_fetched_after_open_and_on_refresh() {
        let f = review_fixture_with(6);
        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();
        let s = svc(
            &f,
            Arc::new(move |_, _| {
                *c.lock().unwrap() += 1;
                Ok(review_comments())
            }),
        );
        s.refresh_comments(); // not open yet: no-op
        s.open(f.repo.clone(), f.pr.clone());
        let evs = until(&s, |e| {
            has(e, |x| matches!(x, ReviewEvent::Comments { .. }))
        });
        let opened = evs
            .iter()
            .position(|e| matches!(e, ReviewEvent::Opened { .. }));
        let comments = evs
            .iter()
            .position(|e| matches!(e, ReviewEvent::Comments { .. }));
        assert!(opened < comments, "{evs:?}");
        let Some(ReviewEvent::Comments { threads, skipped }) = comments.map(|i| &evs[i]) else {
            unreachable!()
        };
        assert_eq!(*skipped, 0);
        assert!(threads.iter().any(|t| t.replies.len() == 2), "threaded");
        s.refresh_comments();
        until(&s, |e| {
            has(e, |x| matches!(x, ReviewEvent::Comments { .. }))
        });
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[test]
    fn comments_failed_does_not_break_diff() {
        let f = review_fixture_with(6);
        let s = svc(&f, Arc::new(|_, _| Err("network error".into())));
        s.open(f.repo.clone(), f.pr.clone());
        let evs = until(&s, |e| {
            has(e, |x| matches!(x, ReviewEvent::CommentsFailed(_)))
                && has(e, |x| matches!(x, ReviewEvent::Opened { .. }))
        });
        assert!(
            !has(&evs, |x| matches!(x, ReviewEvent::Failed(_))),
            "{evs:?}"
        );
        // the screen keeps working: file list, patch, error shown only for comments
        let mut screen = ReviewScreen::new(
            f.repo.clone(),
            f.pr.clone(),
            Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        );
        for e in evs {
            screen.apply(e);
        }
        assert!(screen.failed.is_none() && screen.files.is_some());
        assert!(!screen.comments.loading);
        let err = screen.comments.error.clone().unwrap();
        assert!(err.contains("network error"), "{err}");
        screen.take_requests();
        screen.open_file(crate::demo::DEMO_COMMENTS_FILE);
        for p in screen.take_requests() {
            s.request_patch(p, true);
        }
        let evs = until(&s, |e| has(e, |x| matches!(x, ReviewEvent::Patch { .. })));
        for e in evs {
            screen.apply(e);
        }
        let path = screen.selected_file().unwrap().path.clone();
        assert!(screen.diff.view(&path).is_some());
        // a panicking fetcher is a failure too, not a dead service
        let s2 = svc(&f, Arc::new(|_, _| panic!("injected")));
        s2.open(f.repo.clone(), f.pr.clone());
        until(&s2, |e| {
            has(e, |x| matches!(x, ReviewEvent::CommentsFailed(_)))
        });
    }

    #[test]
    fn comments_event_dropped_after_close() {
        let f = review_fixture_with(6);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let g = gate.clone();
        let s = svc(
            &f,
            Arc::new(move |_, _| {
                let (m, cv) = &*g;
                let mut open = m.lock().unwrap();
                while !*open {
                    open = cv.wait(open).unwrap();
                }
                Ok(review_comments())
            }),
        );
        s.open(f.repo.clone(), f.pr.clone());
        until(&s, |e| has(e, |x| matches!(x, ReviewEvent::Opened { .. })));
        s.close();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        std::thread::sleep(Duration::from_millis(500));
        let evs = s.poll();
        assert!(evs.is_empty(), "{evs:?}");
        // a reopen is a new generation: its own fetch is delivered
        s.open(f.repo.clone(), f.pr.clone());
        until(&s, |e| {
            has(e, |x| matches!(x, ReviewEvent::Comments { .. }))
        });
    }
}

#[test]
fn many_threads_height_update_is_not_quadratic() {
    let n = 100_000u32;
    let rows: Vec<Row> = (1..=n)
        .map(|i| row(RowKind::Context, Some(i), Some(i)))
        .collect();
    let threads: Vec<Thread> = (0..2000u64)
        .map(|i| {
            let mut t = thread(i + 1, at("a.rs", None, Some(i as u32 * 50 + 1)), 1);
            t.root.resolved = i % 3 == 0;
            t
        })
        .collect();
    let mut c = comments_with(threads);
    let mut d = view_of(rows);
    let v = d.view_mut("a.rs").unwrap();
    // timed: placement, the first layout, toggles, re-layouts and a width change
    let start = std::time::Instant::now();
    c.layout(v, 600.0, 0.0, &mut measure);
    for i in 0..20 {
        c.toggle(&(i + 1).into());
        let y = v.offsets.top(50_000);
        c.layout(v, 600.0, y, &mut measure);
        c.layout(v, 600.0, y, &mut measure); // unchanged: no work
    }
    c.layout(v, 900.0, 0.0, &mut measure);
    let took = start.elapsed();
    assert!(took < std::time::Duration::from_secs(5), "{took:?}");
    assert_eq!(v.comments.placement.inline.len(), 2000);
    let w = v.comments.card_w;
    let extra: f32 = c
        .threads
        .iter()
        .map(|t| card_height(t, c.is_expanded(t), w, &mut measure) + 2.0 * CARD_MARGIN)
        .sum();
    let want = n as f32 * ROW_H + extra;
    assert!(
        (v.offsets.total() - want).abs() < 1.0,
        "{}",
        v.offsets.total()
    );
}
