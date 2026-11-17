use super::*;

fn file(path: &str, status: FileStatus, oid: &str) -> DiffFile {
    DiffFile {
        path: path.into(),
        old_path: None,
        status,
        additions: 1,
        deletions: 1,
        binary: false,
        counts_known: true,
        new_oid: oid.into(),
    }
}

fn sample() -> FileList {
    FileList::new(
        vec![
            file("Cargo.lock", FileStatus::Modified, "o0"),
            file("src/api/client.rs", FileStatus::Modified, "o1"),
            file("src/api/old.rs", FileStatus::Deleted, "0000"),
            file("src/lib.rs", FileStatus::Added, "o3"),
            file("web/dist/app.min.js", FileStatus::Modified, "o4"),
            file("web/index.ts", FileStatus::Renamed, "o5"),
        ],
        HashMap::new(),
    )
}

/// The sample in the flat list (the original list tests).
fn sample_list() -> FileList {
    let mut l = sample();
    l.set_view(ViewMode::List);
    l
}

#[test]
fn file_filter_matches_substring_and_status() {
    for mut l in [sample(), sample_list()] {
        l.filter = "API/".into();
        assert_eq!(l.visible(), vec![1, 2]);
        l.statuses.insert('D');
        assert_eq!(l.visible(), vec![2]);
        l.filter.clear();
        l.statuses.insert('A');
        assert_eq!(l.visible(), vec![2, 3]);
    }
    // a rename matches on its old path too
    let mut r = FileList::new(
        vec![DiffFile {
            old_path: Some("legacy/index.js".into()),
            ..file("web/index.ts", FileStatus::Renamed, "o")
        }],
        HashMap::new(),
    );
    r.filter = "legacy".into();
    assert_eq!(r.visible(), vec![0]);
}

#[test]
fn filter_with_no_matches_is_empty_and_navigation_is_a_no_op() {
    let mut l = sample();
    l.select(1);
    l.filter = "no-such-file".into();
    assert!(l.visible().is_empty());
    assert!(l.rows().is_empty(), "no directory rows without files");
    assert_eq!(l.generated_count(), 0);
    assert_eq!(l.select_next(), Some(1));
    assert_eq!(l.select_prev(), Some(1));
}

#[test]
fn viewed_resets_when_oid_changes() {
    let mut viewed = HashMap::new();
    viewed.insert("src/lib.rs".to_string(), "old-oid".to_string());
    viewed.insert("src/api/client.rs".to_string(), "o1".to_string());
    let mut l = FileList::new(sample().files().to_vec(), viewed);
    assert!(!l.is_viewed(3), "the file changed since it was viewed");
    assert!(l.is_viewed(1));
    assert_eq!(l.counter(), "1/6 viewed");
    let c = l.toggle_viewed(3);
    assert_eq!(
        c,
        ViewedChange {
            path: "src/lib.rs".into(),
            oid: "o3".into(),
            viewed: true
        }
    );
    assert!(l.is_viewed(3));
    let c = l.toggle_viewed(1);
    assert!(!c.viewed && !l.is_viewed(1));
    assert_eq!(l.counter(), "1/6 viewed");
}

#[test]
fn hide_viewed_and_generated_defaults() {
    let mut l = sample_list();
    assert!(!l.hide_viewed && !l.show_generated);
    assert!(l.is_generated(0) && l.is_generated(4) && !l.is_generated(1));
    assert_eq!(l.visible(), vec![1, 2, 3, 5]);
    assert_eq!(l.generated_count(), 2);
    assert_eq!(l.first_reviewable(), Some(1));
    l.show_generated = true;
    assert_eq!(l.visible(), vec![1, 2, 3, 5, 0, 4]);
    l.toggle_viewed(2);
    l.hide_viewed = true;
    assert_eq!(l.visible(), vec![1, 3, 5, 0, 4]);
}

#[test]
fn generated_group_row_precedes_expanded_generated_files() {
    let mut l = sample_list();
    let g = |expanded| ListRow::Generated { count: 2, expanded };
    use ListRow::File;
    assert_eq!(
        l.rows().to_vec(),
        vec![File(1), File(2), File(3), File(5), g(false)]
    );
    l.show_generated = true;
    assert_eq!(
        l.rows().to_vec(),
        vec![
            File(1),
            File(2),
            File(3),
            File(5),
            g(true),
            File(0),
            File(4)
        ]
    );
    l.filter = "client".into();
    assert_eq!(
        l.rows().to_vec(),
        vec![File(1)],
        "no group when no generated file matches"
    );
}

