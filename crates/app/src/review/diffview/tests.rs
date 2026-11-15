use super::*;

/// The large-file pipeline on a synthetic 200k-row patch (about 30 MB, like the audit's
/// `large.rs`): bytes to text, rows, widest row. Each step against the code it replaced
/// (kept here as the reference, and checked to give the same result).
/// `cargo test -p app --release --bin nit large_patch_pipeline -- --ignored --nocapture`
#[test]
#[ignore]
fn large_patch_pipeline_bench() {
    use prcore::diff::parse_patch;
    use std::time::Instant;

    fn old_cols(text: &str) -> usize {
        super::super::columns::clusters(text)
            .last()
            .map_or(0, |c| c.col1)
    }
    let mut patch = String::from(
        "diff --git a/large.rs b/large.rs\n--- a/large.rs\n+++ b/large.rs\n@@ -0,0 +1,200000 @@\n",
    );
    for i in 0..200_000 {
        patch.push_str(&format!(
            "+    let value_{i:06} = compute_something(input.field_{i}, \"literal string {i}\", \
             Some(&mut state), options.clone()).unwrap_or_default(); // note {i}\n"
        ));
    }
    let bytes = patch.clone().into_bytes();
    println!("patch: {} bytes", bytes.len());
    let best = |f: &mut dyn FnMut() -> usize| {
        (0..5)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(f());
                t.elapsed()
            })
            .min()
            .unwrap()
    };
    let lossy = best(&mut || String::from_utf8_lossy(&bytes).into_owned().len());
    let zero = best(&mut || String::from_utf8(bytes.clone()).unwrap().len())
        .saturating_sub(best(&mut || bytes.clone().len()));
    // parse before/after: prcore's `parse_patch_matches_the_old_parser_and_is_faster`
    let parse = best(&mut || parse_patch(&patch).len());
    let rows = parse_patch(&patch);
    let cols_old = best(&mut || rows.iter().map(|r| old_cols(&r.text)).max().unwrap_or(0));
    let cols_new = best(&mut || rows.iter().map(|r| columns(&r.text)).max().unwrap_or(0));
    assert_eq!(
        rows.iter().map(|r| old_cols(&r.text)).max(),
        rows.iter().map(|r| columns(&r.text)).max()
    );
    println!("bytes -> text: lossy copy {lossy:?}, validate only {zero:?}");
    println!("parse (new): {parse:?}");
    println!("widest row: clusters {cols_old:?}, ascii fast path {cols_new:?}");
}

#[test]
fn minimap_ticks_are_scaled_monotonic_and_clicks_map_back() {
    let mut o = Offsets::new(100, ROW_H);
    // a tall card under row 10 pushes the later rows down
    o.set_extras([(10, 50.0 * ROW_H)]);
    let ticks = minimap_ticks(
        &o,
        [
            (90, TickKind::Draft),
            (10, TickKind::Unresolved),
            (10, TickKind::Unresolved),
            (10, TickKind::Resolved),
            (50, TickKind::Resolved),
            (500, TickKind::Draft),
        ],
    );
    let rows: Vec<(usize, TickKind)> = ticks.iter().map(|t| (t.row, t.kind)).collect();
    assert_eq!(
        rows,
        [
            (10, TickKind::Unresolved),
            (10, TickKind::Resolved),
            (50, TickKind::Resolved),
            (90, TickKind::Draft)
        ],
        "deduplicated, sorted, rows outside the file dropped"
    );
    assert!(
        ticks.windows(2).all(|w| w[0].frac <= w[1].frac),
        "monotonic"
    );
    let total = o.total();
    assert_eq!(ticks[0].frac, 10.0 * ROW_H / total);
    assert_eq!(ticks[2].frac, o.top(50) / total);
    assert!(
        ticks[2].frac > 0.5,
        "scaled by content height, not row count"
    );
    assert!(ticks.iter().all(|t| (0.0..=1.0).contains(&t.frac)));
    assert_eq!(tick_at(&ticks, ticks[2].frac + 0.001, 0.01), Some(50));
    assert_eq!(tick_at(&ticks, ticks[3].frac - 0.005, 0.01), Some(90));
    assert_eq!(tick_at(&ticks, 0.0, 0.01), None, "nothing near");
    assert_eq!(tick_at(&[], 0.5, 1.0), None);
    assert!(minimap_ticks(&Offsets::new(0, ROW_H), [(0, TickKind::Draft)]).is_empty());
}

