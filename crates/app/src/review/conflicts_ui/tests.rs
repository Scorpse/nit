use prcore::conflicts::{ConflictFile, ConflictKind, Hunk};

use super::*;

fn report(paths: &[&str]) -> ConflictReport {
    ConflictReport {
        tree: "t".repeat(40),
        files: paths
            .iter()
            .map(|p| ConflictFile {
                path: (*p).into(),
                kind: ConflictKind::Content,
            })
            .collect(),
    }
}

fn ls(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

fn hunk() -> Segment {
    Segment::Conflict(Hunk {
        ours: ls(&["o"]),
        base: None,
        theirs: ls(&["t"]),
    })
}

#[test]
fn context_is_cut_to_a_few_lines_around_each_region() {
    let long: Vec<String> = (1..=10).map(|n| format!("l{n}")).collect();
    let segs = [
        Segment::Context(long.clone()),
        hunk(),
        Segment::Context(long.clone()),
        hunk(),
        Segment::Context(ls(&["end"])),
    ];
    let got = lines_of(&segs, "main");
    let kinds: Vec<Kind> = got.iter().map(|l| l.kind).collect();
    use Kind::*;
    assert_eq!(
        kinds,
        [
            Skip,
            Context,
            Context,
            Context, // the start of the file: its last 3 lines
            Head,
            OursLabel,
            Ours,
            TheirsLabel,
            Theirs, //
            Context,
            Context,
            Context,
            Skip,
            Context,
            Context,
            Context, // between: 3 + cut + 3
            Head,
            OursLabel,
            Ours,
            TheirsLabel,
            Theirs,  //
            Context, // short enough to show whole
        ]
    );
    assert_eq!(got[0].text, "7 unchanged lines");
    assert_eq!(got[3].text, "l10");
    assert_eq!(got[4].text, "Conflict 1 of 2");
    assert_eq!(got[5].text, "main (the destination)");
}

#[test]
fn text_without_markers_has_no_lines() {
    assert!(lines_of(&[Segment::Context(ls(&["a", "b"]))], "main").is_empty());
    assert!(lines_of(&[], "main").is_empty());
}

#[test]
fn a_base_section_gets_its_own_label() {
    let segs = [Segment::Conflict(Hunk {
        ours: ls(&["o"]),
        base: Some(ls(&["b"])),
        theirs: ls(&["t"]),
    })];
    let kinds: Vec<Kind> = lines_of(&segs, "main").iter().map(|l| l.kind).collect();
    assert!(kinds.contains(&Kind::BaseLabel) && kinds.contains(&Kind::Base));
}

#[test]
fn a_file_is_asked_for_once_and_its_text_is_cut_at_the_markers() {
    let mut c = Conflicts::default();
    c.select("a.rs");
    assert!(
        c.take_wants().is_empty(),
        "nothing is known before the report"
    );
    c.set_report(Ok(report(&["a.rs", "b.rs"])));
    assert_eq!(c.count(), 2);
    c.select_first();
    c.select("a.rs");
    assert_eq!(c.take_wants(), [("t".repeat(40), "a.rs".to_string())]);
    c.set_text(
        "a.rs".into(),
        Ok(Arc::new("x\n<<<<<<< m\no\n=======\nt\n>>>>>>> p\n".into())),
    );
    assert!(matches!(&c.text("a.rs"), Some(Ok(s)) if s.len() == 2));
    c.select("b.rs");
    c.select("a.rs");
    assert_eq!(c.take_wants().len(), 1, "a.rs is known, b.rs is new");
}

#[test]
fn a_failed_test_is_an_error_not_a_clean_merge() {
    let mut c = Conflicts::default();
    c.set_report(Err("git is too old".into()));
    assert_eq!(c.count(), 0);
    assert_eq!(c.error(), Some("git is too old"));
    assert!(c.paths().is_empty());
}
