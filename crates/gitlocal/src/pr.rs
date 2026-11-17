//! PR-oriented git operations: fetch both branches into private refs, compute the
//! merge-base, list changed files and produce per-file patches.

use crate::runner::{Git, GitAuth, GitError, RunOpts, LAZY_FETCH_TIMEOUT, NETWORK_TIMEOUT};
use crate::{validate_url, LocalRepo};
use prcore::conflicts::{parse_merge_tree, ConflictReport};
use prcore::diff::{
    parse_diff_index, parse_diff_raw, parse_numstat, DiffFile, FileCounts, FileStatus,
};
use prcore::history::{parse_log, CommitInfo, LOG_FORMAT};

/// Upper bound for one file's patch output; beyond this `GitError::TooLarge` is returned.
pub const MAX_PATCH_BYTES: usize = 64 * 1024 * 1024;

/// Upper bound for the raw / numstat file listing of one PR.
pub const MAX_INDEX_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrRefs {
    /// merge-base(dest, source) sha
    pub base: String,
    /// source branch tip sha
    pub head: String,
    /// destination branch tip sha
    pub dest: String,
}

fn bad_ref<T>(m: impl Into<String>) -> Result<T, GitError> {
    Err(GitError::InvalidRef(m.into()))
}

fn validate_branch(name: &str) -> Result<(), GitError> {
    if name.is_empty() || name == "@" {
        return bad_ref("empty branch name");
    }
    if name.starts_with('-') || name.starts_with('/') || name.starts_with('.') {
        return bad_ref(format!("{name:?} has a forbidden leading character"));
    }
    if name.ends_with('/') || name.ends_with('.') || name.ends_with(".lock") {
        return bad_ref(format!("{name:?} has a forbidden ending"));
    }
    if name.contains("..") || name.contains("//") || name.contains("@{") || name.contains("/.") {
        return bad_ref(format!("{name:?} contains a forbidden sequence"));
    }
    if name
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c))
    {
        return bad_ref(format!("{name:?} contains a forbidden character"));
    }
    Ok(())
}

/// A revision or path operand must not be parsed as an option or contain newlines/NULs.
fn validate_operand(what: &str, s: &str) -> Result<(), GitError> {
    if s.is_empty() || s.starts_with('-') || s.chars().any(|c| c == '\n' || c == '\0') {
        return bad_ref(format!("invalid {what}"));
    }
    Ok(())
}

/// Where fetched PR branches go: `refs/nit/pr-<id>/{src,dst}`. Before the rename they went to
/// `refs/prview/...`; such refs left in existing mirrors and clones are never read or written
/// again (only the user's own refs and these namespaces exist there, so nothing collides).
const REF_NAMESPACE: &str = "refs/nit";

/// The host whose PR refs keep the original `refs/nit/pr-<id>` names (no churn in existing
/// clones).
const LEGACY_REF_HOST: &str = "bitbucket.org";

/// A host (`RepoRef::host`: lower-case, maybe with a port or a path prefix) as one path or ref
/// component: lower-case, `[a-z0-9.-]` kept, everything else (`:`, `/`, ...) becomes `_`.
pub fn host_slug(host: &str) -> String {
    host.to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The ref namespace of `host`'s PRs (spec 7.1): Bitbucket keeps `refs/nit`, other hosts use
/// `refs/nit/<host-slug>`, so a clone with remotes on two forges never mixes PR numbers.
pub fn ref_prefix(host: &str) -> String {
    if host.eq_ignore_ascii_case(LEGACY_REF_HOST) {
        REF_NAMESPACE.to_string()
    } else {
        format!("{REF_NAMESPACE}/{}", host_slug(host))
    }
}

/// What to fetch for a PR (spec 7.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchSpec {
    /// Source and destination branches by name (Bitbucket).
    Branches { src: String, dst: String },
    /// The forge's own PR ref (`refs/pull/7/head`, `refs/merge-requests/7/head`) and the
    /// destination branch by name.
    PullRef { src_ref: String, dst: String },
}

