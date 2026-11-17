use super::*;
use gitlocal::PrRefs;
use prcore::diff::Row;

/// About the real widths at the default style; the title is long (a real PR's).
fn widths() -> BarWidths {
    BarWidths {
        fixed: 480.0,
        title: 700.0,
        meta: 220.0,
        branches: 260.0,
        refresh: 130.0,
        open_web: 135.0,
    }
}

#[test]
fn top_bar_keeps_approve_and_request_changes_inline_at_every_width() {
    let w = widths();
    for width in (0..=4000).step_by(25) {
        let p = plan_top_bar(width as f32, &w);
        assert!(
            p.inline.contains(&BarButton::Approve) && p.inline.contains(&BarButton::RequestChanges),
            "{width}: {p:?}"
        );
        assert!(!p.more.contains(&BarButton::Approve));
        assert!(!p.more.contains(&BarButton::RequestChanges));
        // every movable button is in exactly one place
        for b in [BarButton::Refresh, BarButton::OpenWeb] {
            assert!(
                p.inline.contains(&b) != p.more.contains(&b),
                "{width}: {p:?}"
            );
        }
        assert!(p.title_w >= TITLE_MIN_W.min(w.title) && p.title_w <= w.title);
    }
}

#[test]
fn top_bar_drops_branches_first_then_repository_then_buttons() {
    let w = widths();
    let used = |p: &BarPlan| {
        let mut x = w.fixed + p.title_w;
        x += if p.meta { w.meta } else { 0.0 };
        x += if p.branches { w.branches } else { 0.0 };
        for b in &p.inline {
            x += match b {
                BarButton::Refresh => w.refresh,
                BarButton::OpenWeb => w.open_web,
                _ => 0.0,
            };
        }
        x
    };
    let wide = plan_top_bar(3000.0, &w);
    assert!(wide.meta && wide.branches && wide.more.is_empty());
    assert_eq!(wide.title_w, w.title, "room for the full title");
    let mut prev = wide;
    for width in (900..3000).rev().step_by(10) {
        let p = plan_top_bar(width as f32, &w);
        assert!(used(&p) <= width as f32 + 0.01, "{width}: clipped {p:?}");
        // narrower never shows something a wider bar hid
        assert!(!p.branches || prev.branches, "{width}");
        assert!(!p.meta || prev.meta, "{width}");
        assert!(p.inline.len() <= prev.inline.len(), "{width}");
        // hiding order: branches before the repository before any button
        assert!(!p.meta || p.inline.len() == 4, "{width}: {p:?}");
        assert!(!p.branches || p.meta, "{width}: {p:?}");
        prev = p;
    }
    let narrow = plan_top_bar(900.0, &w);
    assert!(!narrow.branches && !narrow.meta, "{narrow:?}");
    assert!(narrow.title_w >= TITLE_MIN_W);
    // Refresh goes into More before Open in Bitbucket
    let mid = plan_top_bar(w.fixed + TITLE_MIN_W + w.open_web + 10.0, &w);
    assert_eq!(mid.more, vec![BarButton::Refresh]);
    let tight = plan_top_bar(w.fixed + TITLE_MIN_W + 10.0, &w);
    assert_eq!(tight.more, vec![BarButton::Refresh, BarButton::OpenWeb]);
}

#[test]
fn widgets_inside_a_card_do_not_pull_the_cursor_back_into_it() {
    // `Ui::put` advanced the cursor to its widget's bottom: a title button made the
    // next card start inside the previous one (the Conversation tab overlap)
    let ctx = egui::Context::default();
    let mut seen = None;
    let mut frame = ctx.run_ui(Default::default(), |ui| {
        let (card, _) = ui.allocate_exact_size(vec2(300.0, 400.0), Sense::hover());
        let after = ui.cursor().top();
        let button = Rect::from_min_size(card.min + vec2(200.0, 8.0), vec2(80.0, 20.0));
        super::super::composer::put_in_card(ui, button, egui::Button::new("Reply"));
        seen = Some((card.bottom(), after, ui.cursor().top()));
    });
    frame.textures_delta.clear();
    let (bottom, before, now) = seen.expect("one pass ran");
    assert_eq!(now, before);
    assert!(before >= bottom);
}

fn file(path: &str, binary: bool) -> DiffFile {
    DiffFile {
        path: path.into(),
        old_path: None,
        status: FileStatus::Modified,
        additions: 2,
        deletions: 1,
        binary,
        counts_known: true,
        new_oid: format!("oid-{path}"),
    }
}

fn opened(files: Vec<DiffFile>) -> ReviewEvent {
    ReviewEvent::Opened {
        refs: PrRefs {
            base: "b".into(),
            head: "h".into(),
            dest: "d".into(),
        },
        files,
    }
}

fn rows() -> Arc<Vec<Row>> {
    Arc::new(vec![Row {
        kind: RowKind::Add,
        old_line: None,
        new_line: Some(1),
        text: "x".into(),
    }])
}

fn patch(path: &str) -> ReviewEvent {
    let rows = rows();
    ReviewEvent::Patch {
        path: path.into(),
        layout: Arc::new(crate::review::diffview::PatchLayout::compute(&rows, false)),
        rows,
        plain: false,
    }
}

fn screen() -> (ReviewScreen, Arc<Mutex<Store>>) {
    let (_, prs, _) = crate::demo::demo_data();
    let pr = prs[0].clone();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    (ReviewScreen::new(pr.repo.clone(), pr, store.clone()), store)
}

fn uncounted(path: &str) -> DiffFile {
    DiffFile {
        additions: 0,
        deletions: 0,
        counts_known: false,
        ..file(path, false)
    }
}

fn counts(c: &[(&str, u32, u32, bool)]) -> ReviewEvent {
    ReviewEvent::Counts {
        counts: Arc::new(
            c.iter()
                .map(|&(p, a, d, b)| prcore::diff::FileCounts {
                    path: p.into(),
                    additions: a,
                    deletions: d,
                    binary: b,
                })
                .collect(),
        ),
    }
}

#[test]
fn counts_arriving_after_the_list_fill_in_files_and_directory_totals() {
    let (mut s, _st) = screen();
    s.apply(opened(vec![
        uncounted("a/x.rs"),
        uncounted("a/y.png"),
        uncounted("b.rs"),
    ]));
    let l = s.files.as_ref().unwrap();
    assert!(!l.counts_known());
    let d = l.tree().dirs.iter().position(|d| d.path == "a").unwrap();
    let before = l.dir_stats()[d];
    assert_eq!((before.counts_unknown, before.additions), (2, 0));
    let rows_before = l.rows();
    // the first file is selected and, binary or not, its patch is asked for
    assert_eq!(s.take_requests(), vec!["a/x.rs".to_string()]);
    s.toggle_viewed(2);
    s.apply(counts(&[
        ("a/x.rs", 3, 1, false),
        ("a/y.png", 0, 0, true),
        ("b.rs", 2, 0, false),
    ]));
    let l = s.files.as_ref().unwrap();
    assert!(l.counts_known());
    let after = l.dir_stats()[d];
    assert_eq!(
        (after.additions, after.deletions, after.counts_unknown),
        (3, 1, 0),
        "directory totals recomputed"
    );
    assert!(l.files()[1].binary && l.files()[1].counts_known);
    assert_eq!(l.selected(), Some(0), "selection unchanged");
    assert!(l.is_viewed(2), "viewed marks unchanged");
    assert_eq!(&*l.rows(), &*rows_before, "same rows, rebuilt");
    assert!(s.counts_error.is_none());
}

