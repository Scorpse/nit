//! How a comment author is labelled on comment cards, and which comments are automated.
// Not called yet: the comment card painter and a "Hide bot comments" toggle will use it.
#![allow(dead_code)]

use prcore::comments::{looks_like_bot_name, Comment, UNKNOWN_AUTHOR};

/// The author name to paint and whether to add a small "bot" tag after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorLabel {
    pub text: String,
    pub bot: bool,
}

pub fn author_label(c: &Comment) -> AuthorLabel {
    let name = c.author.display_name.trim();
    AuthorLabel {
        text: if name.is_empty() {
            UNKNOWN_AUTHOR
        } else {
            name
        }
        .to_string(),
        bot: is_automated(c),
    }
}

/// Posted by an app, integration or bot: flagged by the provider, or a known tool name.
pub fn is_automated(c: &Comment) -> bool {
    c.author_is_bot || looks_like_bot_name(&c.author.display_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prcore::model::UserRef;

    fn by(name: &str, flagged: bool) -> Comment {
        Comment {
            author: UserRef {
                id: String::new(),
                display_name: name.into(),
                login: None,
            },
            author_is_bot: flagged,
            ..Comment::default()
        }
    }

    #[test]
    fn person_has_plain_label() {
        let l = author_label(&by("Ada Lovelace", false));
        assert_eq!(
            l,
            AuthorLabel {
                text: "Ada Lovelace".into(),
                bot: false
            }
        );
        assert!(!is_automated(&by("Ada Lovelace", false)));
    }

    #[test]
    fn provider_flag_marks_bot_whatever_the_name() {
        let l = author_label(&by("Acme Platform", true));
        assert!(l.bot);
        assert_eq!(l.text, "Acme Platform");
        assert!(is_automated(&by("Acme Platform", true)));
    }

    #[test]
    fn known_tool_name_marks_bot_without_flag() {
        for name in [
            "Rovo Dev",
            "dependabot[bot]",
            "Bitbucket Pipelines",
            "CodeRabbit",
        ] {
            assert!(author_label(&by(name, false)).bot, "{name}");
            assert!(is_automated(&by(name, false)), "{name}");
        }
    }

    #[test]
    fn blank_name_falls_back_to_unknown_author() {
        assert_eq!(author_label(&by("  ", true)).text, "Unknown author");
        assert_eq!(author_label(&by("", false)).text, "Unknown author");
        assert_eq!(author_label(&by("  Ben ", false)).text, "Ben");
    }
}