fn rows(n: usize) -> Arc<Vec<Row>> {
    Arc::new(
        (0..n)
            .map(|i| Row {
                kind: RowKind::Context,
                old_line: Some(i as u32 + 1),
                new_line: Some(i as u32 + 1),
                text: "x".repeat(i % 7),
            })
            .collect(),
    )
}

#[test]
fn offsets_binary_search_finds_row_at_y() {
    let o = Offsets::new(100, ROW_H);
    assert_eq!(o.total(), 100.0 * ROW_H);
    assert_eq!(o.row_at(0.0), Some(0));
    assert_eq!(o.row_at(ROW_H - 0.1), Some(0));
    assert_eq!(o.row_at(ROW_H), Some(1));
    assert_eq!(o.row_at(42.5 * ROW_H), Some(42));
    assert_eq!(o.row_at(-5.0), Some(0), "clamped above");
    assert_eq!(o.row_at(1e9), Some(99), "clamped below");
    assert_eq!(Offsets::new(0, ROW_H).row_at(0.0), None);
}

#[test]
fn variable_heights_shift_following_rows() {
    let mut o = Offsets::new(10, ROW_H);
    o.set_extra_height(3, 50.0);
    assert_eq!(o.top(3), 3.0 * ROW_H, "rows above are unchanged");
    assert_eq!(o.height(3), ROW_H + 50.0);
    assert_eq!(o.top(4), 4.0 * ROW_H + 50.0);
    assert_eq!(o.total(), 10.0 * ROW_H + 50.0);
    assert_eq!(
        o.row_at(o.top(3) + ROW_H + 10.0),
        Some(3),
        "inside the extra"
    );
    assert_eq!(o.row_at(o.top(4) + 1.0), Some(4));
    o.set_extra_height(1, 20.0);
    assert_eq!(o.top(4), 4.0 * ROW_H + 70.0);
    o.set_extra_height(3, 0.0);
    assert_eq!(o.top(4), 4.0 * ROW_H + 20.0);
    assert_eq!(o.height(3), ROW_H);
}

#[test]
fn set_extras_matches_one_by_one_updates_and_replaces_old_extras() {
    let mut a = Offsets::new(20, ROW_H);
    let mut b = Offsets::new(20, ROW_H);
    a.set_extra_height(7, 99.0);
    a.set_extras([(3, 40.0), (12, 25.0), (50, 10.0), (5, 0.0)]);
    b.set_extra_height(3, 40.0);
    b.set_extra_height(12, 25.0);
    for i in 0..=20 {
        let (ya, yb) = (a.y[i], b.y[i]);
        assert!((ya - yb).abs() < 1e-3, "row {i}: {ya} vs {yb}");
    }
    assert_eq!(a.height(7), ROW_H, "previous extras are replaced");
    a.set_extra_height(3, 0.0);
    assert_eq!(a.total(), 20.0 * ROW_H + 25.0);
}

#[test]
fn scroll_anchor_survives_height_changes_above_it() {
    let mut o = Offsets::new(100, ROW_H);
    o.set_extras([(60, 30.0)]);
    let y = o.top(50) + 5.0;
    let a = o.anchor_at(y).unwrap();
    assert_eq!(a, (50, 5.0));
    o.set_extras([(10, 200.0), (60, 30.0)]); // grew above
    assert_eq!(o.y_for(a), y + 200.0);
    o.set_extras([(70, 99.0)]); // below only: unchanged
    assert_eq!(o.y_for(a), 50.0 * ROW_H + 5.0);
    // an anchor inside a row's own extra stays inside that row when it shrinks
    o.set_extras([(50, 100.0)]);
    let deep = o.anchor_at(o.top(50) + 80.0).unwrap();
    o.set_extras([]);
    assert_eq!(o.y_for(deep), o.top(51), "clamped to the row's new height");
    assert_eq!(Offsets::new(0, ROW_H).anchor_at(0.0), None);
}

