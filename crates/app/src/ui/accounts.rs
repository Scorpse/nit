//! Account labels, rate-limit pauses and the repository picker entry.

use super::*;

impl App {
    /// The "requested elsewhere" hint over every account's last answer, unless dismissed.
    pub(super) fn elsewhere_text(&self) -> Option<String> {
        let mut all: std::collections::BTreeMap<String, (RepoRef, usize)> = Default::default();
        for (r, n) in self.elsewhere.iter().flat_map(|(_, v)| v) {
            let e = all.entry(r.key()).or_insert_with(|| (r.clone(), 0));
            // the same PR seen through two accounts counts once
            e.1 = e.1.max(*n);
        }
        let repos: Vec<(RepoRef, usize)> = all.into_values().collect();
        crate::backend::elsewhere_hint(&repos)
            .filter(|t| self.elsewhere_dismissed.as_ref() != Some(t))
    }

    /// Shows "Choose repositories"; Back returns to `from`.
    /// Re-reads every account's label (from the store: only on startup and after an account
    /// change, never after a sync event), then the pauses.
    pub(super) fn refresh_accounts(&mut self) {
        self.account_names = match &self.backend {
            Some(b) => b.account_labels(),
            None => crate::demo::demo_accounts()
                .iter()
                .map(|a| (a.id.clone(), crate::backend::default_label(a)))
                .collect(),
        };
        self.refresh_paused();
    }

    /// Which accounts are paused by a rate limit, from the providers only (no store read, so
    /// a sync holding the store never blocks the UI); also names the signed-in account by
    /// its user before its row knows one.
    pub(super) fn refresh_paused(&mut self) {
        let default = AccountId::default_bitbucket();
        for (id, name) in &mut self.account_names {
            if *id == default
                && name.as_str() == default.as_str()
                && !self.me.display_name.is_empty()
            {
                *name = self.me.display_name.clone();
            }
        }
        let paused = match &self.backend {
            Some(b) => b.paused_status(),
            None if self.shot.as_ref().map(|s| s.scene) == Some(Scene::Accounts) => {
                let accounts = crate::demo::demo_accounts();
                crate::demo::demo_paused()
                    .into_iter()
                    .filter_map(|(id, t)| {
                        let a = accounts.iter().find(|a| a.id == id)?;
                        Some((a.forge, id, t))
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        self.paused = paused
            .into_iter()
            .map(|(forge, id, until)| {
                let label = self
                    .account_names
                    .iter()
                    .find(|(a, _)| *a == id)
                    .map_or_else(|| id.as_str().to_string(), |(_, l)| l.clone());
                (forge, id, label, until)
            })
            .collect();
        self.dirty = true;
    }

    /// The accounts besides the signed-in one (none in demo mode), by label, with their
    /// rate-limit pause.
    pub(super) fn other_accounts(&self) -> Vec<settings_ui::OtherAccount> {
        let mut out = self.other_accounts_raw();
        for o in &mut out {
            if let Some((_, l)) = self.account_names.iter().find(|(id, _)| *id == o.id) {
                o.name = l.clone();
            }
            o.paused_until = self
                .paused
                .iter()
                .find(|(_, id, _, _)| *id == o.id)
                .map(|p| p.3);
        }
        out
    }

    pub(super) fn other_accounts_raw(&self) -> Vec<settings_ui::OtherAccount> {
        match &self.backend {
            Some(b) => settings_ui::other_accounts_with(&b.accounts(), |id| {
                b.with_store(|s| crate::backend::token_expiry(s, id))
            }),
            None if matches!(
                self.shot.as_ref().map(|s| s.scene),
                Some(Scene::Accounts | Scene::AddGitLab)
            ) =>
            {
                settings_ui::other_accounts_with(
                    &crate::demo::demo_accounts(),
                    crate::demo::demo_token_expiry,
                )
            }
            None => Vec::new(),
        }
    }

    /// The picker's group name of every account: forge, host and user.
    pub(super) fn account_labels(&self) -> Vec<(prcore::model::AccountId, String)> {
        let accounts = match &self.backend {
            Some(b) => b.accounts(),
            None => crate::demo::demo_accounts(),
        };
        accounts
            .into_iter()
            .map(|a| {
                let label = self.account_names.iter().find(|(id, _)| *id == a.id);
                let name = match label {
                    Some((_, l)) => l.clone(),
                    None if a.display_name.is_empty() => self.me.display_name.clone(),
                    None => a.display_name.clone(),
                };
                let label = format!(
                    "{name} \u{00b7} {} \u{00b7} {}",
                    a.forge.label(),
                    a.git_host
                );
                (a.id, label)
            })
            .collect()
    }

    pub(crate) fn open_picker(&mut self, from: Screen) {
        self.picker_from = from;
        self.picker = None;
        self.screen = Screen::Repos;
    }
}
