use std::collections::HashMap;

use crate::model::{web_ui_limits, DiffStat, PrState, PullRequest, ReviewState};
use crate::stacks::Stack;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Advice {
    ReviewBottomUp,
    ParentChanged { parent: u64, child: u64 },
    ParentMergedRetarget { parent: u64, child: u64 },
    HoldChildApproval { parent: u64, child: u64 },
    MergeOrderViolation { parent: u64, child: u64 },
    ReviewablePerPr,
}

/// Advice for one stack. `stats` is keyed by PR id within the stack's repo.
///
/// Output order: `ReviewBottomUp` first (2+ Open members), then, for each edge in
/// `stack.edges` order, the applicable kinds in this fixed order: `ParentChanged`,
/// `ParentMergedRetarget`, `HoldChildApproval`, `MergeOrderViolation`; finally
/// `ReviewablePerPr` (a stack-level rule). Every per-edge rule requires the child to
/// be Open. Edges whose PRs are missing from `prs` are skipped. `ReviewablePerPr`
/// considers Open members only and needs stats for each of them.
pub fn advise(stack: &Stack, prs: &[PullRequest], stats: &HashMap<u64, DiffStat>) -> Vec<Advice> {
    let find = |id: u64| prs.iter().find(|p| p.repo == stack.repo && p.id == id);
    let has = |p: &PullRequest, s: ReviewState| p.reviewers.iter().any(|r| r.state == s);

    let open: Vec<&PullRequest> = stack
        .members
        .iter()
        .filter_map(|&id| find(id))
        .filter(|p| p.state == PrState::Open)
        .collect();

    let mut out = Vec::new();
    if open.len() >= 2 {
        out.push(Advice::ReviewBottomUp);
    }

    for &(pid, cid) in &stack.edges {
        let (Some(parent), Some(child)) = (find(pid), find(cid)) else {
            continue;
        };
        if child.state != PrState::Open {
            continue;
        }
        let (parent_id, child_id) = (pid, cid);
        match parent.state {
            PrState::Open => {
                // An empty hash means the provider did not report it: unknown, not changed.
                let known = !child.dest_commit.is_empty() && !parent.source_commit.is_empty();
                if known && child.dest_commit != parent.source_commit {
                    out.push(Advice::ParentChanged {
                        parent: parent_id,
                        child: child_id,
                    });
                }
                if has(parent, ReviewState::ChangesRequested) || parent.task_count > 0 {
                    out.push(Advice::HoldChildApproval {
                        parent: parent_id,
                        child: child_id,
                    });
                }
                if has(child, ReviewState::Approved) && !has(parent, ReviewState::Approved) {
                    out.push(Advice::MergeOrderViolation {
                        parent: parent_id,
                        child: child_id,
                    });
                }
            }
            PrState::Merged => {
                out.push(Advice::ParentMergedRetarget {
                    parent: parent_id,
                    child: child_id,
                });
            }
            _ => {}
        }
    }

    let member_stats: Option<Vec<&DiffStat>> = open.iter().map(|p| stats.get(&p.id)).collect();
    if let Some(ms) = member_stats {
        let lines: u32 = ms.iter().map(|s| s.total_lines()).sum();
        let files: u32 = ms.iter().map(|s| s.files).sum();
        // One stack lives in one repo, so its PRs share one forge.
        let forge = open.first().map(|p| p.forge).unwrap_or_default();
        let limits = web_ui_limits(forge);
        let none_exceed = ms.iter().all(|s| !s.exceeds_web_ui(forge));
        let combined_over = limits.max_total_lines.is_some_and(|m| lines > m)
            || limits.max_files.is_some_and(|m| files > m);
        if !ms.is_empty() && none_exceed && combined_over {
            out.push(Advice::ReviewablePerPr);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RepoRef, Reviewer, UserRef};
    use chrono::{TimeZone, Utc};
    use Advice::*;

    fn repo() -> RepoRef {
        RepoRef::parse_bitbucket("acme/api").unwrap()
    }

    fn user() -> UserRef {
        UserRef {
            id: "{u}".into(),
            display_name: "U".into(),
            login: None,
        }
    }

    fn pr(id: u64, state: PrState) -> PullRequest {
        let t = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        PullRequest {
            repo: repo(),
            id,
            title: format!("pr {id}"),
            state,
            draft: false,
            author: user(),
            source_branch: format!("b{id}"),
            dest_branch: "main".into(),
            source_commit: format!("s{id}"),
            dest_commit: "main-head".into(),
            source_repo: repo(),
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

    fn with_review(mut p: PullRequest, state: ReviewState) -> PullRequest {
        p.reviewers.push(Reviewer {
            user: user(),
            state,
        });
        p
    }

    /// Parent 1 and child 2, child based on the parent's head (no ParentChanged).
    fn pair(parent_state: PrState) -> (PullRequest, PullRequest) {
        let parent = pr(1, parent_state);
        let mut child = pr(2, PrState::Open);
        child.dest_commit = parent.source_commit.clone();
        (parent, child)
    }

    fn stack(members: &[u64], edges: &[(u64, u64)]) -> Stack {
        Stack {
            repo: repo(),
            members: members.to_vec(),
            edges: edges.to_vec(),
        }
    }

    fn advise_pair(parent: PullRequest, child: PullRequest) -> Vec<Advice> {
        advise(
            &stack(&[1, 2], &[(1, 2)]),
            &[parent, child],
            &HashMap::new(),
        )
    }

    fn stat(files: u32, added: u32, removed: u32, max: u32) -> DiffStat {
        DiffStat {
            files,
            lines_added: added,
            lines_removed: removed,
            max_file_lines: max,
        }
    }

    #[test]
    fn parent_changed_when_child_base_differs_from_parent_head() {
        let (parent, mut child) = pair(PrState::Open);
        child.dest_commit = "old-parent-head".into();
        assert_eq!(
            advise_pair(parent, child),
            vec![
                ReviewBottomUp,
                ParentChanged {
                    parent: 1,
                    child: 2
                }
            ]
        );
    }

    #[test]
    fn parent_not_changed_when_commits_match() {
        let (parent, child) = pair(PrState::Open);
        assert_eq!(advise_pair(parent, child), vec![ReviewBottomUp]);
    }

    #[test]
    fn unknown_commit_hash_is_not_reported_as_parent_changed() {
        let (mut parent, child) = pair(PrState::Open);
        parent.source_commit = String::new();
        assert_eq!(advise_pair(parent, child), vec![ReviewBottomUp]);
        let (parent, mut child) = pair(PrState::Open);
        child.dest_commit = String::new();
        assert_eq!(advise_pair(parent, child), vec![ReviewBottomUp]);
    }

    #[test]
    fn merged_parent_suggests_retarget() {
        let (parent, child) = pair(PrState::Merged);
        assert_eq!(
            advise_pair(parent, child),
            vec![ParentMergedRetarget {
                parent: 1,
                child: 2
            }]
        );
    }

    #[test]
    fn changes_requested_parent_holds_child() {
        let (parent, child) = pair(PrState::Open);
        let parent = with_review(parent, ReviewState::ChangesRequested);
        assert_eq!(
            advise_pair(parent, child),
            vec![
                ReviewBottomUp,
                HoldChildApproval {
                    parent: 1,
                    child: 2
                }
            ]
        );
    }

    #[test]
    fn open_tasks_on_parent_hold_child() {
        let (mut parent, child) = pair(PrState::Open);
        parent.task_count = 2;
        assert_eq!(
            advise_pair(parent, child),
            vec![
                ReviewBottomUp,
                HoldChildApproval {
                    parent: 1,
                    child: 2
                }
            ]
        );
    }

    #[test]
    fn approved_child_with_unapproved_parent_is_merge_order_violation() {
        let (parent, child) = pair(PrState::Open);
        let parent = with_review(parent, ReviewState::Pending);
        let child = with_review(child, ReviewState::Approved);
        assert_eq!(
            advise_pair(parent, child),
            vec![
                ReviewBottomUp,
                MergeOrderViolation {
                    parent: 1,
                    child: 2
                }
            ]
        );
    }

    #[test]
    fn approved_child_with_approved_parent_is_fine() {
        let (parent, child) = pair(PrState::Open);
        let parent = with_review(parent, ReviewState::Approved);
        let child = with_review(child, ReviewState::Approved);
        assert_eq!(advise_pair(parent, child), vec![ReviewBottomUp]);
    }

    #[test]
    fn reviewable_per_pr_when_sum_over_limit_but_each_under() {
        let (parent, child) = pair(PrState::Open);
        let stats = HashMap::from([
            (1, stat(10, 3001, 1000, 100)),
            (2, stat(10, 3001, 1000, 100)),
        ]);
        assert_eq!(
            advise(&stack(&[1, 2], &[(1, 2)]), &[parent, child], &stats),
            vec![ReviewBottomUp, ReviewablePerPr]
        );
    }

    #[test]
    fn reviewable_per_pr_when_summed_files_over_limit() {
        let (parent, child) = pair(PrState::Open);
        let stats = HashMap::from([(1, stat(100, 10, 10, 10)), (2, stat(101, 10, 10, 10))]);
        assert_eq!(
            advise(&stack(&[1, 2], &[(1, 2)]), &[parent, child], &stats),
            vec![ReviewBottomUp, ReviewablePerPr]
        );
    }

    #[test]
    fn no_reviewable_advice_when_a_member_exceeds_limits() {
        let (parent, child) = pair(PrState::Open);
        let stats = HashMap::from([(1, stat(10, 9000, 0, 100)), (2, stat(10, 3001, 1000, 100))]);
        assert_eq!(
            advise(&stack(&[1, 2], &[(1, 2)]), &[parent, child], &stats),
            vec![ReviewBottomUp]
        );
    }

    #[test]
    fn no_reviewable_advice_when_stats_missing() {
        let (parent, child) = pair(PrState::Open);
        let stats = HashMap::from([(1, stat(10, 7000, 0, 100))]);
        assert_eq!(
            advise(&stack(&[1, 2], &[(1, 2)]), &[parent, child], &stats),
            vec![ReviewBottomUp]
        );
    }

    #[test]
    fn merged_members_are_ignored_for_size_sum() {
        let (parent, child) = pair(PrState::Merged);
        let stats = HashMap::from([(1, stat(10, 7000, 0, 100)), (2, stat(10, 3000, 0, 100))]);
        assert_eq!(
            advise(&stack(&[1, 2], &[(1, 2)]), &[parent, child], &stats),
            vec![ParentMergedRetarget {
                parent: 1,
                child: 2
            }]
        );
    }

    #[test]
    fn review_bottom_up_always_first() {
        let (mut parent, mut child) = pair(PrState::Open);
        parent.task_count = 1;
        child.dest_commit = "old".into();
        let child = with_review(child, ReviewState::Approved);
        let stats = HashMap::from([
            (1, stat(10, 3001, 1000, 100)),
            (2, stat(10, 3001, 1000, 100)),
        ]);
        assert_eq!(
            advise(&stack(&[1, 2], &[(1, 2)]), &[parent, child], &stats),
            vec![
                ReviewBottomUp,
                ParentChanged {
                    parent: 1,
                    child: 2
                },
                HoldChildApproval {
                    parent: 1,
                    child: 2
                },
                MergeOrderViolation {
                    parent: 1,
                    child: 2
                },
                ReviewablePerPr,
            ]
        );
    }

    #[test]
    fn per_edge_advice_follows_edge_order() {
        let (mut p1, c2) = pair(PrState::Open);
        p1.task_count = 1;
        let mut c3 = pr(3, PrState::Open);
        c3.dest_commit = p1.source_commit.clone();
        let s = stack(&[1, 2, 3], &[(1, 2), (1, 3)]);
        assert_eq!(
            advise(&s, &[p1, c2, c3], &HashMap::new()),
            vec![
                ReviewBottomUp,
                HoldChildApproval {
                    parent: 1,
                    child: 2
                },
                HoldChildApproval {
                    parent: 1,
                    child: 3
                },
            ]
        );
    }
}
