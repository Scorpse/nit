//! Word-level diff of a paired deletion/addition: the byte ranges that changed.

use std::ops::Range;

use similar::{ChangeTag, TextDiff};

/// Lines longer than this (chars) get no word diff.
pub const MAX_WORD_DIFF_CHARS: usize = 2_000;

/// Below this similarity (share of non-blank chars the lines have in common, 0..=1) the lines are unrelated: marking nearly
/// everything as changed says nothing, so no ranges are returned.
const MIN_SIMILARITY: f32 = 0.3;

/// Lines at most this many chars long are compared per char: word tokens are too coarse.
const SHORT_CHARS: usize = 4;

/// Changed byte ranges of `old` and `new`, merged where adjacent. Empty for lines over
/// `MAX_WORD_DIFF_CHARS` chars and for lines that are too different (`MIN_SIMILARITY`).
pub fn word_spans(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let limit = MAX_WORD_DIFF_CHARS;
    let (old_chars, new_chars) = (
        old.chars().take(limit + 1).count(),
        new.chars().take(limit + 1).count(),
    );
    if old_chars > limit || new_chars > limit {
        return (Vec::new(), Vec::new());
    }
    let diff = if old_chars.max(new_chars) <= SHORT_CHARS {
        TextDiff::from_chars(old, new)
    } else {
        TextDiff::from_unicode_words(old, new)
    };
    let (mut o, mut n) = (0usize, 0usize);
    let mut same = 0usize; // non-blank chars both lines share
    let (mut del, mut ins): (Vec<Range<usize>>, Vec<Range<usize>>) = (Vec::new(), Vec::new());
    for ch in diff.iter_all_changes() {
        let len = ch.value().len();
        match ch.tag() {
            ChangeTag::Equal => {
                same += ch.value().chars().filter(|c| !c.is_whitespace()).count();
                o += len;
                n += len;
            }
            ChangeTag::Delete => {
                push_merged(&mut del, old, o..o + len);
                o += len;
            }
            ChangeTag::Insert => {
                push_merged(&mut ins, new, n..n + len);
                n += len;
            }
        }
    }
    // blanks match in any two lines, so they do not count towards similarity
    let total = [old, new]
        .iter()
        .map(|t| t.chars().filter(|c| !c.is_whitespace()).count())
        .sum::<usize>();
    if total > 0 && (2 * same) as f32 / (total as f32) < MIN_SIMILARITY {
        return (Vec::new(), Vec::new());
    }
    (del, ins)
}

/// Appends `r`, joining it to the previous range when only whitespace lies between them.
fn push_merged(v: &mut Vec<Range<usize>>, text: &str, r: Range<usize>) {
    if r.is_empty() {
        return;
    }
    match v.last_mut() {
        Some(last) if text[last.end..r.start].chars().all(char::is_whitespace) => last.end = r.end,
        _ => v.push(r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slices<'a>(s: &'a str, r: &[Range<usize>]) -> Vec<&'a str> {
        r.iter().map(|r| &s[r.clone()]).collect()
    }

    #[test]
    fn word_diff_marks_changed_tokens_only() {
        let (o, n) = word_spans("let x = foo(1, 2);", "let x = foo(1, 3);");
        assert_eq!(slices("let x = foo(1, 2);", &o), vec!["2"]);
        assert_eq!(slices("let x = foo(1, 3);", &n), vec!["3"]);
        let (o, n) = word_spans("alpha beta gamma", "alpha delta gamma");
        assert_eq!(slices("alpha beta gamma", &o), vec!["beta"]);
        assert_eq!(slices("alpha delta gamma", &n), vec!["delta"]);
        let (o, n) = word_spans("same", "same");
        assert!(o.is_empty() && n.is_empty());
        // pure insertion marks nothing on the old side
        let (o, n) = word_spans("a b", "a x b");
        assert!(o.is_empty(), "{o:?}");
        assert!(!n.is_empty());
    }

    #[test]
    fn word_diff_unicode_boundaries_valid() {
        let pairs = [
            ("héllo wörld 🎉 ok", "héllo wörld 🎊 ok"),
            ("e\u{301}cole nord", "e\u{301}cole sud"),
            (
                "👨\u{200d}👩\u{200d}👧 family",
                "👨\u{200d}👩\u{200d}👦 family",
            ),
            ("日本語 テスト", "日本人 テスト"),
            ("ab", "ac"),
            ("", "new"),
            ("old", ""),
        ];
        for (old, new) in pairs {
            let (o, n) = word_spans(old, new);
            for (s, ranges) in [(old, &o), (new, &n)] {
                for r in ranges {
                    assert!(r.start < r.end && r.end <= s.len(), "{s:?} {r:?}");
                    assert!(
                        s.is_char_boundary(r.start) && s.is_char_boundary(r.end),
                        "{s:?} {r:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn adjacent_changes_merge_and_long_lines_are_skipped() {
        let (o, _) = word_spans("a b c d", "a x y d");
        assert_eq!(o.len(), 1, "{o:?}");
        let long = "w ".repeat(MAX_WORD_DIFF_CHARS);
        let (o, n) = word_spans(&long, &format!("{long}x"));
        assert!(o.is_empty() && n.is_empty());
    }

    #[test]
    fn unrelated_lines_get_no_marks() {
        let (o, n) = word_spans("completely different text here", "nothing in common at all");
        assert!(o.is_empty() && n.is_empty(), "{o:?} {n:?}");
        // similar lines still do
        let (o, _) = word_spans("let value = compute(a, b);", "let value = compute(a, c);");
        assert!(!o.is_empty());
    }
}
