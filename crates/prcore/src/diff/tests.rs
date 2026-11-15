use super::*;

const Z: &str = "0000000000000000000000000000000000000000";

#[test]
fn index_joins_raw_and_numstat() {
    let raw = format!(
        ":100644 100644 aaa bbb M\0src/a.rs\0\
         :000000 100644 {Z} ccc A\0new.txt\0\
         :100644 000000 ddd {Z} D\0gone.txt\0\
         :100644 100644 eee eee R100\0old name.rs\0new name.rs\0\
         :100644 100644 fff ggg M\0img.png\0"
    );
    let num = "3\t1\tsrc/a.rs\0\
               5\t0\tnew.txt\0\
               0\t7\tgone.txt\0\
               0\t0\t\0old name.rs\0new name.rs\0\
               -\t-\timg.png\0";
    let f = parse_diff_index(raw.as_bytes(), num.as_bytes());
    assert_eq!(f.len(), 5);
    assert_eq!(
        (f[0].status, f[0].additions, f[0].deletions),
        (FileStatus::Modified, 3, 1)
    );
    assert_eq!(f[0].new_oid, "bbb");
    assert_eq!((f[1].status, f[1].additions), (FileStatus::Added, 5));
    assert_eq!(
        (f[2].status, f[2].deletions, f[2].new_oid.as_str()),
        (FileStatus::Deleted, 7, Z)
    );
    assert_eq!(f[3].status, FileStatus::Renamed);
    assert_eq!(f[3].old_path.as_deref(), Some("old name.rs"));
    assert_eq!(f[3].path, "new name.rs");
    assert!(f[4].binary);
    assert_eq!((f[4].additions, f[4].deletions), (0, 0));
    assert!(!f[0].binary);
}

fn raw_and_numstat() -> (String, &'static str) {
    let raw = format!(
        ":100644 100644 aaa bbb M\0src/a.rs\0\
         :000000 100644 {Z} ccc A\0new.txt\0\
         :100644 100644 eee eee R100\0old name.rs\0new name.rs\0\
         :100644 100644 fff ggg M\0img.png\0"
    );
    let num = "3\t1\tsrc/a.rs\0\
               5\t0\tnew.txt\0\
               2\t1\t\0old name.rs\0new name.rs\0\
               -\t-\timg.png\0";
    (raw, num)
}

#[test]
fn raw_list_alone_has_files_with_unknown_counts() {
    let (raw, num) = raw_and_numstat();
    let f = parse_diff_raw(raw.as_bytes());
    assert_eq!(f.len(), 4);
    assert!(f.iter().all(|d| !d.counts_known));
    assert!(f
        .iter()
        .all(|d| d.additions == 0 && d.deletions == 0 && !d.binary));
    assert_eq!(f[2].old_path.as_deref(), Some("old name.rs"));
    assert_eq!(f[2].status, FileStatus::Renamed);
    // the same list as the joined parse, apart from the counts
    let joined = parse_diff_index(raw.as_bytes(), num.as_bytes());
    assert!(joined.iter().all(|d| d.counts_known));
    for (a, b) in f.iter().zip(&joined) {
        assert_eq!(
            (&a.path, &a.old_path, a.status, &a.new_oid),
            (&b.path, &b.old_path, b.status, &b.new_oid)
        );
    }
}

#[test]
fn numstat_counts_name_the_new_path_and_apply_by_path() {
    let (raw, num) = raw_and_numstat();
    let counts = parse_numstat(num.as_bytes());
    let paths: Vec<&str> = counts.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["src/a.rs", "new.txt", "new name.rs", "img.png"]);
    let mut files = parse_diff_raw(raw.as_bytes());
    // sorted by path first, as the review does: matching is by path, not by order
    files.sort_by(|a, b| a.path.cmp(&b.path));
    apply_counts(&mut files, &counts);
    let joined = parse_diff_index(raw.as_bytes(), num.as_bytes());
    for f in &files {
        let j = joined.iter().find(|j| j.path == f.path).unwrap();
        assert_eq!(f, j);
    }
    // a file numstat does not list is known as 0/0 (what the joined parse gives)
    let mut files = parse_diff_raw(raw.as_bytes());
    apply_counts(&mut files, &counts[..1]);
    assert!(files.iter().all(|d| d.counts_known));
    assert_eq!((files[1].additions, files[3].binary), (0, false));
}

#[test]
fn index_handles_path_with_spaces_and_unicode() {
    let raw = ":100644 100644 a b M\0dir with space/héllo wörld ✓.txt\0";
    let num = "1\t2\tdir with space/héllo wörld ✓.txt\0";
    let f = parse_diff_index(raw.as_bytes(), num.as_bytes());
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].path, "dir with space/héllo wörld ✓.txt");
    assert_eq!((f[0].additions, f[0].deletions), (1, 2));
}