#[test]
fn a_failed_count_is_shown_and_the_list_stays_usable() {
    let (mut s, _st) = screen();
    s.apply(opened(vec![uncounted("a.rs"), uncounted("b.rs")]));
    s.apply(ReviewEvent::CountsFailed("Could not count".into()));
    assert_eq!(s.counts_error.as_deref(), Some("Could not count"));
    s.select(1);
    assert_eq!(
        s.take_requests(),
        vec!["a.rs".to_string(), "b.rs".to_string()]
    );
}

#[test]
fn gutter_press_opens_the_composer_and_never_begins_a_selection() {
    use super::super::diffview::Offsets;
    let geo = DiffGeometry {
        char_w: 8.0,
        num_w: 48.0,
    };
    let mut offsets = Offsets::new(4, ROW_H);
    offsets.set_extras([(1, 60.0)]);
    let y = offsets.top(2) + 5.0;
    // every x of the composer's gutter widget, including the "+" button at its left edge
    let mut x = 0.0;
    while x < geo.gutter_w() {
        let p = press_action(hit_test(&offsets, x, y, geo.text_x()));
        assert_eq!(p, Press::GutterClick(2), "x = {x}");
        x += 1.0;
    }
    assert_eq!(
        press_action(hit_test(&offsets, geo.text_x() + 1.0, y, geo.text_x())),
        Press::Begin(2)
    );
    // the space under a row (comment cards, the composer) starts nothing
    let card = offsets.top(1) + ROW_H + 10.0;
    assert_eq!(
        press_action(hit_test(&offsets, geo.text_x() + 50.0, card, geo.text_x())),
        Press::Ignore
    );
}

fn r(kind: RowKind, old: Option<u32>, new: Option<u32>, text: &str) -> Row {
    Row {
        kind,
        old_line: old,
        new_line: new,
        text: text.into(),
    }
}

/// A screen showing `a.rs` with `rows`, its viewport 20 rows high.
fn screen_with(rows: Vec<Row>) -> ReviewScreen {
    let (mut s, _) = screen();
    s.apply(opened(vec![file("a.rs", false)]));
    let rows = Arc::new(rows);
    s.apply(ReviewEvent::Patch {
        path: "a.rs".into(),
        layout: Arc::new(crate::review::diffview::PatchLayout::compute(&rows, false)),
        rows,
        plain: false,
    });
    s.diff_viewport = (0.0, 20.0 * ROW_H);
    // the file was just opened: its (zero) scroll is restored once
    s.restore_scroll = None;
    s
}

/// Three hunks of 40 rows: a header, then context lines (new line = old line + 1).
fn hunks() -> Vec<Row> {
    (0..120u32)
        .map(|i| {
            if i % 40 == 0 {
                r(RowKind::HunkHeader, None, None, "@@ -1 +1 @@")
            } else {
                r(RowKind::Context, Some(i), Some(i + 1), &format!("line {i}"))
            }
        })
        .collect()
}

fn cursor_row(s: &ReviewScreen) -> Option<usize> {
    s.cursor().map(|c| c.row)
}

#[test]
fn vim_motions_move_the_cursor_and_keep_it_in_view() {
    let mut s = screen_with(hunks());
    let key = |s: &mut ReviewScreen, a, n: Option<u32>| {
        s.key_action(a, n);
        cursor_row(s)
    };
    assert_eq!(cursor_row(&s), None);
    assert_eq!(
        key(&mut s, Action::Down, None),
        Some(0),
        "the first j shows it"
    );
    assert_eq!(key(&mut s, Action::Down, None), Some(1));
    assert_eq!(key(&mut s, Action::Down, Some(5)), Some(6));
    assert_eq!(key(&mut s, Action::Up, Some(2)), Some(4));
    assert_eq!(key(&mut s, Action::Up, Some(99)), Some(0), "clamped");
    assert!(s.restore_scroll.is_none(), "in view: no scroll, no jitter");
    assert_eq!(key(&mut s, Action::Bottom, None), Some(119));
    let y = s.restore_scroll.take().unwrap().y;
    // the last row sits two rows above the viewport's bottom edge
    assert_eq!(y, 120.0 * ROW_H + 2.0 * ROW_H - 20.0 * ROW_H);
    assert_eq!(key(&mut s, Action::Top, None), Some(0));
    assert_eq!(key(&mut s, Action::NextHunk, None), Some(40));
    assert_eq!(key(&mut s, Action::NextHunk, None), Some(80));
    assert_eq!(
        key(&mut s, Action::NextHunk, None),
        Some(80),
        "no more hunks"
    );
    assert_eq!(key(&mut s, Action::PrevHunk, None), Some(40));
    assert_eq!(key(&mut s, Action::NextHunk, Some(2)), Some(80));
    // {N}G: the new file's line N (row i shows new line i + 1)
    assert_eq!(key(&mut s, Action::Bottom, Some(10)), Some(9));
    assert_eq!(
        key(&mut s, Action::Top, Some(10_000)),
        Some(119),
        "past the end"
    );
    // Ctrl-d / Ctrl-u: half the viewport (10 rows); the page moves with the cursor
    key(&mut s, Action::Top, None);
    s.restore_scroll = None;
    s.diff_viewport = (0.0, 20.0 * ROW_H);
    assert_eq!(key(&mut s, Action::HalfDown, None), Some(10));
    assert_eq!(s.restore_scroll.take().unwrap().y, 10.0 * ROW_H);
    s.diff_viewport = (10.0 * ROW_H, 20.0 * ROW_H);
    assert_eq!(key(&mut s, Action::HalfUp, None), Some(0));
    assert_eq!(s.restore_scroll.take().unwrap().y, 0.0);
    // not on the Conversation tab
    s.tab = Tab::Conversation;
    assert_eq!(key(&mut s, Action::Down, None), Some(0));
}

#[test]
fn reveal_scrolls_only_a_row_out_of_view() {
    let h = 20.0 * ROW_H;
    assert_eq!(reveal_y(5.0 * ROW_H, 0.0, h), None);
    assert_eq!(
        reveal_y(0.0, 0.0, h),
        Some(-2.0 * ROW_H),
        "clamped to 0 by the caller"
    );
    assert_eq!(reveal_y(30.0 * ROW_H, 0.0, h), Some(13.0 * ROW_H));
    assert_eq!(reveal_y(3.0 * ROW_H, 10.0 * ROW_H, h), Some(1.0 * ROW_H));
    assert_eq!(reveal_y(3.0 * ROW_H, 0.0, 0.0), None, "never drawn yet");
}

