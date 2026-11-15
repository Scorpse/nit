//! GitLab diff positions (spec 7.3, anchor model `DiffRefs`): where a line of an MR's diff
//! is, as the `position` of a new discussion ([discussions]).
//!
//! - an added line sends `new_line` only, a removed line `old_line` only, a context line both;
//! - a multi-line comment adds `line_range { start, end }`, each end with its `line_code`
//!   (`sha1(path)_<old>_<new>`, the counters GitLab's own diff parser gives that line) and
//!   its `type` (`new`, `old`, or none for a context line);
//! - a file comment is `position_type: file` with no lines.
//!
//! Lines are looked up in GitLab's own diff of the reviewed version; a line that is not in
//! one of its hunks is not placed (the caller refuses it).
//!
//! [discussions]: https://docs.gitlab.com/api/discussions/

use prcore::draft::Side;
use serde_json::{json, Map, Value};

/// What a line is in the MR diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Added,
    Removed,
    Context,
}

/// One line of a file's diff with both of GitLab's counters: for an added line `old` is the
/// old-side counter at that point (the next old line), for a removed line `new` likewise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: u32,
    pub new: u32,
}

impl DiffLine {
    /// `old_line` as GitLab wants it (none for an added line).
    pub fn old_line(&self) -> Option<u32> {
        (self.kind != LineKind::Added).then_some(self.old)
    }

    /// `new_line` as GitLab wants it (none for a removed line).
    pub fn new_line(&self) -> Option<u32> {
        (self.kind != LineKind::Removed).then_some(self.new)
    }
}

/// `@@ -a[,b] +c[,d] @@` -> (a, c).
fn hunk_start(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let first = |s: &str| s.split(',').next()?.parse::<u32>().ok();
    Some((first(old)?, first(new)?))
}

/// Every line of a unified diff (one file, as GitLab's `diff` field has it) with its
/// counters. Lines before the first hunk header and `\ No newline` markers are skipped.
pub fn diff_lines(diff: &str) -> Vec<DiffLine> {
    let mut out = Vec::new();
    let mut at: Option<(u32, u32)> = None;
    for l in diff.lines() {
        if l.starts_with("@@") {
            at = hunk_start(l);
            continue;
        }
        let Some((old, new)) = at.as_mut() else {
            continue;
        };
        let kind = match l.as_bytes().first() {
            Some(b'+') => LineKind::Added,
            Some(b'-') => LineKind::Removed,
            Some(b' ') => LineKind::Context,
            // `\ No newline at end of file`
            Some(b'\\') => continue,
            // an empty context line whose blank was stripped
            None => LineKind::Context,
            _ => continue,
        };
        out.push(DiffLine {
            kind,
            old: *old,
            new: *new,
        });
        match kind {
            LineKind::Added => *new += 1,
            LineKind::Removed => *old += 1,
            LineKind::Context => {
                *old += 1;
                *new += 1;
            }
        }
    }
    out
}

/// The diff line at `line` on `side`: on the new side an added or context line, on the old
/// side a removed or context line. None when the line is in no hunk.
pub fn locate(diff: &str, side: Side, line: u32) -> Option<DiffLine> {
    diff_lines(diff).into_iter().find(|d| match side {
        Side::New => d.kind != LineKind::Removed && d.new == line,
        Side::Old => d.kind != LineKind::Added && d.old == line,
    })
}

/// GitLab's `line_code` of a diff line: `sha1(path)_<old>_<new>`.
pub fn line_code(path: &str, l: &DiffLine) -> String {
    format!("{}_{}_{}", sha1_hex(path.as_bytes()), l.old, l.new)
}

/// The three SHAs of one MR diff version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffRefs {
    pub base: String,
    pub start: String,
    pub head: String,
}

fn base_position(
    refs: &DiffRefs,
    old_path: &str,
    new_path: &str,
    kind: &str,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("base_sha".into(), refs.base.as_str().into());
    m.insert("start_sha".into(), refs.start.as_str().into());
    m.insert("head_sha".into(), refs.head.as_str().into());
    m.insert("position_type".into(), kind.into());
    m.insert("new_path".into(), new_path.into());
    m.insert("old_path".into(), old_path.into());
    m
}

/// `position` of a file comment.
pub fn file_position(refs: &DiffRefs, old_path: &str, new_path: &str) -> Value {
    Value::Object(base_position(refs, old_path, new_path, "file"))
}

fn range_end(path: &str, l: &DiffLine) -> Value {
    let kind = match l.kind {
        LineKind::Added => json!("new"),
        LineKind::Removed => json!("old"),
        LineKind::Context => Value::Null,
    };
    json!({
        "line_code": line_code(path, l),
        "type": kind,
        "old_line": l.old_line(),
        "new_line": l.new_line(),
    })
}

/// `position` of a line comment ending on `end`; `start` (when another line) makes it a
/// multi-line comment (`line_range`). The `line_code`s are computed on `new_path`.
pub fn text_position(
    refs: &DiffRefs,
    old_path: &str,
    new_path: &str,
    end: &DiffLine,
    start: Option<&DiffLine>,
) -> Value {
    let mut m = base_position(refs, old_path, new_path, "text");
    if let Some(n) = end.new_line() {
        m.insert("new_line".into(), n.into());
    }
    if let Some(o) = end.old_line() {
        m.insert("old_line".into(), o.into());
    }
    if let Some(s) = start.filter(|s| *s != end) {
        m.insert(
            "line_range".into(),
            json!({ "start": range_end(new_path, s), "end": range_end(new_path, end) }),
        );
    }
    Value::Object(m)
}