/// Fetches the PR (`spec`) into `<prefix>/pr-<id>/{src,dst}` (`prefix` from `ref_prefix`)
/// and computes the merge-base from those refs.
pub fn fetch_pr(
    git: &Git,
    local: &LocalRepo,
    https_url: &str,
    auth: &GitAuth,
    prefix: &str,
    pr_id: u64,
    spec: &FetchSpec,
) -> Result<PrRefs, GitError> {
    fetch_pr_inner(git, local, https_url, auth, prefix, pr_id, spec, false)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_pr_inner(
    git: &Git,
    local: &LocalRepo,
    url: &str,
    auth: &GitAuth,
    prefix: &str,
    pr_id: u64,
    spec: &FetchSpec,
    allow_file: bool,
) -> Result<PrRefs, GitError> {
    let (src_from, dest_branch) = match spec {
        FetchSpec::Branches { src, dst } => {
            validate_branch(src)?;
            (format!("refs/heads/{src}"), dst)
        }
        FetchSpec::PullRef { src_ref, dst } => {
            match src_ref.strip_prefix("refs/") {
                Some(rest) => validate_branch(rest)?,
                None => return bad_ref(format!("{src_ref:?} is not a full ref name")),
            }
            (src_ref.clone(), dst)
        }
    };
    validate_branch(dest_branch)?;
    if prefix != REF_NAMESPACE
        && !prefix
            .strip_prefix("refs/nit/")
            .is_some_and(|h| !h.is_empty() && h == host_slug(h))
    {
        return bad_ref(format!("{prefix:?} is not a nit ref namespace"));
    }
    validate_url(url, allow_file)?;
    let src_ref = format!("{prefix}/pr-{pr_id}/src");
    let dst_ref = format!("{prefix}/pr-{pr_id}/dst");
    let src_spec = format!("+{src_from}:{src_ref}");
    let dst_spec = format!("+refs/heads/{dest_branch}:{dst_ref}");
    let opts = RunOpts {
        auth: Some(auth),
        url: Some(url),
        timeout: Some(NETWORK_TIMEOUT),
        ..RunOpts::default()
    };
    git.run_with(
        &local.path,
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--",
            url,
            &src_spec,
            &dst_spec,
        ],
        opts,
    )?;
    let [head, dest] = resolve_commits(git, local, [&src_ref, &dst_ref])?;
    let base = match git.run(&local.path, &["merge-base", &dest, &head], None) {
        Ok(out) => String::from_utf8_lossy(&out).trim().to_string(),
        Err(GitError::Failed {
            code: Some(1),
            stderr,
        }) if stderr.trim().is_empty() => return Err(GitError::NoMergeBase),
        Err(e) => return Err(e),
    };
    if base.is_empty() {
        return Err(GitError::NoMergeBase);
    }
    Ok(PrRefs { base, head, dest })
}

/// Commit ids of two revisions (each peeled with `^{commit}`) from one `git rev-parse`.
/// Both must resolve to a full hexadecimal id, or the whole call fails.
pub fn resolve_commits(
    git: &Git,
    local: &LocalRepo,
    revs: [&str; 2],
) -> Result<[String; 2], GitError> {
    for r in revs {
        validate_operand("revision", r)?;
    }
    let specs = revs.map(|r| format!("{r}^{{commit}}"));
    let out = git.run(&local.path, &["rev-parse", &specs[0], &specs[1]], None)?;
    let text = String::from_utf8_lossy(&out);
    let ids: Vec<&str> = text.lines().map(str::trim).collect();
    let is_id = |s: &str| matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
    match ids.as_slice() {
        [a, b] if is_id(a) && is_id(b) => Ok([a.to_string(), b.to_string()]),
        _ => bad_ref("the branches did not resolve to commits"),
    }
}

/// Most commits `commit_log` lists; a PR with more shows its newest ones.
pub const MAX_LOG_COMMITS: usize = 500;

/// The commits of the PR, oldest first: those reachable from `refs.head` and not from
/// `refs.base` (at most `MAX_LOG_COMMITS`, the newest). Reads commit objects only, so a
/// blobless mirror downloads nothing.
pub fn commit_log(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
) -> Result<Vec<CommitInfo>, GitError> {
    validate_operand("revision", &refs.base)?;
    validate_operand("revision", &refs.head)?;
    let range = format!("{}..{}", refs.base, refs.head);
    let max = format!("--max-count={MAX_LOG_COMMITS}");
    let out = git.run(
        &local.path,
        &["log", "-z", "--reverse", LOG_FORMAT, &max, &range, "--"],
        None,
    )?;
    Ok(parse_log(&out))
}

/// The list of conflicted files git may print.
const MAX_MERGE_CHECK_BYTES: usize = 4 * 1024 * 1024;

const DIFF_COMMON: [&str; 4] = ["-M", "--no-ext-diff", "--no-textconv", "--no-color"];

