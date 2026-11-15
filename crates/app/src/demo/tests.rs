use super::*;
use crate::inbox::{build_inbox, stack_advice, View};

#[test]
fn demo_comments_cover_every_placement() {
    use crate::review::comments_ui::place;
    let f = review_fixture_with(6);
    let patch = f.git(&["diff", "main...feature/demo", "--", DEMO_COMMENTS_FILE]);
    let rows = prcore::diff::parse_patch(&patch);
    let threads = prcore::comments::build_threads(review_comments().comments);
    let ids: Vec<usize> = (0..threads.len())
        .filter(|&i| {
            threads[i]
                .root
                .anchor
                .as_ref()
                .is_some_and(|a| a.path == DEMO_COMMENTS_FILE)
        })
        .collect();
    let p = place(&rows, &threads, &ids);
    let root = |i: usize| threads[i].root.id.clone();
    let inline: Vec<(u64, prcore::diff::RowKind)> = p
        .inline
        .iter()
        .flat_map(|(r, ts)| ts.iter().map(move |&t| (t, *r)))
        .map(|(t, r)| (root(t).as_u64().unwrap(), rows[r].kind))
        .collect();
    use prcore::diff::RowKind::*;
    let mut inline = inline;
    inline.sort_by_key(|x| x.0);
    assert_eq!(inline, vec![(1, Add), (4, Context), (8, Del)], "{rows:?}");
    // 21 is flagged by Bitbucket: outdated although its line is shown
    let mut outdated: Vec<u64> = p
        .outdated
        .iter()
        .map(|&t| root(t).as_u64().unwrap())
        .collect();
    outdated.sort_unstable();
    assert_eq!(outdated, [9, 21]);
    assert!(rows.iter().any(|r| r.new_line == Some(8)));
    assert_eq!(
        p.file_level
            .iter()
            .map(|&t| root(t).as_u64().unwrap())
            .collect::<Vec<_>>(),
        [7]
    );
    assert!(threads.iter().any(|t| t.root.anchor.is_none()), "PR-level");
    let resolved = threads.iter().find(|t| t.root.id == 4.into()).unwrap();
    assert!(resolved.root.resolved && resolved.replies.iter().any(|r| r.deleted));
    assert_eq!(
        threads
            .iter()
            .find(|t| t.root.id == 1.into())
            .unwrap()
            .replies
            .len(),
        2
    );
}
use prcore::advice::Advice;
use prcore::model::PrState;
use std::collections::HashSet;
use std::mem::discriminant;

#[test]
fn demo_app_review_has_142_files_in_nested_dirs_with_comments_on_them() {
    let extra = demo_tree_files();
    assert_eq!(DEMO_REVIEW_FILES + extra.len(), 142);
    let f = review_fixture_demo();
    let status = f.git(&["diff", "--name-status", "-M", "main...feature/demo"]);
    assert_eq!(status.lines().count(), 142, "{status}");
    assert!(status.contains("packages/engine/test/catalogue.test.ts"));
    assert!(status
        .lines()
        .any(|l| l.starts_with('M') && l.ends_with("README.md")));
    let paths: Vec<&str> = extra.iter().map(|(p, _)| p.as_str()).collect();
    for c in review_comments_demo().comments {
        if c.id > 100.into() {
            let p = c.anchor.unwrap().path;
            assert!(paths.contains(&p.as_str()), "{p}");
        }
    }
}

#[test]
fn review_fixture_has_every_change_kind() {
    let f = review_fixture();
    assert_eq!(f.git(&["rev-parse", "feature/demo"]), f.head);
    let status = f.git(&["diff", "--name-status", "-M", "main...feature/demo"]);
    let lines: Vec<&str> = status.lines().collect();
    assert_eq!(lines.len(), DEMO_REVIEW_FILES, "{status}");
    for kind in ['A', 'D', 'M', 'R'] {
        assert!(
            lines.iter().any(|l| l.starts_with(kind)),
            "{kind}: {status}"
        );
    }
    let numstat = f.git(&["diff", "--numstat", "main...feature/demo", "--", "assets"]);
    assert!(numstat.starts_with("-\t-\t"), "binary: {numstat}");
    assert!(f.url.starts_with("file:///"));
    assert_eq!(f.pr.source_commit, f.head);
    assert!(!f.pr.is_fork());
}

#[test]
fn demo_data_contains_every_scenario() {
    let (me, prs, stats) = demo_data();
    assert!(prs.len() >= 600, "prs: {}", prs.len());
    let repos: HashSet<_> = prs.iter().map(|p| p.repo.clone()).collect();
    assert!(repos.len() >= 15, "repos: {}", repos.len());

    let inbox = build_inbox(&prs, &stats, &me, View::All, "");
    assert!(inbox.stacks.len() >= 5, "stacks: {}", inbox.stacks.len());
    assert!(
        inbox.stacks.iter().any(|s| s.members.len() >= 3),
        "3-deep stack"
    );

    assert!(prs.iter().filter(|p| p.is_fork()).count() >= 1, "forks");
    assert!(prs.iter().any(|p| p.draft), "drafts");
    assert!(prs.iter().any(|p| !p.title.is_ascii()), "unicode titles");
    assert!(prs.iter().any(|p| p.author == me), "own PRs");
    assert!(
        inbox
            .rows
            .iter()
            .any(|r| r.pr.reviewers.iter().any(|x| x.user == me)),
        "needs-me PRs"
    );
    assert!(
        prs.iter().any(|p| p.state == PrState::Merged),
        "merged parent"
    );

    let too_big: Vec<_> = stats
        .values()
        .filter(|d| d.exceeds_web_ui(prcore::model::Forge::BitbucketCloud))
        .collect();
    assert!(too_big.len() >= 3, "too-big: {}", too_big.len());
    assert!(
        too_big.iter().any(|d| d.total_lines() > 8000),
        "over 8000 lines"
    );
    assert!(too_big.iter().any(|d| d.files > 200), "over 200 files");
    assert!(
        too_big.iter().any(|d| d.max_file_lines > 2000),
        "file over 2000 lines"
    );

    let all: Vec<Advice> = (0..inbox.stacks.len())
        .flat_map(|i| stack_advice(&inbox, &prs, &stats, i))
        .collect();
    let probes = [
        Advice::ReviewBottomUp,
        Advice::ParentChanged {
            parent: 0,
            child: 0,
        },
        Advice::ParentMergedRetarget {
            parent: 0,
            child: 0,
        },
        Advice::HoldChildApproval {
            parent: 0,
            child: 0,
        },
        Advice::MergeOrderViolation {
            parent: 0,
            child: 0,
        },
        Advice::ReviewablePerPr,
    ];
    for probe in probes {
        assert!(
            all.iter().any(|a| discriminant(a) == discriminant(&probe)),
            "missing advice {probe:?}"
        );
    }
}
