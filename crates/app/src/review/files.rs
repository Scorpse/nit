//! The review screen's file list: filter, status chips, viewed state, keyboard navigation,
//! the directory tree of the changed files and the comment marks per file.
//! Pure state; drawing lives in `review::ui`.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use prcore::diff::{apply_counts, is_generated, DiffFile, FileCounts, FileStatus};
use prcore::draft::{Draft, DraftKind};

use super::comments_ui::{is_flagged, is_open, scope, Comments, Scope};
use crate::list::{nav_step, Nav};

/// One-letter chip for a file status.
pub fn status_letter(s: FileStatus) -> char {
    match s {
        FileStatus::Added => 'A',
        FileStatus::Modified => 'M',
        FileStatus::Deleted => 'D',
        FileStatus::Renamed => 'R',
        FileStatus::Copied => 'C',
        FileStatus::TypeChanged => 'T',
    }
}

/// One row of the drawn file list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRow {
    File(usize),
    /// A directory of the tree (index into `Tree::dirs`).
    Dir(usize),
    /// The collapsible group of generated files that match the filter.
    Generated {
        count: usize,
        expanded: bool,
    },
    /// The header above the paths with comments that are not in the diff.
    GhostHeader {
        count: usize,
    },
    /// A path with comments that is not in the diff (index into `FileList::ghosts`).
    Ghost(usize),
}

/// The keyboard focus of the list: a file (then also the selected file), a directory, or a
/// "not in diff" entry (then also the selected one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    File(usize),
    Dir(usize),
    Ghost(usize),
}

/// A path with comment threads that is not in the current diff (a later push removed the
/// file or reverted its changes): listed after the files so its threads can be found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ghost {
    pub path: String,
    /// Every thread here is outdated (`outdated == threads()`).
    pub mark: CommentMark,
}

/// How the changed files are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Tree,
    List,
}

/// Store meta key that remembers the view mode.
pub const VIEW_META_KEY: &str = "review_file_view";

impl ViewMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ViewMode::Tree => "tree",
            ViewMode::List => "list",
        }
    }

    /// A stored value; anything unknown (or nothing) is the default, the tree.
    pub fn parse(s: Option<&str>) -> ViewMode {
        match s {
            Some("list") => ViewMode::List,
            _ => ViewMode::Tree,
        }
    }
}

/// Tri-state of a directory's "viewed" box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    Off,
    Mixed,
    On,
}

/// A viewed mark to persist with `Store::set_viewed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewedChange {
    pub path: String,
    pub oid: String,
    pub viewed: bool,
}

/// Comment threads and drafts of one file (or, summed, of a directory).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommentMark {
    pub unresolved: u32,
    pub resolved: u32,
    /// Unsent drafts (file, inline and replies to its threads).
    pub drafts: u32,
    /// Threads among the above known to be outdated without the diff rows (old path of a
    /// rename, line threads of a binary file, threads Bitbucket flags, every thread of a
    /// file not in the diff).
    pub outdated: u32,
}

impl CommentMark {
    pub fn threads(&self) -> u32 {
        self.unresolved + self.resolved
    }

    /// Every thread is outdated (the chip is drawn dimmer).
    pub fn all_outdated(&self) -> bool {
        self.threads() > 0 && self.outdated >= self.threads()
    }

    /// Has threads or drafts (the "Commented" filter).
    pub fn any(&self) -> bool {
        self.threads() > 0 || self.drafts > 0
    }

    pub fn add(&mut self, o: &CommentMark) {
        self.unresolved += o.unresolved;
        self.resolved += o.resolved;
        self.drafts += o.drafts;
        self.outdated += o.outdated;
    }

    /// The bubble chip: (number, unresolved?). The number is the unresolved threads when
    /// there are any (amber), else every thread (grey); None without threads.
    pub fn chip(&self) -> Option<(u32, bool)> {
        if self.unresolved > 0 {
            Some((self.unresolved, true))
        } else if self.resolved > 0 {
            Some((self.resolved, false))
        } else {
            None
        }
    }

    pub fn tooltip(&self) -> String {
        let mut s = format!(
            "{} unresolved, {} resolved, {} drafts",
            self.unresolved, self.resolved, self.drafts
        );
        if self.outdated > 0 {
            s.push_str(&format!(" (outdated: {})", self.outdated));
        }
        s
    }
}