#[test]
fn visible_range_for_viewport_is_bounded() {
    let mut v = DiffView::default();
    v.set_patch_computed("big.rs", rows(1_000_000), true);
    let top = 600_000.0 * ROW_H + 3.0;
    let r = v.rows_to_layout("big.rs", top, 1200.0);
    assert!(r.len() < 200, "{r:?}");
    assert!(r.start <= 600_000 && r.end > 600_000 + 60, "{r:?}");
    let end = v.rows_to_layout("big.rs", 1e12, 900.0);
    assert!(end.len() < 200 && end.end == 1_000_000, "{end:?}");
    assert!(v.rows_to_layout("other.rs", 0.0, 900.0).is_empty());
}

#[test]
fn scroll_is_remembered_per_file_and_only_the_current_file_is_held() {
    let mut v = DiffView::default();
    v.set_patch_computed("a.rs", rows(50), false);
    v.set_scroll("a.rs", Vec2::new(0.0, 120.0));
    v.set_patch_computed("b.rs", rows(5), false);
    assert!(v.view("a.rs").is_none() && v.view("b.rs").is_some());
    assert_eq!(v.scroll("a.rs"), Vec2::new(0.0, 120.0));
    assert_eq!(v.scroll("b.rs"), Vec2::ZERO);
    v.set_extra_height("b.rs", 2, 30.0);
    assert_eq!(v.view("b.rs").unwrap().offsets.total(), 5.0 * ROW_H + 30.0);
}

#[test]
fn max_cols_tracks_the_widest_expanded_row() {
    let mut v = DiffView::default();
    let r = vec![
        Row {
            kind: RowKind::Add,
            old_line: None,
            new_line: Some(1),
            text: "\tab".into(),
        },
        Row {
            kind: RowKind::Add,
            old_line: None,
            new_line: Some(2),
            text: "abc".into(),
        },
    ];
    v.set_patch_computed("t.rs", Arc::new(r), false);
    assert_eq!(v.view("t.rs").unwrap().max_cols, 6);
}

#[test]
fn set_patch_takes_the_precomputed_layout_as_is() {
    let mut v = DiffView::default();
    let r = rows(7);
    // the layout comes from the worker; set_patch never recomputes it
    let layout = Arc::new(PatchLayout {
        max_cols: 99,
        partner: vec![NO_PARTNER; r.len()],
        plain: false,
        split: Default::default(),
    });
    v.set_patch("a.rs", r.clone(), false, layout.clone());
    assert_eq!(v.view("a.rs").unwrap().max_cols, 99);
    assert_eq!(PatchLayout::compute(&r, false).max_cols, 6);
    assert!(
        PatchLayout::compute(&r, true).partner.is_empty(),
        "plain: no word diff"
    );
    let pairs = Arc::new(vec![
        kind_row(RowKind::Del, "a"),
        kind_row(RowKind::Add, "b"),
    ]);
    assert_eq!(PatchLayout::compute(&pairs, false).partner, vec![1, 0]);
}

#[test]
fn unchanged_extra_height_is_a_no_op() {
    let mut o = Offsets::new(10, ROW_H);
    assert!(o.set_extra_height(3, 50.0));
    assert!(!o.set_extra_height(3, 50.0), "same value");
    assert!(!o.set_extra_height(5, 0.0), "already none");
    assert!(!o.set_extra_height(99, 10.0), "out of range");
    assert!(o.set_extra_height(3, 0.0));
    assert_eq!(o.total(), 10.0 * ROW_H);
}

