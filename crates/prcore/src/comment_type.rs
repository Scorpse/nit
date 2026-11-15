//! Optional comment types ("nit", "issue", ...): stored with a draft, and posted as a
//! leading `**type:** ` text prefix. Comments of others that follow the same convention
//! are recognised by `split_prefix`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CommentType {
    Issue,
    Nit,
    Question,
    Suggestion,
    Praise,
}

/// Every type with its word, in the order the composer offers them. Adding a type is a
/// variant plus a row here.
pub const COMMENT_TYPES: [(CommentType, &str); 5] = [
    (CommentType::Issue, "issue"),
    (CommentType::Nit, "nit"),
    (CommentType::Question, "question"),
    (CommentType::Suggestion, "suggestion"),
    (CommentType::Praise, "praise"),
];

impl CommentType {
    /// The lowercase word: "nit", "issue", ...; also the stored form.
    pub fn as_str(self) -> &'static str {
        COMMENT_TYPES
            .iter()
            .find(|(t, _)| *t == self)
            .map_or("", |(_, w)| w)
    }

    /// The type named `word` (case-insensitive).
    pub fn parse(word: &str) -> Option<CommentType> {
        COMMENT_TYPES
            .iter()
            .find(|(_, w)| w.eq_ignore_ascii_case(word))
            .map(|(t, _)| *t)
    }

    /// `**nit:** ` as put in front of the posted text.
    pub fn prefix(self) -> String {
        format!("**{}:** ", self.as_str())
    }

    /// The next type in the composer's cycle: none, then each type, then none again.
    pub fn cycle(t: Option<CommentType>, forward: bool) -> Option<CommentType> {
        let n = COMMENT_TYPES.len() + 1;
        // position 0 is "none", 1..=n-1 the types
        let at = t.map_or(0, |t| {
            1 + COMMENT_TYPES.iter().position(|(x, _)| *x == t).unwrap_or(0)
        });
        let next = if forward {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        };
        (next > 0).then(|| COMMENT_TYPES[next - 1].0)
    }
}

/// The leading type of a comment body and the text after it: `**nit:** text` gives
/// `(Some(Nit), "text")`. The word is case-insensitive; the prefix must be followed by
/// whitespace or end the body. Anything else gives `(None, body)`.
pub fn split_prefix(body: &str) -> (Option<CommentType>, &str) {
    let parsed = body.strip_prefix("**").and_then(|rest| {
        let (word, after) = rest.split_once(":**")?;
        let t = CommentType::parse(word)?;
        let rest = match after.chars().next() {
            None => after,
            Some(c) if c.is_whitespace() => {
                let r = after.trim_start_matches([' ', '\t']);
                r.strip_prefix("\r\n")
                    .or_else(|| r.strip_prefix('\n'))
                    .unwrap_or(r)
            }
            Some(_) => return None,
        };
        Some((t, rest))
    });
    match parsed {
        Some((t, rest)) => (Some(t), rest),
        None => (None, body),
    }
}

/// The text to post for a draft of type `t`: `**t:** text`. Idempotent: a text that already
/// starts with the same type's prefix is returned unchanged, so the prefix never doubles.
pub fn with_prefix(t: Option<CommentType>, text: &str) -> String {
    match t {
        None => text.to_string(),
        Some(t) if split_prefix(text).0 == Some(t) => text.to_string(),
        Some(t) => format!("{}{text}", t.prefix()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_round_trip_and_are_unique() {
        for (t, w) in COMMENT_TYPES {
            assert_eq!(t.as_str(), w);
            assert_eq!(CommentType::parse(w), Some(t));
            assert_eq!(CommentType::parse(&w.to_uppercase()), Some(t));
        }
        assert_eq!(CommentType::parse("todo"), None);
        assert_eq!(CommentType::parse(""), None);
    }

    #[test]
    fn prefix_is_bold_word_and_colon() {
        assert_eq!(CommentType::Nit.prefix(), "**nit:** ");
        assert_eq!(CommentType::Issue.prefix(), "**issue:** ");
        assert_eq!(with_prefix(Some(CommentType::Nit), "x"), "**nit:** x");
        assert_eq!(with_prefix(None, "x"), "x");
    }

    #[test]
    fn prefix_round_trips_for_every_type() {
        let texts = [
            "plain",
            "two\nlines",
            "",
            "über straße ✓",
            "`code` here",
            "**bold** start",
        ];
        for (t, _) in COMMENT_TYPES {
            for x in texts {
                let posted = with_prefix(Some(t), x);
                assert_eq!(split_prefix(&posted), (Some(t), x), "{posted:?}");
            }
        }
    }

    #[test]
    fn prefix_is_never_doubled() {
        let once = with_prefix(Some(CommentType::Nit), "rename");
        let twice = with_prefix(Some(CommentType::Nit), &once);
        assert_eq!(once, twice);
        assert_eq!(twice.matches("**nit:**").count(), 1);
        // typed by hand in another case: kept as typed, not doubled
        assert_eq!(
            with_prefix(Some(CommentType::Nit), "**Nit:** rename"),
            "**Nit:** rename"
        );
        // another type's prefix is the user's text: the chosen type still leads
        assert_eq!(
            with_prefix(Some(CommentType::Nit), "**issue:** x"),
            "**nit:** **issue:** x"
        );
    }

    #[test]
    fn split_recognises_the_convention_of_others() {
        use CommentType::*;
        assert_eq!(split_prefix("**Issue:** breaks"), (Some(Issue), "breaks"));
        assert_eq!(
            split_prefix("**question:**\nwhy?"),
            (Some(Question), "why?")
        );
        assert_eq!(split_prefix("**praise:**\r\nnice"), (Some(Praise), "nice"));
        assert_eq!(split_prefix("**nit:**"), (Some(Nit), ""));
        assert_eq!(
            split_prefix("**nit:**  two spaces"),
            (Some(Nit), "two spaces")
        );
        for plain in [
            "**nit:**x",
            "**note:** x",
            "nit: x",
            "**nit**: x",
            " **nit:** x",
            "**bold** text",
            "",
            "**",
        ] {
            assert_eq!(split_prefix(plain), (None, plain), "{plain:?}");
        }
    }

    #[test]
    fn cycle_visits_none_and_every_type() {
        let mut t = None;
        let mut seen = Vec::new();
        for _ in 0..=COMMENT_TYPES.len() {
            t = CommentType::cycle(t, true);
            seen.push(t);
        }
        assert_eq!(seen.last(), Some(&None), "back to none");
        assert_eq!(seen[0], Some(COMMENT_TYPES[0].0));
        assert_eq!(CommentType::cycle(None, false), Some(CommentType::Praise));
        assert_eq!(CommentType::cycle(Some(CommentType::Issue), false), None);
    }
}