#[test]
fn index_edge_cases_do_not_panic() {
    assert!(parse_diff_index(b"", b"").is_empty());
    // truncated rename
    let mut raw = b":100644 100644 a b R90\0only-one".to_vec();
    assert!(parse_diff_index(&raw, b"").is_empty());
    // non-UTF-8 path, mode-only change, empty added file, missing numstat entry
    raw = b":100755 100644 a b M\0bad\xff.sh\0:000000 100644 0 c A\0empty\0".to_vec();
    let f = parse_diff_index(&raw, b"0\t0\tbad\xff.sh\0");
    assert_eq!(f.len(), 2);
    assert!(f[0].path.contains('\u{fffd}'));
    assert_eq!((f[1].additions, f[1].binary), (0, false));
}

#[test]
fn patch_numbers_lines_across_two_hunks() {
    let p = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n\
             @@ -1,3 +1,3 @@ fn x\n a\n-b\n+B\n c\n@@ -10 +10,2 @@\n-x\n+y\n+z\n";
    let r = parse_patch(p);
    let nums: Vec<_> = r.iter().map(|r| (r.kind, r.old_line, r.new_line)).collect();
    assert_eq!(
        nums,
        vec![
            (RowKind::HunkHeader, None, None),
            (RowKind::Context, Some(1), Some(1)),
            (RowKind::Del, Some(2), None),
            (RowKind::Add, None, Some(2)),
            (RowKind::Context, Some(3), Some(3)),
            (RowKind::HunkHeader, None, None),
            (RowKind::Del, Some(10), None),
            (RowKind::Add, None, Some(10)),
            (RowKind::Add, None, Some(11)),
        ]
    );
    assert_eq!(r[2].text, "b");
}

#[test]
fn patch_no_newline_marker_has_no_line_numbers() {
    let p = "@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n";
    let r = parse_patch(p);
    assert_eq!(r.len(), 5);
    assert_eq!(r[2].kind, RowKind::NoNewline);
    assert_eq!((r[2].old_line, r[2].new_line), (None, None));
    assert_eq!(r[3].new_line, Some(1));
    assert_eq!(r[4].kind, RowKind::NoNewline);
}

#[test]
fn patch_crlf_text_keeps_content_without_cr_artifacts() {
    let p = "@@ -1,2 +1,2 @@\r\n a\r\n-b\r\n+c\r\n";
    let r = parse_patch(p);
    assert_eq!(r.len(), 4);
    assert_eq!(r[1].text, "a\r");
    assert_eq!(r[3].new_line, Some(2));
    assert_eq!(r[2].old_line, Some(2));
}

#[test]
fn patch_empty_input_gives_no_rows() {
    assert!(parse_patch("").is_empty());
    assert!(parse_patch("diff --git a/x b/x\nBinary files a/x and b/x differ\n").is_empty());
    assert!(parse_patch("@@ garbage @@\n+x\n").is_empty());
}

#[test]
fn patch_dashes_in_content_are_not_file_headers() {
    let p = "@@ -1,2 +1,1 @@\n--- a\n-b\n+c\n";
    let r = parse_patch(p);
    assert_eq!(r.len(), 4);
    assert_eq!(r[1].kind, RowKind::Del);
    assert_eq!(r[1].text, "-- a");
}

#[test]
fn patch_very_long_line_is_kept() {
    let long = "x".repeat(50_000);
    let r = parse_patch(&format!("@@ -0,0 +1 @@\n+{long}\n"));
    assert_eq!(r[1].text.len(), 50_000);
}

#[test]
fn patch_mismatched_counts_do_not_swallow_next_header() {
    let r = parse_patch(
        "@@ -1,3 +1,3 @@
 a
@@ -10 +10 @@
-x
+y
",
    );
    let k: Vec<_> = r.iter().map(|r| (r.kind, r.old_line, r.new_line)).collect();
    assert_eq!(
        k,
        vec![
            (RowKind::HunkHeader, None, None),
            (RowKind::Context, Some(1), Some(1)),
            (RowKind::HunkHeader, None, None),
            (RowKind::Del, Some(10), None),
            (RowKind::Add, None, Some(10)),
        ]
    );
    let r = parse_patch(
        "@@ -1,3 +1,3 @@
 a
diff --git a/y b/y
--- a/y
+++ b/y
@@ -5 +5 @@
-q
+r
",
    );
    assert_eq!(r.len(), 5);
    assert_eq!(r[2].kind, RowKind::HunkHeader);
    assert_eq!(r[3].old_line, Some(5));
}

#[test]
fn patch_line_number_overflow_does_not_panic() {
    let r = parse_patch(
        "@@ -4294967295,2 +1 @@
 a
 b
",
    );
    assert_eq!(r.len(), 3);
}

