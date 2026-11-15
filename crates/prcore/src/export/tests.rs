use super::*;
use crate::comments::Anchor;
use crate::model::{PrState, RepoRef};
use chrono::{TimeZone, Utc};

fn user(n: &str) -> UserRef {
    UserRef {
        id: format!("{{{n}}}"),
        display_name: n.into(),
        login: None,
    }
}

fn pr() -> PullRequest {
    let t = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let repo = RepoRef::parse_bitbucket("acme/api").unwrap();
    PullRequest {
        repo: repo.clone(),
        id: 65,
        title: "Retry uploads on 5xx".into(),
        state: PrState::Open,
        draft: false,
        author: user("Ana Souza"),
        source_branch: "feature/retry".into(),
        dest_branch: "main".into(),
        source_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        dest_commit: "fedcba98".into(),
        source_repo: repo,
        comment_count: 0,
        task_count: 0,
        reviewers: vec![],
        created_on: t,
        updated_on: t,
        forge: Default::default(),
        account: Default::default(),
        web_url: String::new(),
        node_id: None,
        review_requested_from_me: false,
        locked: false,
    }
}

fn comment(id: u64, parent: Option<u64>, who: &str, body: &str) -> Comment {
    let t = Utc.timestamp_opt(1_700_000_000 + id as i64, 0).unwrap();
    Comment {
        id: id.into(),
        parent_id: parent.map(Into::into),
        author: user(who),
        body: body.into(),
        created_on: t,
        updated_on: t,
        deleted: false,
        resolved: false,
        anchor: None,
        author_is_bot: false,
        thread: Some(crate::comments::ThreadId::from(id)),
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

fn thread(root: Comment, replies: Vec<Comment>) -> Thread {
    Thread { root, replies }
}

fn draft(id: i64, kind: DraftKind, text: &str) -> Draft {
    Draft {
        id,
        repo: RepoRef::parse_bitbucket("acme/api").unwrap(),
        pr_id: 65,
        kind,
        path: None,
        line: None,
        side: None,
        start: None,
        reply_to: None,
        text: text.into(),
        source_commit: pr().source_commit,
        comment_type: None,
    }
}

fn row(kind: RowKind, old: Option<u32>, new: Option<u32>, text: &str) -> Row {
    Row {
        kind,
        old_line: old,
        new_line: new,
        text: text.into(),
    }
}

/// src/upload.rs: a hunk where line 11 changed and 12 was added.
fn upload_rows() -> Vec<Row> {
    vec![
        row(RowKind::HunkHeader, None, None, "@@ -9,4 +9,5 @@"),
        row(
            RowKind::Context,
            Some(9),
            Some(9),
            "fn upload(req: &Req) {\r",
        ),
        row(RowKind::Context, Some(10), Some(10), "    let n = 3;"),
        row(RowKind::Del, Some(11), None, "    retry(n);"),
        row(RowKind::Add, None, Some(11), "    retry(n * 2);"),
        row(RowKind::Add, None, Some(12), "    log(\"done ✓\");"),
        row(RowKind::Context, Some(12), Some(13), "}"),
    ]
}

/// docs/USAGE.md: a line holding a markdown fence.
fn doc_rows() -> Vec<Row> {
    vec![
        row(RowKind::HunkHeader, None, None, "@@ -1,2 +1,3 @@"),
        row(RowKind::Add, None, Some(1), "````sh"),
        row(RowKind::Add, None, Some(2), "nit --demo"),
    ]
}

struct Fixture {
    threads: Vec<Thread>,
    drafts: Vec<Draft>,
    upload: Vec<Row>,
    doc: Vec<Row>,
    paths: HashSet<String>,
}

fn fixture() -> Fixture {
    let mut resolved = comment(4, None, "Bo Chen", "**praise:** Nice cleanup.");
    resolved.anchor = at("src/upload.rs", None, Some(9));
    resolved.resolved = true;
    let mut old_side = comment(5, None, "Ana Souza", "Why was this removed?");
    old_side.anchor = at("src/upload.rs", Some(11), None);
    let mut outdated = comment(6, None, "Ana Souza", "**issue:** This leaks the handle.");
    outdated.anchor = at("src/upload.rs", None, Some(40)).map(|a| Anchor {
        outdated: true,
        ..a
    });
    let mut ghost = comment(7, None, "Renovate Bot", "Lock file regenerated.");
    ghost.anchor = at("Cargo.lock", None, Some(3));
    ghost.author_is_bot = true;
    let mut file_level = comment(8, None, "Bo Chen", "Please add a module doc.");
    file_level.anchor = at("src/upload.rs", None, None);
    let mut fence = comment(
        9,
        None,
        "Bo Chen",
        "**suggestion:** Use a longer fence:\n\n```sh\nnit --demo\n```\nand `code` ``ticks``.",
    );
    fence.anchor = at("docs/USAGE.md", None, Some(2));
    let mut new_line = comment(
        10,
        None,
        "Zoë Ørsted",
        "**nit:** Ce n'est pas très clair — 名前を変えて?\nSecond line.\n\n  indented third.",
    );
    new_line.anchor = at("src/upload.rs", None, Some(12));
    let mut deleted = comment(11, Some(10), "Bo Chen", "gone");
    deleted.deleted = true;
    let threads = vec![
        thread(
            comment(
                1,
                None,
                "Ana Souza",
                "**question:** Is the retry count configurable?",
            ),
            vec![comment(2, Some(1), "Bo Chen", "Not yet.")],
        ),
        thread(resolved, vec![]),
        thread(old_side, vec![]),
        thread(outdated, vec![]),
        thread(ghost, vec![]),
        thread(file_level, vec![]),
        thread(fence, vec![]),
        thread(
            new_line,
            vec![
                deleted,
                comment(12, Some(10), "Ana Souza", "**nit:** Agreed."),
            ],
        ),
    ];
    let mut inline = draft(21, DraftKind::Inline, "Name this constant.");
    inline.path = Some("src/upload.rs".into());
    inline.side = Some(Side::New);
    inline.line = Some(11);
    inline.comment_type = Some(CommentType::Issue);
    let mut reply = draft(22, DraftKind::Reply, "Thanks!\nFollow-up in #66.");
    reply.reply_to = Some(ThreadId::from(10));
    let mut reply_resolved = draft(23, DraftKind::Reply, "Agreed, resolved.");
    reply_resolved.reply_to = Some(ThreadId::from(4));
    let mut orphan = draft(24, DraftKind::Reply, "Reply to a thread that is gone.");
    orphan.reply_to = Some(ThreadId::from(999));
    let mut pr_level = draft(25, DraftKind::Pr, "Looks good overall.");
    pr_level.comment_type = Some(CommentType::Praise);
    let mut old_inline = draft(26, DraftKind::Inline, "Written on an older push.");
    old_inline.path = Some("src/upload.rs".into());
    old_inline.side = Some(Side::Old);
    old_inline.line = Some(10);
    old_inline.source_commit = "aaaaaaaaaaaa".into();
    let paths = ["src/upload.rs", "docs/USAGE.md"]
        .into_iter()
        .map(String::from)
        .collect();
    Fixture {
        threads,
        drafts: vec![pr_level, old_inline, orphan, reply_resolved, reply, inline],
        upload: upload_rows(),
        doc: doc_rows(),
        paths,
    }
}

fn run(f: &Fixture, scope: ExportScope, loaded: bool) -> String {
    let me = user("Me Myself");
    let mut patches: HashMap<String, &[Row]> = HashMap::new();
    if loaded {
        patches.insert("src/upload.rs".into(), &f.upload);
        patches.insert("docs/USAGE.md".into(), &f.doc);
    }
    let pr = pr();
    export_markdown(
        &ExportInput {
            pr: &pr,
            threads: &f.threads,
            drafts: &f.drafts,
            me: Some(&me),
            diff_paths: Some(&f.paths),
            patches: &patches,
        },
        scope,
    )
}

/// Compares with `testdata/export/<name>.md`; `UPDATE_GOLDEN=1` rewrites the file.
fn golden(name: &str, got: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/export")
        .join(format!("{name}.md"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
    }
    // a checkout may turn the file's line endings into CRLF
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n");
    assert_eq!(
        got, want,
        "golden {name} differs; UPDATE_GOLDEN=1 rewrites it"
    );
}

#[test]
fn golden_unresolved_with_patches() {
    golden(
        "unresolved",
        &run(&fixture(), ExportScope::Unresolved, true),
    );
}

#[test]
fn golden_everything_with_patches() {
    golden(
        "everything",
        &run(&fixture(), ExportScope::Everything, true),
    );
}

#[test]
fn golden_my_drafts_without_patches() {
    golden("my-drafts", &run(&fixture(), ExportScope::MyDrafts, false));
}

#[test]
fn golden_empty() {
    let f = Fixture {
        threads: vec![],
        drafts: vec![],
        ..fixture()
    };
    golden("empty", &run(&f, ExportScope::Everything, true));
}

#[test]
fn export_is_deterministic_whatever_the_input_order() {
    let f = fixture();
    let a = run(&f, ExportScope::Everything, true);
    let mut g = fixture();
    g.threads.reverse();
    g.drafts.reverse();
    assert_eq!(a, run(&g, ExportScope::Everything, true));
    assert_eq!(a, run(&f, ExportScope::Everything, true));
}

#[test]
fn scopes_pick_their_items() {
    let f = fixture();
    let count = |s| {
        run(&f, s, false)
            .lines()
            .filter(|l| l.starts_with("## "))
            .count()
    };
    // 6 drafts, of which the reply to thread 10 sits under that thread when shown
    assert_eq!(count(ExportScope::MyDrafts), 6);
    // 7 unresolved threads + 5 drafts not attached to one of them
    assert_eq!(count(ExportScope::Unresolved), 12);
    // + the resolved thread, which now holds its reply draft
    assert_eq!(count(ExportScope::Everything), 12);
    assert!(run(&f, ExportScope::Unresolved, false).contains("- Items: 12\n"));
}

#[test]
fn code_spans_and_fences_outgrow_backticks() {
    assert_eq!(code("a.rs"), "`a.rs`");
    assert_eq!(code("we`ird"), "``we`ird``");
    assert_eq!(code("`x`"), "`` `x` ``");
    let mut out = String::new();
    excerpt(&mut out, &doc_rows(), Side::New, 2);
    assert!(
        out.starts_with("`````diff\n+````sh\n+nit --demo\n`````\n"),
        "{out}"
    );
    let mut out = String::new();
    excerpt(&mut out, &upload_rows(), Side::Old, 11);
    assert_eq!(
        out, "```diff\n fn upload(req: &Req) {\n     let n = 3;\n-    retry(n);\n```\n\n",
        "two context rows, CR dropped"
    );
    // the first row of a hunk has no context; a missing line gives nothing
    let mut out = String::new();
    excerpt(&mut out, &upload_rows(), Side::New, 9);
    assert_eq!(out, "```diff\n fn upload(req: &Req) {\n```\n\n");
    let mut out = String::new();
    excerpt(&mut out, &upload_rows(), Side::New, 500);
    assert!(out.is_empty());
}

#[test]
fn comment_fences_stay_inside_their_quote() {
    let md = run(&fixture(), ExportScope::Unresolved, true);
    for l in md
        .lines()
        .filter(|l| l.contains("nit --demo") && !l.starts_with('+'))
    {
        assert!(l.starts_with("> "), "{l:?}");
    }
}