/// Threads and drafts per diff file. A thread counts on the file it is routed to (its own
/// path, or the renamed file whose old path it is on); PR-level and unplaced threads (on a
/// path that is not in the diff) belong to no file. A reply draft counts on its thread's
/// file.
pub fn comment_marks(
    c: &Comments,
    drafts: &[Draft],
    files: &[DiffFile],
) -> HashMap<String, CommentMark> {
    let binary: HashMap<&str, bool> = files.iter().map(|f| (f.path.as_str(), f.binary)).collect();
    let mut out: HashMap<String, CommentMark> = HashMap::new();
    let mut thread_path: HashMap<prcore::comments::ThreadId, &str> = HashMap::new();
    for path in c.paths() {
        let Some(&bin) = binary.get(path) else {
            continue;
        };
        let m = out.entry(path.to_string()).or_default();
        let own = c.for_path(path).iter().map(|&t| (t, false));
        let old = c.on_old_path(path).iter().map(|&t| (t, true));
        for (t, on_old) in own.chain(old) {
            let th = &c.threads[t];
            if let Some(id) = th.id() {
                thread_path.insert(id.clone(), path);
            }
            if is_open(th) {
                m.unresolved += 1;
            } else {
                m.resolved += 1;
            }
            let line = matches!(scope(th), Scope::Line { .. });
            if on_old || (line && (bin || is_flagged(th))) {
                m.outdated += 1;
            }
        }
    }
    for d in drafts {
        let path = match d.kind {
            DraftKind::Pr => None,
            DraftKind::File | DraftKind::Inline => d.path.as_deref(),
            DraftKind::Reply => d
                .reply_to
                .as_ref()
                .and_then(|p| thread_path.get(p).copied()),
        };
        if let Some(p) = path.filter(|p| binary.contains_key(p)) {
            out.entry(p.to_string()).or_default().drafts += 1;
        }
    }
    out.retain(|_, m| m.any());
    out
}

/// One entry per path with threads that is not in the diff, sorted by path; every thread
/// counts as outdated, and a reply draft counts on its thread's path.
pub fn ghost_entries(c: &Comments, drafts: &[Draft]) -> Vec<Ghost> {
    c.ghost_paths()
        .into_iter()
        .map(|path| {
            let mut mark = CommentMark::default();
            for &t in c.on_ghost(path) {
                let th = &c.threads[t];
                if is_open(th) {
                    mark.unresolved += 1;
                } else {
                    mark.resolved += 1;
                }
                mark.outdated += 1;
                mark.drafts += drafts.iter().filter(|d| d.replies_to(th)).count() as u32;
            }
            Ghost {
                path: path.to_string(),
                mark,
            }
        })
        .collect()
}

/// A directory of the changed-files tree.
#[derive(Debug, Clone)]
pub struct DirNode {
    /// Shown name; a chain of single-child directories is one node "a/b/c".
    pub name: String,
    /// Full path from the repository root ("" for the root).
    pub path: String,
    /// Indentation level of its row (children of the root are 0).
    pub depth: u16,
    pub parent: Option<usize>,
    /// Child directories and files, each sorted case-insensitively.
    pub dirs: Vec<usize>,
    pub files: Vec<usize>,
    /// Every file below it, as a range of `Tree::order`.
    pub span: Range<usize>,
}

/// The directory tree of the (non-generated) changed files. `dirs[0]` is the root, which
/// is not drawn; a parent's index is always smaller than its children's.
#[derive(Debug, Clone)]
pub struct Tree {
    pub dirs: Vec<DirNode>,
    /// Files in tree display order (directories first, then files, alphabetical).
    pub order: Vec<usize>,
    /// Per file: its directory (None for files left out of the tree), its row depth and
    /// its position in `order`.
    pub file_dir: Vec<Option<usize>>,
    pub file_depth: Vec<u16>,
    pub rank: Vec<usize>,
}

#[derive(Default)]
struct RawDir {
    dirs: BTreeMap<String, RawDir>,
    files: Vec<usize>,
}