/// Patch prefixes are fixed so a batch can read each section's header back, whatever the
/// repository's `diff.noprefix` / `diff.mnemonicPrefix` say. Rows never include headers.
const PATCH_PREFIXES: [&str; 2] = ["--src-prefix=a/", "--dst-prefix=b/"];

/// Budget for the path operands of one batched `git diff`, as the command line spends them
/// (Windows limits a whole command line to 32,767 characters). A larger batch is split.
pub const MAX_PATHSPEC_BYTES: usize = 24 * 1024;

/// A diff / show command: bounded output, and killed after `LAZY_FETCH_TIMEOUT` since a
/// blobless mirror (or a partial user clone) may fetch blobs over the network.
fn run_diff(
    git: &Git,
    local: &LocalRepo,
    args: &[&str],
    auth: Option<&GitAuth>,
    max_bytes: usize,
) -> Result<Vec<u8>, GitError> {
    run_diff_cancellable(git, local, args, auth, max_bytes, None)
}

fn run_diff_cancellable(
    git: &Git,
    local: &LocalRepo,
    args: &[&str],
    auth: Option<&GitAuth>,
    max_bytes: usize,
    cancel: Option<&dyn Fn() -> bool>,
) -> Result<Vec<u8>, GitError> {
    let opts = RunOpts {
        auth,
        max_bytes: Some(max_bytes),
        timeout: Some(LAZY_FETCH_TIMEOUT),
        cancel,
        ..RunOpts::default()
    };
    git.run_with(&local.path, args, opts)
}

/// One listing `git diff <mode> -M base head --`, bounded and optionally cancellable.
fn index_run(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    auth: Option<&GitAuth>,
    mode: &[&str],
    cancel: Option<&dyn Fn() -> bool>,
) -> Result<Vec<u8>, GitError> {
    validate_operand("revision", &refs.base)?;
    validate_operand("revision", &refs.head)?;
    let mut args = vec!["diff"];
    args.extend(mode);
    args.extend(DIFF_COMMON);
    args.extend([refs.base.as_str(), refs.head.as_str(), "--"]);
    run_diff_cancellable(git, local, &args, auth, MAX_INDEX_BYTES, cancel)
}

const RAW_MODE: [&str; 4] = ["--raw", "-z", "--full-index", "--no-abbrev"];
const NUMSTAT_MODE: [&str; 2] = ["--numstat", "-z"];

/// Lists the files changed between `refs.base` and `refs.head`, with their line counts.
///
/// `auth` is needed when `local` is a blobless mirror: git lazily fetches blobs over HTTPS.
pub fn file_index(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    auth: Option<&GitAuth>,
) -> Result<Vec<DiffFile>, GitError> {
    let raw = index_run(git, local, refs, auth, &RAW_MODE, None)?;
    let numstat = index_run(git, local, refs, auth, &NUMSTAT_MODE, None)?;
    Ok(parse_diff_index(&raw, &numstat))
}

/// The changed files alone (`--raw`: names, status, renames, oids), with their counts
/// unknown. Cheap: git compares tree entries and reads only the added and deleted files
/// (rename detection); it does not diff the modified ones (or, on a blobless mirror,
/// download them). The counts come from `file_counts`.
pub fn file_list(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    auth: Option<&GitAuth>,
) -> Result<Vec<DiffFile>, GitError> {
    let raw = index_run(git, local, refs, auth, &RAW_MODE, None)?;
    Ok(parse_diff_raw(&raw))
}

