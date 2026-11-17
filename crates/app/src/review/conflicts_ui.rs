//! The merge conflicts of the open PR with its destination: the files, and the merged text of
//! the one being looked at, cut into conflicted regions with a few lines of context.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use prcore::conflicts::{parse_markers, ConflictReport, Segment};

/// Lines of unconflicted text kept on each side of a conflicted region.
pub const CONTEXT: usize = 3;

/// What a shown line is, for its colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// "N unchanged lines".
    Skip,
    Context,
    /// "Conflict 2 of 3".
    Head,
    /// The line that names a side ("main (destination)").
    OursLabel,
    Ours,
    BaseLabel,
    Base,
    TheirsLabel,
    Theirs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    pub text: String,
}

fn line(kind: Kind, text: impl Into<String>) -> Line {
    Line {
        kind,
        text: text.into(),
    }
}

/// The merged text of a file as lines to show: every conflicted region with both sides, the
/// unconflicted text around it cut to `CONTEXT` lines. Empty when the text has no conflict
/// markers (a file deleted on one side has none).
pub fn lines_of(segments: &[Segment], dest: &str) -> Vec<Line> {
    let total = segments
        .iter()
        .filter(|s| matches!(s, Segment::Conflict(_)))
        .count();
    if total == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut n = 0;
    for (i, seg) in segments.iter().enumerate() {
        match seg {
            Segment::Context(ls) => {
                let first = i == 0;
                let last = i + 1 == segments.len();
                let head = if first { 0 } else { ls.len().min(CONTEXT) };
                let tail = if last { 0 } else { ls.len().min(CONTEXT) };
                if head + tail >= ls.len() {
                    out.extend(ls.iter().map(|l| line(Kind::Context, l)));
                    continue;
                }
                out.extend(ls[..head].iter().map(|l| line(Kind::Context, l)));
                let cut = ls.len() - head - tail;
                out.push(line(Kind::Skip, format!("{cut} unchanged lines")));
                out.extend(ls[ls.len() - tail..].iter().map(|l| line(Kind::Context, l)));
            }
            Segment::Conflict(h) => {
                n += 1;
                out.push(line(Kind::Head, format!("Conflict {n} of {total}")));
                out.push(line(Kind::OursLabel, format!("{dest} (the destination)")));
                out.extend(h.ours.iter().map(|l| line(Kind::Ours, l)));
                if let Some(base) = &h.base {
                    out.push(line(Kind::BaseLabel, "before either change"));
                    out.extend(base.iter().map(|l| line(Kind::Base, l)));
                }
                out.push(line(Kind::TheirsLabel, "this pull request"));
                out.extend(h.theirs.iter().map(|l| line(Kind::Theirs, l)));
            }
        }
    }
    out
}

/// What the Conflicts tab shows.
#[derive(Debug, Default)]
pub struct Conflicts {
    /// `None` until the trial merge has answered.
    pub report: Option<Result<ConflictReport, String>>,
    /// The conflicted file on the right.
    pub selected: Option<String>,
    /// The merged text of the files asked for, cut at its markers.
    texts: HashMap<String, Result<Vec<Segment>, String>>,
    wants: Vec<(String, String)>,
}

impl Conflicts {
    pub fn set_report(&mut self, report: Result<ConflictReport, String>) {
        self.report = Some(report);
    }

    pub fn report(&self) -> Option<&ConflictReport> {
        self.report.as_ref()?.as_ref().ok()
    }

    /// Conflicted files; 0 when it merges cleanly or is not known.
    pub fn count(&self) -> usize {
        self.report().map_or(0, |r| r.files.len())
    }

    pub fn error(&self) -> Option<&str> {
        self.report.as_ref()?.as_ref().err().map(String::as_str)
    }

    pub fn paths(&self) -> HashSet<String> {
        self.report()
            .map(|r| r.files.iter().map(|f| f.path.clone()).collect())
            .unwrap_or_default()
    }

    /// Shows `path`'s merged text, asking for it once.
    pub fn select(&mut self, path: &str) {
        self.selected = Some(path.to_string());
        let Some(report) = self.report() else { return };
        if !self.texts.contains_key(path) && !self.wants.iter().any(|(_, p)| p == path) {
            self.wants.push((report.tree.clone(), path.to_string()));
        }
    }

    /// The first conflicted file, chosen when the tab is first looked at.
    pub fn select_first(&mut self) {
        if self.selected.is_some() {
            return;
        }
        if let Some(path) = self
            .report()
            .and_then(|r| r.files.first())
            .map(|f| f.path.clone())
        {
            self.select(&path);
        }
    }

    pub fn set_text(&mut self, path: String, result: Result<Arc<String>, String>) {
        self.texts.insert(path, result.map(|t| parse_markers(&t)));
    }

    pub fn text(&self, path: &str) -> Option<&Result<Vec<Segment>, String>> {
        self.texts.get(path)
    }

    /// (tree, path) pairs to ask the service for, drained by the host.
    pub fn take_wants(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.wants)
    }
}

#[cfg(test)]
mod tests;
