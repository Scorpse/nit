//! List selection, row recompute and the detail pane.

use super::*;

impl App {
    /// Selects a PR of the list; its detail pane is rebuilt at once.
    pub(super) fn select_pr(&mut self, sel: (RepoRef, u64)) {
        if self.selected.as_ref() != Some(&sel) {
            self.selected = Some(sel);
            self.dirty = true;
            self.recompute();
        }
    }

    /// Rebuilds rows and the detail pane. Runs only after data, view, query or selection changed.
    pub(super) fn recompute(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.chips = scope_chips(&self.data.prs, &self.account_names);
        if !self.chips.iter().any(|(s, _)| *s == self.scope) {
            self.scope = Scope::All;
        }
        let scoped: Vec<PullRequest>;
        let prs: &[PullRequest] = if self.scope == Scope::All {
            &self.data.prs
        } else {
            scoped = self
                .data
                .prs
                .iter()
                .filter(|p| self.scope.matches(p))
                .cloned()
                .collect();
            &scoped
        };
        self.inbox = build_inbox_for(
            prs,
            &self.data.stats,
            &self.me,
            &self.data.account_users,
            effective_view(self.me_known(), self.view),
            &self.query,
        );
        self.items = build_items(&self.inbox);
        self.detail = self
            .selected
            .as_ref()
            .and_then(|(repo, id)| self.make_detail(repo, *id));
    }

    /// Who I am, the stack PR `id` belongs to and its repository's PRs, for the review
    /// screen's actions and warnings.
    pub(crate) fn review_context(
        &self,
        repo: &RepoRef,
        id: u64,
    ) -> crate::review::actions_ui::ReviewContext {
        let stack = self
            .inbox
            .stacks
            .iter()
            .find(|s| s.repo == *repo && s.members.contains(&id))
            .cloned();
        let prs = self
            .data
            .prs
            .iter()
            .filter(|p| p.repo == *repo)
            .map(|p| (p.id, p.clone()))
            .collect();
        crate::review::actions_ui::ReviewContext {
            me: self.me_known().then(|| self.me.clone()),
            stack,
            prs,
        }
    }

    pub(super) fn make_detail(&self, repo: &RepoRef, id: u64) -> Option<Detail> {
        let pr: &PullRequest = self
            .data
            .prs
            .iter()
            .find(|p| p.repo == *repo && p.id == id)?;
        let slot = self
            .inbox
            .stacks
            .iter()
            .position(|s| s.repo == *repo && s.members.contains(&id));
        let (stack, advice, debug) = match slot {
            Some(si) => {
                let s = &self.inbox.stacks[si];
                let pos = s.members.iter().position(|&m| m == id).unwrap_or(0) + 1;
                let texts = stack_advice(&self.inbox, &self.data.prs, &self.data.stats, si)
                    .iter()
                    .map(advice_text)
                    .collect();
                let parent = s.edges.iter().find(|&&(_, c)| c == id).and_then(|&(p, _)| {
                    self.data.prs.iter().find(|x| x.repo == *repo && x.id == p)
                });
                let debug = stack_debug_line(pr, parent);
                (Some((pos, s.members.len())), texts, Some(debug))
            }
            None => (None, Vec::new(), None),
        };
        Some(Detail {
            pr: pr.clone(),
            stat: self.data.stats.get(&(repo.clone(), id)).cloned(),
            stack,
            advice,
            debug,
        })
    }
}
