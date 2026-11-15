use super::*;
use std::collections::HashSet;

fn row(kind: RowKind, text: &str) -> Row {
    Row {
        kind,
        old_line: None,
        new_line: None,
        text: text.into(),
    }
}

fn colors(spans: &[(Range<usize>, Color32)]) -> HashSet<Color32> {
    spans.iter().map(|(_, c)| *c).collect()
}

#[test]
fn rust_multiline_string_colored_consistently_within_hunk() {
    let rows = vec![
        row(RowKind::HunkHeader, "@@ -1,3 +1,3 @@"),
        row(RowKind::Add, "let s = \"first line"),
        row(RowKind::Add, "fn not_code() { let x = 1; }"),
        row(RowKind::Add, "last\";"),
    ];
    let sp = highlight_rows("src/lib.rs", &rows);
    assert_eq!(sp.len(), rows.len());
    assert!(sp[0].is_empty());
    let string_color = sp[1].iter().rev().find(|(r, _)| r.start > 8).unwrap().1;
    assert_eq!(colors(&sp[2]), HashSet::from([string_color]), "{:?}", sp[2]);
    // the same text outside a string is multi-coloured
    let code = highlight_rows("a.rs", &[row(RowKind::Add, "fn not_code() { let x = 1; }")]);
    assert!(colors(&code[0]).len() > 1);
}

#[test]
fn unknown_extension_returns_plain() {
    let rows = vec![row(RowKind::Add, "fn x() {}"), row(RowKind::Del, "y")];
    for p in ["file.zzzzz", "noext", ""] {
        let sp = highlight_rows(p, &rows);
        assert_eq!(sp.len(), 2);
        assert!(sp.iter().all(|s| s.is_empty()), "{p}");
    }
}

#[test]
fn hunk_reset_isolates_state() {
    let rows = vec![
        row(RowKind::HunkHeader, "@@ -1 +1 @@"),
        row(RowKind::Add, "/* comment opens"),
        row(RowKind::HunkHeader, "@@ -50 +50 @@"),
        row(RowKind::Add, "fn x() { let y = 1; }"),
    ];
    let sp = highlight_rows("a.rs", &rows);
    let alone = highlight_rows("a.rs", &rows[2..]);
    assert_eq!(sp[3], alone[1]);
    assert!(colors(&sp[3]).len() > 1);
}

#[test]
fn del_lines_use_old_side_state() {
    let rows = vec![
        row(RowKind::HunkHeader, "@@ -1,3 +1,3 @@"),
        row(RowKind::Del, "/* old comment opens"),
        row(RowKind::Add, "fn code() { let z = 1; }"),
        row(RowKind::Del, "fn still_comment() {}"),
        row(RowKind::Add, "let w = 2;"),
    ];
    let sp = highlight_rows("a.rs", &rows);
    assert!(colors(&sp[2]).len() > 1, "add is code: {:?}", sp[2]);
    assert_eq!(colors(&sp[3]).len(), 1, "del is in the old comment");
    assert!(colors(&sp[4]).len() > 1);
}

#[test]
fn context_advances_both_sides() {
    let rows = vec![
        row(RowKind::Context, "/* open"),
        row(RowKind::Del, "fn a() {}"),
        row(RowKind::Add, "fn b() {}"),
    ];
    let sp = highlight_rows("a.rs", &rows);
    assert_eq!(colors(&sp[1]).len(), 1);
    assert_eq!(colors(&sp[2]).len(), 1);
}

#[test]
fn very_long_line_skipped_over_2k_chars() {
    let rows = vec![
        row(RowKind::Add, &"a".repeat(2_001)),
        row(RowKind::Add, "fn x() {}"),
    ];
    let sp = highlight_rows("a.rs", &rows);
    assert!(sp[0].is_empty());
    assert!(!sp[1].is_empty());
    let ok = highlight_rows("a.rs", &[row(RowKind::Add, &"a".repeat(2_000))]);
    assert!(!ok[0].is_empty());
}

#[test]
fn spans_cover_only_valid_char_boundaries() {
    let rows = vec![
        row(RowKind::Add, "let s = \"héllo wörld 🎉 日本\"; // ünï\t✓"),
        row(RowKind::Add, "\tfn é() {}\r"),
        row(RowKind::Context, ""),
        row(RowKind::NoNewline, "\\ No newline at end of file"),
    ];
    let sp = highlight_rows("a.rs", &rows);
    assert!(sp[2].is_empty() && sp[3].is_empty());
    for (r, s) in rows.iter().zip(&sp) {
        let mut end = 0;
        for (range, _) in s {
            assert!(range.start < range.end && range.end <= r.text.len());
            assert!(r.text.is_char_boundary(range.start), "{r:?} {range:?}");
            assert!(r.text.is_char_boundary(range.end), "{r:?} {range:?}");
            assert!(range.start >= end, "overlap");
            end = range.end;
        }
    }
}