fn change() -> Vec<Row> {
    vec![
        r(RowKind::HunkHeader, None, None, "@@ -1,4 +1,3 @@"),
        r(RowKind::Context, Some(1), Some(1), "same"),
        r(RowKind::Del, Some(2), None, "old a"),
        r(RowKind::Del, Some(3), None, "old b"),
        r(RowKind::Add, None, Some(2), "new a"),
        r(RowKind::Context, Some(4), Some(3), "tail"),
    ]
}

fn inline(side: Side, line: u32) -> Act {
    Act::Open(Target::Inline {
        path: "a.rs".into(),
        side,
        line,
        start: None,
    })
}

#[test]
fn in_the_split_view_the_cursor_moves_by_line_and_comments_its_side() {
    let mut s = screen_with(change());
    s.split_shown = true;
    // lines: 0 header, 1 same, 2 (old a | new a), 3 (old b | pad), 4 tail
    s.key_action(Action::Down, None);
    s.key_action(Action::Down, Some(2));
    assert_eq!(cursor_row(&s), Some(4), "line 2, right side: new a");
    let fx = s.key_action(Action::Comment, None);
    assert_eq!(fx.acts, [inline(Side::New, 2)]);
    s.key_action(Action::SideLeft, None);
    assert_eq!(cursor_row(&s), Some(2), "same line, left side: old a");
    assert_eq!(
        s.key_action(Action::Comment, None).acts,
        [inline(Side::Old, 2)]
    );
    // the right side is padding on line 3: the cursor shows the left row there
    s.key_action(Action::SideRight, None);
    s.key_action(Action::Down, None);
    assert_eq!(cursor_row(&s), Some(3));
    assert_eq!(
        s.key_action(Action::Comment, None).acts,
        [inline(Side::Old, 3)]
    );
    // and keeps preferring the right side on the next line (a context row)
    s.key_action(Action::Down, None);
    assert_eq!(cursor_row(&s), Some(5));
    assert_eq!(
        s.key_action(Action::Comment, None).acts,
        [inline(Side::New, 3)]
    );
    s.key_action(Action::SideLeft, None);
    assert_eq!(
        s.key_action(Action::Comment, None).acts,
        [inline(Side::Old, 4)]
    );
    // on a hunk header there is nothing to comment on
    s.key_action(Action::Top, None);
    assert!(s.key_action(Action::Comment, None).acts.is_empty());
}

#[test]
fn unified_comment_at_the_cursor_and_file_comment() {
    let mut s = screen_with(change());
    s.key_action(Action::Down, None);
    s.key_action(Action::Down, Some(2));
    assert_eq!(cursor_row(&s), Some(2));
    assert_eq!(
        s.key_action(Action::Comment, None).acts,
        [inline(Side::Old, 2)]
    );
    s.key_action(Action::Down, Some(3));
    assert_eq!(
        s.key_action(Action::Comment, None).acts,
        [inline(Side::New, 3)]
    );
    assert_eq!(
        s.key_action(Action::FileComment, None).acts,
        [Act::Open(Target::File("a.rs".into()))]
    );
}

#[test]
fn visual_mode_selects_whole_rows_on_the_cursor_side() {
    let mut s = screen_with(change());
    s.key_action(Action::Down, None);
    s.key_action(Action::Down, None);
    s.key_action(Action::Visual, None);
    s.key_action(Action::Down, Some(2));
    let v = s.diff.view("a.rs").unwrap();
    let sel = v.sel.sel.unwrap();
    assert_eq!(selected_text(&v.rows, &sel), "same\nold a\nold b");
    // c comments the cursor row and ends visual mode
    assert_eq!(
        s.key_action(Action::Comment, None).acts,
        [inline(Side::Old, 3)]
    );
    assert!(s.visual.is_none());
    // split view: rows of the cursor's side only
    s.split_shown = true;
    s.key_action(Action::Top, None);
    s.key_action(Action::Down, None);
    s.key_action(Action::Visual, None);
    s.key_action(Action::Down, Some(3));
    let v = s.diff.view("a.rs").unwrap();
    let st = &v.sel;
    assert_eq!(st.side, Some(SplitSide::Right));
    assert_eq!(
        selected_text_split(&v.rows, v.split(), SplitSide::Right, &st.sel.unwrap()),
        "same\nnew a\ntail"
    );
    s.key_action(Action::Cancel, None);
    assert!(s.visual.is_none());
}

#[test]
fn visual_mode_comments_on_the_line_range_where_the_forge_takes_ranges() {
    let mut s = screen_with(change());
    s.composer.forge = prcore::model::Forge::GitHub;
    s.key_action(Action::Down, None);
    s.key_action(Action::Down, None);
    s.key_action(Action::Visual, None);
    s.key_action(Action::Down, Some(2));
    // rows "same" (new 1) to "old b" (old 3)
    assert_eq!(
        s.key_action(Action::Comment, None).acts,
        [Act::Open(Target::Inline {
            path: "a.rs".into(),
            side: Side::Old,
            line: 3,
            start: Some((Side::New, 1)),
        })]
    );
    assert!(s.visual.is_none());
}

#[test]
fn search_finds_rows_and_steps_through_them_in_screen_order() {
    let mut s = screen_with(change());
    s.key_action(Action::Search, None);
    assert!(s.search.open && s.search.focus);
    s.search.query = "a".into();
    // "same", "old a", "new a", "tail" (smart case)
    s.key_action(Action::SearchNext, None);
    assert_eq!(cursor_row(&s), Some(1));
    s.key_action(Action::SearchNext, None);
    assert_eq!(cursor_row(&s), Some(2));
    s.key_action(Action::SearchPrev, None);
    assert_eq!(cursor_row(&s), Some(1));
    s.key_action(Action::SearchPrev, None);
    assert_eq!(cursor_row(&s), Some(5), "wraps");
    // split view order: line 2 holds "old a" (left) then "new a" (right)
    s.split_shown = true;
    s.key_action(Action::Top, None);
    s.key_action(Action::SearchNext, Some(2));
    assert_eq!(cursor_row(&s), Some(2));
    s.key_action(Action::SearchNext, None);
    assert_eq!(cursor_row(&s), Some(4), "same line, right side");
    s.search.query = "zzz".into();
    s.key_action(Action::SearchNext, None);
    assert_eq!(cursor_row(&s), Some(4), "no match: the cursor stays");
    s.key_action(Action::Cancel, None);
    assert!(!s.search.open);
}