fn cmp_ci(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// The last component of a path (what a file row of the tree shows).
pub fn file_name(path: &str) -> &str {
    path.rfind('/').map_or(path, |k| &path[k + 1..])
}

impl Tree {
    /// The tree of `paths[i]` for every `i` with `include[i]`.
    pub fn build(paths: &[&str], include: &[bool]) -> Tree {
        let mut root = RawDir::default();
        for (i, p) in paths.iter().enumerate() {
            if !include[i] {
                continue;
            }
            let mut node = &mut root;
            let mut parts: Vec<&str> = p.split('/').collect();
            parts.pop();
            for comp in parts.into_iter().filter(|c| !c.is_empty()) {
                node = node.dirs.entry(comp.to_string()).or_default();
            }
            node.files.push(i);
        }
        let n = paths.len();
        let mut t = Tree {
            dirs: Vec::new(),
            order: Vec::new(),
            file_dir: vec![None; n],
            file_depth: vec![0; n],
            rank: vec![usize::MAX; n],
        };
        t.emit(root, String::new(), String::new(), 0, None, paths);
        for (k, &f) in t.order.iter().enumerate() {
            t.rank[f] = k;
        }
        t
    }

    fn emit(
        &mut self,
        mut raw: RawDir,
        mut name: String,
        mut path: String,
        depth: u16,
        parent: Option<usize>,
        paths: &[&str],
    ) -> usize {
        if parent.is_some() {
            // a directory holding exactly one directory and no files is shown as one row
            while raw.files.is_empty() && raw.dirs.len() == 1 {
                let (k, child) = std::mem::take(&mut raw.dirs).into_iter().next().unwrap();
                name = format!("{name}/{k}");
                path = format!("{path}/{k}");
                raw = child;
            }
        }
        let id = self.dirs.len();
        let child_depth = if parent.is_some() { depth + 1 } else { 0 };
        self.dirs.push(DirNode {
            name,
            path: path.clone(),
            depth,
            parent,
            dirs: Vec::new(),
            files: Vec::new(),
            span: 0..0,
        });
        let start = self.order.len();
        let mut kids: Vec<(String, RawDir)> = raw.dirs.into_iter().collect();
        kids.sort_by(|a, b| cmp_ci(&a.0, &b.0));
        for (k, r) in kids {
            let cpath = if path.is_empty() {
                k.clone()
            } else {
                format!("{path}/{k}")
            };
            let c = self.emit(r, k, cpath, child_depth, Some(id), paths);
            self.dirs[id].dirs.push(c);
        }
        let mut files = raw.files;
        files.sort_by(|&a, &b| cmp_ci(file_name(paths[a]), file_name(paths[b])));
        for &f in &files {
            self.order.push(f);
            self.file_dir[f] = Some(id);
            self.file_depth[f] = child_depth;
        }
        self.dirs[id].files = files;
        self.dirs[id].span = start..self.order.len();
        id
    }

    /// Directories above file `i`, innermost first (the root excluded).
    pub fn ancestors(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        let mut cur = self.file_dir.get(i).copied().flatten();
        std::iter::from_fn(move || {
            let d = cur?;
            let p = self.dirs[d].parent?;
            cur = Some(p);
            Some(d)
        })
    }
}

/// Totals of every file below a directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirStats {
    pub files: u32,
    /// Sums over the files whose counts are known (see `counts_unknown`).
    pub additions: u64,
    pub deletions: u64,
    /// Files below whose counts have not arrived yet: the totals are partial ("…").
    pub counts_unknown: u32,
    pub viewed: u32,
    pub marks: CommentMark,
}

impl DirStats {
    pub fn tri(&self) -> Tri {
        match self.viewed {
            0 => Tri::Off,
            v if v >= self.files => Tri::On,
            _ => Tri::Mixed,
        }
    }
}

/// Everything `FileList::rows` depends on besides the (fixed) files.
#[derive(Clone, PartialEq)]
struct RowsKey {
    filter: String,
    statuses: Vec<char>,
    hide_viewed: bool,
    show_generated: bool,
    commented_only: bool,
    view: ViewMode,
    viewed_rev: u64,
    expand_rev: u64,
    marks_rev: u64,
    counts_rev: u64,
}

type StatsCache = RefCell<Option<((u64, u64, u64), Arc<[DirStats]>)>>;