#[test]
fn jk_navigation_clamps_at_both_ends() {
    for mut l in [sample(), sample_list()] {
        assert_eq!(l.select_next(), Some(1), "no selection: first shown file");
        assert_eq!(l.select_next(), Some(2));
        assert_eq!(l.select_next(), Some(3));
        assert_eq!(l.select_next(), Some(5));
        assert_eq!(l.select_next(), Some(5), "clamped at the end");
        assert_eq!(l.select_prev(), Some(3));
        l.select(1);
        assert_eq!(l.select_prev(), Some(1), "clamped at the start");
        // Cargo.lock is generated, so collapsed (not shown): its display slot is after
        // the shown non-generated files, and `j` from it clamps to the last shown file
        l.select(0);
        assert_eq!(l.select_next(), Some(5));
        l.select(0);
        assert_eq!(l.select_prev(), Some(5));
    }
}

#[test]
fn select_from_a_hidden_selection_follows_display_order() {
    let mut l = sample_list();
    l.show_generated = true;
    l.toggle_viewed(0);
    l.hide_viewed = true;
    // display order is 1, 2, 3, 5, then generated 0, 4; Cargo.lock (0) is hidden
    assert_eq!(l.visible(), vec![1, 2, 3, 5, 4]);
    l.select(0);
    assert_eq!(l.select_next(), Some(4));
    l.select(0);
    assert_eq!(l.select_prev(), Some(5));
    // a hidden non-generated file sits between its index neighbours
    let mut m = sample_list();
    m.toggle_viewed(2);
    m.hide_viewed = true;
    m.select(2);
    assert_eq!(m.select_next(), Some(3));
    m.select(2);
    assert_eq!(m.select_prev(), Some(1));
}

#[test]
fn empty_file_list() {
    let mut l = FileList::new(vec![], HashMap::new());
    assert!(l.visible().is_empty());
    assert!(l.rows().is_empty());
    assert_eq!(l.counter(), "0/0 viewed");
    assert_eq!(l.first_reviewable(), None);
    assert_eq!(l.select_next(), None);
    assert_eq!(l.select_prev(), None);
    assert_eq!(l.selected(), None);
    l.nav(Nav::Down, 10);
    l.left();
    l.right();
    assert_eq!(l.cursor(), None);
}

#[test]
fn only_generated_files_open_the_first_one() {
    let l = FileList::new(
        vec![file("Cargo.lock", FileStatus::Modified, "o")],
        HashMap::new(),
    );
    assert_eq!(l.first_reviewable(), Some(0));
}

use prcore::comments::{Anchor, Comment, Thread};
use prcore::model::UserRef;

fn th(id: u64, path: Option<&str>, to: Option<u32>, resolved: bool) -> Thread {
    let t = jiff::Timestamp::from_second(1_700_000_000).unwrap();
    Thread {
        root: Comment {
            id: id.into(),
            parent_id: None,
            author: UserRef {
                id: "{a}".into(),
                display_name: "Ana".into(),
                login: None,
            },
            body: "b".into(),
            created_on: t,
            updated_on: t,
            deleted: false,
            resolved,
            anchor: path.map(|p| Anchor {
                path: p.into(),
                from: None,
                to,
                start: None,
                outdated: false,
                old_path: None,
                commit: None,
            }),
            author_is_bot: false,
            thread: Some(prcore::comments::ThreadId::from(id)),
            can_resolve: true,
        },
        replies: vec![],
    }
}

fn draft(kind: DraftKind, path: Option<&str>, parent: Option<u64>) -> Draft {
    Draft {
        id: 1,
        repo: prcore::model::RepoRef::parse_bitbucket("w/r").unwrap(),
        pr_id: 1,
        kind,
        path: path.map(Into::into),
        line: None,
        side: None,
        start: None,
        reply_to: parent.map(Into::into),
        text: "t".into(),
        source_commit: "c".into(),
        comment_type: None,
    }
}

#[test]
fn file_comment_counts() {
    let mut c = Comments::new();
    c.set(
        vec![
            th(1, Some("src/lib.rs"), Some(3), false),
            th(2, Some("src/lib.rs"), None, false),
            th(3, Some("src/lib.rs"), Some(9), true),
            th(4, Some("web/index.ts"), Some(1), false),
            th(5, None, None, false),
            th(6, Some("gone.rs"), Some(1), false),
        ],
        0,
    );
    let mut l = sample();
    c.set_files(l.files());
    assert_eq!(l.comment_count(3), 0, "nothing before comments load");
    l.set_marks(comment_marks(&c, &[], l.files()));
    assert_eq!(
        l.comment_count(3),
        2,
        "inline + file-level, resolved excluded"
    );
    assert_eq!(l.mark(3).resolved, 1);
    assert_eq!(l.comment_count(5), 1);
    assert_eq!(l.comment_count(1), 0);
    assert_eq!(
        c.unresolved(),
        5,
        "PR-level and unknown paths count in the total"
    );
    l.set_marks(HashMap::new());
    assert_eq!(l.comment_count(3), 0, "a refresh replaces the counts");
}

