//! Commit hash comparison. Bitbucket returns abbreviated hashes (12 characters), git full ones.

/// Both name the same commit: at least 7 characters each, one a prefix of the other (case and
/// surrounding whitespace ignored). An empty or short value never matches.
pub fn same_commit(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim().to_lowercase(), b.trim().to_lowercase());
    a.len().min(b.len()) >= 7 && (a.starts_with(&b) || b.starts_with(&a))
}

/// Like `same_commit`, but an empty `remote` means "unknown" and never counts as a change,
/// and short values match only when equal.
pub fn same_commit_or_unknown(remote: &str, head: &str) -> bool {
    let (a, b) = (remote.trim().to_lowercase(), head.trim().to_lowercase());
    if a.is_empty() {
        return true;
    }
    if a.len().min(b.len()) < 7 {
        return a == b;
    }
    a.starts_with(&b) || b.starts_with(&a)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn strict_allows_an_abbreviated_hash_and_nothing_short() {
        assert!(same_commit(&HEAD[..12], HEAD));
        assert!(same_commit(HEAD, &HEAD[..7]));
        assert!(same_commit(" 0123456789AB ", HEAD));
        assert!(!same_commit(&HEAD[..6], HEAD));
        assert!(!same_commit("", HEAD));
        assert!(!same_commit("", ""));
        assert!(!same_commit("0123", "0123"));
        assert!(!same_commit("fedcba9876543210", HEAD));
    }

    #[test]
    fn unknown_remote_is_not_a_change() {
        assert!(same_commit_or_unknown("", HEAD));
        assert!(same_commit_or_unknown("  ", HEAD));
        assert!(same_commit_or_unknown(&HEAD[..12], HEAD));
        assert!(same_commit_or_unknown("0123", "0123"));
        assert!(!same_commit_or_unknown("0123", HEAD));
        assert!(!same_commit_or_unknown("fedcba9876543210", HEAD));
        assert!(!same_commit_or_unknown(HEAD, ""));
    }
}
