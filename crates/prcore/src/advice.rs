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
mod tests;
