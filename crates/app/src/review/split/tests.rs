use super::*;
use crate::review::selection::Pos;

fn row(kind: RowKind, old: Option<u32>, new: Option<u32>, text: &str) -> Row {
    Row {
        kind,
        old_line: old,
        new_line: new,
        text: text.into(),
    }
}

fn del(n: u32, t: &str) -> Row {
    row(RowKind::Del, Some(n), None, t)
}
fn add(n: u32, t: &str) -> Row {
    row(RowKind::Add, None, Some(n), t)
}
fn ctx(o: u32, n: u32, t: &str) -> Row {
    row(RowKind::Context, Some(o), Some(n), t)
}
fn hunk(t: &str) -> Row {
    row(RowKind::HunkHeader, None, None, t)
}
fn nonl() -> Row {
    row(
        RowKind::NoNewline,
        None,
        None,
        "\\ No newline at end of file",
    )
}

/// Lines as (left, right) row indices.
fn pairs(rows: &[Row]) -> Vec<(Option<usize>, Option<usize>)> {
    pair_lines(rows)
        .lines
        .iter()
        .map(|l| (l.left(), l.right()))
        .collect()
}

fn check_line_of(rows: &[Row]) {
    let m = pair_lines(rows);
    assert_eq!(m.line_of.len(), rows.len());
    for (r, &l) in m.line_of.iter().enumerate() {
        let line = m.lines[l as usize];
        assert!(
            line.left() == Some(r) || line.right() == Some(r),
            "row {r} is on its line {l}"
        );
    }
    // every row appears exactly once per side it is on
    let shown: usize = m
        .lines
        .iter()
        .map(|l| {
            if l.shared() {
                1
            } else {
                usize::from(l.left().is_some()) + usize::from(l.right().is_some())
            }
        })
        .sum();
    assert_eq!(shown, rows.len());
}

#[test]
fn context_and_headers_are_on_both_sides() {
    let rows = vec![hunk("@@ -1,2 +1,2 @@"), ctx(1, 1, "a"), ctx(2, 2, "b")];
    assert_eq!(
        pairs(&rows),
        [(Some(0), Some(0)), (Some(1), Some(1)), (Some(2), Some(2))]
    );
    let m = pair_lines(&rows);
    assert!(m.lines.iter().all(|l| l.shared()));
    check_line_of(&rows);
}

#[test]
fn unequal_runs_line_up_row_by_row_and_pad_the_shorter_side() {
    // 3 deletions, 1 addition, then 1 deletion, 3 additions
    let rows = vec![
        ctx(1, 1, "x"),
        del(2, "a"),
        del(3, "b"),
        del(4, "c"),
        add(2, "A"),
        ctx(5, 3, "y"),
        del(6, "d"),
        add(4, "D"),
        add(5, "E"),
        add(6, "F"),
    ];
    assert_eq!(
        pairs(&rows),
        [
            (Some(0), Some(0)),
            (Some(1), Some(4)),
            (Some(2), None),
            (Some(3), None),
            (Some(5), Some(5)),
            (Some(6), Some(7)),
            (None, Some(8)),
            (None, Some(9)),
        ]
    );
    check_line_of(&rows);
    let m = pair_lines(&rows);
    assert_eq!(
        m.line_of(4),
        1,
        "the addition sits on the first deletion's line"
    );
    assert_eq!(m.line_of(9), 7);
}

#[test]
fn only_additions_or_only_deletions() {
    let adds = vec![hunk("@@ -0,0 +1,2 @@"), add(1, "a"), add(2, "b")];
    assert_eq!(
        pairs(&adds),
        [(Some(0), Some(0)), (None, Some(1)), (None, Some(2))]
    );
    check_line_of(&adds);
    let dels = vec![hunk("@@ -1,2 +0,0 @@"), del(1, "a"), del(2, "b")];
    assert_eq!(
        pairs(&dels),
        [(Some(0), Some(0)), (Some(1), None), (Some(2), None)]
    );
    check_line_of(&dels);
    assert!(pair_lines(&[]).lines.is_empty());
}

#[test]
fn an_add_run_before_a_del_run_does_not_pair_with_it() {
    // the word diff pairs a Del run only with the Add run after it; so does the split
    let rows = vec![add(1, "A"), del(1, "a"), add(2, "B")];
    assert_eq!(pairs(&rows), [(None, Some(0)), (Some(1), Some(2))]);
    check_line_of(&rows);
}

#[test]
fn no_newline_markers_stay_on_their_side() {
    // both ends lack a newline: the markers line up
    let both = vec![del(1, "old"), nonl(), add(1, "new"), nonl()];
    assert_eq!(pairs(&both), [(Some(0), Some(2)), (Some(1), Some(3))]);
    check_line_of(&both);
    // only the old file lacked it
    let old = vec![del(1, "old"), nonl(), add(1, "new")];
    assert_eq!(pairs(&old), [(Some(0), Some(2)), (Some(1), None)]);
    check_line_of(&old);
    // only the new file lacks it
    let new = vec![del(1, "old"), add(1, "new"), nonl()];
    assert_eq!(pairs(&new), [(Some(0), Some(1)), (None, Some(2))]);
    check_line_of(&new);
    // after a context row it is on both sides
    let c = vec![ctx(1, 1, "end"), nonl()];
    assert_eq!(pairs(&c), [(Some(0), Some(0)), (Some(1), Some(1))]);
    check_line_of(&c);
}

