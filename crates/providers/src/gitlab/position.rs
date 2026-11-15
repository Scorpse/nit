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
mod tests;