#[test]
fn file_keys_viewed_back_filter_copy_and_help() {
    let (mut s, _) = screen();
    s.apply(opened(vec![
        file("a.rs", false),
        file("b.rs", false),
        file("c.rs", false),
    ]));
    let sel = |s: &ReviewScreen| s.selected_path().map(str::to_string);
    assert_eq!(sel(&s).as_deref(), Some("a.rs"));
    s.key_action(Action::NextFile, Some(2));
    assert_eq!(sel(&s).as_deref(), Some("c.rs"));
    s.key_action(Action::PrevFile, None);
    assert_eq!(sel(&s).as_deref(), Some("b.rs"));
    let i = s.files.as_ref().unwrap().selected().unwrap();
    assert!(!s.files.as_ref().unwrap().is_viewed(i));
    s.key_action(Action::ToggleViewed, None);
    assert!(s.files.as_ref().unwrap().is_viewed(i));
    assert!(s.key_action(Action::Back, None).back);
    assert!(s.key_action(Action::FocusFilter, None).focus_filter);
    assert_eq!(
        s.key_action(Action::CopyPath, None).copy.as_deref(),
        Some("b.rs")
    );
    s.key_action(Action::Help, None);
    assert!(s.help_open);
    s.key_action(Action::Cancel, None);
    assert!(!s.help_open, "Esc closes the help first");
}

#[test]
fn diff_layout_is_saved_and_read_back() {
    let (mut s, store) = screen();
    s.apply(opened(vec![file("a.rs", false)]));
    assert_eq!(s.layout, DiffLayout::Unified);
    s.key_action(Action::ToggleLayout, None);
    assert_eq!(s.layout, DiffLayout::Split);
    s.flush_writes();
    let saved = store.lock().unwrap().get_meta(LAYOUT_META_KEY).unwrap();
    assert_eq!(saved.as_deref(), Some("split"));
    let (_, prs, _) = crate::demo::demo_data();
    let mut t = ReviewScreen::new(prs[0].repo.clone(), prs[0].clone(), store.clone());
    t.apply(opened(vec![file("a.rs", false)]));
    assert_eq!(t.layout, DiffLayout::Split, "the next review starts split");
}

#[test]
fn key_events_map_typed_characters_and_modified_letters() {
    let text = |t: &str| egui::Event::Text(t.into());
    let key = |k: Key, m: egui::Modifiers| egui::Event::Key {
        key: k,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: m,
    };
    assert_eq!(key_in(&text("j"), false), Some(KeyIn::Char('j')));
    assert_eq!(key_in(&text("{"), false), Some(KeyIn::Char('{')));
    assert_eq!(key_in(&text("G"), false), Some(KeyIn::Char('G')));
    assert_eq!(key_in(&text("s"), true), None, "Alt+s is not typing");
    assert_eq!(key_in(&text(" "), false), None);
    assert_eq!(key_in(&text("ab"), false), None, "pasted or composed text");
    assert_eq!(
        key_in(&key(Key::D, egui::Modifiers::CTRL), false),
        Some(KeyIn::Ctrl('d'))
    );
    assert_eq!(
        key_in(&key(Key::S, egui::Modifiers::ALT), true),
        Some(KeyIn::Alt('s'))
    );
    assert_eq!(
        key_in(&key(Key::Enter, egui::Modifiers::NONE), false),
        Some(KeyIn::Enter)
    );
    assert_eq!(
        key_in(&key(Key::Enter, egui::Modifiers::CTRL), false),
        None,
        "Ctrl+Enter is the composer's"
    );
    assert_eq!(
        key_in(&key(Key::Backspace, egui::Modifiers::NONE), false),
        Some(KeyIn::Backspace)
    );
    assert_eq!(
        key_in(&key(Key::J, egui::Modifiers::NONE), false),
        None,
        "via its text"
    );
}

/// Draws the review's central pane in a headless egui context of `size`, `frames` times.
fn render(s: &mut ReviewScreen, size: Vec2, frames: usize) -> Vec<Duration> {
    let ctx = egui::Context::default();
    (0..frames)
        .map(|_| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                ..Default::default()
            };
            let t = Instant::now();
            let mut out = ctx.run_ui(input, |ui| {
                let mut acts = Vec::new();
                egui::CentralPanel::default().show(ui, |ui| draw_center(ui, s, &mut acts));
            });
            out.textures_delta.clear();
            t.elapsed()
        })
        .collect()
}

#[test]
fn the_split_view_draws_and_narrow_windows_fall_back_to_unified() {
    let mut s = screen_with(change());
    s.layout = DiffLayout::Split;
    render(&mut s, vec2(1400.0, 800.0), 3);
    assert!(s.split_shown);
    let v = s.diff.view("a.rs").unwrap();
    assert_eq!(
        v.split_offsets_ref().unwrap().rows(),
        5,
        "split lines laid out"
    );
    render(&mut s, vec2(800.0, 800.0), 3);
    assert!(!s.split_shown, "too narrow: unified");
    assert_eq!(s.layout, DiffLayout::Split, "the choice is kept");
}

/// 200k rows (like the audit's large file): ui() time per frame of the diff pane,
/// unified against split, headless (no GPU). Run with
/// `cargo test -p app --release --bin nit split_frame_time -- --ignored --nocapture`
#[test]
#[ignore]
fn split_frame_time_bench() {
    let rows: Vec<Row> = (0..200_000u32)
        .map(|i| match i % 10 {
            0 => r(RowKind::HunkHeader, None, None, "@@ -1,9 +1,9 @@"),
            1..=3 => r(
                RowKind::Del,
                Some(i),
                None,
                "    let value = compute_something(input.field, \"literal\", Some(&mut state));",
            ),
            4..=7 => r(
                RowKind::Add,
                None,
                Some(i),
                "    let value = compute_other(input.field, \"literal\", Some(&mut state), opts);",
            ),
            _ => r(
                RowKind::Context,
                Some(i),
                Some(i),
                "    // a context line of ordinary length",
            ),
        })
        .collect();
    let mut s = screen_with(rows);
    for layout in [DiffLayout::Unified, DiffLayout::Split] {
        s.layout = layout;
        for at in [0.0, 1_000_000.0 * ROW_H] {
            s.restore_scroll = Some(vec2(0.0, at));
            let times = render(&mut s, vec2(1400.0, 900.0), 120);
            // the first frames lay out (and build the split offsets once)
            let first = times[0];
            let rest = &times[20..];
            let avg = rest.iter().sum::<Duration>() / rest.len() as u32;
            let max = rest.iter().max().unwrap();
            println!("{layout:?} at y={at}: first frame {first:?}, then avg {avg:?}, max {max:?}");
        }
    }
}

#[test]
fn c_key_targets_hovered_row_then_last_selected_new_side_first() {
    let rows = vec![
        Row {
            kind: RowKind::HunkHeader,
            old_line: None,
            new_line: None,
            text: "@@".into(),
        },
        Row {
            kind: RowKind::Context,
            old_line: Some(4),
            new_line: Some(6),
            text: "a".into(),
        },
        Row {
            kind: RowKind::Del,
            old_line: Some(5),
            new_line: None,
            text: "b".into(),
        },
    ];
    let at = |side, line| {
        Some(Target::Inline {
            path: "a.rs".into(),
            side,
            line,
            start: None,
        })
    };
    assert_eq!(
        c_key_target(&rows, "a.rs", Some(1), Some(2)),
        at(Side::New, 6)
    );
    assert_eq!(c_key_target(&rows, "a.rs", None, Some(2)), at(Side::Old, 5));
    assert_eq!(
        c_key_target(&rows, "a.rs", Some(0), None),
        None,
        "hunk header"
    );
    assert_eq!(c_key_target(&rows, "a.rs", None, None), None);
    assert_eq!(c_key_target(&rows, "a.rs", Some(9), None), None);
}

