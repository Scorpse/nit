//! Pure parsing of `git diff --raw -z -M` / `--numstat -z -M` output and unified patches.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
    pub binary: bool,
    /// Whether `additions`, `deletions` and `binary` come from `--numstat` yet. A list from
    /// `--raw` alone (`parse_diff_raw`) has them unknown: zero and false until
    /// `apply_counts`.
    pub counts_known: bool,
    /// Post-image object id. Callers must pass `--full-index` to `git diff --raw`
    /// so this is the full id. Deleted files have an all-zero oid.
    pub new_oid: String,
}

impl DiffFile {
    /// True when `new_oid` is all zeros (any length), i.e. there is no post-image.
    pub fn is_deleted_oid(&self) -> bool {
        !self.new_oid.is_empty() && self.new_oid.bytes().all(|b| b == b'0')
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    HunkHeader,
    Context,
    Add,
    Del,
    NoNewline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    /// Line content without the leading `+`/`-`/space (CR, if any, is kept).
    pub text: String,
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Splits NUL-terminated records; a trailing terminator yields no empty last token.
fn nul_tokens(data: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = data.split(|&b| b == 0).collect();
    if v.last().is_some_and(|t| t.is_empty()) {
        v.pop();
    }
    v
}

/// One file's line counts from `git diff --numstat -z -M`; `path` is the new path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCounts {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    pub binary: bool,
}

/// Parses `git diff --numstat -z -M` output, in git's order.
pub fn parse_numstat(numstat_z: &[u8]) -> Vec<FileCounts> {
    let toks = nul_tokens(numstat_z);
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        i += 1;
        let mut parts = t.splitn(3, |&b| b == b'\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let path = if path.is_empty() {
            // rename/copy: old and new paths follow as separate tokens
            let new = toks.get(i + 1).copied().unwrap_or_default();
            i += 2;
            new
        } else {
            path
        };
        let binary = a == b"-" || d == b"-";
        let num = |s: &[u8]| {
            std::str::from_utf8(s)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        };
        out.push(FileCounts {
            path: lossy(path),
            additions: if binary { 0 } else { num(a) },
            deletions: if binary { 0 } else { num(d) },
            binary,
        });
    }
    out
}

/// Fills in the counts of `files` from `counts`, matched by path (the order of `files` does
/// not matter). Every file is known afterwards: one numstat has no entry for is 0/0, not
/// binary, as with `parse_diff_index`.
pub fn apply_counts(files: &mut [DiffFile], counts: &[FileCounts]) {
    let by_path: std::collections::HashMap<&str, &FileCounts> =
        counts.iter().map(|c| (c.path.as_str(), c)).collect();
    for f in files {
        let c = by_path.get(f.path.as_str());
        f.additions = c.map_or(0, |c| c.additions);
        f.deletions = c.map_or(0, |c| c.deletions);
        f.binary = c.is_some_and(|c| c.binary);
        f.counts_known = true;
    }
}

/// Joins `git diff --raw -z -M` and `git diff --numstat -z -M` output by order (both list
/// the files in the same order). The counts are known; see `parse_diff_raw` for the list
/// alone.
pub fn parse_diff_index(raw_z: &[u8], numstat_z: &[u8]) -> Vec<DiffFile> {
    let stats = parse_numstat(numstat_z);
    let mut files = parse_diff_raw(raw_z);
    for (idx, f) in files.iter_mut().enumerate() {
        if let Some(s) = stats.get(idx) {
            (f.additions, f.deletions, f.binary) = (s.additions, s.deletions, s.binary);
        }
        f.counts_known = true;
    }
    files
}