#[test]
fn comment_marks_count_old_paths_outdated_binary_and_drafts_but_not_unplaced() {
    let files = vec![
        DiffFile {
            old_path: Some("old/name.rs".into()),
            ..file("new/name.rs", FileStatus::Renamed, "o1")
        },
        DiffFile {
            binary: true,
            ..file("img/logo.png", FileStatus::Modified, "o2")
        },
        file("a.rs", FileStatus::Modified, "o3"),
    ];
    let mut c = Comments::new();
    c.set_files(&files);
    c.set(
        vec![
            th(1, Some("old/name.rs"), Some(2), false),
            th(2, Some("new/name.rs"), Some(2), true),
            th(3, Some("img/logo.png"), Some(1), false),
            th(4, Some("img/logo.png"), None, false),
            th(5, Some("not/in/diff.rs"), Some(1), false),
            th(6, None, None, false),
        ],
        0,
    );
    let drafts = vec![
        draft(DraftKind::Inline, Some("a.rs"), None),
        draft(DraftKind::File, Some("a.rs"), None),
        draft(DraftKind::Reply, None, Some(1)),
        draft(DraftKind::Reply, None, Some(5)),
        draft(DraftKind::Pr, None, None),
        draft(DraftKind::Inline, Some("not/in/diff.rs"), None),
    ];
    let m = comment_marks(&c, &drafts, &files);
    let renamed = m["new/name.rs"];
    assert_eq!(
        renamed,
        CommentMark {
            unresolved: 1,
            resolved: 1,
            drafts: 1,
            outdated: 1
        },
        "the old-path thread counts on the renamed file, as outdated; its reply draft too"
    );
    let bin = m["img/logo.png"];
    assert_eq!(
        (bin.unresolved, bin.outdated),
        (2, 1),
        "line threads of a binary"
    );
    assert_eq!(m["a.rs"].drafts, 2);
    assert_eq!(m["a.rs"].threads(), 0);
    assert!(
        !m.contains_key("not/in/diff.rs"),
        "unplaced belongs to no file"
    );
    assert_eq!(m.len(), 3);
    assert_eq!(bin.chip(), Some((2, true)));
    assert_eq!(
        CommentMark {
            resolved: 3,
            ..Default::default()
        }
        .chip(),
        Some((3, false)),
        "all resolved: grey, every thread"
    );
    assert_eq!(m["a.rs"].chip(), None, "drafts only: no bubble");
    assert_eq!(
        renamed.tooltip(),
        "1 unresolved, 1 resolved, 1 drafts (outdated: 1)"
    );
}

#[test]
fn flagged_line_threads_count_as_outdated_and_all_outdated_dims_the_chip() {
    let files = vec![file("a.rs", FileStatus::Modified, "o")];
    let mut c = Comments::new();
    c.set_files(&files);
    let mut flagged = th(1, Some("a.rs"), Some(3), false);
    flagged.root.anchor.as_mut().unwrap().outdated = true;
    c.set(vec![flagged, th(2, Some("a.rs"), Some(4), false)], 0);
    let m = comment_marks(&c, &[], &files)["a.rs"];
    assert_eq!((m.unresolved, m.outdated), (2, 1));
    assert!(!m.all_outdated(), "one current thread: normal chip");
    let only = CommentMark {
        resolved: 2,
        outdated: 2,
        ..Default::default()
    };
    assert!(only.all_outdated());
    assert!(!CommentMark::default().all_outdated(), "no threads");
}

/// Threads on paths that are not in the diff: one "not in diff" entry per path.
fn ghost_comments(files: &[DiffFile]) -> Comments {
    let mut c = Comments::new();
    c.set_files(files);
    let mut resolved = th(3, Some("docker-compose.yaml"), None, true);
    resolved.replies.push(Comment {
        id: 30.into(),
        parent_id: Some(3.into()),
        thread: Some(prcore::comments::ThreadId::from(30)),
        ..resolved.root.clone()
    });
    c.set(
        vec![
            th(1, Some("docker-compose.yaml"), Some(14), false),
            th(2, Some("build/module-bundles/modules.lock"), Some(3), false),
            resolved,
            th(4, Some("src/lib.rs"), Some(1), false),
            th(5, None, None, false),
        ],
        0,
    );
    c
}