#[test]
fn opened_selects_first_non_generated_file_and_requests_its_patch() {
    let (mut s, _) = screen();
    assert!(s.phase.is_some() && s.files.is_none());
    s.apply(ReviewEvent::Phase("Fetching branches...".into()));
    assert_eq!(s.phase.as_deref(), Some("Fetching branches..."));
    s.apply(opened(vec![
        file("Cargo.lock", false),
        file("src/a.rs", false),
    ]));
    assert!(s.phase.is_none());
    assert_eq!(s.selected_file().unwrap().path, "src/a.rs");
    assert_eq!(s.take_requests(), vec!["src/a.rs".to_string()]);
    assert!(s.take_requests().is_empty());
    assert!(!s.settled());
    s.apply(patch("src/a.rs"));
    assert!(s.diff.view("src/a.rs").is_some() && s.settled());
}

#[test]
fn patches_for_other_files_are_ignored_and_binary_files_are_not_requested() {
    let (mut s, _) = screen();
    s.apply(opened(vec![file("a.rs", false), file("logo.png", true)]));
    s.take_requests();
    s.apply(patch("other.rs"));
    assert!(s.diff.view("other.rs").is_none());
    s.step(true);
    assert_eq!(s.selected_file().unwrap().path, "logo.png");
    assert!(s.take_requests().is_empty(), "binary: placeholder only");
    assert!(s.settled());
}

#[test]
fn patch_failure_is_per_file_and_retry_requests_again() {
    let (mut s, _) = screen();
    s.apply(opened(vec![file("a.rs", false)]));
    s.take_requests();
    s.apply(ReviewEvent::PatchFailed {
        path: "a.rs".into(),
        error: "Could not load the diff".into(),
    });
    assert!(s.failed.is_none() && s.settled());
    assert!(s.patch_errors.contains_key("a.rs"));
    s.retry_patch("a.rs");
    assert!(s.patch_errors.is_empty());
    assert_eq!(s.take_requests(), vec!["a.rs".to_string()]);
}

#[test]
fn failed_and_stale_set_their_banners() {
    let (mut s, _) = screen();
    s.apply(ReviewEvent::Failed("Git is unavailable".into()));
    assert_eq!(s.failed.as_deref(), Some("Git is unavailable"));
    assert!(s.phase.is_none() && s.settled());
    s.apply(ReviewEvent::Stale);
    assert!(s.stale);
}

#[test]
fn viewed_marks_are_stored_with_the_new_oid() {
    let (mut s, store) = screen();
    let (repo, id) = (s.repo.clone(), s.pr.id);
    store
        .lock()
        .unwrap()
        .set_viewed(&repo, id, "b.rs", "oid-b.rs", true)
        .unwrap();
    s.apply(opened(vec![file("a.rs", false), file("b.rs", false)]));
    assert_eq!(s.files.as_ref().unwrap().counter(), "1/2 viewed");
    s.toggle_viewed(0);
    s.flush_writes();
    let m = store.lock().unwrap().viewed_map(&repo, id).unwrap();
    assert_eq!(m.get("a.rs").map(String::as_str), Some("oid-a.rs"));
    s.toggle_viewed(1);
    s.flush_writes();
    let m = store.lock().unwrap().viewed_map(&repo, id).unwrap();
    assert!(!m.contains_key("b.rs"));
}

#[test]
fn viewed_write_goes_through_writer_and_failure_is_reported() {
    let (mut s, store) = screen();
    let (repo, id) = (s.repo.clone(), s.pr.id);
    s.apply(opened(vec![file("a.rs", false), file("b.rs", false)]));
    // another thread holds the store (a sync writing): the click does not wait for it
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let store = store.clone();
        std::thread::spawn(move || {
            let _guard = store.lock().unwrap();
            locked_tx.send(()).unwrap();
            // bounded: a click that waits for the store fails the timing check
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(2));
        })
    };
    locked_rx.recv().unwrap();
    let t = Instant::now();
    s.toggle_viewed(0);
    assert!(t.elapsed() < std::time::Duration::from_millis(500));
    // optimistic: shown at once, stored once the store is free
    assert!(s.files.as_ref().unwrap().is_viewed(0));
    let _ = release_tx.send(());
    holder.join().unwrap();
    s.flush_writes();
    let m = store.lock().unwrap().viewed_map(&repo, id).unwrap();
    assert_eq!(m.get("a.rs").map(String::as_str), Some("oid-a.rs"));
    assert_eq!(s.store_error, None);
    // a write that fails shows its "Could not save" text on the next frame
    s.writes.write("Could not save the viewed mark", |_| {
        Err(store::StoreError::Corrupt)
    });
    s.writes.flush();
    s.poll_writes(&egui::Context::default());
    assert_eq!(
        s.store_error.as_deref(),
        Some("Could not save the viewed mark")
    );
}

#[test]
fn closing_the_review_stores_queued_marks() {
    let (mut s, store) = screen();
    let (repo, id) = (s.repo.clone(), s.pr.id);
    s.apply(opened(vec![file("a.rs", false), file("b.rs", false)]));
    s.toggle_viewed(0);
    s.toggle_viewed(1);
    s.toggle_viewed(0);
    drop(s);
    let m = store.lock().unwrap().viewed_map(&repo, id).unwrap();
    assert_eq!(m.keys().collect::<Vec<_>>(), vec!["b.rs"], "{m:?}");
}

#[test]
fn switching_back_restores_scroll_and_rerequests_the_patch() {
    let (mut s, _) = screen();
    s.apply(opened(vec![file("a.rs", false), file("b.rs", false)]));
    s.take_requests();
    s.apply(patch("a.rs"));
    s.diff.set_scroll("a.rs", Vec2::new(0.0, 90.0));
    s.step(true);
    assert_eq!(s.take_requests(), vec!["b.rs".to_string()]);
    s.apply(patch("b.rs"));
    s.step(false);
    assert_eq!(s.restore_scroll, Some(Vec2::new(0.0, 90.0)));
    assert_eq!(s.take_requests(), vec!["a.rs".to_string()]);
}

#[test]
fn empty_pr_settles_with_no_selection() {
    let (mut s, _) = screen();
    s.apply(opened(vec![]));
    assert!(s.selected_file().is_none() && s.settled());
    s.step(true);
    assert!(s.take_requests().is_empty());
}

#[test]
fn open_review_switches_to_the_review_screen() {
    let mut app = App::demo(None, false);
    let (_, prs, _) = crate::demo::demo_data();
    let pr = prs.iter().find(|p| !p.is_fork()).unwrap().clone();
    open_review(&mut app, pr.repo.clone(), pr);
    assert!(app.screen == Screen::Review && app.review.pending.is_some());
}