#[test]
fn highlight_100_rows_under_budget() {
    let rows: Vec<Row> = (0..100)
        .map(|i| row(RowKind::Add, &format!("let v{i} = compute({i}, \"x\");")))
        .collect();
    let sp = highlight_rows("a.rs", &rows);
    assert_eq!(sp.len(), 100);
    assert!(sp.iter().all(|s| !s.is_empty()));
}

#[test]
fn skipped_long_line_still_advances_parse_state() {
    let long_comment = format!("/* {}", "x".repeat(10_050));
    let rows = vec![
        row(RowKind::Add, &long_comment),
        row(RowKind::Add, "fn inside_comment() { let a = 1; }"),
    ];
    let sp = highlight_rows("a.rs", &rows);
    assert!(sp[0].is_empty());
    assert_eq!(
        colors(&sp[1]).len(),
        1,
        "still inside the comment: {:?}",
        sp[1]
    );
}

const TS_SAMPLE: &[&str] = &[
    "import type { Catalogue, Entry } from \"./catalogue\";",
    "import { describe, expect, it } from \"vitest\";",
    "",
    "export interface Page<T extends Entry = Entry> {",
    "  items: readonly T[];",
    "  next?: string;",
    "}",
    "",
    "export const byCountry = <T extends Entry>(xs: T[], cc: string): T[] =>",
    "  xs.filter((x) => x.country === cc);",
    "",
    "describe(\"catalogue\", () => {",
    "  it(\"formats\", async () => {",
    "    const label = `entry ${entry.id}: ${entry.name.toUpperCase()}`;",
    "    expect(label).toMatch(/^entry \\d+/);",
    "  });",
    "});",
];

const TSX_SAMPLE: &[&str] = &[
    "import React, { useState } from \"react\";",
    "type Props = { rows: Array<{ id: number; name: string }> };",
    "export function CatalogueTable({ rows }: Props): JSX.Element {",
    "  const [open, setOpen] = useState<boolean>(false);",
    "  return (",
    "    <table className=\"catalogue\" onClick={() => setOpen(!open)}>",
    "      {rows.map((r) => <tr key={r.id}><td>{`#${r.id}`}</td></tr>)}",
    "    </table>",
    "  );",
    "}",
];

fn added(lines: &[&str]) -> Vec<Row> {
    std::iter::once(row(RowKind::HunkHeader, "@@ -0,0 +1,20 @@"))
        .chain(lines.iter().map(|l| row(RowKind::Add, l)))
        .collect()
}

fn all_colors(sp: &Spans) -> HashSet<Color32> {
    sp.iter().flat_map(|s| colors(s)).collect()
}

#[test]
fn typescript_sample_is_coloured() {
    for path in ["src/catalogue.test.ts", "a.mts", "b.cts"] {
        let rows = added(TS_SAMPLE);
        let sp = highlight_rows(path, &rows);
        let n = all_colors(&sp).len();
        assert!(n >= 3, "{path}: {n} colours");
        // the template string line itself has several colours
        assert!(colors(&sp[14]).len() >= 2, "{path}: {:?}", sp[14]);
    }
}

#[test]
fn tsx_sample_is_coloured() {
    let rows = added(TSX_SAMPLE);
    let sp = highlight_rows("web/CatalogueTable.tsx", &rows);
    assert!(all_colors(&sp).len() >= 3, "{sp:?}");
    assert!(colors(&sp[6]).len() >= 2, "the JSX line: {:?}", sp[6]);
}