/// The parser as it was before it stopped collecting every line into a list first.
fn old_parse_patch(patch: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let (mut old_left, mut new_left) = (0u32, 0u32);
    let mut pieces: Vec<&str> = patch.split('\n').collect();
    if pieces.last().is_some_and(|l| l.is_empty()) {
        pieces.pop();
    }
    for line in pieces {
        if line.starts_with("diff ") {
            old_left = 0;
            new_left = 0;
            continue;
        }
        let in_hunk = old_left > 0 || new_left > 0;
        if line.starts_with("@@ ") {
            if let Some((a, b, c, d)) = parse_hunk_header(line) {
                (old_no, new_no, old_left, new_left) = (a, c, b, d);
                rows.push(Row {
                    kind: RowKind::HunkHeader,
                    old_line: None,
                    new_line: None,
                    text: line.to_string(),
                });
            }
            continue;
        }
        if line.starts_with('\\') {
            if !rows.is_empty() {
                rows.push(Row {
                    kind: RowKind::NoNewline,
                    old_line: None,
                    new_line: None,
                    text: line.to_string(),
                });
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (RowKind::Add, &line[1..]),
            Some(b'-') => (RowKind::Del, &line[1..]),
            Some(b' ') => (RowKind::Context, &line[1..]),
            None => (RowKind::Context, ""),
            Some(_) => continue,
        };
        let (old_line, new_line) = match kind {
            RowKind::Add => {
                new_left = new_left.saturating_sub(1);
                let n = new_no;
                new_no = new_no.saturating_add(1);
                (None, Some(n))
            }
            RowKind::Del => {
                old_left = old_left.saturating_sub(1);
                let o = old_no;
                old_no = old_no.saturating_add(1);
                (Some(o), None)
            }
            _ => {
                old_left = old_left.saturating_sub(1);
                new_left = new_left.saturating_sub(1);
                let (o, n) = (old_no, new_no);
                old_no = old_no.saturating_add(1);
                new_no = new_no.saturating_add(1);
                (Some(o), Some(n))
            }
        };
        rows.push(Row {
            kind,
            old_line,
            new_line,
            text: text.to_string(),
        });
    }
    rows
}

#[test]
fn parse_patch_matches_the_old_parser_on_edge_cases() {
    let cases = [
        "",
        "\n",
        "\n\n",
        "@@ -1 +1 @@\n-a\n+b",
        "@@ -1 +1 @@\n-a\n+b\n",
        "@@ -1,2 +1,2 @@\n a\n\n-b\n+c\n\n",
        "@@ -1,2 +1,2 @@\r\n a\r\n-b\r\n+c\r\n",
        "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n\\ No newline at end of file\n c\n",
    ];
    for p in cases {
        assert_eq!(parse_patch(p), old_parse_patch(p), "{p:?}");
    }
}

/// `cargo test -p prcore --release parse_patch_bench -- --ignored --nocapture`
#[test]
#[ignore]
fn parse_patch_bench() {
    let mut patch =
        String::from("diff --git a/l b/l\n--- a/l\n+++ b/l\n@@ -1,100000 +1,100000 @@\n");
    for i in 0..100_000 {
        patch.push_str(&format!(
            "-    let value_{i:06} = compute_something(input.field_{i}, \"literal {i}\", \
             Some(&mut state), options.clone()).unwrap_or_default(); // old {i}\n\
             +    let value_{i:06} = compute_something(input.field_{i}, \"literal {i}\", \
             Some(&mut state), options.clone()).unwrap_or_default(); // new {i}\n"
        ));
    }
    let best = |f: &dyn Fn() -> Vec<Row>| {
        (0..7)
            .map(|_| {
                let t = std::time::Instant::now();
                let rows = f();
                let e = t.elapsed();
                drop(rows);
                e
            })
            .min()
            .unwrap()
    };
    assert_eq!(parse_patch(&patch), old_parse_patch(&patch));
    let old = best(&|| old_parse_patch(&patch));
    let new = best(&|| parse_patch(&patch));
    println!(
        "200,001-row patch ({} bytes): old parse {old:?}, new parse {new:?}",
        patch.len()
    );
}

#[test]
fn deleted_oid_detects_abbreviated_zeros() {
    let mk = |oid: &str| DiffFile {
        path: String::new(),
        old_path: None,
        status: FileStatus::Deleted,
        additions: 0,
        deletions: 0,
        binary: false,
        counts_known: true,
        new_oid: oid.into(),
    };
    assert!(mk("0000000").is_deleted_oid());
    assert!(mk(Z).is_deleted_oid());
    assert!(!mk("0a00000").is_deleted_oid());
    assert!(!mk("").is_deleted_oid());
}

#[test]
fn generated_globs() {
    let cases = [
        ("Cargo.lock", true),
        ("sub/Cargo.lock", true),
        ("package-lock.json", true),
        ("web/yarn.lock", true),
        ("pnpm-lock.yaml", true),
        ("a/app.min.js", true),
        ("a/app.min.css", true),
        ("Gemfile.lock", true),
        ("tests/x.snap", true),
        ("src/generated/a.rs", true),
        ("generated/a.rs", true),
        ("web/dist/a.js", true),
        ("third/vendor/x.c", true),
        ("api/foo.pb.go", true),
        ("Foo.g.cs", true),
        ("src/main.rs", false),
        ("src/distinct/a.rs", false),
        ("app.js", false),
        ("lockfile.txt", false),
    ];
    for (p, want) in cases {
        assert_eq!(is_generated(p), want, "{p}");
    }
}