#[test]
fn open_review_ignores_pull_requests_that_are_not_open() {
    use prcore::model::PrState;
    let (_, prs, _) = crate::demo::demo_data();
    for state in [PrState::Merged, PrState::Declined, PrState::Superseded] {
        let mut app = App::demo(None, false);
        let before = app.screen == Screen::Review;
        let mut pr = prs.iter().find(|p| !p.is_fork()).unwrap().clone();
        pr.state = state.clone();
        open_review(&mut app, pr.repo.clone(), pr);
        assert_eq!(app.screen == Screen::Review, before, "{state:?}");
        assert!(app.review.pending.is_none(), "{state:?}");
    }
}

#[test]
fn review_keys_are_off_while_the_submit_dialog_is_open() {
    let none = egui::Modifiers::NONE;
    assert!(review_keys_allowed(false, none, false));
    assert!(!review_keys_allowed(false, none, true));
    assert!(!review_keys_allowed(true, none, false));
}

#[test]
fn c_key_is_off_while_any_modal_is_open() {
    use prcore::comments::ReviewAction;
    let (mut s, _) = screen();
    let poster = Arc::new(MemoryPoster::new("h"));
    s.actions.connect(poster.clone(), None);
    s.composer
        .connect(poster, Arc::new(|_| Err("no rows".into())), None);
    let none = egui::Modifiers::NONE;
    assert!(!s.modal_open());
    // the review action confirmation
    s.request_action(ReviewAction::Approve);
    assert!(s.actions.confirm.is_some());
    assert!(s.modal_open());
    assert!(!review_keys_allowed(false, none, s.modal_open()));
    s.action_act(ActionAct::Cancel);
    assert!(!s.modal_open());
    // the submit confirmation
    s.act(Act::Open(Target::Pr));
    s.composer.editor.as_mut().unwrap().text = "x".into();
    s.act(Act::Save);
    s.act(Act::Submit);
    assert!(s.composer.confirm.is_some());
    assert!(!review_keys_allowed(false, none, s.modal_open()));
}

#[test]
fn real_mode_without_backend_errors_instead_of_using_demo_data() {
    let (_, prs, _) = crate::demo::demo_data();
    let pr = prs.iter().find(|p| !p.is_fork()).unwrap().clone();
    let mut host = ReviewHost::default();
    assert!(!host.demo_mode, "demo is never the default");
    host.start(&egui::Context::default(), None, pr.repo.clone(), pr.clone());
    let s = host.screen.as_ref().unwrap();
    assert!(s.failed.is_some(), "an error is shown");
    assert!(host.service.is_none(), "no demo repository is opened");
    assert!(
        !s.actions.can_post() && !s.composer.can_post(),
        "no fake server"
    );
    assert_eq!((&s.repo, s.pr.id), (&pr.repo, pr.id), "not the demo PR");
    // demo mode says so explicitly
    let app = App::demo(None, false);
    assert!(app.review.demo_mode);
}

#[test]
fn recheck_due_never_while_unfocused() {
    let t0 = Instant::now();
    let later = t0 + RECHECK_EVERY;
    assert!(!recheck_due(later - Duration::from_secs(1), t0, Some(true)));
    assert!(recheck_due(later, t0, Some(true)));
    assert!(
        recheck_due(later, t0, None),
        "unknown focus counts as focused"
    );
    assert!(!recheck_due(
        later + Duration::from_secs(3600),
        t0,
        Some(false)
    ));
    // an hour unfocused, then focus gain: due at once, then the minute starts again
    let mut t = RecheckTimer::new(t0);
    assert!(!t.due(t0, Some(false)));
    assert!(!t.due(t0 + Duration::from_secs(3600), Some(false)));
    assert!(t.due(t0 + Duration::from_secs(3601), Some(true)));
    assert!(!t.due(t0 + Duration::from_secs(3602), Some(true)));
}

