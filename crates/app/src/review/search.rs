//! `/` search in the current diff: smart case (a query without capitals ignores ASCII case),
//! matches as byte ranges for highlighting, and the next / previous match in display order.

use std::ops::Range;

use prcore::diff::{Row, RowKind};

/// Ignore ASCII case unless the query has a capital letter (vim's smartcase).
pub fn ignores_case(query: &str) -> bool {
    !query.chars().any(char::is_uppercase)
}

/// Byte ranges of the non-overlapping matches of `query` in `text`. ASCII case is ignored
/// when `fold` (other characters must match exactly), so every range starts and ends on a
/// character boundary.
pub fn find_all(text: &str, query: &str, fold: bool) -> Vec<Range<usize>> {
    let (h, q) = (text.as_bytes(), query.as_bytes());
    let mut out = Vec::new();
    if q.is_empty() || q.len() > h.len() {
        return out;
    }
    let mut i = 0;
    while i + q.len() <= h.len() {
        let hit = if fold {
            h[i..i + q.len()].eq_ignore_ascii_case(q)
        } else {
            &h[i..i + q.len()] == q
        };
        if hit {
            out.push(i..i + q.len());
            i += q.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Whether `text` contains `query` (as `find_all`, without collecting).
fn contains(text: &str, query: &str, fold: bool) -> bool {
    if fold {
        let (h, q) = (text.as_bytes(), query.as_bytes());
        q.len() <= h.len() && h.windows(q.len()).any(|w| w.eq_ignore_ascii_case(q))
    } else {
        text.contains(query)
    }
}

/// Rows with a match, in row order. O(rows): run when a search is confirmed, not per frame.
/// Hunk headers and "\ No newline" markers are not searched.
pub fn matching_rows(rows: &[Row], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let fold = ignores_case(query);
    rows.iter()
        .enumerate()
        .filter(|(_, r)| !matches!(r.kind, RowKind::HunkHeader | RowKind::NoNewline))
        .filter(|(_, r)| contains(&r.text, query, fold))
        .map(|(i, _)| i)
        .collect()
}

/// The match after (`forward`) or before `at` in display order, wrapping around; `key` gives
/// a row's position on screen (its row, or its split line and side).
pub fn step_match<K: Ord + Copy>(
    matches: &[usize],
    at: K,
    forward: bool,
    key: impl Fn(usize) -> K,
) -> Option<usize> {
    let keyed = matches.iter().map(|&r| (key(r), r));
    if forward {
        let after = keyed.clone().filter(|&(k, _)| k > at).min();
        after.or_else(|| keyed.min()).map(|(_, r)| r)
    } else {
        let before = keyed.clone().filter(|&(k, _)| k < at).max();
        before.or_else(|| keyed.max()).map(|(_, r)| r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: RowKind, text: &str) -> Row {
        Row {
            kind,
            old_line: None,
            new_line: None,
            text: text.into(),
        }
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn smart_case_and_byte_ranges() {
        assert!(ignores_case("foo"));
        assert!(!ignores_case("Foo"));
        assert_eq!(find_all("Foo foo FOO", "foo", true), [0..3, 4..7, 8..11]);
        assert_eq!(find_all("Foo foo FOO", "Foo", false), [0..3]);
        assert_eq!(find_all("aaaa", "aa", true), [0..2, 2..4], "no overlaps");
        assert!(find_all("abc", "", true).is_empty());
        assert!(find_all("ab", "abc", true).is_empty());
        // non-ASCII: exact bytes, ranges on character boundaries
        let t = "héllo wörld";
        let m = find_all(t, "wör", true);
        assert_eq!(m, [7..11]);
        assert_eq!(&t[m[0].clone()], "wör");
        assert!(
            find_all("ÉCOLE", "école", true).is_empty(),
            "only ASCII case folds"
        );
    }

    #[test]
    fn matching_rows_skip_headers_and_markers() {
        let rows = vec![
            row(RowKind::HunkHeader, "@@ fn main @@"),
            row(RowKind::Context, "fn main() {"),
            row(RowKind::Del, "  old()"),
            row(RowKind::Add, "  MAIN_LOOP()"),
            row(RowKind::NoNewline, "\\ No newline at end of file"),
        ];
        assert_eq!(matching_rows(&rows, "main"), [1, 3]);
        assert_eq!(matching_rows(&rows, "MAIN"), [3]);
        assert_eq!(matching_rows(&rows, "newline"), Vec::<usize>::new());
        assert!(matching_rows(&rows, "").is_empty());
    }

    #[test]
    fn next_and_previous_match_wrap_in_display_order() {
        let m = [2, 5, 9];
        let id = |r: usize| r;
        assert_eq!(step_match(&m, 0, true, id), Some(2));
        assert_eq!(step_match(&m, 2, true, id), Some(5));
        assert_eq!(step_match(&m, 9, true, id), Some(2), "wraps");
        assert_eq!(step_match(&m, 5, false, id), Some(2));
        assert_eq!(step_match(&m, 2, false, id), Some(9), "wraps back");
        assert_eq!(step_match(&[], 0, true, id), None);
        // split view: a deleted row (left) and the added row on the same line come in
        // line order, left before right, not row order
        let line_side = |r: usize| match r {
            2 => (1, 0),
            5 => (1, 1),
            9 => (0, 1),
            _ => unreachable!(),
        };
        assert_eq!(step_match(&m, (0, 0), true, line_side), Some(9));
        assert_eq!(step_match(&m, (0, 1), true, line_side), Some(2));
        assert_eq!(step_match(&m, (1, 0), true, line_side), Some(5));
    }
}