#[test]
fn tabs_expand_to_four_columns_and_clip() {
    assert_eq!(columns("\tx"), 5);
    assert_eq!(columns("ab\tc"), 5);
    assert_eq!(columns("abc\r"), 3);
    assert_eq!(visible_columns("ab\tc", 0, 10), "ab  c");
    assert_eq!(visible_columns("ab\tcd", 3, 2), " c");
    assert_eq!(visible_columns("hello\r", 0, 10), "hello");
    assert_eq!(visible_columns("héllo wörld", 6, 3), "wör");
    assert_eq!(visible_columns("short", 10, 5), "");
    let long = "y".repeat(20_000);
    assert_eq!(visible_columns(&long, 19_998, 10), "yy");
}

fn kind_row(kind: RowKind, text: &str) -> Row {
    Row {
        kind,
        old_line: None,
        new_line: None,
        text: text.into(),
    }
}

#[test]
fn styled_runs_match_visible_columns_with_tabs_and_multibyte() {
    let (red, blue) = (Color32::RED, Color32::BLUE);
    // bytes: \t=0 h=1 é=2..4 l=4 \t=5 w=6 ö=7..9 r=9 \r=10
    let text = "\thél\twör\r";
    let spans = vec![(0..4, red), (5..9, blue)];
    for (start, count) in [(0, 40), (0, 3), (2, 5), (4, 6), (7, 2), (30, 5)] {
        let st = styled_columns(text, &spans, &[], start, count);
        let joined: String = st.runs.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            joined,
            visible_columns(text, start, count),
            "{start} {count}"
        );
    }
    let st = styled_columns(text, &spans, &[], 0, 40);
    assert_eq!(st.runs[0], ("    hé".to_string(), Some(red)));
    assert_eq!(st.runs[1], ("l".to_string(), None));
    assert_eq!(st.runs[2], (" wö".to_string(), Some(blue)));
    assert_eq!(st.runs[3], ("r".to_string(), None));
}

#[test]
#[allow(clippy::single_range_in_vec_init)]
fn styled_marks_are_columns_relative_to_start() {
    // "ab\tcd": columns a0 b1 tab2-3 c4 d5; marking bytes of "b\tc" = 1..4
    let st = styled_columns("ab\tcd", &[], &[1..4], 0, 10);
    assert_eq!(st.marks, [1..5]);
    let st = styled_columns("ab\tcd", &[], &[1..4], 3, 10);
    assert_eq!(st.marks, [0..2], "clipped at the left edge and shifted");
    let st = styled_columns("héllo", &[], &[1..3], 0, 10);
    assert_eq!(st.marks, [1..2], "a multi-byte char is one column");
    assert!(styled_columns("abc", &[], &[], 0, 10).marks.is_empty());
}

#[test]
fn rows_pair_in_order_within_del_add_runs() {
    use RowKind::*;
    let rows: Vec<Row> = [
        (HunkHeader, "@@"),
        (Del, "a"),
        (Del, "b"),
        (Del, "c"),
        (Add, "A"),
        (Add, "B"),
        (Context, "x"),
        (Add, "lone"),
        (Del, "d"),
    ]
    .iter()
    .map(|(k, t)| kind_row(*k, t))
    .collect();
    let p = pair_rows(&rows);
    assert_eq!((p[1], p[4]), (4, 1));
    assert_eq!((p[2], p[5]), (5, 2));
    for i in [0, 3, 6, 7, 8] {
        assert_eq!(p[i], NO_PARTNER, "row {i}");
    }
}

#[test]
fn no_newline_marker_does_not_split_a_del_add_pair() {
    use RowKind::*;
    let rows: Vec<Row> = [
        (Del, "old"),
        (NoNewline, "\\ No newline at end of file"),
        (Add, "new"),
        (NoNewline, "\\ No newline at end of file"),
    ]
    .iter()
    .map(|(k, t)| kind_row(*k, t))
    .collect();
    let p = pair_rows(&rows);
    assert_eq!((p[0], p[2]), (2, 0));
    assert_eq!((p[1], p[3]), (NO_PARTNER, NO_PARTNER));
}