#[test]
fn file_names_map_to_their_syntax() {
    let cases = [
        ("a/b.ts", Some("TypeScript")),
        ("x.mts", Some("TypeScript")),
        ("x.cts", Some("TypeScript")),
        ("c/D.tsx", Some("TypeScriptReact")),
        ("e.js", Some("JavaScript (Babel)")),
        ("e.jsx", Some("JavaScript (Babel)")),
        ("e.mjs", Some("JavaScript (Babel)")),
        ("e.cjs", Some("JavaScript (Babel)")),
        ("package.json", Some("JSON")),
        ("tsconfig.jsonc", Some("JSON")),
        (".github/workflows/ci.yml", Some("YAML")),
        ("pnpm-workspace.yaml", Some("YAML")),
        ("Cargo.toml", Some("TOML")),
        ("README.md", Some("Markdown")),
        ("scripts/build.sh", Some("Bourne Again Shell (bash)")),
        ("tools/run.ps1", Some("PowerShell")),
        ("Dockerfile", Some("Dockerfile")),
        ("docker/api.Dockerfile", Some("Dockerfile")),
        (".gitignore", Some("Git Ignore")),
        ("pkg/.gitignore", Some("Git Ignore")),
        ("app/Main.kt", Some("Kotlin")),
        ("src/lib.rs", Some("Rust")),
        ("noext", None),
        ("file.zzzzz", None),
        ("", None),
    ];
    for (path, want) in cases {
        assert_eq!(syntax_for(path).map(|s| s.name.as_str()), want, "{path}");
    }
}

/// `cargo test -p app --release --bin nit warm_up_time -- --ignored --nocapture`
/// (alone: the syntax set is loaded once per process).
#[test]
#[ignore]
fn warm_up_time() {
    let t = std::time::Instant::now();
    LazyLock::force(&SYNTAXES);
    let load = t.elapsed();
    LazyLock::force(&THEME);
    let theme = t.elapsed() - load;
    warm();
    let total = t.elapsed();
    let t2 = std::time::Instant::now();
    let sp = highlight_rows("again.ts", &added(TS_SAMPLE));
    println!(
        "syntax set {load:?}, theme {theme:?}, warm total {total:?}; \
         warm {} TS rows {:?}; {} syntaxes",
        sp.len(),
        t2.elapsed(),
        SYNTAXES.syntaxes().len()
    );
}

/// Runs `highlight_chunked` with no limits; returns its spans, its end, every emitted
/// range in order, and the spans as merged from the emitted steps.
fn chunked(
    path: &str,
    rows: &[Row],
    step: usize,
    screen: Option<usize>,
) -> (Spans, HlEnd, Vec<Range<usize>>, Spans) {
    let mut merged: Spans = vec![Vec::new(); rows.len()];
    let mut ranges = Vec::new();
    let mut emit = |r: Range<usize>, s: &[RowSpans]| {
        merged[r.clone()].clone_from_slice(s);
        ranges.push(r);
        Next::Continue
    };
    let (spans, end) = highlight_chunked(
        path,
        rows,
        Chunking {
            rows: step,
            budget: Duration::from_secs(3600),
            cancelled: &|| false,
            on_screen: &|| screen,
            emit: &mut emit,
        },
    );
    (spans, end, ranges, merged)
}

/// A TypeScript file of `hunks` hunks of `len` rows; hunk 2 opens a block comment that
/// its own rows continue (state that must not leak into hunk 3).
fn ts_hunks(hunks: usize, len: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for h in 0..hunks {
        rows.push(row(
            RowKind::HunkHeader,
            &format!("@@ -{0},{len} +{0},{len} @@", h * 100),
        ));
        for i in 0..len {
            let text = match (h, i) {
                (2, 0) => "/* a comment opens".to_string(),
                _ => TS_SAMPLE[(h + i) % TS_SAMPLE.len()].to_string(),
            };
            let kind = [RowKind::Context, RowKind::Add, RowKind::Del][(h + i) % 3];
            rows.push(row(kind, &text));
        }
    }
    rows
}

#[test]
fn chunked_output_equals_whole_file_output() {
    let f = crate::demo::review_fixture_demo();
    let patch = f.git(&["diff", "-U3", "-M", "main...feature/demo"]);
    // every demo file's hunks (the TypeScript ones first: they sort before `src/`)
    let mut demo = prcore::diff::parse_patch(&patch);
    demo.truncate(900);
    let fixtures: Vec<(&str, Vec<Row>)> = vec![
        ("demo.ts", demo.clone()),
        ("demo.rs", demo.clone()),
        ("demo.tsx", demo),
        ("many.ts", ts_hunks(12, 37)),
        ("one-big-hunk.ts", ts_hunks(1, 1500)),
        ("headless.rs", ts_hunks(3, 20)[1..].to_vec()),
        ("plain.zzz", ts_hunks(2, 5)),
    ];
    for (path, rows) in &fixtures {
        let whole = highlight_rows(path, rows);
        let n = rows.len();
        for step in [1, 7, CHUNK_ROWS] {
            for screen in [None, Some(n / 2), Some(n.saturating_sub(1)), Some(n + 10)] {
                let (spans, end, ranges, merged) = chunked(path, rows, step, screen);
                let what = format!("{path} step {step} screen {screen:?}");
                assert_eq!(end, HlEnd::Done, "{what}");
                assert_eq!(spans, whole, "{what}");
                assert_eq!(merged, whole, "{what}: emitted steps");
                assert!(ranges.iter().all(|r| r.len() <= step), "{what}");
                let rows_emitted: usize = ranges.iter().map(|r| r.len()).sum();
                assert_eq!(rows_emitted, if syntax_for(path).is_some() { n } else { 0 });
            }
        }
    }
}