#[test]
fn ghost_entries_group_threads_not_in_the_diff_by_path() {
    let l = sample();
    let c = ghost_comments(l.files());
    let drafts = vec![
        draft(DraftKind::Reply, None, Some(1)),
        draft(DraftKind::Reply, None, Some(4)),
    ];
    let g = ghost_entries(&c, &drafts);
    assert_eq!(
        g,
        vec![
            Ghost {
                path: "build/module-bundles/modules.lock".into(),
                mark: CommentMark {
                    unresolved: 1,
                    outdated: 1,
                    ..Default::default()
                },
            },
            Ghost {
                path: "docker-compose.yaml".into(),
                mark: CommentMark {
                    unresolved: 1,
                    resolved: 1,
                    drafts: 1,
                    outdated: 2,
                },
            },
        ],
        "sorted by path; every thread outdated; the reply draft counts on its thread's file"
    );
    assert!(g.iter().all(|x| x.mark.all_outdated()));
}

#[test]
fn ghost_rows_follow_the_files_and_are_selectable() {
    for view in [ViewMode::Tree, ViewMode::List] {
        let mut l = sample();
        l.set_view(view);
        let c = ghost_comments(l.files());
        assert!(!l.has_marks());
        l.set_ghosts(ghost_entries(&c, &[]));
        assert!(l.has_marks(), "the marker column shows for ghosts too");
        let rows = l.rows();
        let n = rows.len();
        assert_eq!(
            &rows[n - 3..],
            &[
                ListRow::GhostHeader { count: 2 },
                ListRow::Ghost(0),
                ListRow::Ghost(1)
            ],
            "{view:?}: after the files and the generated group"
        );
        assert_eq!(l.depth(ListRow::Ghost(0)), 0);
        // the filter text narrows them, a status chip hides them (they have no status)
        l.filter = "docker".into();
        assert_eq!(
            &l.rows()[..],
            &[ListRow::GhostHeader { count: 1 }, ListRow::Ghost(1)]
        );
        l.filter.clear();
        l.statuses.insert('M');
        assert!(!l.rows().iter().any(|r| matches!(r, ListRow::Ghost(_))));
        l.statuses.clear();
        // "Commented" keeps them: they carry threads
        l.commented_only = true;
        assert!(l.rows().contains(&ListRow::Ghost(0)));
        l.commented_only = false;

        // selecting a ghost leaves no file selected; selecting a file clears the ghost
        l.select(1);
        l.select_ghost(1);
        assert_eq!(
            (l.selected(), l.selected_ghost(), l.cursor()),
            (None, Some(1), Some(Cursor::Ghost(1)))
        );
        let rows = l.rows();
        assert_eq!(l.cursor_pos(&rows), Some(rows.len() - 1));
        // keyboard: up from the first ghost skips the header to the last file row
        l.select_ghost(0);
        l.nav(Nav::Up, 1);
        assert_eq!(l.selected_ghost(), None);
        assert!(l.selected().is_some());
        l.nav(Nav::End, 1);
        assert_eq!(l.selected_ghost(), Some(1), "End reaches the last ghost");
        l.select(3);
        assert_eq!(l.selected_ghost(), None);
        // j / k walk files only
        assert!(!l.nav_order().is_empty());
        l.select_ghost(5); // out of range: ignored
        assert_eq!(l.selected_ghost(), None);
        assert_eq!(l.ghosts()[1].path, "docker-compose.yaml");
    }
}

