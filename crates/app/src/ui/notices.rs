//! Notice and advice texts.

use super::*;

pub const AUTH_REJECTED: &str = "Email or token rejected. Re-enter them or sign out.";
pub const ACCESS_DENIED_HINT: &str = "Access denied on all repos: the token may be missing scopes";
pub(super) const DENIED: &str = "access denied";

/// True when a finished sync could read nothing because of 403s: every repo failed with
/// "access denied" or, with no repos found at all, every workspace listing did.
pub fn access_denied_everywhere(total: usize, denied: usize, errors: &[(RepoRef, String)]) -> bool {
    if total > 0 {
        return denied >= total;
    }
    let mut listings = errors.iter().filter(|(r, _)| r.slug == "*").peekable();
    listings.peek().is_some() && listings.all(|(_, e)| e == DENIED)
}

/// Short commit hash for the debug line; empty means the API did not report one.
pub(super) fn short_hash(h: &str) -> &str {
    if h.is_empty() {
        "unknown"
    } else {
        h.get(..7).unwrap_or(h)
    }
}

/// Debug details for a stack member, so the owner can compare raw hashes with the advice.
pub fn stack_debug_line(pr: &PullRequest, parent: Option<&PullRequest>) -> String {
    match parent {
        Some(p) => format!(
            "debug: destination.commit {} | parent #{} source.commit {}",
            short_hash(&pr.dest_commit),
            p.id,
            short_hash(&p.source_commit)
        ),
        None => format!(
            "debug: source.commit {} | destination.commit {}",
            short_hash(&pr.source_commit),
            short_hash(&pr.dest_commit)
        ),
    }
}

/// Plain-language sentence for one piece of stack advice.
pub fn advice_text(advice: &Advice) -> String {
    match advice {
        Advice::ReviewBottomUp => "Review this stack from the bottom up: start with the first PR \
            and work toward the last, because each PR builds on the one before it."
            .into(),
        Advice::ParentChanged { parent, child } => format!(
            "#{parent} changed after #{child} was opened on top of it, so the diff and comments \
             of #{child} may be stale. Ask the author to rebase #{child}, then re-review it."
        ),
        Advice::ParentMergedRetarget { parent, child } => format!(
            "#{parent} is already merged but #{child} still targets its branch. Ask the author to \
             retarget #{child} to the new base before you review it."
        ),
        Advice::HoldChildApproval { parent, child } => format!(
            "Hold off approving #{child}: #{parent} has requested changes or open tasks, and \
             #{child} may change with it."
        ),
        Advice::MergeOrderViolation { parent, child } => format!(
            "#{child} is approved but #{parent} is not. #{parent} must be merged first, so do not \
             merge #{child} yet."
        ),
        Advice::ReviewablePerPr => "Together these PRs are over Bitbucket's diff limits, but each \
            one fits. Review them one PR at a time in Bitbucket."
            .into(),
    }
}

/// The one-line notice after the upgrade that started syncing only chosen repositories.
pub fn preselected_notice(n: usize) -> String {
    let s = if n == 1 { "y" } else { "ies" };
    format!(
        "nit now syncs only repositories you choose; the {n} repositor{s} you already had \
         pull requests from stay chosen."
    )
}