#[test]
fn chunks_start_at_the_hunk_on_screen_then_go_down_then_up() {
    let rows = ts_hunks(10, 50); // hunk k is rows 51k..51k+51
    let (_, _, ranges, _) = chunked("a.ts", &rows, 1000, Some(6 * 51 + 20));
    let starts: Vec<usize> = ranges.iter().map(|r| r.start).collect();
    assert_eq!(starts[0], 6 * 51, "the hunk on screen first: {ranges:?}");
    // then the hunks below it (one contiguous step), then those above, nearest first
    assert_eq!(ranges[0], 6 * 51..10 * 51);
    assert_eq!(&starts[1..], &[5 * 51, 4 * 51, 3 * 51, 2 * 51, 51, 0]);
}

#[test]
fn chunked_stops_between_rows_when_cancelled() {
    let rows = ts_hunks(4, 100);
    let asked = std::cell::Cell::new(0);
    let mut emitted = 0;
    let mut emit = |r: Range<usize>, _: &[RowSpans]| {
        emitted += r.len();
        Next::Continue
    };
    let cancelled = || {
        asked.set(asked.get() + 1);
        asked.get() > 30
    };
    let (spans, end) = highlight_chunked(
        "a.ts",
        &rows,
        Chunking {
            rows: 1000,
            budget: Duration::from_secs(3600),
            cancelled: &cancelled,
            on_screen: &|| None,
            emit: &mut emit,
        },
    );
    assert_eq!(end, HlEnd::Cancelled);
    assert_eq!(asked.get(), 31, "asked before every row, stopped at once");
    assert_eq!(emitted, 0, "an unfinished step is not handed out");
    assert!(spans[31..].iter().all(|s| s.is_empty()));
}

#[test]
fn chunked_keeps_what_is_done_when_the_budget_runs_out() {
    // load and compile the syntax first, so the budget below is spent on rows and the
    // result does not depend on how long a cold start takes
    let _ = highlight_rows("a.ts", &ts_hunks(1, 50));
    let rows = ts_hunks(1, 20_000);
    let mut merged: Spans = vec![Vec::new(); rows.len()];
    let mut emit = |r: Range<usize>, s: &[RowSpans]| {
        merged[r].clone_from_slice(s);
        Next::Continue
    };
    let (spans, end) = highlight_chunked(
        "a.ts",
        &rows,
        Chunking {
            rows: CHUNK_ROWS,
            budget: Duration::from_millis(30),
            cancelled: &|| false,
            on_screen: &|| None,
            emit: &mut emit,
        },
    );
    assert_eq!(end, HlEnd::OverBudget);
    let coloured = spans.iter().filter(|s| !s.is_empty()).count();
    assert!(coloured > 0 && coloured < rows.len() / 2, "{coloured}");
    assert!(spans.last().unwrap().is_empty(), "the rest stays plain");
    assert_eq!(merged, spans, "what was done was handed out");
}

#[test]
fn chunked_resends_everything_coloured_when_asked() {
    let rows = ts_hunks(3, 30);
    let mut calls = 0;
    let mut seen: Vec<Range<usize>> = Vec::new();
    let mut emit = |r: Range<usize>, _: &[RowSpans]| {
        calls += 1;
        seen.push(r);
        if calls == 2 {
            Next::Resend
        } else {
            Next::Continue
        }
    };
    highlight_chunked(
        "a.ts",
        &rows,
        Chunking {
            rows: 20,
            budget: Duration::from_secs(3600),
            cancelled: &|| false,
            on_screen: &|| None,
            emit: &mut emit,
        },
    );
    // steps 0..20 and 20..40 (a step runs on into the next hunk), the resend of 0..40,
    // then the rest
    assert_eq!(seen[..3], [0..20, 20..40, 0..40]);
    assert_eq!(seen.last().unwrap().end, rows.len());
}