#[test]
fn rows_are_cached_until_an_input_changes() {
    use ListRow::File;
    let mut l = sample_list();
    let first = l.rows();
    assert_eq!(l.rows_built(), 1);
    for _ in 0..5 {
        assert_eq!(l.rows(), first);
        assert_eq!(l.visible(), vec![1, 2, 3, 5]);
    }
    assert_eq!(l.rows_built(), 1, "unchanged inputs: no recomputation");
    type Change = (&'static str, fn(&mut FileList), Vec<ListRow>);
    let changes: [Change; 5] = [
        (
            "filter",
            |l| l.filter = "API/".into(),
            vec![File(1), File(2)],
        ),
        (
            "statuses",
            |l| {
                l.statuses.insert('D');
            },
            vec![File(2)],
        ),
        (
            "viewed",
            |l| {
                l.toggle_viewed(2);
            },
            vec![File(2)],
        ),
        ("hide viewed", |l| l.hide_viewed = true, vec![]),
        (
            "show generated",
            |l| {
                l.filter.clear();
                l.statuses.clear();
                l.show_generated = true;
            },
            vec![
                File(1),
                File(3),
                File(5),
                ListRow::Generated {
                    count: 2,
                    expanded: true,
                },
                File(0),
                File(4),
            ],
        ),
    ];
    for (n, (what, change, want)) in changes.into_iter().enumerate() {
        change(&mut l);
        assert_eq!(l.rows().to_vec(), want, "{what}");
        assert_eq!(l.rows().to_vec(), want, "{what}");
        assert_eq!(l.rows_built(), n + 2, "{what}: recomputed exactly once");
    }
}

#[test]
fn tree_rows_are_invalidated_by_expansion_marks_commented_and_view() {
    let mut l = sample();
    l.rows();
    let built = l.rows_built();
    l.rows();
    assert_eq!(l.rows_built(), built);
    let src = dir(&l, "src");
    type Change = (&'static str, Box<dyn Fn(&mut FileList)>);
    let changes: Vec<Change> = vec![
        ("collapse", Box::new(move |l| l.toggle_dir(src))),
        ("expand all", Box::new(|l| l.expand_all())),
        ("collapse all", Box::new(|l| l.collapse_all())),
        (
            "marks",
            Box::new(|l| {
                let m = CommentMark {
                    unresolved: 1,
                    ..Default::default()
                };
                l.set_marks([("src/lib.rs".to_string(), m)].into_iter().collect())
            }),
        ),
        ("commented", Box::new(|l| l.commented_only = true)),
        ("view", Box::new(|l| l.set_view(ViewMode::List))),
    ];
    for (k, (what, change)) in changes.into_iter().enumerate() {
        change(&mut l);
        l.rows();
        l.rows();
        assert_eq!(l.rows_built(), built + k + 1, "{what}: recomputed once");
    }
    // the same marks again are no change
    let m = CommentMark {
        unresolved: 1,
        ..Default::default()
    };
    l.set_marks([("src/lib.rs".to_string(), m)].into_iter().collect());
    l.rows();
    assert_eq!(l.rows_built(), built + 6);
}

fn dir(l: &FileList, path: &str) -> usize {
    l.tree()
        .dirs
        .iter()
        .position(|d| d.path == path)
        .unwrap_or_else(|| panic!("no dir {path}: {:?}", l.tree().dirs))
}

fn names(t: &Tree, paths: &[&str]) -> Vec<String> {
    // pre-order dump: "d:name@depth" and "f:path@depth"
    fn walk(t: &Tree, d: usize, paths: &[&str], out: &mut Vec<String>) {
        for &c in &t.dirs[d].dirs {
            out.push(format!("d:{}@{}", t.dirs[c].name, t.dirs[c].depth));
            walk(t, c, paths, out);
        }
        for &f in &t.dirs[d].files {
            out.push(format!("f:{}@{}", paths[f], t.file_depth[f]));
        }
    }
    let mut out = Vec::new();
    walk(t, 0, paths, &mut out);
    out
}

#[test]
fn tree_nests_compacts_and_sorts_dirs_first_case_insensitively() {
    let paths = [
        "packages/readers/test/b.ts",
        "packages/readers/test/A.ts",
        "packages/verifiers/test/v.ts",
        "README.md",
        "a/b/c/deep.rs",
        "Zeta/x.rs",
        "alpha.rs",
        "docs/dir with space/naïve ünïcode.md",
        "x/test/one.rs",
        "y/test/two.rs",
    ];
    let t = Tree::build(&paths, &[true; 10]);
    assert_eq!(
        names(&t, &paths),
        vec![
            "d:a/b/c@0",
            "f:a/b/c/deep.rs@1",
            "d:docs/dir with space@0",
            "f:docs/dir with space/naïve ünïcode.md@1",
            "d:packages@0",
            "d:readers/test@1",
            "f:packages/readers/test/A.ts@2",
            "f:packages/readers/test/b.ts@2",
            "d:verifiers/test@1",
            "f:packages/verifiers/test/v.ts@2",
            "d:x/test@0",
            "f:x/test/one.rs@1",
            "d:y/test@0",
            "f:y/test/two.rs@1",
            "d:Zeta@0",
            "f:Zeta/x.rs@1",
            "f:alpha.rs@0",
            "f:README.md@0",
        ]
    );
    // "test" under x and under y are different directories
    let tests: Vec<&str> = t
        .dirs
        .iter()
        .filter(|d| d.name.ends_with("test"))
        .map(|d| d.path.as_str())
        .collect();
    assert_eq!(
        tests,
        [
            "packages/readers/test",
            "packages/verifiers/test",
            "x/test",
            "y/test"
        ]
    );
    // spans cover every file below, in display order
    let pk = t.dirs.iter().position(|d| d.path == "packages").unwrap();
    let below: Vec<&str> = t.dirs[pk].span.clone().map(|k| paths[t.order[k]]).collect();
    assert_eq!(below.len(), 3);
    assert_eq!(t.order.len(), 10);
    assert!(t
        .dirs
        .iter()
        .enumerate()
        .skip(1)
        .all(|(k, d)| d.parent.unwrap() < k));
    assert_eq!(file_name("a/b/c/deep.rs"), "deep.rs");
    assert_eq!(file_name("README.md"), "README.md");
}

#[test]
fn tree_of_a_single_file_and_of_root_files_only() {
    let t = Tree::build(&["src/only.rs"], &[true]);
    assert_eq!(
        names(&t, &["src/only.rs"]),
        vec!["d:src@0", "f:src/only.rs@1"]
    );
    let p = ["b.rs", "a.rs"];
    let t = Tree::build(&p, &[true, true]);
    assert_eq!(names(&t, &p), vec!["f:a.rs@0", "f:b.rs@0"]);
    assert_eq!(t.dirs.len(), 1, "the root only");
    let t = Tree::build(&p, &[false, true]);
    assert_eq!(t.order, vec![1], "excluded files are not in the tree");
    assert_eq!(t.file_dir[0], None);
}

fn nested() -> FileList {
    let mut f = vec![
        file("packages/readers/test/a.test.ts", FileStatus::Added, "1"),
        file("packages/readers/test/b.test.ts", FileStatus::Modified, "2"),
        file("packages/readers/src/index.ts", FileStatus::Modified, "3"),
        file(
            "packages/verifiers/test/v.test.ts",
            FileStatus::Deleted,
            "4",
        ),
        file("README.md", FileStatus::Modified, "5"),
        file("pnpm-lock.yaml", FileStatus::Modified, "6"),
    ];
    for (k, x) in f.iter_mut().enumerate() {
        x.additions = 10 * (k as u32 + 1);
        x.deletions = k as u32;
    }
    FileList::new(f, HashMap::new())
}

#[test]
fn tree_rows_with_directories_and_generated_group_last() {
    use ListRow::{Dir, File};
    let l = nested();
    let (pk, rd, rt, rs, vt) = (
        dir(&l, "packages"),
        dir(&l, "packages/readers"),
        dir(&l, "packages/readers/test"),
        dir(&l, "packages/readers/src"),
        dir(&l, "packages/verifiers/test"),
    );
    assert_eq!(
        l.rows().to_vec(),
        vec![
            Dir(pk),
            Dir(rd),
            Dir(rs),
            File(2),
            Dir(rt),
            File(0),
            File(1),
            Dir(vt),
            File(3),
            File(4),
            ListRow::Generated {
                count: 1,
                expanded: false
            },
        ]
    );
    assert_eq!(l.tree().dirs[vt].name, "verifiers/test", "compacted");
    assert_eq!(l.depth(File(0)), 3);
    assert_eq!(l.depth(Dir(rt)), 2);
    assert_eq!(l.depth(File(4)), 0);
    assert_eq!(l.nav_order(), vec![2, 0, 1, 3, 4]);
}

#[test]
fn dir_stats_aggregate_counts_viewed_and_marks_up_the_tree() {
    let mut l = nested();
    let mk = |u, r, d| CommentMark {
        unresolved: u,
        resolved: r,
        drafts: d,
        outdated: 0,
    };
    l.set_marks(
        [
            ("packages/readers/test/a.test.ts".to_string(), mk(2, 0, 0)),
            ("packages/readers/src/index.ts".to_string(), mk(0, 1, 1)),
            ("packages/verifiers/test/v.test.ts".to_string(), mk(1, 1, 0)),
        ]
        .into_iter()
        .collect(),
    );
    l.toggle_viewed(0);
    let st = l.dir_stats();
    let pk = st[dir(&l, "packages")];
    assert_eq!(pk.files, 4);
    assert_eq!(pk.additions, 10 + 20 + 30 + 40);
    assert_eq!(pk.deletions, 1 + 2 + 3);
    assert_eq!(pk.viewed, 1);
    assert_eq!(pk.marks, mk(3, 2, 1));
    assert_eq!(st[dir(&l, "packages/readers/test")].marks, mk(2, 0, 0));
    assert_eq!(st[0].files, 5, "the root holds every tree file");
    // cached until a viewed mark changes
    assert!(Arc::ptr_eq(&st, &l.dir_stats()));
    l.toggle_viewed(1);
    assert_eq!(l.dir_stats()[dir(&l, "packages")].viewed, 2);
}

#[test]
fn tri_state_viewed_and_batch_toggle() {
    let mut l = nested();
    let rt = dir(&l, "packages/readers/test");
    let pk = dir(&l, "packages");
    assert_eq!(l.dir_stats()[rt].tri(), Tri::Off);
    l.toggle_viewed(0);
    assert_eq!(l.dir_stats()[rt].tri(), Tri::Mixed);
    let ch = l.toggle_dir_viewed(rt);
    assert_eq!(ch.len(), 1, "only the file that changes");
    assert_eq!(ch[0].path, "packages/readers/test/b.test.ts");
    assert!(ch[0].viewed);
    assert_eq!(l.dir_stats()[rt].tri(), Tri::On);
    assert_eq!(l.dir_stats()[pk].tri(), Tri::Mixed);
    let ch = l.toggle_dir_viewed(rt);
    assert_eq!(ch.len(), 2);
    assert!(ch.iter().all(|c| !c.viewed));
    assert_eq!(l.dir_stats()[rt].tri(), Tri::Off);
    let ch = l.toggle_dir_viewed(pk);
    assert_eq!(ch.len(), 4);
    assert_eq!(l.dir_stats()[pk].tri(), Tri::On);
    assert!(!l.is_viewed(4), "files outside stay");
    assert_eq!(
        DirStats {
            files: 0,
            ..Default::default()
        }
        .tri(),
        Tri::Off
    );
}

#[test]
fn filter_keeps_ancestors_and_auto_expands() {
    use ListRow::{Dir, File};
    let mut l = nested();
    let pk = dir(&l, "packages");
    l.collapse_all();
    assert_eq!(l.rows().to_vec()[..2], [Dir(pk), File(4)]);
    l.filter = "b.test".into();
    let rd = dir(&l, "packages/readers");
    let rt = dir(&l, "packages/readers/test");
    assert_eq!(l.rows().to_vec(), vec![Dir(pk), Dir(rd), Dir(rt), File(1)]);
    assert!(l.dir_expanded(pk), "expanded while filtering");
    l.filter.clear();
    l.statuses.insert('D');
    let vt = dir(&l, "packages/verifiers/test");
    assert_eq!(l.rows().to_vec(), vec![Dir(pk), Dir(vt), File(3)]);
    l.statuses.clear();
    l.commented_only = true;
    assert!(l.rows().is_empty(), "nothing commented yet");
    l.set_marks(
        [(
            "README.md".to_string(),
            CommentMark {
                drafts: 1,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
    );
    assert_eq!(l.rows().to_vec(), vec![File(4)]);
}

#[test]
fn hide_viewed_removes_directories_left_empty() {
    use ListRow::{Dir, File};
    let mut l = nested();
    let vt = dir(&l, "packages/verifiers/test");
    l.toggle_viewed(3);
    l.hide_viewed = true;
    let rows = l.rows().to_vec();
    assert!(!rows.contains(&Dir(vt)), "{rows:?}");
    assert!(!rows.contains(&File(3)));
    let pk = dir(&l, "packages");
    l.toggle_dir_viewed(pk);
    assert_eq!(
        l.rows().to_vec(),
        vec![
            File(4),
            ListRow::Generated {
                count: 1,
                expanded: false
            }
        ]
    );
}

#[test]
fn selecting_a_file_expands_its_directories() {
    let mut l = nested();
    l.collapse_all();
    assert!(!l.visible().contains(&1));
    l.select(1);
    assert!(l.visible().contains(&1));
    assert!(l.dir_expanded(dir(&l, "packages/readers/test")));
    assert!(
        !l.dir_expanded(dir(&l, "packages/verifiers/test")),
        "other directories stay collapsed"
    );
    assert_eq!(l.cursor(), Some(Cursor::File(1)));
}

#[test]
fn jk_walks_across_collapsed_directories_and_expands_the_target() {
    let mut l = nested();
    l.collapse_all();
    assert_eq!(l.select_next(), Some(2), "into a collapsed directory");
    assert!(l.visible().contains(&2));
    assert_eq!(l.select_next(), Some(0));
    assert_eq!(l.select_next(), Some(1));
    assert_eq!(l.select_next(), Some(3));
    assert!(l.dir_expanded(dir(&l, "packages/verifiers/test")));
    assert_eq!(l.select_next(), Some(4));
    assert_eq!(l.select_next(), Some(4), "clamped");
    assert_eq!(l.select_prev(), Some(3));
    // the flat list walks by index
    l.set_view(ViewMode::List);
    l.select(0);
    assert_eq!(l.select_next(), Some(1));
    assert_eq!(l.select_next(), Some(2));
}

#[test]
fn arrows_walk_visible_rows_including_directories() {
    use ListRow::{Dir, File};
    let mut l = nested();
    let pk = dir(&l, "packages");
    l.nav(Nav::Down, 5);
    assert_eq!(l.cursor(), Some(Cursor::Dir(pk)), "first row");
    assert_eq!(l.selected(), None, "a directory selects no file");
    l.nav(Nav::End, 5);
    assert_eq!(
        l.cursor(),
        Some(Cursor::File(4)),
        "generated header skipped"
    );
    assert_eq!(l.selected(), Some(4));
    l.nav(Nav::Down, 5);
    assert_eq!(l.selected(), Some(4), "clamped");
    l.nav(Nav::Home, 5);
    assert_eq!(l.cursor(), Some(Cursor::Dir(pk)));
    l.nav(Nav::PageDown, 3);
    let rows = l.rows();
    assert_eq!(rows[3], File(2));
    assert_eq!(l.selected(), Some(2));
    l.nav(Nav::Up, 5);
    assert_eq!(
        l.cursor(),
        Some(Cursor::Dir(dir(&l, "packages/readers/src")))
    );
    l.nav(Nav::PageUp, 10);
    assert_eq!(l.cursor(), Some(Cursor::Dir(pk)));
    // collapsed directories hide their rows from the arrows
    l.toggle_dir(pk);
    l.nav(Nav::Down, 1);
    assert_eq!(l.cursor(), Some(Cursor::File(4)));
    assert_eq!(l.rows()[0], Dir(pk));
}

#[test]
fn left_and_right_collapse_expand_and_move_in_the_tree() {
    let mut l = nested();
    let (pk, rd, rs) = (
        dir(&l, "packages"),
        dir(&l, "packages/readers"),
        dir(&l, "packages/readers/src"),
    );
    l.focus_dir(pk);
    l.left();
    assert!(!l.dir_expanded(pk), "left collapses");
    l.left();
    assert_eq!(
        l.cursor(),
        Some(Cursor::Dir(pk)),
        "no parent row above the top"
    );
    l.right();
    assert!(l.dir_expanded(pk), "right expands");
    assert_eq!(l.cursor(), Some(Cursor::Dir(pk)));
    l.right();
    assert_eq!(
        l.cursor(),
        Some(Cursor::Dir(rd)),
        "then moves into the first child"
    );
    l.right();
    assert_eq!(l.cursor(), Some(Cursor::Dir(rs)));
    l.right();
    assert_eq!(l.cursor(), Some(Cursor::File(2)), "into its first file");
    assert_eq!(l.selected(), Some(2));
    l.right();
    assert_eq!(l.cursor(), Some(Cursor::File(2)), "nothing on a file");
    l.left();
    assert_eq!(l.cursor(), Some(Cursor::Dir(rs)), "file: to its directory");
    l.left();
    assert!(!l.dir_expanded(rs));
    l.left();
    assert_eq!(
        l.cursor(),
        Some(Cursor::Dir(rd)),
        "collapsed: to the parent"
    );
    // root-level file: no parent row
    l.select(4);
    l.left();
    assert_eq!(l.cursor(), Some(Cursor::File(4)));
    // the flat list ignores both
    l.set_view(ViewMode::List);
    l.select(0);
    l.left();
    l.right();
    assert_eq!(l.cursor(), Some(Cursor::File(0)));
}

#[test]
fn view_mode_round_trips_through_the_store_value() {
    assert_eq!(ViewMode::parse(None), ViewMode::Tree);
    assert_eq!(ViewMode::parse(Some("list")), ViewMode::List);
    assert_eq!(ViewMode::parse(Some("tree")), ViewMode::Tree);
    assert_eq!(ViewMode::parse(Some("bogus")), ViewMode::Tree);
    for v in [ViewMode::Tree, ViewMode::List] {
        assert_eq!(ViewMode::parse(Some(v.as_str())), v);
    }
    assert_eq!(sample().view(), ViewMode::Tree, "the tree by default");
}

#[test]
fn thousand_files_in_two_hundred_dirs_build_fast() {
    let files: Vec<DiffFile> = (0..1000)
        .map(|i| {
            file(
                &format!("pkg{:02}/mod{}/sub/file_{i:04}.rs", i % 20, (i / 20) % 10),
                FileStatus::Modified,
                "o",
            )
        })
        .collect();
    let t = std::time::Instant::now();
    let mut l = FileList::new(files, HashMap::new());
    let rows = l.rows();
    let st = l.dir_stats();
    assert!(l.tree().dirs.len() >= 200, "{}", l.tree().dirs.len());
    assert_eq!(st[0].files, 1000);
    assert_eq!(rows.len(), 1000 + l.tree().dirs.len() - 1);
    l.filter = "file_09".into();
    assert!(!l.rows().is_empty());
    l.collapse_all();
    l.toggle_viewed(5);
    l.rows();
    l.dir_stats();
    assert!(
        t.elapsed() < std::time::Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn status_letters() {
    assert_eq!(status_letter(FileStatus::Added), 'A');
    assert_eq!(status_letter(FileStatus::Modified), 'M');
    assert_eq!(status_letter(FileStatus::Deleted), 'D');
    assert_eq!(status_letter(FileStatus::Renamed), 'R');
}