/// The files of `git diff --raw -z -M` output, with their counts unknown (`counts_known`
/// false; zero and not binary until `apply_counts`).
pub fn parse_diff_raw(raw_z: &[u8]) -> Vec<DiffFile> {
    let toks = nul_tokens(raw_z);
    let mut files = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let meta = toks[i];
        i += 1;
        if meta.first() != Some(&b':') {
            continue;
        }
        let meta = lossy(&meta[1..]);
        let f: Vec<&str> = meta.split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        let new_oid = f[3].to_string();
        let letter = f[4].chars().next().unwrap_or('M');
        let status = match letter {
            'A' => FileStatus::Added,
            'D' => FileStatus::Deleted,
            'R' => FileStatus::Renamed,
            'C' => FileStatus::Copied,
            'T' => FileStatus::TypeChanged,
            _ => FileStatus::Modified,
        };
        let two = matches!(status, FileStatus::Renamed | FileStatus::Copied);
        let (old_path, path) = if two {
            if i + 1 >= toks.len() {
                break;
            }
            let p = (Some(lossy(toks[i])), lossy(toks[i + 1]));
            i += 2;
            p
        } else {
            if i >= toks.len() {
                break;
            }
            let p = (None, lossy(toks[i]));
            i += 1;
            p
        };
        files.push(DiffFile {
            path,
            old_path,
            status,
            additions: 0,
            deletions: 0,
            binary: false,
            counts_known: false,
            new_oid,
        });
    }
    files
}

/// Parses `@@ -a[,b] +c[,d] @@` into (a, b, c, d); a missing count means 1.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split(" @@").next()?;
    let range = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (a, b) = range(old)?;
    let (c, d) = range(new)?;
    Some((a, b, c, d))
}

/// Parses a unified patch into display rows. File headers are skipped.
pub fn parse_patch(patch: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let (mut old_left, mut new_left) = (0u32, 0u32);
    // lines are walked in place (no intermediate list of every line); a final newline ends
    // the last line (an empty patch is one empty line, which is never a row). Sizing `rows`
    // from a newline count first was measured slower than letting it grow.
    let body = patch.strip_suffix('\n').unwrap_or(patch);
    for line in body.split('\n') {
        if line.starts_with("diff ") {
            // New file section: end any (miscounted) hunk so its headers are skipped.
            old_left = 0;
            new_left = 0;
            continue;
        }
        let in_hunk = old_left > 0 || new_left > 0;
        // No content line can start with '@', so a valid header always starts a new hunk.
        if line.starts_with("@@ ") {
            if let Some((a, b, c, d)) = parse_hunk_header(line) {
                old_no = a;
                new_no = c;
                old_left = b;
                new_left = d;
                rows.push(Row {
                    kind: RowKind::HunkHeader,
                    old_line: None,
                    new_line: None,
                    text: line.to_string(),
                });
            }
            continue;
        }
        if line.starts_with('\\') {
            if !rows.is_empty() {
                rows.push(Row {
                    kind: RowKind::NoNewline,
                    old_line: None,
                    new_line: None,
                    text: line.to_string(),
                });
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (RowKind::Add, &line[1..]),
            Some(b'-') => (RowKind::Del, &line[1..]),
            Some(b' ') => (RowKind::Context, &line[1..]),
            None => (RowKind::Context, ""),
            Some(_) => continue,
        };
        let (old_line, new_line) = match kind {
            RowKind::Add => {
                new_left = new_left.saturating_sub(1);
                let n = new_no;
                new_no = new_no.saturating_add(1);
                (None, Some(n))
            }
            RowKind::Del => {
                old_left = old_left.saturating_sub(1);
                let o = old_no;
                old_no = old_no.saturating_add(1);
                (Some(o), None)
            }
            _ => {
                old_left = old_left.saturating_sub(1);
                new_left = new_left.saturating_sub(1);
                let (o, n) = (old_no, new_no);
                old_no = old_no.saturating_add(1);
                new_no = new_no.saturating_add(1);
                (Some(o), Some(n))
            }
        };
        rows.push(Row {
            kind,
            old_line,
            new_line,
            text: text.to_string(),
        });
    }
    rows
}

/// True for lock files, minified/generated output and vendored paths (collapsed by default).
pub fn is_generated(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    if matches!(
        name,
        "Cargo.lock" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml"
    ) {
        return true;
    }
    if [".min.js", ".min.css", ".lock", ".snap", ".pb.go", ".g.cs"]
        .iter()
        .any(|s| name.ends_with(s))
    {
        return true;
    }
    let rooted = format!("/{path}");
    ["/generated/", "/dist/", "/vendor/"]
        .iter()
        .any(|d| rooted.contains(d))
}

#[cfg(test)]
mod tests;
