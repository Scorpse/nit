//! The PR's commits and which of them came after my last review.

use prcore::history::{new_since, CommitInfo, ReviewMark};

/// What the Commits tab shows. The commits (local git) and my last review (the forge) arrive
/// separately, in either order.
#[derive(Debug, Default)]
pub struct Commits {
    /// Oldest first; `None` until listed.
    pub list: Option<Vec<CommitInfo>>,
    pub error: Option<String>,
    /// `None` until answered; `Some(None)`: no review of mine, or the forge does not say.
    mark: Option<Option<ReviewMark>>,
    pub mark_error: Option<String>,
}

impl Commits {
    pub fn set_list(&mut self, list: Vec<CommitInfo>) {
        self.list = Some(list);
        self.error = None;
    }

    pub fn fail(&mut self, e: String) {
        self.error = Some(e);
    }

    pub fn set_mark(&mut self, mark: Option<ReviewMark>) {
        self.mark = Some(mark);
        self.mark_error = None;
    }

    pub fn fail_mark(&mut self, e: String) {
        self.mark_error = Some(e);
    }

    /// Per commit of `list`: came after my last review. Nothing is marked without a review.
    pub fn new_flags(&self) -> Vec<bool> {
        let list = self.list.as_deref().unwrap_or_default();
        match self.mark.as_ref().and_then(Option::as_ref) {
            Some(m) => new_since(list, m),
            None => vec![false; list.len()],
        }
    }

    /// The newest commit that is not new (the PR's head when nothing is new; empty when every
    /// commit is). `None` until the commits and my last review are known, when I have no
    /// review (nothing to be caught up to) and when the PR has no commits of its own.
    pub fn caught_up_head(&self) -> Option<String> {
        let list = self.list.as_ref()?;
        self.mark.as_ref()?.as_ref()?;
        if list.is_empty() {
            return None;
        }
        let flags = self.new_flags();
        let seen = list.iter().zip(&flags).rev().find(|(_, new)| !**new);
        Some(seen.map(|(c, _)| c.id.clone()).unwrap_or_default())
    }

    pub fn new_count(&self) -> usize {
        self.new_flags().into_iter().filter(|n| *n).count()
    }

    /// The line under the tab's title, once something is known about my last review.
    pub fn note(&self) -> Option<String> {
        if let Some(e) = &self.mark_error {
            return Some(format!(
                "Could not read your last review ({e}); no commit is marked new."
            ));
        }
        match self.mark.as_ref()? {
            None => Some("You have not reviewed this yet.".into()),
            Some(m) => {
                let when = m.at.format("%Y-%m-%d %H:%M UTC");
                Some(match self.new_count() {
                    0 => format!("No commits since your last review ({when})."),
                    1 => format!("1 commit since your last review ({when})."),
                    n => format!("{n} commits since your last review ({when})."),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests;