#[test]
fn word_ranges_are_lazy_memoized_and_skipped_for_plain_files() {
    let rows = Arc::new(vec![
        kind_row(RowKind::Del, "let x = 1;"),
        kind_row(RowKind::Add, "let x = 2;"),
    ]);
    let mut v = DiffView::default();
    v.set_patch_computed("a.rs", rows.clone(), false);
    let fv = v.view("a.rs").unwrap();
    assert!(fv.words.borrow().is_empty(), "nothing before drawing");
    assert_eq!(*fv.word_ranges(0).unwrap(), vec![8..9]);
    assert_eq!(fv.words.borrow().len(), 2, "the partner is memoized too");
    assert_eq!(*fv.word_ranges(1).unwrap(), vec![8..9]);
    v.set_patch_computed("p.rs", rows, true);
    assert!(v.view("p.rs").unwrap().word_ranges(0).is_none());
}

#[test]
fn highlight_is_applied_only_to_the_matching_current_file() {
    let mut v = DiffView::default();
    v.set_patch_computed("a.rs", rows(2), false);
    v.set_highlight("b.rs", 2, 0..2, Arc::new(vec![vec![], vec![]]));
    assert!(v.view("a.rs").unwrap().spans.is_none());
    v.set_highlight("a.rs", 1, 0..1, Arc::new(vec![vec![]]));
    assert!(v.view("a.rs").unwrap().spans.is_none(), "wrong row count");
    v.set_highlight("a.rs", 2, 0..3, Arc::new(vec![vec![], vec![], vec![]]));
    assert!(
        v.view("a.rs").unwrap().spans.is_none(),
        "range past the rows"
    );
    v.set_highlight("a.rs", 2, 0..2, Arc::new(vec![vec![], vec![]]));
    assert!(v.view("a.rs").unwrap().spans.is_some());
}

#[test]
fn highlight_steps_are_merged_into_the_current_file() {
    let (red, blue) = (Color32::RED, Color32::BLUE);
    let mut v = DiffView::default();
    v.set_patch_computed("a.rs", rows(5), false);
    v.set_highlight("a.rs", 5, 3..5, Arc::new(vec![vec![(0..1, red)]; 2]));
    let sp = v.view("a.rs").unwrap().spans.clone().unwrap();
    assert_eq!(sp.len(), 5);
    assert!(sp[..3].iter().all(|s| s.is_empty()), "not coloured yet");
    assert_eq!(sp[4], vec![(0..1, red)]);
    drop(sp);
    v.set_highlight("a.rs", 5, 0..3, Arc::new(vec![vec![(0..1, blue)]; 3]));
    let sp = v.view("a.rs").unwrap().spans.clone().unwrap();
    assert_eq!(sp[0], vec![(0..1, blue)]);
    assert_eq!(sp[3], vec![(0..1, red)], "earlier step kept");
    // a whole file's colours are taken as they are (no copy)
    let whole = Arc::new(vec![Vec::new(); 5]);
    v.set_highlight("a.rs", 5, 0..5, whole.clone());
    assert!(Arc::ptr_eq(
        v.view("a.rs").unwrap().spans.as_ref().unwrap(),
        &whole
    ));
    // a step for a plain file is ignored
    v.set_patch_computed("p.rs", rows(2), true);
    v.set_highlight("p.rs", 2, 0..1, Arc::new(vec![vec![]]));
    assert!(v.view("p.rs").unwrap().spans.is_none());
}

const MIXED: &str = "a\t日本e\u{301}😀x\u{200d}y\tz\r";

#[test]
fn styled_runs_equal_visible_columns_for_wide_combining_and_tabs() {
    let (red, blue) = (Color32::RED, Color32::BLUE);
    let spans = vec![(0..4, red), (7..12, blue)];
    let total = columns(MIXED);
    for start in 0..total + 2 {
        for count in 0..total + 2 {
            let st = styled_columns(MIXED, &spans, &[], start, count);
            let joined: String = st.runs.iter().map(|(t, _)| t.as_str()).collect();
            assert_eq!(
                joined,
                visible_columns(MIXED, start, count),
                "{start} {count}"
            );
            assert!(display_cols(&joined) <= count, "{start} {count}");
        }
    }
}