/// SHA-1 of `data` as lowercase hex (GitLab's `line_code` prefix; not used for security).
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, c) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `small.txt` of the live throwaway MR (GitLab 19.5.1): line 3 removed, "added A" added
    /// after line 5, line 8 changed.
    const SMALL: &str = "@@ -1,10 +1,10 @@\n line 1\n line 2\n-line 3\n line 4\n line 5\n+added A\n line 6\n line 7\n-line 8\n+line 8 changed\n line 9\n line 10\n";

    fn refs() -> DiffRefs {
        DiffRefs {
            base: "b".into(),
            start: "s".into(),
            head: "h".into(),
        }
    }

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            sha1_hex(&[b'a'; 1000]),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
    }

    #[test]
    fn added_removed_and_context_lines_get_both_counters() {
        use LineKind::*;
        let d = |kind, old, new| DiffLine { kind, old, new };
        assert_eq!(locate(SMALL, Side::New, 5), Some(d(Added, 6, 5)));
        assert_eq!(locate(SMALL, Side::Old, 3), Some(d(Removed, 3, 3)));
        assert_eq!(locate(SMALL, Side::New, 3), Some(d(Context, 4, 3)));
        // a context line found from either side
        assert_eq!(locate(SMALL, Side::Old, 4), Some(d(Context, 4, 3)));
        assert_eq!(locate(SMALL, Side::New, 8), Some(d(Added, 9, 8)));
        assert_eq!(locate(SMALL, Side::Old, 8), Some(d(Removed, 8, 8)));
        // not in the diff, or the wrong side of a change
        assert_eq!(locate(SMALL, Side::New, 11), None);
        assert_eq!(locate(SMALL, Side::Old, 11), None);
        assert_eq!(locate(SMALL, Side::Old, 0), None);
        let far = "@@ -2197,7 +2197,7 @@ big line 2196\n big line 2197\n-big line 2200\n+big line 2200 changed\n";
        assert_eq!(locate(far, Side::New, 2198), Some(d(Added, 2199, 2198)));
        assert_eq!(locate(far, Side::New, 100), None);
        // two hunks; no-newline markers are not lines
        let two =
            "@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+B\n@@ -40 +40,2 @@\n x\n+y\n";
        assert_eq!(locate(two, Side::New, 2), Some(d(Added, 3, 2)));
        assert_eq!(locate(two, Side::New, 41), Some(d(Added, 41, 41)));
        assert_eq!(locate(two, Side::New, 20), None);
    }

    #[test]
    fn line_code_format() {
        let l = DiffLine {
            kind: LineKind::Added,
            old: 6,
            new: 5,
        };
        assert_eq!(
            line_code("small.txt", &l),
            format!("{}_6_5", sha1_hex(b"small.txt"))
        );
        assert_eq!(line_code("a", &l).len(), 40 + 4);
    }

    #[test]
    fn added_line_sends_new_line_only() {
        let l = locate(SMALL, Side::New, 5).unwrap();
        let p = text_position(&refs(), "small.txt", "small.txt", &l, None);
        assert_eq!(
            p,
            json!({"base_sha": "b", "start_sha": "s", "head_sha": "h", "position_type": "text",
                   "new_path": "small.txt", "old_path": "small.txt", "new_line": 5})
        );
    }

    #[test]
    fn deleted_line_sends_old_line_only() {
        let l = locate(SMALL, Side::Old, 3).unwrap();
        let p = text_position(&refs(), "small.txt", "small.txt", &l, None);
        assert_eq!(p["old_line"], 3);
        assert!(p.get("new_line").is_none());
    }

    #[test]
    fn context_sends_both() {
        let l = locate(SMALL, Side::New, 3).unwrap();
        let p = text_position(&refs(), "o.txt", "small.txt", &l, Some(&l));
        assert_eq!(
            (p["old_line"].clone(), p["new_line"].clone()),
            (json!(4), json!(3))
        );
        assert_eq!(p["old_path"], "o.txt");
        // the start equal to the end is one line: no range
        assert!(p.get("line_range").is_none());
    }

    #[test]
    fn multi_line_sends_a_line_range_with_codes_and_types() {
        let start = locate(SMALL, Side::New, 6).unwrap();
        let end = locate(SMALL, Side::New, 8).unwrap();
        let p = text_position(&refs(), "small.txt", "small.txt", &end, Some(&start));
        let sha = sha1_hex(b"small.txt");
        assert_eq!(
            p["line_range"],
            json!({
                "start": {"line_code": format!("{sha}_6_6"), "type": null, "old_line": 6, "new_line": 6},
                "end": {"line_code": format!("{sha}_9_8"), "type": "new", "old_line": null, "new_line": 8},
            })
        );
        assert_eq!(p["new_line"], 8);
        let removed = locate(SMALL, Side::Old, 3).unwrap();
        let p = text_position(&refs(), "small.txt", "small.txt", &end, Some(&removed));
        assert_eq!(p["line_range"]["start"]["type"], "old");
        assert_eq!(p["line_range"]["start"]["new_line"], Value::Null);
    }

    #[test]
    fn file_position_has_no_lines() {
        assert_eq!(
            file_position(&refs(), "old-name.txt", "new-name.txt"),
            json!({"base_sha": "b", "start_sha": "s", "head_sha": "h", "position_type": "file",
                   "new_path": "new-name.txt", "old_path": "old-name.txt"})
        );
    }
}