#[test]
fn a_file_shown_again_while_it_runs_is_resent_and_background_jobs_wait() {
    let mut q = HighlightQueue::default();
    let rows = rows_of(3);
    q.request_background("n.rs", &rows);
    q.request("a.rs", &rows);
    assert!(q.has_visible());
    let a = q.pop().unwrap();
    assert_eq!((a.path.as_str(), a.priority), ("a.rs", HlPriority::Visible));
    assert!(
        q.pop().is_none(),
        "background jobs are not taken for the screen"
    );
    // shown again while it runs: the job is told to resend
    assert!(q.request("a.rs", &rows).is_none());
    assert!(a.resend.load(Ordering::SeqCst));
    assert!(q.finish("a.rs", Some(Arc::new(vec![Vec::new(); 3]))));
    // a queued background job asked for by the screen moves to the screen's queue
    q.request("n.rs", &rows);
    assert!(q.pop_background().is_none());
    assert_eq!(q.pop().unwrap().priority, HlPriority::Visible);
    // a remembered or pending file is not queued again in the background
    q.request_background("a.rs", &rows);
    q.request_background("n.rs", &rows);
    assert!(q.pop_background().is_none());
    for i in 0..10 {
        q.request_background(&format!("b{i}.rs"), &rows);
    }
    assert_eq!(q.pop_background().unwrap().path, "b9.rs", "newest first");
    assert_eq!(q.background.len(), MAX_BACKGROUND - 1);
}

#[test]
fn every_mapped_syntax_exists() {
    for (_, syn) in BY_NAME.iter().chain(BY_EXT) {
        assert!(SYNTAXES.find_syntax_by_name(syn).is_some(), "{syn}");
    }
}

#[test]
fn typescript_spans_golden() {
    let text = "const s = `héllo ${wörld}`; // ✓";
    let sp = highlight_rows("a.ts", &[row(RowKind::Add, text)]);
    let ranges: Vec<Range<usize>> = sp[0].iter().map(|(r, _)| r.clone()).collect();
    for r in &ranges {
        assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
    }
    assert_eq!(ranges, GOLDEN_TS, "{:?}", sp[0]);
}

/// "const", " ", "s", " = `", "héllo ${", "wörld", "}", "`; ", "// ✓"
const GOLDEN_TS: [Range<usize>; 9] = [
    0..5,
    5..6,
    6..7,
    7..11,
    11..20,
    20..26,
    26..27,
    27..30,
    30..36,
];

fn rows_of(n: usize) -> Arc<Vec<Row>> {
    Arc::new(vec![row(RowKind::Add, "x"); n])
}

#[test]
fn stale_highlight_jobs_are_dropped_and_newest_runs_first() {
    let mut q = HighlightQueue::default();
    let rows = rows_of(1);
    for i in 0..40 {
        assert!(q.request(&format!("f{i}.rs"), &rows).is_none());
    }
    assert_eq!(q.pop().unwrap().path, "f39.rs", "last requested first");
    assert_eq!(q.pop().unwrap().path, "f38.rs");
    assert!(q.pop().is_none(), "the 38 older ones were dropped");
    // a dropped path can be requested again
    q.finish("f39.rs", None);
    assert!(q.request("f0.rs", &rows).is_none());
    assert_eq!(q.pop().unwrap().path, "f0.rs");
}

#[test]
fn requesting_a_queued_path_again_makes_it_newest() {
    let mut q = HighlightQueue::default();
    let rows = rows_of(1);
    q.request("a.rs", &rows);
    q.request("b.rs", &rows);
    q.request("a.rs", &rows);
    assert_eq!(q.queue.len(), 2);
    assert_eq!(q.pop().unwrap().path, "a.rs");
}

#[test]
fn remembered_colours_are_bounded_by_bytes_and_count() {
    let mut q = HighlightQueue::default();
    let rows = rows_of(1);
    // ~19 MB of colours: over the per-file threshold, not remembered
    let huge: Spans = vec![vec![(0..1, Color32::RED)]; 400_000];
    q.finish("huge.rs", Some(Arc::new(huge)));
    assert!(q
        .request("huge.rs", &Arc::new(vec![row(RowKind::Add, "x"); 400_000]))
        .is_none());
    // ~12 MB each: only a few fit in 64 MiB
    for i in 0..8 {
        let s: Spans = vec![vec![(0..1, Color32::RED)]; 250_000];
        q.finish(&format!("m{i}.rs"), Some(Arc::new(s)));
    }
    let total: usize = q.done.iter().map(|(_, s)| spans_bytes(s)).sum();
    assert!(total <= REMEMBERED_BYTES, "{total}");
    assert!(q.done.len() < 8);
    assert!(q.done.back().is_some_and(|(p, _)| p == "m7.rs"));
    let _ = rows;
}
