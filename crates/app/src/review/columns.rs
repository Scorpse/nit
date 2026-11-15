//! The one column system of the diff: tabs expand to `TAB_WIDTH` stops, double-width
//! characters (unicode-width 2) advance two columns, combining marks, joiners and CR advance
//! none and stay with their base character. Painting (`diffview`, `ui`) and selection
//! (`selection`) both measure text only through this module.

use std::iter::Peekable;
use std::str::CharIndices;

use unicode_width::UnicodeWidthChar;

use super::diffview::TAB_WIDTH;

/// One character with the zero-width characters that belong to it (combining marks, joiners,
/// the character after a joiner, CR): the unit the pointer can land between.
pub struct Cluster {
    pub col0: usize,
    pub col1: usize,
    pub b0: usize,
    pub b1: usize,
    pub base: char,
}

pub struct Clusters<'a> {
    it: Peekable<CharIndices<'a>>,
    col: usize,
}

pub fn clusters(text: &str) -> Clusters<'_> {
    Clusters {
        it: text.char_indices().peekable(),
        col: 0,
    }
}

fn char_w(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(1)
}

const ZWJ: char = '\u{200d}';

fn extends(prev: char, ch: char) -> bool {
    ch == '\r'
        || prev == ZWJ
        || ('\u{1F3FB}'..='\u{1F3FF}').contains(&ch)
        || (ch != '\t' && UnicodeWidthChar::width(ch) == Some(0))
}

impl Iterator for Clusters<'_> {
    type Item = Cluster;

    fn next(&mut self) -> Option<Cluster> {
        let (b0, base) = self.it.next()?;
        let col0 = self.col;
        let w = match base {
            '\t' => (col0 / TAB_WIDTH + 1) * TAB_WIDTH - col0,
            '\r' => 0,
            c => char_w(c),
        };
        let (mut b1, mut last) = (b0 + base.len_utf8(), base);
        while let Some(&(b, ch)) = self.it.peek() {
            if !extends(last, ch) {
                break;
            }
            (b1, last) = (b + ch.len_utf8(), ch);
            self.it.next();
        }
        self.col += w;
        Some(Cluster {
            col0,
            col1: self.col,
            b0,
            b1,
            base,
        })
    }
}

/// Width of the whole text in display columns.
pub fn display_cols(text: &str) -> usize {
    // printable ASCII (no tab, CR or other control) is one column per byte: most code rows
    if text.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return text.len();
    }
    clusters(text).last().map_or(0, |c| c.col1)
}

/// Display column of byte offset `byte` (the start of the character it is in).
#[cfg(test)]
pub fn byte_to_col(text: &str, byte: usize) -> usize {
    let mut col = 0;
    for c in clusters(text) {
        if c.b0 > byte {
            break;
        }
        col = if byte >= c.b1 { c.col1 } else { c.col0 };
    }
    col
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_fast_path_matches_the_cluster_walk() {
        let printable: String = (0x20u8..0x7f).map(char::from).collect();
        let cases = [
            "",
            "fn main() { let x = 1; }",
            printable.as_str(),
            "\tindented",
            "a\tb\tc",
            "crlf\r",
            "bell\x07",
            "nul\0",
            "del\x7f",
            "héllo",
            "日本語",
            "e\u{301}",
            "👍🏽 ok",
        ];
        for s in cases {
            assert_eq!(
                display_cols(s),
                clusters(s).last().map_or(0, |c| c.col1),
                "{s:?}"
            );
        }
    }
}
