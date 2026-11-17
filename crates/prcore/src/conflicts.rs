//! Merge conflicts of a PR with its destination branch, as `git merge-tree` reports them:
//! the conflicted files and what kind of conflict each is, and the conflict markers inside
//! one file's merged text.

/// What kind of conflict a file has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    /// Changed on both sides.
    Content,
    /// Added on both sides, differently.
    BothAdded,
    /// Deleted on the destination branch, changed in the PR.
    DeletedOnDest,
    /// Deleted in the PR, changed on the destination branch.
    DeletedInPr,
    /// Anything else (a rename or a type change, say).
    Other,
}

impl ConflictKind {
    /// One line for the file list; `dest` is the destination branch's name.
    pub fn describe(self, dest: &str) -> String {
        match self {
            ConflictKind::Content => "changed on both sides".into(),
            ConflictKind::BothAdded => "added on both sides".into(),
            ConflictKind::DeletedOnDest => format!("deleted on {dest}, changed in this PR"),
            ConflictKind::DeletedInPr => format!("deleted in this PR, changed on {dest}"),
            ConflictKind::Other => "conflicting change".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictFile {
    pub path: String,
    pub kind: ConflictKind,
}

/// The result of trial-merging the PR's head into its destination. No files: it merges
/// cleanly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictReport {
    /// The tree git wrote for the merge: conflicted files carry their conflict markers.
    pub tree: String,
    pub files: Vec<ConflictFile>,
}

/// Reads the output of `git merge-tree --write-tree -z --no-messages <dest> <head>`: the
/// merged tree's id, then one `<mode> <object> <stage>\t<path>` entry per conflicted stage.
/// `None` when it does not look like that.
pub fn parse_merge_tree(out: &[u8]) -> Option<ConflictReport> {
    let mut parts = out.split(|b| *b == 0);
    let tree = std::str::from_utf8(parts.next()?).ok()?.trim().to_string();
    if !is_object_id(&tree) {
        return None;
    }
    // (path, stages present as a bit set: bit 1, 2 or 3)
    let mut seen: Vec<(String, u8)> = Vec::new();
    for entry in parts {
        if entry.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(entry);
        let (meta, path) = text.split_once('\t')?;
        let stage: u8 = meta.rsplit(' ').next()?.parse().ok()?;
        if !(1..=3).contains(&stage) {
            return None;
        }
        match seen.iter_mut().find(|(p, _)| p == path) {
            Some((_, mask)) => *mask |= 1 << stage,
            None => seen.push((path.to_string(), 1 << stage)),
        }
    }
    let files = seen
        .into_iter()
        .map(|(path, mask)| ConflictFile {
            path,
            kind: kind_of(mask),
        })
        .collect();
    Some(ConflictReport { tree, files })
}

fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Stage 1 is the common ancestor, 2 the destination ("ours"), 3 the PR ("theirs").
fn kind_of(mask: u8) -> ConflictKind {
    let (base, ours, theirs) = (mask & 2 != 0, mask & 4 != 0, mask & 8 != 0);
    match (base, ours, theirs) {
        (true, true, true) => ConflictKind::Content,
        (false, true, true) => ConflictKind::BothAdded,
        (true, true, false) => ConflictKind::DeletedInPr,
        (true, false, true) => ConflictKind::DeletedOnDest,
        _ => ConflictKind::Other,
    }
}

/// One conflicted region of a file: the destination's lines and the PR's lines (and the
/// common ancestor's, when git wrote them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub ours: Vec<String>,
    pub base: Option<Vec<String>>,
    pub theirs: Vec<String>,
}

/// A file's merged text, cut into unconflicted lines and conflicted regions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Context(Vec<String>),
    Conflict(Hunk),
}

/// Whether `line` is a conflict marker of kind `c` (`<`, `|`, `=` or `>`): seven of them,
/// then the end of the line or a space.
fn marker(line: &str, c: char) -> bool {
    let rest = line.trim_start_matches(c);
    line.len() - rest.len() == 7 && (rest.is_empty() || rest.starts_with(' '))
}

/// Cuts `text` at its conflict markers. A region that is never closed stays plain text.
pub fn parse_markers(text: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut context: Vec<String> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if !marker(lines[i], '<') {
            context.push(lines[i].to_string());
            i += 1;
            continue;
        }
        match read_hunk(&lines, i + 1) {
            Some((hunk, next)) => {
                if !context.is_empty() {
                    out.push(Segment::Context(std::mem::take(&mut context)));
                }
                out.push(Segment::Conflict(hunk));
                i = next;
            }
            None => {
                context.push(lines[i].to_string());
                i += 1;
            }
        }
    }
    if !context.is_empty() {
        out.push(Segment::Context(context));
    }
    out
}

/// The region whose opening marker is just before `from`: its hunk and the line after the
/// closing marker.
fn read_hunk(lines: &[&str], from: usize) -> Option<(Hunk, usize)> {
    let (mut ours, mut base, mut theirs) = (Vec::new(), None::<Vec<String>>, Vec::new());
    let mut side = 0; // 0 ours, 1 base, 2 theirs
    for (n, line) in lines.iter().enumerate().skip(from) {
        if side == 0 && marker(line, '|') {
            side = 1;
            base = Some(Vec::new());
        } else if side < 2 && marker(line, '=') {
            side = 2;
        } else if side == 2 && marker(line, '>') {
            return Some((Hunk { ours, base, theirs }, n + 1));
        } else if marker(line, '<') {
            return None;
        } else {
            let target = match side {
                0 => &mut ours,
                1 => base.as_mut()?,
                _ => &mut theirs,
            };
            target.push(line.to_string());
        }
    }
    None
}

/// The conflicts as text for a chat, a ticket or an agent: what is merged into what, the
/// files, and the commands that reproduce the conflict locally.
pub fn summary_markdown(
    pr_title: &str,
    source_branch: &str,
    dest_branch: &str,
    report: &ConflictReport,
) -> String {
    let mut s = format!(
        "Merge conflicts: \"{pr_title}\" ({source_branch} into {dest_branch})\n\n{} conflicted file(s):\n",
        report.files.len()
    );
    for f in &report.files {
        s.push_str(&format!(
            "- `{}`: {}\n",
            f.path,
            f.kind.describe(dest_branch)
        ));
    }
    s.push_str(&format!(
        "\nTo reproduce:\n\n```\ngit fetch origin\ngit switch {source_branch}\ngit merge origin/{dest_branch}\n```\n"
    ));
    s
}

#[cfg(test)]
mod tests;