/// Line counts and the binary flag of every changed file (`--numstat`, which diffs every
/// file); `apply_counts` joins them to `file_list` by path. Stops with `Cancelled` (the
/// running git killed) as soon as `cancel` returns true.
pub fn file_counts(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    auth: Option<&GitAuth>,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<FileCounts>, GitError> {
    let out = index_run(git, local, refs, auth, &NUMSTAT_MODE, Some(cancel))?;
    Ok(parse_numstat(&out))
}

/// Unified diff (3 lines of context) for one file; renames pass both paths so git pairs them.
pub fn file_patch(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    file: &DiffFile,
    auth: Option<&GitAuth>,
) -> Result<String, GitError> {
    let mut args = patch_args(refs)?;
    args.extend(pathspecs(file));
    let out = run_diff(git, local, &args, auth, MAX_PATCH_BYTES)?;
    Ok(utf8_owned(out))
}

/// The bytes as text without a copy when they are valid UTF-8 (git output nearly always
/// is); lossy (U+FFFD for bad sequences, the same text as before) only otherwise.
pub(crate) fn utf8_owned(out: Vec<u8>) -> String {
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `git diff` arguments up to and including `--`, shared by `file_patch` and the batch.
fn patch_args(refs: &PrRefs) -> Result<Vec<&str>, GitError> {
    validate_operand("revision", &refs.base)?;
    validate_operand("revision", &refs.head)?;
    let mut args = vec!["--literal-pathspecs", "diff", "-U3"];
    args.extend(DIFF_COMMON);
    args.extend(PATCH_PREFIXES);
    args.extend([refs.base.as_str(), refs.head.as_str(), "--"]);
    Ok(args)
}

/// Path operands for one file; renames pass both paths so git pairs them.
fn pathspecs(file: &DiffFile) -> Vec<&str> {
    match (file.status, file.old_path.as_deref()) {
        (FileStatus::Renamed, Some(old)) => vec![old, file.path.as_str()],
        _ => vec![file.path.as_str()],
    }
}

/// What one operand costs on a command line: the text, a separating space, quotes, and
/// escapes for quotes and backslashes (an overestimate on purpose).
fn arg_cost(s: &str) -> usize {
    s.len() + 3 + s.bytes().filter(|b| matches!(b, b'"' | b'\\')).count()
}

/// Splits `files` (as indexes) into runs whose operands fit `budget`; a rename's two paths
/// always stay together. A single file over the budget gets a run of its own.
fn pathspec_chunks(files: &[&DiffFile], budget: usize) -> Vec<Vec<usize>> {
    let mut chunks: Vec<Vec<usize>> = Vec::new();
    let mut used = 0;
    for (i, f) in files.iter().enumerate() {
        let cost: usize = pathspecs(f).iter().map(|p| arg_cost(p)).sum();
        match chunks.last_mut() {
            Some(c) if used + cost <= budget => {
                c.push(i);
                used += cost;
            }
            _ => {
                chunks.push(vec![i]);
                used = cost;
            }
        }
    }
    chunks
}

/// Result of `batch_patches`: patches attributed to requested files (by index into the
/// request, each byte-identical to what `file_patch` returns for that file) and the indexes
/// of requested files that got no trustworthy section.
#[derive(Debug, Default)]
pub struct PatchBatch {
    pub found: Vec<(usize, String)>,
    pub missing: Vec<usize>,
}

/// Unified diffs for many files with one `git diff` per `MAX_PATHSPEC_BYTES` of paths.
///
/// The output is split on `diff --git ` at line starts, and every section is attributed by
/// the paths in its own header (and `rename from` / `rename to`), never by position. A
/// file is `found` only when exactly one section names exactly its paths (a rename only
/// with the same old path). A section whose header cannot be read makes its whole run
/// untrusted. Everything not found is `missing`; callers use `file_patch` for those.
/// A failed run marks its files missing; the error is returned only when every run failed.
pub fn batch_patches(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    files: &[&DiffFile],
    auth: Option<&GitAuth>,
) -> Result<PatchBatch, GitError> {
    batch_patches_inner(git, local, refs, files, auth, None)
}

/// `batch_patches` that stops as soon as `cancel` returns true: the running `git diff` (and
/// what it started) is killed, no further run starts, and `Cancelled` is returned.
pub fn batch_patches_cancellable(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    files: &[&DiffFile],
    auth: Option<&GitAuth>,
    cancel: &dyn Fn() -> bool,
) -> Result<PatchBatch, GitError> {
    batch_patches_inner(git, local, refs, files, auth, Some(cancel))
}

fn batch_patches_inner(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    files: &[&DiffFile],
    auth: Option<&GitAuth>,
    cancel: Option<&dyn Fn() -> bool>,
) -> Result<PatchBatch, GitError> {
    let base = patch_args(refs)?;
    let mut batch = PatchBatch::default();
    let mut first_err = None;
    let chunks = pathspec_chunks(files, MAX_PATHSPEC_BYTES);
    let mut failed = 0;
    for chunk in &chunks {
        if cancel.is_some_and(|c| c()) {
            return Err(GitError::Cancelled);
        }
        let mut args = base.clone();
        for &i in chunk {
            args.extend(pathspecs(files[i]));
        }
        let out = match run_diff_cancellable(git, local, &args, auth, MAX_PATCH_BYTES, cancel) {
            Ok(out) => out,
            Err(GitError::Cancelled) => return Err(GitError::Cancelled),
            Err(e) => {
                failed += 1;
                first_err.get_or_insert(e);
                batch.missing.extend(chunk);
                continue;
            }
        };
        let wanted: Vec<&DiffFile> = chunk.iter().map(|&i| files[i]).collect();
        for (j, text) in attribute(&out, &wanted).into_iter().enumerate() {
            match text {
                Some(t) => batch.found.push((chunk[j], t)),
                None => batch.missing.push(chunk[j]),
            }
        }
    }
    match first_err {
        Some(e) if failed == chunks.len() => Err(e),
        _ => Ok(batch),
    }
}

/// Like `file_patch` for every file in `files`, with as few git processes as possible:
/// `batch_patches`, then `file_patch` for each file the batch could not attribute.
/// Returns `(path, patch)` in the order of `files`.
pub fn file_patches(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    files: &[&DiffFile],
    auth: Option<&GitAuth>,
) -> Result<Vec<(String, String)>, GitError> {
    let batch = batch_patches(git, local, refs, files, auth)?;
    let mut out: Vec<Option<String>> = vec![None; files.len()];
    for (i, text) in batch.found {
        out[i] = Some(text);
    }
    for i in batch.missing {
        out[i] = Some(file_patch(git, local, refs, files[i], auth)?);
    }
    Ok(files
        .iter()
        .zip(out)
        .map(|(f, t)| (f.path.clone(), t.unwrap_or_default()))
        .collect())
}

const SECTION_START: &[u8] = b"diff --git ";

/// Splits batch output into per-file sections; None when it does not start with one.
fn split_sections(out: &[u8]) -> Option<Vec<&[u8]>> {
    if out.is_empty() {
        return Some(Vec::new());
    }
    if !out.starts_with(SECTION_START) {
        return None;
    }
    let mut starts = vec![0];
    for (i, &b) in out.iter().enumerate() {
        if b == b'\n' && out[i + 1..].starts_with(SECTION_START) {
            starts.push(i + 1);
        }
    }
    starts.push(out.len());
    Some(starts.windows(2).map(|w| &out[w[0]..w[1]]).collect())
}

/// Reads a C-quoted git path (`"..."` with `\"`, `\\`, `\t`, `\n`, ... and `\ooo` octal
/// escapes) from the start of `s`; returns its bytes and what follows the closing quote.
fn unquote(s: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut out = Vec::new();
    let mut i = 1;
    if s.first() != Some(&b'"') {
        return None;
    }
    while i < s.len() {
        match s[i] {
            b'"' => return Some((out, &s[i + 1..])),
            b'\\' => {
                let c = *s.get(i + 1)?;
                i += 2;
                out.push(match c {
                    b'a' => 7,
                    b'b' => 8,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 11,
                    b'f' => 12,
                    b'r' => b'\r',
                    b'"' | b'\\' => c,
                    b'0'..=b'7' => {
                        let digits = s.get(i - 1..i + 2)?;
                        if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                            return None;
                        }
                        i += 2;
                        let v = digits
                            .iter()
                            .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                        u8::try_from(v).ok()?
                    }
                    _ => return None,
                });
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    None
}

/// A path as git prints it after `rename from ` / `rename to `: quoted or verbatim.
fn plain_or_quoted(s: &[u8]) -> Option<Vec<u8>> {
    if s.first() == Some(&b'"') {
        let (p, rest) = unquote(s)?;
        rest.is_empty().then_some(p)
    } else {
        Some(s.to_vec())
    }
}

/// The one path of a header `a/P b/P` or `"a/P" "b/P"` (old and new the same).
fn same_path_header(rest: &[u8]) -> Option<Vec<u8>> {
    let (a, b) = if rest.first() == Some(&b'"') {
        let (a, tail) = unquote(rest)?;
        let (b, end) = unquote(tail.strip_prefix(b" ")?)?;
        if !end.is_empty() {
            return None;
        }
        (a, b)
    } else {
        // unquoted paths never contain a quote; equal halves are the only reading
        if rest.contains(&b'"') || rest.len() < 5 || !(rest.len() - 5).is_multiple_of(2) {
            return None;
        }
        let n = (rest.len() - 5) / 2;
        let (a, b) = (&rest[..n + 2], &rest[n + 3..]);
        if rest[n + 2] != b' ' {
            return None;
        }
        (a.to_vec(), b.to_vec())
    };
    let a = a.strip_prefix(b"a/")?;
    let b = b.strip_prefix(b"b/")?;
    (a == b).then(|| a.to_vec())
}

/// (old path, new path, renamed) named by one section; None when it cannot be read.
fn section_id(sec: &[u8]) -> Option<(String, String, bool)> {
    let mut lines = sec.split(|&b| b == b'\n');
    let first = lines.next()?.strip_prefix(SECTION_START)?;
    let (mut from, mut to) = (None, None);
    for line in lines {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.starts_with(b"@@")
            || line.starts_with(b"--- ")
            || line.starts_with(b"+++ ")
            || line.starts_with(b"Binary files ")
            || line.starts_with(b"GIT binary patch")
        {
            break;
        }
        if let Some(p) = line.strip_prefix(b"rename from ") {
            from = Some(plain_or_quoted(p)?);
        } else if let Some(p) = line.strip_prefix(b"rename to ") {
            to = Some(plain_or_quoted(p)?);
        } else if line.starts_with(b"copy from ") || line.starts_with(b"copy to ") {
            return None;
        }
    }
    let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    match (from, to) {
        (Some(f), Some(t)) => Some((s(&f), s(&t), true)),
        (None, None) => {
            let p = s(&same_path_header(first)?);
            Some((p.clone(), p, false))
        }
        _ => None,
    }
}

/// For each of `wanted`, its section's text when exactly one section names exactly its
/// paths; all None when any section cannot be read (nothing of that output is trusted).
fn attribute(out: &[u8], wanted: &[&DiffFile]) -> Vec<Option<String>> {
    let none = || vec![None; wanted.len()];
    let Some(sections) = split_sections(out) else {
        return none();
    };
    let mut by_id: std::collections::HashMap<(String, String, bool), Vec<&[u8]>> =
        std::collections::HashMap::new();
    for sec in sections {
        let Some(id) = section_id(sec) else {
            return none();
        };
        by_id.entry(id).or_default().push(sec);
    }
    wanted
        .iter()
        .map(|f| {
            let key = match (f.status, f.old_path.as_deref()) {
                (FileStatus::Renamed, Some(old)) => (old.to_string(), f.path.clone(), true),
                _ => (f.path.clone(), f.path.clone(), false),
            };
            match by_id.get(&key).map(Vec::as_slice) {
                Some([one]) => Some(String::from_utf8_lossy(one).into_owned()),
                _ => None,
            }
        })
        .collect()
}

/// Trial-merges the PR's head into the destination (`git merge-tree`: no work tree, no ref
/// moves; it writes the merged objects, which nothing refers to). The report lists the
/// conflicted files; `report.tree` holds each with its conflict markers (read one with
/// `file_text`). Needs git 2.38. A blobless mirror may fetch the blobs it needs.
pub fn merge_check(
    git: &Git,
    local: &LocalRepo,
    refs: &PrRefs,
    auth: Option<&GitAuth>,
) -> Result<ConflictReport, GitError> {
    validate_operand("revision", &refs.dest)?;
    validate_operand("revision", &refs.head)?;
    let opts = RunOpts {
        auth,
        max_bytes: Some(MAX_MERGE_CHECK_BYTES),
        timeout: Some(LAZY_FETCH_TIMEOUT),
        ok_code: Some(1),
        ..RunOpts::default()
    };
    let out = git.run_with(
        &local.path,
        &[
            "merge-tree",
            "--write-tree",
            "-z",
            "--no-messages",
            &refs.dest,
            &refs.head,
        ],
        opts,
    )?;
    parse_merge_tree(&out).ok_or_else(|| GitError::Io("unexpected merge-tree output".into()))
}

/// Contents of `path` at `rev` (`git show rev:path`), lossy-decoded.
pub fn file_text(
    git: &Git,
    local: &LocalRepo,
    rev: &str,
    path: &str,
    auth: Option<&GitAuth>,
) -> Result<String, GitError> {
    validate_operand("revision", rev)?;
    // a path is only ever the tail of `rev:path`, so a leading dash is harmless
    if path.is_empty() || path.contains(['\n', '\0']) {
        return bad_ref("invalid path");
    }
    let spec = format!("{rev}:{path}");
    let out = run_diff(
        git,
        local,
        &["show", "--no-textconv", "--end-of-options", &spec],
        auth,
        MAX_PATCH_BYTES,
    )?;
    Ok(utf8_owned(out))
}

#[cfg(test)]
mod tests;
