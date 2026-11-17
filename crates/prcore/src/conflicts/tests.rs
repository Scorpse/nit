use super::*;

const TREE: &str = "0123456789abcdef0123456789abcdef01234567";

fn entry(stage: u8, path: &str) -> String {
    format!("100644 {TREE} {stage}\t{path}\0")
}

#[test]
fn a_clean_merge_is_just_a_tree() {
    let r = parse_merge_tree(format!("{TREE}\0").as_bytes()).unwrap();
    assert_eq!(r.tree, TREE);
    assert!(r.files.is_empty());
}

#[test]
fn each_conflicted_path_gets_a_kind_from_its_stages() {
    let mut out = format!("{TREE}\0");
    for (stage, path) in [
        (1, "both.rs"),
        (2, "both.rs"),
        (3, "both.rs"),
        (2, "added.rs"),
        (3, "added.rs"),
        (1, "gone-in-pr.rs"),
        (2, "gone-in-pr.rs"),
        (1, "gone-on-dest.rs"),
        (3, "gone-on-dest.rs"),
    ] {
        out.push_str(&entry(stage, path));
    }
    out.push('\0');
    let r = parse_merge_tree(out.as_bytes()).unwrap();
    let got: Vec<_> = r.files.iter().map(|f| (f.path.as_str(), f.kind)).collect();
    assert_eq!(
        got,
        [
            ("both.rs", ConflictKind::Content),
            ("added.rs", ConflictKind::BothAdded),
            ("gone-in-pr.rs", ConflictKind::DeletedInPr),
            ("gone-on-dest.rs", ConflictKind::DeletedOnDest),
        ]
    );
}

#[test]
fn output_that_is_not_merge_tree_output_is_refused() {
    assert!(parse_merge_tree(b"").is_none());
    assert!(parse_merge_tree(b"usage: git merge-tree\n").is_none());
    let bad_stage = format!("{TREE}\0100644 {TREE} 9\tx\0");
    assert!(parse_merge_tree(bad_stage.as_bytes()).is_none());
}

#[test]
fn a_path_with_spaces_and_unicode_is_kept() {
    let out = format!(
        "{TREE}\0{}{}",
        entry(2, "dir/my file é.rs"),
        entry(3, "dir/my file é.rs")
    );
    let r = parse_merge_tree(out.as_bytes()).unwrap();
    assert_eq!(r.files[0].path, "dir/my file é.rs");
}

fn lines(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn markers_cut_the_text_into_context_and_hunks() {
    let text = "a\nb\n<<<<<<< main\nours\n=======\ntheirs 1\ntheirs 2\n>>>>>>> pr\nz\n";
    assert_eq!(
        parse_markers(text),
        [
            Segment::Context(lines(&["a", "b"])),
            Segment::Conflict(Hunk {
                ours: lines(&["ours"]),
                base: None,
                theirs: lines(&["theirs 1", "theirs 2"]),
            }),
            Segment::Context(lines(&["z"])),
        ]
    );
}

#[test]
fn a_base_section_is_kept_when_git_writes_one() {
    let text = "<<<<<<< a\nours\n||||||| base\nold\n=======\nnew\n>>>>>>> b\n";
    let Segment::Conflict(h) = &parse_markers(text)[0] else {
        panic!("a hunk")
    };
    assert_eq!(h.base, Some(lines(&["old"])));
}

#[test]
fn an_unclosed_region_and_lookalike_lines_stay_plain_text() {
    let open = "x\n<<<<<<< main\nours\n=======\ntheirs\n";
    assert_eq!(
        parse_markers(open),
        [Segment::Context(lines(&[
            "x",
            "<<<<<<< main",
            "ours",
            "=======",
            "theirs"
        ]))]
    );
    // eight markers, or text after the markers without a space, is content
    let like = "<<<<<<<< no\n=======x\n>>>>>>>y\n";
    assert_eq!(parse_markers(like).len(), 1);
    assert!(matches!(parse_markers(like)[0], Segment::Context(_)));
}

#[test]
fn the_summary_names_the_files_and_the_commands() {
    let r = ConflictReport {
        tree: TREE.into(),
        files: vec![ConflictFile {
            path: "src/a.rs".into(),
            kind: ConflictKind::DeletedOnDest,
        }],
    };
    let s = summary_markdown("Add a", "feat/a", "main", &r);
    assert!(s.contains("\"Add a\" (feat/a into main)"), "{s}");
    assert!(s.contains("- `src/a.rs`: deleted on main, changed in this PR"));
    assert!(s.contains("git switch feat/a") && s.contains("git merge origin/main"));
}
