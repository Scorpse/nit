//! Inbox view-model: pure filtering, grouping and sorting of pull requests.
//! No UI types live here.

use std::cmp::Reverse;
use std::collections::HashMap;

use prcore::advice::{advise, Advice};
use prcore::model::{
    AccountId, DiffStat, Forge, PrState, PullRequest, RepoRef, ReviewState, UserRef,
};
use prcore::stacks::{detect_stacks, Stack};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Mine,
    NeedsMe,
    All,
    TooBig,
    Stacked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxRow {
    pub pr: PullRequest,
    pub stat: Option<DiffStat>,
    /// Index into `Inbox::stacks`.
    pub stack: Option<usize>,
    pub fork_unsupported: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inbox {
    pub rows: Vec<InboxRow>,
    pub stacks: Vec<Stack>,
}

/// Builds the rows for `view`, filtered by `query`.
///
/// Stacks are detected over ALL given PRs (merged parents anchor stacks), but only
/// `Open` PRs become rows. Row order: repo full name ascending; within a repo, stack
/// groups first (stacks in `Inbox::stacks` order, members base first), then ungrouped
/// PRs by `updated_on` descending; ties break by PR id ascending.
#[cfg(test)]
pub fn build_inbox(
    prs: &[PullRequest],
    stats: &HashMap<(RepoRef, u64), DiffStat>,
    me: &UserRef,
    view: View,
    query: &str,
) -> Inbox {
    build_inbox_for(prs, stats, me, &HashMap::new(), view, query)
}

/// `build_inbox` with each account's own user: "Mine" and "Needs me" compare a PR with the
/// user of the account it was synced through (`users`, account id to user id), else `me`.
/// GitHub's and GitLab's "Needs me" is the provider's `review_requested_from_me` (GitHub
/// drops a reviewer from the request list after a review, and team requests count; GitLab
/// keeps a per-reviewer state and separate approvals). Fork PRs are
/// unsupported on Bitbucket only; elsewhere they are listed (decision D2) but never stacked.
pub fn build_inbox_for(
    prs: &[PullRequest],
    stats: &HashMap<(RepoRef, u64), DiffStat>,
    me: &UserRef,
    users: &HashMap<AccountId, String>,
    view: View,
    query: &str,
) -> Inbox {
    let me_id = |pr: &PullRequest| {
        users
            .get(&pr.account)
            .filter(|u| !u.is_empty())
            .map_or(me.id.as_str(), String::as_str)
            .to_string()
    };
    let stacks = detect_stacks(prs);
    let stack_of: HashMap<(&RepoRef, u64), (usize, usize)> = stacks
        .iter()
        .enumerate()
        .flat_map(|(si, s)| {
            s.members
                .iter()
                .enumerate()
                .map(move |(mi, &id)| ((&s.repo, id), (si, mi)))
        })
        .collect();
    let needle = query.trim().to_lowercase();

    let mut keyed: Vec<(_, InboxRow)> = Vec::new();
    for pr in prs.iter().filter(|p| p.state == PrState::Open) {
        let fork = pr.is_fork();
        let unsupported = fork && pr.forge == Forge::BitbucketCloud;
        let stat = stats.get(&(pr.repo.clone(), pr.id)).cloned();
        let slot = if fork {
            None
        } else {
            stack_of.get(&(&pr.repo, pr.id)).copied()
        };
        let in_view = match view {
            View::All => true,
            _ if unsupported => false,
            View::Mine => pr.author.id == me_id(pr),
            View::NeedsMe => {
                let mine = me_id(pr);
                !pr.draft
                    && match pr.forge {
                        Forge::GitHub | Forge::GitLab => pr.review_requested_from_me,
                        _ => pr
                            .reviewers
                            .iter()
                            .any(|r| r.user.id == mine && r.state == ReviewState::Pending),
                    }
            }
            View::TooBig => stat.as_ref().is_some_and(|s| s.exceeds_web_ui(pr.forge)),
            View::Stacked => slot.is_some(),
        };
        if !in_view || !matches_query(pr, &needle) {
            continue;
        }
        // (repo, stack group before ungrouped, stack index, member index, newest first, id)
        let key = match slot {
            Some((si, mi)) => (
                pr.repo.full_name(),
                0,
                si,
                mi,
                Reverse(pr.updated_on),
                pr.id,
            ),
            None => (pr.repo.full_name(), 1, 0, 0, Reverse(pr.updated_on), pr.id),
        };
        keyed.push((
            key,
            InboxRow {
                pr: pr.clone(),
                stat,
                stack: slot.map(|(si, _)| si),
                fork_unsupported: unsupported,
            },
        ));
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    Inbox {
        rows: keyed.into_iter().map(|(_, r)| r).collect(),
        stacks,
    }
}

/// Which accounts' PRs the inbox shows (its filter chips).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    All,
    Forge(Forge),
    Account(AccountId),
}

impl Scope {
    pub fn matches(&self, pr: &PullRequest) -> bool {
        match self {
            Scope::All => true,
            Scope::Forge(f) => pr.forge == *f,
            Scope::Account(a) => pr.account == *a,
        }
    }
}

/// The inbox's filter chips for `prs`: none while they come from one account; else "All",
/// one per forge when there are several, and one per account (named by `labels`) when some
/// forge has more than one.
pub fn scope_chips(prs: &[PullRequest], labels: &[(AccountId, String)]) -> Vec<(Scope, String)> {
    let mut accounts: Vec<(&AccountId, Forge)> = Vec::new();
    for p in prs {
        if !accounts.iter().any(|(a, _)| **a == p.account) {
            accounts.push((&p.account, p.forge));
        }
    }
    if accounts.len() < 2 {
        return Vec::new();
    }
    let forges: Vec<Forge> = [Forge::BitbucketCloud, Forge::GitHub, Forge::GitLab]
        .into_iter()
        .filter(|f| accounts.iter().any(|(_, af)| af == f))
        .collect();
    let mut out = vec![(Scope::All, "All".to_string())];
    if forges.len() > 1 {
        out.extend(
            forges
                .iter()
                .map(|f| (Scope::Forge(*f), f.label().to_string())),
        );
    }
    if accounts.len() > forges.len() {
        let rank = |a: &AccountId| {
            labels
                .iter()
                .position(|(id, _)| id == a)
                .unwrap_or(usize::MAX)
        };
        accounts.sort_by(|x, y| rank(x.0).cmp(&rank(y.0)).then(x.0.cmp(y.0)));
        out.extend(accounts.into_iter().map(|(a, _)| {
            let name = labels
                .iter()
                .find(|(id, _)| id == a)
                .map_or_else(|| a.as_str().to_string(), |(_, l)| l.clone());
            (Scope::Account(a.clone()), name)
        }));
    }
    out
}

fn matches_query(pr: &PullRequest, needle: &str) -> bool {
    needle.is_empty()
        || [
            pr.title.as_str(),
            &pr.repo.full_name(),
            pr.source_branch.as_str(),
            pr.author.display_name.as_str(),
        ]
        .iter()
        .any(|f| f.to_lowercase().contains(needle))
}

/// Advice for `inbox.stacks[stack]`; re-keys `stats` to the stack's repo.
pub fn stack_advice(
    inbox: &Inbox,
    prs: &[PullRequest],
    stats: &HashMap<(RepoRef, u64), DiffStat>,
    stack: usize,
) -> Vec<Advice> {
    let Some(s) = inbox.stacks.get(stack) else {
        return Vec::new();
    };
    let repo_prs: Vec<PullRequest> = prs.iter().filter(|p| p.repo == s.repo).cloned().collect();
    let repo_stats: HashMap<u64, DiffStat> = stats
        .iter()
        .filter(|((r, _), _)| *r == s.repo)
        .map(|((_, id), d)| (*id, d.clone()))
        .collect();
    advise(s, &repo_prs, &repo_stats)
}

#[cfg(test)]
mod tests;