#[test]
fn comment_target_is_the_old_line_on_the_left_and_the_new_line_on_the_right() {
    let rows = vec![
        hunk("@@ -10,3 +20,2 @@"),
        ctx(10, 20, "same"),
        del(11, "gone"),
        del(12, "also gone"),
        add(21, "new"),
        nonl(),
    ];
    let m = pair_lines(&rows);
    let t = |l: usize, s| comment_target(&rows, m.lines[l], s);
    assert_eq!(t(0, SplitSide::Left), None, "hunk header");
    assert_eq!(t(1, SplitSide::Left), Some((Side::Old, 10)));
    assert_eq!(t(1, SplitSide::Right), Some((Side::New, 20)));
    assert_eq!(t(2, SplitSide::Left), Some((Side::Old, 11)));
    assert_eq!(t(2, SplitSide::Right), Some((Side::New, 21)));
    assert_eq!(t(3, SplitSide::Left), Some((Side::Old, 12)));
    assert_eq!(t(3, SplitSide::Right), None, "the no-newline marker");
    assert_eq!(m.lines.len(), 4);
    let padded = vec![del(1, "a"), del(2, "b"), add(1, "A")];
    let p = pair_lines(&padded);
    assert_eq!(
        comment_target(&padded, p.lines[1], SplitSide::Right),
        None,
        "padding"
    );
    assert_eq!(
        comment_target(&padded, p.lines[1], SplitSide::Left),
        Some((Side::Old, 2))
    );
    // the side a row is shown on
    assert_eq!(m.side_of(2, SplitSide::Right), SplitSide::Left);
    assert_eq!(m.side_of(4, SplitSide::Left), SplitSide::Right);
    assert_eq!(
        m.side_of(1, SplitSide::Left),
        SplitSide::Left,
        "shared: preferred"
    );
    assert_eq!(m.side_of(1, SplitSide::Right), SplitSide::Right);
}

#[test]
fn extras_of_both_rows_of_a_line_add_up() {
    let rows = vec![ctx(1, 1, "x"), del(2, "a"), add(2, "A"), ctx(3, 3, "y")];
    let m = pair_lines(&rows);
    let e = split_extras(&m, [(2, 30.0), (0, 10.0), (1, 5.0), (9, 99.0)]);
    assert_eq!(e, [(0, 10.0), (1, 35.0)]);
    assert!(split_extras(&m, []).is_empty());
}

#[test]
fn a_selection_copies_one_side_only() {
    let rows = vec![
        hunk("@@ -1,3 +1,2 @@"),
        ctx(1, 1, "keep"),
        del(2, "old two"),
        del(3, "old three"),
        add(2, "new two"),
        nonl(),
    ];
    let m = pair_lines(&rows);
    let all = Selection::all(m.lines.len()).unwrap();
    assert_eq!(
        selected_text_split(&rows, &m, SplitSide::Left, &all),
        "@@ -1,3 +1,2 @@\nkeep\nold two\nold three"
    );
    assert_eq!(
        selected_text_split(&rows, &m, SplitSide::Right, &all),
        "@@ -1,3 +1,2 @@\nkeep\nnew two"
    );
    // columns apply to the side's own text
    let part = Selection {
        anchor: Pos { row: 2, col: 4 },
        head: Pos { row: 3, col: 3 },
    };
    assert_eq!(
        selected_text_split(&rows, &m, SplitSide::Left, &part),
        "two\nold"
    );
    assert_eq!(
        selected_text_split(&rows, &m, SplitSide::Right, &part),
        "two",
        "padding is skipped"
    );
    let empty = Selection {
        anchor: Pos { row: 1, col: 0 },
        head: Pos { row: 1, col: 0 },
    };
    assert_eq!(selected_text_split(&rows, &m, SplitSide::Left, &empty), "");
}

#[test]
fn layout_is_stored_and_toggled_and_narrow_windows_fall_back() {
    for l in [DiffLayout::Unified, DiffLayout::Split] {
        assert_eq!(DiffLayout::parse(Some(l.as_str())), l);
        assert_eq!(l.toggled().toggled(), l);
    }
    assert_eq!(DiffLayout::parse(None), DiffLayout::Unified);
    assert_eq!(DiffLayout::parse(Some("weird")), DiffLayout::Unified);
    assert!(split_fits(1400.0, 1000.0));
    assert!(!split_fits(899.0, 1000.0), "narrow window");
    assert!(!split_fits(1400.0, 500.0), "narrow pane (wide file list)");
}

/// 200k rows (as the audit's large file) pair in a few milliseconds and stay compact.
#[test]
fn pairing_a_large_file_is_linear() {
    let rows: Vec<Row> = (0..200_000u32)
        .map(|i| match i % 5 {
            0 => ctx(i, i, "c"),
            1 | 2 => del(i, "d"),
            _ => add(i, "a"),
        })
        .collect();
    let t = std::time::Instant::now();
    let m = pair_lines(&rows);
    assert!(
        t.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(m.lines.len(), 120_000, "each 2 + 2 change is 2 lines");
    assert_eq!(m.bytes(), 120_000 * 8 + 200_000 * 4);
}