#[test]
fn scroll_offset_into_wide_chars_pads_cut_glyph() {
    // 日 = columns 0-1, 本 = 2-3
    assert_eq!(visible_columns("日本", 0, 4), "日本");
    assert_eq!(visible_columns("日本", 1, 3), " 本", "cut at the left edge");
    assert_eq!(
        visible_columns("日本", 0, 3),
        "日 ",
        "cut at the right edge"
    );
    assert_eq!(visible_columns("日本", 3, 5), " ");
    assert_eq!(visible_columns("a日", 1, 1), " ");
    // marks stay with their base and are never cut off from it
    assert_eq!(visible_columns("e\u{301}x", 0, 1), "e\u{301}");
    assert_eq!(visible_columns("e\u{301}x", 1, 1), "x");
    // a tab cut by the left edge keeps only its visible columns
    assert_eq!(visible_columns("a\tb", 2, 4), "  b");
}

#[test]
fn max_cols_counts_wide_chars_as_two() {
    let mut v = DiffView::default();
    let r = |t: &str| Row {
        kind: RowKind::Add,
        old_line: None,
        new_line: Some(1),
        text: t.into(),
    };
    v.set_patch_computed(
        "w.rs",
        Arc::new(vec![r("日本"), r("abc"), r("\t日")]),
        false,
    );
    assert_eq!(
        v.view("w.rs").unwrap().max_cols,
        6,
        "tab 4 + one wide char 2"
    );
    assert_eq!(columns("日本語"), 6);
    assert_eq!(columns("e\u{301}"), 1, "a mark adds no column");
    assert_eq!(columns("😀"), 2);
}

#[test]
#[allow(clippy::single_range_in_vec_init)]
fn word_diff_marks_align_with_selection_columns() {
    use crate::review::columns::byte_to_col;
    use crate::review::selection::col_to_x;
    // the changed word "bb" starts after a tab, a wide char and a combining mark
    let text = "\t日e\u{301} bb c";
    let start_b = text.find("bb").unwrap();
    let c0 = byte_to_col(text, start_b);
    assert_eq!(c0, 4 + 2 + 1 + 1);
    let st = styled_columns(text, &[], &[start_b..start_b + 2], 0, 40);
    assert_eq!(st.marks, [c0..c0 + 2]);
    // the selection maps that column to the x where the mark is drawn
    assert_eq!(col_to_x(text, c0, 8.0), c0 as f32 * 8.0);
    // scrolled: marks are relative to the first visible column
    let st = styled_columns(text, &[], &[start_b..start_b + 2], 5, 40);
    assert_eq!(st.marks, [c0 - 5..c0 - 3]);
    // a mark over a wide char covers both of its columns
    let st = styled_columns("x日y", &[], &[1..4], 0, 10);
    assert_eq!(st.marks, [1..3]);
}

#[test]
fn highlight_x_matches_drawn_x_for_wide_and_combining() {
    use crate::review::selection::col_to_x;
    let w = 8.0;
    let (red, blue) = (Color32::RED, Color32::BLUE);
    // colour changes inside tabs, wide chars and mark sequences
    let spans = vec![(0..1, red), (4..8, blue), (8..13, red), (13..20, blue)];
    for start in [0usize, 1, 4, 6] {
        let st = styled_columns(MIXED, &spans, &[], start, 60);
        assert!(st.runs.len() >= 3);
        for (col, text, _) in st.run_columns() {
            // the run is drawn at x(start + col); the selection highlight uses col_to_x
            let drawn_x = (start + col) as f32 * w;
            assert_eq!(
                col_to_x(MIXED, start + col, w),
                drawn_x,
                "{start}+{col} {text:?}"
            );
        }
        let end = st
            .run_columns()
            .last()
            .map(|(c, t, _)| c + display_cols(t))
            .unwrap();
        assert_eq!(start + end, columns(MIXED), "runs reach the last column");
    }
}