pub struct FileList {
    files: Vec<DiffFile>,
    generated: Vec<bool>,
    tree: Tree,
    /// path -> oid the file had when it was marked viewed (from the store).
    viewed: HashMap<String, String>,
    /// path -> threads and drafts (filled once comments load).
    marks: HashMap<String, CommentMark>,
    /// Paths with threads that are not in the diff (filled once comments load).
    ghosts: Vec<Ghost>,
    /// The selected "not in diff" entry (then no file is selected).
    selected_ghost: Option<usize>,
    /// Case-insensitive substring of the path (or the old path of a rename).
    pub filter: String,
    /// Only these statuses are shown; empty shows every status.
    pub statuses: HashSet<char>,
    pub hide_viewed: bool,
    /// Only files with comment threads or drafts.
    pub commented_only: bool,
    /// Generated/lock files are collapsed (hidden) unless this is set.
    pub show_generated: bool,
    view: ViewMode,
    /// Collapsed directories of the tree (all are expanded at first).
    collapsed: HashSet<usize>,
    selected: Option<usize>,
    cursor: Option<Cursor>,
    /// Bumped whenever a viewed mark, the expansion or the marks change (cache keys).
    viewed_rev: u64,
    expand_rev: u64,
    marks_rev: u64,
    /// Bumped when numstat's counts arrive.
    counts_rev: u64,
    /// `rows()` of the last key it was computed for; the list is drawn every frame.
    rows_cache: RefCell<Option<(RowsKey, Arc<[ListRow]>)>>,
    stats_cache: StatsCache,
    /// How often the rows were recomputed (tests).
    #[cfg(test)]
    rows_built: std::cell::Cell<usize>,
}

impl FileList {
    pub fn new(files: Vec<DiffFile>, viewed: HashMap<String, String>) -> FileList {
        let generated: Vec<bool> = files.iter().map(|f| is_generated(&f.path)).collect();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let include: Vec<bool> = generated.iter().map(|g| !g).collect();
        let tree = Tree::build(&paths, &include);
        FileList {
            files,
            generated,
            tree,
            viewed,
            marks: HashMap::new(),
            ghosts: Vec::new(),
            selected_ghost: None,
            filter: String::new(),
            statuses: HashSet::new(),
            hide_viewed: false,
            commented_only: false,
            show_generated: false,
            view: ViewMode::Tree,
            collapsed: HashSet::new(),
            selected: None,
            cursor: None,
            viewed_rev: 0,
            expand_rev: 0,
            marks_rev: 0,
            counts_rev: 0,
            rows_cache: RefCell::new(None),
            stats_cache: RefCell::new(None),
            #[cfg(test)]
            rows_built: std::cell::Cell::new(0),
        }
    }

