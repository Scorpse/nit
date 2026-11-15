use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use crate::model::{PrState, PullRequest, RepoRef};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stack {
    pub repo: RepoRef,
    pub members: Vec<u64>,
    pub edges: Vec<(u64, u64)>,
}

/// Finds stacked PRs: chains/trees where a child PR targets its parent's source branch.
///
/// Rules: PRs are grouped per repo; forks and Declined/Superseded PRs are ignored.
/// A child must be Open. When several PRs share the parent's source branch, the
/// parent is chosen by Open over Merged, then newest `updated_on`, then lowest id.
/// Members are ordered parent-before-child (Kahn), ties by ascending id; nodes
/// left over by cycles are dropped. Only stacks with 2+ members are returned,
/// sorted by repo full name then first member id.
pub fn detect_stacks(prs: &[PullRequest]) -> Vec<Stack> {
    // Keyed by host too: the same `ns/slug` on two forges is two repos.
    let mut by_repo: BTreeMap<(String, String), Vec<&PullRequest>> = BTreeMap::new();
    for pr in prs {
        let eligible = !pr.is_fork() && matches!(pr.state, PrState::Open | PrState::Merged);
        if eligible {
            let key = (pr.repo.host.clone(), pr.repo.full_name());
            by_repo.entry(key).or_default().push(pr);
        }
    }

    let mut stacks = Vec::new();
    for group in by_repo.values() {
        stacks.extend(stacks_in_repo(group));
    }
    stacks.sort_by_key(|s| (s.repo.full_name(), s.members[0]));
    stacks
}

fn stacks_in_repo(group: &[&PullRequest]) -> Vec<Stack> {
    // child id -> chosen parent id
    let mut parent_of: BTreeMap<u64, u64> = BTreeMap::new();
    for child in group.iter().filter(|c| c.state == PrState::Open) {
        let best = group
            .iter()
            .filter(|p| p.id != child.id && p.source_branch == child.dest_branch)
            .min_by_key(|p| (p.state != PrState::Open, Reverse(p.updated_on), p.id));
        if let Some(p) = best {
            parent_of.insert(child.id, p.id);
        }
    }

    // Union-find over linked ids to get connected components.
    let mut uf: BTreeMap<u64, u64> = BTreeMap::new();
    for (&c, &p) in &parent_of {
        let (rc, rp) = (find(&mut uf, c), find(&mut uf, p));
        if rc != rp {
            uf.insert(rc, rp);
        }
    }
    let mut components: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for id in uf.keys().copied().collect::<Vec<_>>() {
        components.entry(find(&mut uf, id)).or_default().push(id);
    }

    let repo = group[0].repo.clone();
    let mut stacks = Vec::new();
    for nodes in components.values() {
        let node_set: BTreeSet<u64> = nodes.iter().copied().collect();
        let mut indegree: BTreeMap<u64, usize> = node_set.iter().map(|&n| (n, 0)).collect();
        let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for (&c, &p) in parent_of.iter().filter(|(c, _)| node_set.contains(c)) {
            *indegree.get_mut(&c).unwrap() += 1;
            children.entry(p).or_default().push(c);
        }

        let mut ready: BTreeSet<u64> = indegree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&n, _)| n)
            .collect();
        let mut members = Vec::new();
        let mut edges = Vec::new();
        while let Some(n) = ready.pop_first() {
            members.push(n);
            for &c in children.get(&n).into_iter().flatten() {
                edges.push((n, c));
                let d = indegree.get_mut(&c).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.insert(c);
                }
            }
        }
        edges.sort();
        if members.len() >= 2 {
            stacks.push(Stack {
                repo: repo.clone(),
                members,
                edges,
            });
        }
    }
    stacks
}

fn find(uf: &mut BTreeMap<u64, u64>, x: u64) -> u64 {
    let mut root = x;
    while let Some(&p) = uf.get(&root) {
        if p == root {
            break;
        }
        root = p;
    }
    uf.entry(root).or_insert(root);
    uf.insert(x, root);
    root
}

#[cfg(test)]
mod tests;