#[test]
fn viewed_rows_stay_readable_on_the_selection_fill() {
    fn lin(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    fn lum(c: Color32) -> f64 {
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }
    fn contrast(a: Color32, b: Color32) -> f64 {
        (lum(a).max(lum(b)) + 0.05) / (lum(a).min(lum(b)) + 0.05)
    }
    let sel = crate::theme::SELECTION;
    let (dim, grey) = viewed_colors(true);
    assert!(contrast(dim, sel) >= 3.0, "dir {}", contrast(dim, sel));
    assert!(contrast(grey, sel) >= 3.0, "name {}", contrast(grey, sel));
    assert!(contrast(DIM, sel) < 3.0, "the plain dim is the bug");
    assert_eq!(viewed_colors(false), (DIM, GREY));
}

#[test]
fn recheck_due_on_focus_gain_and_every_minute() {
    let t0 = Instant::now();
    let mut t = RecheckTimer::new(t0);
    assert!(!t.due(t0, Some(true)));
    assert!(
        !t.due(t0 + Duration::from_secs(10), Some(false)),
        "focus lost"
    );
    assert!(
        t.due(t0 + Duration::from_secs(11), Some(true)),
        "focus gained"
    );
    assert!(
        !t.due(t0 + Duration::from_secs(12), Some(true)),
        "still focused"
    );
    assert!(!t.due(t0 + Duration::from_secs(70), None));
    assert!(
        t.due(t0 + Duration::from_secs(71), None),
        "a minute after the last"
    );
    assert!(!t.due(t0 + Duration::from_secs(72), Some(true)));
    assert!(t.due(t0 + Duration::from_secs(131), Some(true)));
}

#[test]
fn nav_keys_need_no_focus_and_no_command_modifiers() {
    let none = egui::Modifiers::NONE;
    assert!(nav_keys_allowed(false, none));
    assert!(nav_keys_allowed(false, egui::Modifiers::SHIFT));
    assert!(!nav_keys_allowed(true, none), "a text field has focus");
    for m in [
        egui::Modifiers::CTRL,
        egui::Modifiers::ALT,
        egui::Modifiers::COMMAND,
        egui::Modifiers::MAC_CMD,
    ] {
        assert!(!nav_keys_allowed(false, m), "{m:?}");
    }
}

#[test]
fn list_keys_yield_to_text_fields_modals_modifiers_and_the_diff_pane() {
    let none = egui::Modifiers::NONE;
    assert!(list_keys_allowed(false, none, false, false));
    assert!(list_keys_allowed(
        false,
        egui::Modifiers::SHIFT,
        false,
        false
    ));
    assert!(
        !list_keys_allowed(true, none, false, false),
        "filter or composer"
    );
    assert!(
        !list_keys_allowed(false, none, true, false),
        "a modal is open"
    );
    assert!(
        !list_keys_allowed(false, none, false, true),
        "pointer over the diff"
    );
    for m in [
        egui::Modifiers::CTRL,
        egui::Modifiers::ALT,
        egui::Modifiers::COMMAND,
    ] {
        assert!(!list_keys_allowed(false, m, false, false), "{m:?}");
    }
}

#[test]
fn file_view_mode_is_read_from_and_saved_to_the_store() {
    let (mut s, store) = screen();
    s.apply(opened(vec![file("a/x.rs", false), file("b.rs", false)]));
    assert_eq!(s.files.as_ref().unwrap().view(), ViewMode::Tree, "default");
    s.set_file_view(ViewMode::List);
    s.flush_writes();
    let saved = store.lock().unwrap().get_meta(VIEW_META_KEY).unwrap();
    assert_eq!(saved.as_deref(), Some("list"));
    // the next review starts in the remembered mode
    let (_, prs, _) = crate::demo::demo_data();
    let mut t = ReviewScreen::new(prs[0].repo.clone(), prs[0].clone(), store.clone());
    t.apply(opened(vec![file("a/x.rs", false)]));
    assert_eq!(t.files.as_ref().unwrap().view(), ViewMode::List);
}

#[test]
fn list_keys_move_toggle_and_open_through_the_screen() {
    let (mut s, store) = screen();
    let (repo, id) = (s.repo.clone(), s.pr.id);
    s.apply(opened(vec![
        file("d/a.rs", false),
        file("d/b.rs", false),
        file("z.rs", false),
    ]));
    s.take_requests();
    assert_eq!(s.files.as_ref().unwrap().selected(), Some(0));
    // Up from a.rs: its directory row (no file selected anew, no patch asked for)
    s.list_key(ListKey::Nav(Nav::Up));
    assert!(matches!(
        s.files.as_ref().unwrap().cursor(),
        Some(Cursor::Dir(_))
    ));
    assert!(s.take_requests().is_empty());
    // Space on the directory marks both files viewed in one go
    s.list_key(ListKey::Space);
    s.flush_writes();
    let m = store.lock().unwrap().viewed_map(&repo, id).unwrap();
    assert_eq!(m.len(), 2);
    s.list_key(ListKey::Nav(Nav::End));
    assert_eq!(s.files.as_ref().unwrap().selected(), Some(2));
    assert_eq!(s.take_requests(), vec!["z.rs".to_string()]);
    s.list_key(ListKey::Space);
    assert!(s.files.as_ref().unwrap().is_viewed(2));
    s.tab = Tab::Conversation;
    s.list_key(ListKey::Enter);
    assert_eq!(s.tab, Tab::Diff, "Enter on a file shows its diff");
    // j/k keep working the same way
    s.step(false);
    assert_eq!(s.files.as_ref().unwrap().selected(), Some(1));
}

#[test]
fn comment_marks_follow_threads_and_drafts() {
    let (mut s, _) = screen();
    s.apply(opened(vec![file("a.rs", false), file("b.rs", false)]));
    assert!(!s.files.as_ref().unwrap().has_marks());
    s.composer.open_editor(Target::File("b.rs".into()));
    if let Some(e) = s.composer.editor.as_mut() {
        e.text = "note".into();
    }
    s.composer.save_editor();
    s.refresh_marks(false);
    let l = s.files.as_ref().unwrap();
    assert_eq!(l.mark(1).drafts, 1, "a saved draft shows without a refresh");
    assert_eq!(l.mark(0), CommentMark::default());
}

#[test]
fn threads_on_files_not_in_the_diff_are_listed_and_opened_by_path() {
    use prcore::comments::{Anchor, Comment, Thread};
    let t = |id: u64, path: &str, to: u32| Thread {
        root: Comment {
            id: id.into(),
            body: "b".into(),
            anchor: Some(Anchor {
                path: path.into(),
                to: Some(to),
                ..Default::default()
            }),
            thread: Some(prcore::comments::ThreadId::from(id)),
            ..Default::default()
        },
        replies: vec![],
    };
    let (mut s, _) = screen();
    s.apply(opened(vec![file("a.rs", false), file("b.rs", false)]));
    s.apply(ReviewEvent::Comments {
        threads: vec![t(1, "gone.yaml", 3), t(2, "a.rs", 1)],
        skipped: 0,
    });
    let l = s.files.as_ref().unwrap();
    assert_eq!(l.ghosts().len(), 1);
    assert_eq!(l.ghosts()[0].path, "gone.yaml");
    assert!(l.ghosts()[0].mark.all_outdated());
    assert!(l.rows().contains(&ListRow::Ghost(0)));
    // a Conversation click on its thread shows the "not in diff" entry
    s.tab = Tab::Conversation;
    s.jump_to_thread(0);
    assert_eq!(s.tab, Tab::Diff);
    let l = s.files.as_ref().unwrap();
    assert_eq!((l.selected_ghost(), l.selected()), (Some(0), None));
    assert!(s.selected_file().is_none() && s.settled());
    assert_eq!(s.pending_reveal, Some(1.into()));
    // a thread on a file of the diff: the file, scrolled to it once laid out
    s.jump_to_thread(1);
    assert_eq!(s.selected_file().map(|f| f.path.as_str()), Some("a.rs"));
    assert_eq!(s.pending_reveal, Some(2.into()));
    assert_eq!(s.files.as_ref().unwrap().selected_ghost(), None);
    assert!(!s.open_path("nowhere.rs"));
    // Space on a ghost does nothing, Enter shows it
    s.files.as_mut().unwrap().select_ghost(0);
    s.tab = Tab::Conversation;
    s.list_key(ListKey::Space);
    s.list_key(ListKey::Enter);
    assert_eq!(s.tab, Tab::Diff);
}

fn opened_at(files: Vec<DiffFile>, head: &str) -> ReviewEvent {
    ReviewEvent::Opened {
        refs: PrRefs {
            base: "b".into(),
            head: head.into(),
            dest: "d".into(),
        },
        files,
    }
}

#[test]
fn reload_clears_stale_and_adopts_the_newly_fetched_head() {
    let (mut s, store) = screen();
    s.pr.source_commit = "old".into();
    s.apply(opened_at(vec![file("a.rs", false)], "new1"));
    s.apply(ReviewEvent::Stale);
    assert!(s.stale);
    let (repo, pr) = s.reload_target();
    assert_eq!(
        pr.source_commit, "",
        "the outdated inbox head is not compared again"
    );
    let mut again = ReviewScreen::new(repo, pr, store);
    assert!(!again.stale);
    again.apply(opened_at(vec![file("a.rs", false)], "new2"));
    assert!(!again.stale);
    assert_eq!(again.pr.source_commit, "new2", "now the head on screen");
    // a head already known is not overwritten by a fetch
    let (mut known, _) = screen();
    known.pr.source_commit = "abc".into();
    known.apply(opened_at(vec![], "def"));
    assert_eq!(known.pr.source_commit, "abc");
}

#[test]
fn reload_keeps_drafts_and_flags_older_head_inline_drafts_blocked() {
    use super::super::composer::{DraftState, FileRows, RowSource};
    use prcore::draft::Draft;
    use std::time::{Duration, Instant};
    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";
    let (mut s, store) = screen();
    s.pr.source_commit = OLD.into();
    s.apply(opened_at(vec![file("a.rs", false)], OLD));
    let draft = |kind: DraftKind, text: &str| Draft {
        id: 0,
        repo: s.repo.clone(),
        pr_id: s.pr.id,
        kind,
        path: (kind == DraftKind::Inline).then(|| "a.rs".to_string()),
        line: (kind == DraftKind::Inline).then_some(1),
        side: (kind == DraftKind::Inline).then_some(Side::New),
        start: None,
        reply_to: None,
        text: text.into(),
        source_commit: OLD.into(),
        comment_type: None,
    };
    let (inline, pr_level) = {
        let st = store.lock().unwrap();
        (
            st.save_draft(&draft(DraftKind::Inline, "on old code"))
                .unwrap(),
            st.save_draft(&draft(DraftKind::Pr, "overall")).unwrap(),
        )
    };
    // the PR moved: stale, then Reload re-fetches at the new head
    s.apply(ReviewEvent::Stale);
    let (repo, pr) = s.reload_target();
    let mut again = ReviewScreen::new(repo.clone(), pr.clone(), store.clone());
    let poster = Arc::new(MemoryPoster::new(NEW));
    let rows: RowSource = Arc::new(move |_| {
        Ok(FileRows {
            repo: repo.clone(),
            pr_id: pr.id,
            head: NEW.into(),
            merge_base: String::new(),
            rows: rows(),
            binary: false,
        })
    });
    again.composer.connect(poster.clone(), rows, None);
    again.apply(opened_at(vec![file("a.rs", false)], NEW));
    assert!(!again.stale);
    let ids: Vec<i64> = again.composer.drafts.iter().map(|d| d.id).collect();
    assert_eq!(ids, [inline, pr_level], "drafts kept");
    // re-validated on open: the inline draft is flagged before any submit
    assert_eq!(again.composer.state(inline), DraftState::Outdated);
    assert_eq!(again.composer.state(pr_level), DraftState::Idle);
    // submit posts the PR-level draft only; the inline one stays a blocked draft
    again.act(Act::Submit);
    again.act(Act::Confirm);
    let start = Instant::now();
    while again.composer.running() {
        again.poll_composer();
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(poster.posted.lock().unwrap().len(), 1);
    assert_eq!(again.composer.state(inline), DraftState::Outdated);
    let left = store
        .lock()
        .unwrap()
        .list_drafts(&again.repo, again.pr.id)
        .unwrap();
    assert_eq!(left.iter().map(|d| d.id).collect::<Vec<_>>(), [inline]);
}

#[test]
fn back_and_reload_wait_for_a_resolve_in_flight() {
    let (mut s, _) = screen();
    let poster = Arc::new(MemoryPoster::new("h"));
    *poster.hold_resolve.lock().unwrap() = true;
    s.composer
        .connect(poster.clone(), Arc::new(|_| Err("no rows".into())), None);
    let t = jiff::Timestamp::now();
    let root = prcore::comments::Comment {
        id: 5.into(),
        parent_id: None,
        author: prcore::model::UserRef {
            id: "{a}".into(),
            display_name: "Ana".into(),
            login: None,
        },
        body: "b".into(),
        created_on: t,
        updated_on: t,
        deleted: false,
        resolved: false,
        anchor: None,
        author_is_bot: false,
        thread: Some(prcore::comments::ThreadId::from(5)),
        can_resolve: true,
    };
    s.apply(ReviewEvent::Comments {
        threads: prcore::comments::build_threads(vec![root]),
        skipped: 0,
    });
    assert!(!s.busy());
    s.act(Act::Resolve(5.into()));
    assert!(s.composer.is_resolving(&5.into()));
    assert!(
        s.busy(),
        "Back and Reload are disabled while the resolve runs"
    );
    poster.release();
    let start = Instant::now();
    while s.busy() {
        s.poll_composer();
        assert!(start.elapsed() < Duration::from_secs(10), "resolve hung");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(s.comments.threads[0].root.resolved, "the resolve landed");
}

#[test]
fn screen_actions_disabled_when_stale_but_pr_comments_still_post() {
    use prcore::comments::ReviewAction;
    let (mut s, _) = screen();
    let poster = Arc::new(MemoryPoster::new("h"));
    s.actions.connect(poster.clone(), None);
    s.apply(opened_at(vec![file("a.rs", false)], "h"));
    s.apply(ReviewEvent::Stale);
    s.request_action(ReviewAction::Approve);
    assert!(s.actions.confirm.is_none());
    assert_eq!(
        s.actions.disabled_reason(s.stale),
        Some(super::super::actions_ui::STALE_ACTIONS)
    );
    // PR-level comments may still post
    s.composer
        .connect(poster, Arc::new(|_| Err("no rows".into())), None);
    s.act(Act::Open(Target::Pr));
    s.composer.editor.as_mut().unwrap().text = "still fine".into();
    s.act(Act::Save);
    assert_eq!(s.composer.postable().len(), 1);
}

#[test]
fn after_reload_stale_returns_only_when_the_probe_reports_another_head() {
    use std::time::{Duration, Instant};
    let f = crate::demo::review_fixture_with(2);
    let wait = |svc: &ReviewService, want_stale: bool| -> (bool, Vec<ReviewEvent>) {
        let start = Instant::now();
        let mut evs = Vec::new();
        let mut opened_at = None;
        loop {
            evs.extend(svc.poll());
            if evs.iter().any(|e| matches!(e, ReviewEvent::Stale)) {
                return (true, evs);
            }
            if opened_at.is_none()
                && evs
                    .iter()
                    .any(|e| matches!(e, ReviewEvent::Opened { .. } | ReviewEvent::Failed(_)))
            {
                opened_at = Some(Instant::now());
            }
            if opened_at.is_some_and(|t| t.elapsed() > Duration::from_millis(500)) {
                return (false, evs);
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "{want_stale}: {evs:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let probe = Arc::new(Mutex::new(f.head.clone()));
    let p = probe.clone();
    let st = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let svc = ReviewService::with_local(
        st.clone(),
        f.repo_path.clone(),
        egui::Context::default(),
        Arc::new(move |_, _| Ok(p.lock().unwrap().clone())),
        Arc::new(|_, _| Ok(Default::default())),
    );
    // the inbox copy is behind the fetched head: Stale
    let mut pr = f.pr.clone();
    pr.source_commit = f.git(&["rev-parse", "main"]);
    let mut s = ReviewScreen::new(f.repo.clone(), pr.clone(), st.clone());
    svc.open(f.repo.clone(), pr);
    let (stale, evs) = wait(&svc, true);
    assert!(stale, "{evs:?}");
    evs.into_iter().for_each(|e| s.apply(e));
    // Reload with the probe agreeing: no Stale
    let (repo, pr) = s.reload_target();
    svc.open(repo.clone(), pr.clone());
    let (stale, evs) = wait(&svc, false);
    assert!(!stale, "{evs:?}");
    // Reload while the server reports another head: Stale again
    *probe.lock().unwrap() = "0123456789abcdef".into();
    svc.open(repo, pr);
    let (stale, evs) = wait(&svc, true);
    assert!(stale, "{evs:?}");
}