    pub fn files(&self) -> &[DiffFile] {
        &self.files
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn view(&self) -> ViewMode {
        self.view
    }

    pub fn set_view(&mut self, v: ViewMode) {
        self.view = v;
    }

    #[cfg(test)]
    pub fn is_generated(&self, i: usize) -> bool {
        self.generated[i]
    }

    /// Unresolved comment threads on file `i`; 0 until comments are loaded.
    #[cfg(test)]
    pub fn comment_count(&self, i: usize) -> usize {
        self.mark(i).unresolved as usize
    }

    /// Threads and drafts of file `i`.
    pub fn mark(&self, i: usize) -> CommentMark {
        self.marks
            .get(&self.files[i].path)
            .copied()
            .unwrap_or_default()
    }

    /// Some file (or "not in diff" entry) has threads or drafts (the marker column is shown).
    pub fn has_marks(&self) -> bool {
        !self.marks.is_empty() || !self.ghosts.is_empty()
    }

    /// Replaces the marks (path -> threads and drafts); a no-op when nothing changed.
    pub fn set_marks(&mut self, marks: HashMap<String, CommentMark>) {
        if marks != self.marks {
            self.marks = marks;
            self.marks_rev += 1;
        }
    }

    /// The "not in diff" entries.
    pub fn ghosts(&self) -> &[Ghost] {
        &self.ghosts
    }

    /// Replaces the "not in diff" entries; a no-op when nothing changed. A selected entry
    /// stays selected while its path is still listed.
    pub fn set_ghosts(&mut self, ghosts: Vec<Ghost>) {
        if ghosts == self.ghosts {
            return;
        }
        let sel = self
            .selected_ghost
            .map(|g| self.ghosts[g].path.clone())
            .and_then(|p| ghosts.iter().position(|x| x.path == p));
        if self.selected_ghost.is_some() {
            self.selected_ghost = sel;
            self.cursor = sel.map(Cursor::Ghost);
        } else if let Some(Cursor::Ghost(_)) = self.cursor {
            self.cursor = None;
        }
        self.ghosts = ghosts;
        self.marks_rev += 1;
    }

    pub fn selected_ghost(&self) -> Option<usize> {
        self.selected_ghost
    }

    /// Selects "not in diff" entry `g` (the cursor follows); no file stays selected.
    pub fn select_ghost(&mut self, g: usize) {
        if g < self.ghosts.len() {
            self.selected_ghost = Some(g);
            self.selected = None;
            self.cursor = Some(Cursor::Ghost(g));
        }
    }

    /// The "not in diff" entry of `path`.
    pub fn ghost_of(&self, path: &str) -> Option<usize> {
        self.ghosts.iter().position(|g| g.path == path)
    }

    /// Viewed only while the file still has the oid it was marked at.
    pub fn is_viewed(&self, i: usize) -> bool {
        let f = &self.files[i];
        self.viewed.get(&f.path) == Some(&f.new_oid)
    }

    fn set_viewed_flag(&mut self, i: usize, viewed: bool) -> ViewedChange {
        self.viewed_rev += 1;
        let f = &self.files[i];
        if viewed {
            self.viewed.insert(f.path.clone(), f.new_oid.clone());
        } else {
            self.viewed.remove(&f.path);
        }
        ViewedChange {
            path: f.path.clone(),
            oid: f.new_oid.clone(),
            viewed,
        }
    }

    /// Flips the viewed mark of file `i`; the caller persists the returned change.
    pub fn toggle_viewed(&mut self, i: usize) -> ViewedChange {
        let viewed = !self.is_viewed(i);
        self.set_viewed_flag(i, viewed)
    }

    /// The tri-state box of directory `d`: marks every file below it viewed, or (when all
    /// are) clears them. Returns only the files whose mark changed.
    pub fn toggle_dir_viewed(&mut self, d: usize) -> Vec<ViewedChange> {
        let span = self.tree.dirs[d].span.clone();
        let to = !span.clone().all(|k| self.is_viewed(self.tree.order[k]));
        let mut out = Vec::new();
        for k in span {
            let i = self.tree.order[k];
            if self.is_viewed(i) != to {
                out.push(self.set_viewed_flag(i, to));
            }
        }
        out
    }

    /// Fills in the line counts and binary flags (numstat arrived after the list), by path;
    /// directory totals and rows are recomputed. Viewed marks, the selection and the order
    /// do not depend on counts and stay.
    pub fn apply_counts(&mut self, counts: &[FileCounts]) {
        apply_counts(&mut self.files, counts);
        self.counts_rev += 1;
    }

    /// Every file's counts are in.
    pub fn counts_known(&self) -> bool {
        self.files.iter().all(|f| f.counts_known)
    }

    /// Totals per directory (index as `Tree::dirs`); cached until a viewed mark, the marks
    /// or the counts change.
    pub fn dir_stats(&self) -> Arc<[DirStats]> {
        let key = (self.viewed_rev, self.marks_rev, self.counts_rev);
        if let Some((k, s)) = self.stats_cache.borrow().as_ref() {
            if *k == key {
                return s.clone();
            }
        }
        let dirs = &self.tree.dirs;
        let mut st = vec![DirStats::default(); dirs.len()];
        for d in (0..dirs.len()).rev() {
            for &i in &dirs[d].files {
                let f = &self.files[i];
                let s = &mut st[d];
                s.files += 1;
                s.additions += u64::from(f.additions);
                s.deletions += u64::from(f.deletions);
                s.counts_unknown += u32::from(!f.counts_known);
                s.viewed += u32::from(self.is_viewed(i));
                s.marks.add(&self.mark(i));
            }
            if let Some(p) = dirs[d].parent {
                let c = st[d];
                let s = &mut st[p];
                s.files += c.files;
                s.additions += c.additions;
                s.deletions += c.deletions;
                s.counts_unknown += c.counts_unknown;
                s.viewed += c.viewed;
                s.marks.add(&c.marks);
            }
        }
        let st: Arc<[DirStats]> = st.into();
        *self.stats_cache.borrow_mut() = Some((key, st.clone()));
        st
    }

    /// Filter text, status chips or "Commented" narrow the files: every directory is shown
    /// expanded meanwhile.
    pub fn is_filtering(&self) -> bool {
        !self.query().is_empty() || !self.statuses.is_empty() || self.commented_only
    }

    /// Whether directory `d`'s children are shown.
    pub fn dir_expanded(&self, d: usize) -> bool {
        self.is_filtering() || !self.collapsed.contains(&d)
    }

    pub fn set_dir_expanded(&mut self, d: usize, open: bool) {
        let changed = if open {
            self.collapsed.remove(&d)
        } else {
            self.collapsed.insert(d)
        };
        if changed {
            self.expand_rev += 1;
        }
    }

    pub fn toggle_dir(&mut self, d: usize) {
        let open = self.collapsed.contains(&d);
        self.set_dir_expanded(d, open);
    }

    pub fn expand_all(&mut self) {
        if !self.collapsed.is_empty() {
            self.collapsed.clear();
            self.expand_rev += 1;
        }
    }

    pub fn collapse_all(&mut self) {
        self.collapsed = (1..self.tree.dirs.len()).collect();
        self.expand_rev += 1;
    }

    fn expand_to(&mut self, i: usize) {
        let up: Vec<usize> = self.tree.ancestors(i).collect();
        for d in up {
            self.set_dir_expanded(d, true);
        }
    }

    /// Indentation level of a row (0 in the flat list).
    pub fn depth(&self, row: ListRow) -> u16 {
        match (self.view, row) {
            (ViewMode::List, _)
            | (_, ListRow::Generated { .. } | ListRow::GhostHeader { .. } | ListRow::Ghost(_)) => 0,
            (ViewMode::Tree, ListRow::Dir(d)) => self.tree.dirs[d].depth,
            (ViewMode::Tree, ListRow::File(i)) => match self.tree.file_dir[i] {
                Some(_) => self.tree.file_depth[i],
                None => 0,
            },
        }
    }

    /// Matches the lowercased filter text `q`, the status chips and "Commented" (ignores
    /// "hide viewed" and collapsing).
    fn matches(&self, i: usize, q: &str) -> bool {
        let f = &self.files[i];
        if !self.statuses.is_empty() && !self.statuses.contains(&status_letter(f.status)) {
            return false;
        }
        if self.commented_only && !self.mark(i).any() {
            return false;
        }
        q.is_empty()
            || f.path.to_lowercase().contains(q)
            || f.old_path
                .as_ref()
                .is_some_and(|p| p.to_lowercase().contains(q))
    }

    fn query(&self) -> String {
        self.filter.trim().to_lowercase()
    }

    /// Indices of the shown files in display order (collapsed directories hide theirs).
    #[cfg(test)]
    pub fn visible(&self) -> Vec<usize> {
        self.rows()
            .iter()
            .filter_map(|r| match r {
                ListRow::File(i) => Some(*i),
                _ => None,
            })
            .collect()
    }

    /// Files `j` / `k` walk: every file that passes the filter and "hide viewed", in
    /// display order as if every directory were expanded (the generated ones only while
    /// their group is expanded).
    pub fn nav_order(&self) -> Vec<usize> {
        let q = self.query();
        let shown = |i: usize| self.matches(i, &q) && !(self.hide_viewed && self.is_viewed(i));
        let n = self.files.len();
        let main: Vec<usize> = match self.view {
            ViewMode::List => (0..n).filter(|&i| !self.generated[i]).collect(),
            ViewMode::Tree => self.tree.order.clone(),
        };
        let mut out: Vec<usize> = main.into_iter().filter(|&i| shown(i)).collect();
        if self.show_generated {
            out.extend((0..n).filter(|&i| self.generated[i] && shown(i)));
        }
        out
    }

    /// Generated files that match the filter (shown as one collapsible row).
    #[cfg(test)]
    pub fn generated_count(&self) -> usize {
        let q = self.query();
        (0..self.files.len())
            .filter(|&i| self.generated[i] && self.matches(i, &q))
            .count()
    }

    fn rows_key(&self) -> RowsKey {
        let mut statuses: Vec<char> = self.statuses.iter().copied().collect();
        statuses.sort_unstable();
        RowsKey {
            filter: self.filter.clone(),
            statuses,
            hide_viewed: self.hide_viewed,
            show_generated: self.show_generated,
            commented_only: self.commented_only,
            view: self.view,
            viewed_rev: self.viewed_rev,
            expand_rev: self.expand_rev,
            marks_rev: self.marks_rev,
            counts_rev: self.counts_rev,
        }
    }

    /// Rows to draw: shown files (and, in the tree, their directories), with the generated
    /// group header before its files. Cached until an input changes.
    pub fn rows(&self) -> Arc<[ListRow]> {
        let key = self.rows_key();
        if let Some((k, rows)) = self.rows_cache.borrow().as_ref() {
            if *k == key {
                return rows.clone();
            }
        }
        let rows: Arc<[ListRow]> = self.compute_rows().into();
        *self.rows_cache.borrow_mut() = Some((key, rows.clone()));
        rows
    }

    fn compute_rows(&self) -> Vec<ListRow> {
        #[cfg(test)]
        self.rows_built.set(self.rows_built.get() + 1);
        let q = self.query();
        let n = self.files.len();
        let shown: Vec<bool> = (0..n)
            .map(|i| self.matches(i, &q) && !(self.hide_viewed && self.is_viewed(i)))
            .collect();
        let gen_count = (0..n)
            .filter(|&i| self.generated[i] && self.matches(i, &q))
            .count();
        let mut out: Vec<ListRow> = Vec::new();
        match self.view {
            ViewMode::List => {
                out.extend(
                    (0..n)
                        .filter(|&i| !self.generated[i] && shown[i])
                        .map(ListRow::File),
                );
            }
            ViewMode::Tree => {
                // shown files below each directory, bottom-up: empty directories are left out
                let dirs = &self.tree.dirs;
                let mut cnt = vec![0usize; dirs.len()];
                for d in (0..dirs.len()).rev() {
                    cnt[d] += dirs[d].files.iter().filter(|&&f| shown[f]).count();
                    if let Some(p) = dirs[d].parent {
                        cnt[p] += cnt[d];
                    }
                }
                let filtering = self.is_filtering();
                let mut stack: Vec<(usize, bool)> = vec![(0, false)];
                // (dir, children emitted): a directory's files follow its subdirectories
                while let Some((d, done)) = stack.pop() {
                    if done {
                        out.extend(
                            dirs[d]
                                .files
                                .iter()
                                .filter(|&&f| shown[f])
                                .map(|&f| ListRow::File(f)),
                        );
                        continue;
                    }
                    if d != 0 {
                        out.push(ListRow::Dir(d));
                        if !filtering && self.collapsed.contains(&d) {
                            continue;
                        }
                    }
                    stack.push((d, true));
                    for &c in dirs[d].dirs.iter().rev() {
                        if cnt[c] > 0 {
                            stack.push((c, false));
                        }
                    }
                }
            }
        }
        if gen_count > 0 {
            out.push(ListRow::Generated {
                count: gen_count,
                expanded: self.show_generated,
            });
        }
        if self.show_generated {
            out.extend(
                (0..n)
                    .filter(|&i| self.generated[i] && shown[i])
                    .map(ListRow::File),
            );
        }
        // paths not in the diff have no status: a status chip hides them
        if self.statuses.is_empty() {
            let ghosts: Vec<usize> = (0..self.ghosts.len())
                .filter(|&g| q.is_empty() || self.ghosts[g].path.to_lowercase().contains(&q))
                .collect();
            if !ghosts.is_empty() {
                out.push(ListRow::GhostHeader {
                    count: ghosts.len(),
                });
                out.extend(ghosts.into_iter().map(ListRow::Ghost));
            }
        }
        out
    }

    #[cfg(test)]
    fn rows_built(&self) -> usize {
        self.rows_built.get()
    }

    /// "12/340 viewed".
    pub fn counter(&self) -> String {
        let n = (0..self.files.len()).filter(|&i| self.is_viewed(i)).count();
        format!("{n}/{} viewed", self.files.len())
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }

    /// Selects file `i` (the cursor follows) and expands the directories above it.
    pub fn select(&mut self, i: usize) {
        if i < self.files.len() {
            self.selected = Some(i);
            self.selected_ghost = None;
            self.cursor = Some(Cursor::File(i));
            self.expand_to(i);
        }
    }

    /// Puts the keyboard focus on directory `d` (the selected file stays).
    pub fn focus_dir(&mut self, d: usize) {
        if d > 0 && d < self.tree.dirs.len() {
            self.cursor = Some(Cursor::Dir(d));
        }
    }

    /// The first file that is not generated (the one to open first), else the first file.
    pub fn first_reviewable(&self) -> Option<usize> {
        let n = self.files.len();
        (0..n)
            .find(|&i| !self.generated[i])
            .or((n > 0).then_some(0))
    }

    /// `j`: the next shown file, clamped at the end. Returns the new selection.
    pub fn select_next(&mut self) -> Option<usize> {
        self.step(true)
    }

    /// `k`: the previous shown file, clamped at the start. Returns the new selection.
    pub fn select_prev(&mut self) -> Option<usize> {
        self.step(false)
    }

    fn step(&mut self, forward: bool) -> Option<usize> {
        let vis = self.nav_order();
        let (Some(&first), Some(&last)) = (vis.first(), vis.last()) else {
            return self.selected;
        };
        let next = match self.selected {
            None => first,
            Some(cur) => match vis.iter().position(|&i| i == cur) {
                Some(p) if forward => vis[(p + 1).min(vis.len() - 1)],
                Some(p) => vis[p.saturating_sub(1)],
                // not shown (filtered, viewed-and-hidden or collapsed): the nearest shown
                // file in that direction of display order
                None if forward => {
                    let at = self.display_rank(cur);
                    vis.iter()
                        .copied()
                        .find(|&i| self.display_rank(i) > at)
                        .unwrap_or(last)
                }
                None => {
                    let at = self.display_rank(cur);
                    vis.iter()
                        .rev()
                        .copied()
                        .find(|&i| self.display_rank(i) < at)
                        .unwrap_or(first)
                }
            },
        };
        self.select(next);
        self.selected
    }

    /// Position of file `i` in display order when everything is shown: the tree (or the
    /// non-generated files by index), then the generated ones.
    fn display_rank(&self, i: usize) -> usize {
        if self.generated[i] {
            self.files.len() + i
        } else if self.view == ViewMode::Tree {
            self.tree.rank[i]
        } else {
            i
        }
    }

    /// Row of the cursor (or of the selected file) among `rows`.
    pub fn cursor_pos(&self, rows: &[ListRow]) -> Option<usize> {
        let want = |c: Cursor| match c {
            Cursor::File(i) => ListRow::File(i),
            Cursor::Dir(d) => ListRow::Dir(d),
            Cursor::Ghost(g) => ListRow::Ghost(g),
        };
        self.cursor
            .and_then(|c| rows.iter().position(|r| *r == want(c)))
            .or_else(|| {
                let s = self.selected?;
                rows.iter().position(|r| *r == ListRow::File(s))
            })
    }

    fn put_cursor(&mut self, row: ListRow) {
        match row {
            ListRow::File(i) => self.select(i),
            ListRow::Dir(d) => self.focus_dir(d),
            ListRow::Ghost(g) => self.select_ghost(g),
            ListRow::Generated { .. } | ListRow::GhostHeader { .. } => {}
        }
    }

    /// Up / Down / PageUp / PageDown / Home / End over the visible rows (directories
    /// included, the generated group header skipped); a file reached is selected.
    pub fn nav(&mut self, key: Nav, page: usize) {
        let rows = self.rows();
        let cur = self.cursor_pos(&rows);
        let selectable = |k: usize| {
            !matches!(
                rows[k],
                ListRow::Generated { .. } | ListRow::GhostHeader { .. }
            )
        };
        if let Some(to) = nav_step(rows.len(), selectable, cur, key, page) {
            self.put_cursor(rows[to]);
        }
    }

    /// Right: expands the directory under the cursor, or moves into its first child when
    /// it is already expanded. Nothing on a file or in the flat list.
    pub fn right(&mut self) {
        if self.view != ViewMode::Tree {
            return;
        }
        let Some(Cursor::Dir(d)) = self.cursor else {
            return;
        };
        if !self.dir_expanded(d) {
            self.set_dir_expanded(d, true);
            return;
        }
        let rows = self.rows();
        let Some(pos) = rows.iter().position(|r| *r == ListRow::Dir(d)) else {
            return;
        };
        if let Some(&next) = rows.get(pos + 1) {
            let child = match next {
                ListRow::Dir(c) => self.tree.dirs[c].parent == Some(d),
                ListRow::File(i) => self.tree.file_dir[i] == Some(d),
                ListRow::Generated { .. } | ListRow::GhostHeader { .. } | ListRow::Ghost(_) => {
                    false
                }
            };
            if child {
                self.put_cursor(next);
            }
        }
    }

    /// Left: collapses the directory under the cursor, or (on a file or a collapsed
    /// directory) moves to its parent directory.
    pub fn left(&mut self) {
        if self.view != ViewMode::Tree {
            return;
        }
        let parent = match self.cursor {
            Some(Cursor::Dir(d)) => {
                if self.dir_expanded(d) && !self.is_filtering() {
                    self.set_dir_expanded(d, false);
                    return;
                }
                self.tree.dirs[d].parent
            }
            Some(Cursor::File(i)) => self.tree.file_dir[i],
            Some(Cursor::Ghost(_)) | None => None,
        };
        if let Some(p) = parent.filter(|&p| p != 0) {
            self.focus_dir(p);
        }
    }
}

#[cfg(test)]
mod tests;
