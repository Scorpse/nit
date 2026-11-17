//! The PR's commits and which of them came after my last review, and what a chosen commit (or
//! all the new ones together) changed.

use std::collections::HashMap;
use std::sync::Arc;

use prcore::diff::DiffFile;
use prcore::history::{new_since, CommitInfo, ReviewMark};

use super::service::CommitRange;

/// A patch longer than this many lines is cut (the rest stays in git).
pub const MAX_PATCH_LINES: usize = 20_000;

/// What the Commits tab asks the service for, drained by the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    Files(CommitRange),
    Patch(CommitRange, DiffFile),
}

/// One shown patch: the lines of git's unified diff of a file in a range.
#[derive(Debug)]
pub struct ShownPatch {
    pub range: CommitRange,
    pub path: String,
    pub lines: Result<Vec<String>, String>,
    pub cut: bool,
}

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
    /// The merge base of the PR: where "all the new commits" start when none was seen.
    pub pr_base: String,
    /// The range whose files are shown on the right.
    pub selected: Option<CommitRange>,
    pub files: HashMap<CommitRange, Result<Vec<DiffFile>, String>>,
    /// The file whose patch is shown: its range and path.
    pub open_file: Option<(CommitRange, String)>,
    pub patch: Option<ShownPatch>,
    wants: Vec<Want>,
}

/// The range of one commit: from its parent.
pub fn commit_range(c: &CommitInfo) -> CommitRange {
    CommitRange {
        base: format!("{}^", c.id),
        head: c.id.clone(),
    }
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

    /// Everything after my last review as one range: from the commit I was caught up to (the
    /// PR's base when every commit is new) to the head. `None` when nothing is new.
    pub fn since_review(&self) -> Option<CommitRange> {
        if self.new_count() == 0 {
            return None;
        }
        let head = self.list.as_ref()?.last()?.id.clone();
        let base = match self.caught_up_head()? {
            c if c.is_empty() => self.pr_base.clone(),
            c => c,
        };
        (!base.is_empty()).then_some(CommitRange { base, head })
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
                let when = m.at.strftime("%Y-%m-%d %H:%M UTC");
                Some(match self.new_count() {
                    0 => format!("No commits since your last review ({when})."),
                    1 => format!("1 commit since your last review ({when})."),
                    n => format!("{n} commits since your last review ({when})."),
                })
            }
        }
    }

    /// Shows what `range` changed: its files, asked for once.
    pub fn select(&mut self, range: CommitRange) {
        if self.selected.as_ref() == Some(&range) {
            return;
        }
        if !self.files.contains_key(&range) {
            self.wants.push(Want::Files(range.clone()));
        }
        self.selected = Some(range);
        self.open_file = None;
        self.patch = None;
    }

    pub fn set_files(&mut self, range: CommitRange, files: Result<Vec<DiffFile>, String>) {
        self.files.insert(range, files);
    }

    /// Shows `file`'s patch in the selected range.
    pub fn open(&mut self, file: &DiffFile) {
        let Some(range) = self.selected.clone() else {
            return;
        };
        self.open_file = Some((range.clone(), file.path.clone()));
        self.patch = None;
        self.wants.push(Want::Patch(range, file.clone()));
    }

    /// A patch that arrived; ignored when another file was chosen meanwhile.
    pub fn set_patch(
        &mut self,
        range: CommitRange,
        path: String,
        result: Result<Arc<String>, String>,
    ) {
        if self.open_file.as_ref() != Some(&(range.clone(), path.clone())) {
            return;
        }
        let mut cut = false;
        let lines = result.map(|text| {
            let mut it = text.lines().map(str::to_string);
            let mut lines: Vec<String> = it.by_ref().take(MAX_PATCH_LINES).collect();
            cut = it.next().is_some();
            lines.shrink_to_fit();
            lines
        });
        self.patch = Some(ShownPatch {
            range,
            path,
            lines,
            cut,
        });
    }

    pub fn take_wants(&mut self) -> Vec<Want> {
        std::mem::take(&mut self.wants)
    }
}

#[cfg(test)]
mod tests;
