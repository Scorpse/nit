use super::*;

/// `small.txt` of the live throwaway MR (GitLab 19.5.1): line 3 removed, "added A" added
/// after line 5, line 8 changed.
const SMALL: &str = "@@ -1,10 +1,10 @@\n line 1\n line 2\n-line 3\n line 4\n line 5\n+added A\n line 6\n line 7\n-line 8\n+line 8 changed\n line 9\n line 10\n";

fn refs() -> DiffRefs {
    DiffRefs {
        base: "b".into(),
        start: "s".into(),
        head: "h".into(),
    }
}

#[test]
fn sha1_matches_known_vectors() {
    assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    assert_eq!(
        sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    assert_eq!(
        sha1_hex(&[b'a'; 1000]),
        "291e9a6c66994949b57ba5e650361e98fc36b1ba"
    );
}

#[test]
fn added_removed_and_context_lines_get_both_counters() {
    use LineKind::*;
    let d = |kind, old, new| DiffLine { kind, old, new };
    assert_eq!(locate(SMALL, Side::New, 5), Some(d(Added, 6, 5)));
    assert_eq!(locate(SMALL, Side::Old, 3), Some(d(Removed, 3, 3)));
    assert_eq!(locate(SMALL, Side::New, 3), Some(d(Context, 4, 3)));
    // a context line found from either side
    assert_eq!(locate(SMALL, Side::Old, 4), Some(d(Context, 4, 3)));
    assert_eq!(locate(SMALL, Side::New, 8), Some(d(Added, 9, 8)));
    assert_eq!(locate(SMALL, Side::Old, 8), Some(d(Removed, 8, 8)));
    // not in the diff, or the wrong side of a change
    assert_eq!(locate(SMALL, Side::New, 11), None);
    assert_eq!(locate(SMALL, Side::Old, 11), None);
    assert_eq!(locate(SMALL, Side::Old, 0), None);
    let far = "@@ -2197,7 +2197,7 @@ big line 2196\n big line 2197\n-big line 2200\n+big line 2200 changed\n";
    assert_eq!(locate(far, Side::New, 2198), Some(d(Added, 2199, 2198)));
    assert_eq!(locate(far, Side::New, 100), None);
    // two hunks; no-newline markers are not lines
    let two =
        "@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+B\n@@ -40 +40,2 @@\n x\n+y\n";
    assert_eq!(locate(two, Side::New, 2), Some(d(Added, 3, 2)));
    assert_eq!(locate(two, Side::New, 41), Some(d(Added, 41, 41)));
    assert_eq!(locate(two, Side::New, 20), None);
}

#[test]
fn line_code_format() {
    let l = DiffLine {
        kind: LineKind::Added,
        old: 6,
        new: 5,
    };
    assert_eq!(
        line_code("small.txt", &l),
        format!("{}_6_5", sha1_hex(b"small.txt"))
    );
    assert_eq!(line_code("a", &l).len(), 40 + 4);
}

#[test]
fn added_line_sends_new_line_only() {
    let l = locate(SMALL, Side::New, 5).unwrap();
    let p = text_position(&refs(), "small.txt", "small.txt", &l, None);
    assert_eq!(
        p,
        json!({"base_sha": "b", "start_sha": "s", "head_sha": "h", "position_type": "text",
               "new_path": "small.txt", "old_path": "small.txt", "new_line": 5})
    );
}

#[test]
fn deleted_line_sends_old_line_only() {
    let l = locate(SMALL, Side::Old, 3).unwrap();
    let p = text_position(&refs(), "small.txt", "small.txt", &l, None);
    assert_eq!(p["old_line"], 3);
    assert!(p.get("new_line").is_none());
}

#[test]
fn context_sends_both() {
    let l = locate(SMALL, Side::New, 3).unwrap();
    let p = text_position(&refs(), "o.txt", "small.txt", &l, Some(&l));
    assert_eq!(
        (p["old_line"].clone(), p["new_line"].clone()),
        (json!(4), json!(3))
    );
    assert_eq!(p["old_path"], "o.txt");
    // the start equal to the end is one line: no range
    assert!(p.get("line_range").is_none());
}

#[test]
fn multi_line_sends_a_line_range_with_codes_and_types() {
    let start = locate(SMALL, Side::New, 6).unwrap();
    let end = locate(SMALL, Side::New, 8).unwrap();
    let p = text_position(&refs(), "small.txt", "small.txt", &end, Some(&start));
    let sha = sha1_hex(b"small.txt");
    assert_eq!(
        p["line_range"],
        json!({
            "start": {"line_code": format!("{sha}_6_6"), "type": null, "old_line": 6, "new_line": 6},
            "end": {"line_code": format!("{sha}_9_8"), "type": "new", "old_line": null, "new_line": 8},
        })
    );
    assert_eq!(p["new_line"], 8);
    let removed = locate(SMALL, Side::Old, 3).unwrap();
    let p = text_position(&refs(), "small.txt", "small.txt", &end, Some(&removed));
    assert_eq!(p["line_range"]["start"]["type"], "old");
    assert_eq!(p["line_range"]["start"]["new_line"], Value::Null);
}

#[test]
fn file_position_has_no_lines() {
    assert_eq!(
        file_position(&refs(), "old-name.txt", "new-name.txt"),
        json!({"base_sha": "b", "start_sha": "s", "head_sha": "h", "position_type": "file",
               "new_path": "new-name.txt", "old_path": "old-name.txt"})
    );
}
