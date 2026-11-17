use super::*;
use crate::model::UserRef;
use jiff::Timestamp;

fn repo(name: &str) -> RepoRef {
    RepoRef::parse_bitbucket(name).unwrap()
}

fn pr(repo_name: &str, id: u64, src: &str, dst: &str, state: PrState) -> PullRequest {
    let t = Timestamp::from_second(1_700_000_000 + id as i64).unwrap();
    PullRequest {
        repo: repo(repo_name),
        id,
        title: format!("pr {id}"),
        state,
        draft: false,
        author: UserRef {
            id: "{u}".into(),
            display_name: "U".into(),
            login: None,
        },
        source_branch: src.into(),
        dest_branch: dst.into(),
        source_commit: "a".into(),
        dest_commit: "b".into(),
        source_repo: repo(repo_name),
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
        checks: Default::default(),
        has_conflicts: false,
        new_commits: false,
    }
}

use PrState::*;

#[test]
fn chain_of_three_is_ordered_base_first() {
    let prs = vec![
        pr("acme/api", 3, "c", "b", Open),
        pr("acme/api", 1, "a", "main", Open),
        pr("acme/api", 2, "b", "a", Open),
    ];
    let stacks = detect_stacks(&prs);
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].repo, repo("acme/api"));
    assert_eq!(stacks[0].members, vec![1, 2, 3]);
    assert_eq!(stacks[0].edges, vec![(1, 2), (2, 3)]);
}

#[test]
fn tree_with_two_children() {
    let prs = vec![
        pr("acme/api", 1, "a", "main", Open),
        pr("acme/api", 5, "x", "a", Open),
        pr("acme/api", 2, "y", "a", Open),
    ];
    let stacks = detect_stacks(&prs);
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].members, vec![1, 2, 5]);
    assert_eq!(stacks[0].edges, vec![(1, 2), (1, 5)]);
}

#[test]
fn lone_pr_is_not_a_stack() {
    assert!(detect_stacks(&[pr("acme/api", 1, "a", "main", Open)]).is_empty());
}

#[test]
fn prs_in_different_repos_never_link() {
    let prs = vec![
        pr("acme/api", 1, "a", "main", Open),
        pr("acme/web", 2, "b", "a", Open),
    ];
    assert!(detect_stacks(&prs).is_empty());
}

#[test]
fn fork_pr_is_excluded() {
    let mut child = pr("acme/api", 2, "b", "a", Open);
    child.source_repo = repo("someone/api");
    let prs = vec![pr("acme/api", 1, "a", "main", Open), child];
    assert!(detect_stacks(&prs).is_empty());
}

#[test]
fn declined_and_superseded_are_excluded() {
    let prs = vec![
        pr("acme/api", 1, "a", "main", Declined),
        pr("acme/api", 2, "b", "a", Open),
        pr("acme/api", 3, "c", "b", Superseded),
    ];
    assert!(detect_stacks(&prs).is_empty());
}

#[test]
fn merged_parent_with_open_child_is_a_stack() {
    let prs = vec![
        pr("acme/api", 1, "a", "main", Merged),
        pr("acme/api", 2, "b", "a", Open),
    ];
    let stacks = detect_stacks(&prs);
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].members, vec![1, 2]);
    assert_eq!(stacks[0].edges, vec![(1, 2)]);
}

#[test]
fn merged_child_does_not_link() {
    let prs = vec![
        pr("acme/api", 1, "a", "main", Open),
        pr("acme/api", 2, "b", "a", Merged),
    ];
    assert!(detect_stacks(&prs).is_empty());
}

#[test]
fn self_targeting_pr_is_ignored() {
    assert!(detect_stacks(&[pr("acme/api", 1, "a", "a", Open)]).is_empty());
}

#[test]
fn two_pr_cycle_is_dropped_not_looped() {
    let prs = vec![
        pr("acme/api", 1, "a", "b", Open),
        pr("acme/api", 2, "b", "a", Open),
    ];
    assert!(detect_stacks(&prs).is_empty());
}

#[test]
fn same_source_branch_prefers_open_parent() {
    let prs = vec![
        pr("acme/api", 1, "a", "main", Merged),
        pr("acme/api", 2, "a", "main", Open),
        pr("acme/api", 3, "c", "a", Open),
    ];
    let stacks = detect_stacks(&prs);
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].members, vec![2, 3]);
    assert_eq!(stacks[0].edges, vec![(2, 3)]);
}

#[test]
fn stacks_are_ordered_by_repo_then_first_member() {
    let prs = vec![
        pr("acme/web", 1, "a", "main", Open),
        pr("acme/web", 2, "b", "a", Open),
        pr("acme/api", 9, "p", "main", Open),
        pr("acme/api", 10, "q", "p", Open),
        pr("acme/api", 4, "m", "main", Open),
        pr("acme/api", 5, "n", "m", Open),
    ];
    let firsts: Vec<(String, u64)> = detect_stacks(&prs)
        .iter()
        .map(|s| (s.repo.full_name(), s.members[0]))
        .collect();
    assert_eq!(
        firsts,
        vec![
            ("acme/api".into(), 4),
            ("acme/api".into(), 9),
            ("acme/web".into(), 1)
        ]
    );
}
