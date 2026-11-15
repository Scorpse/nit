//! PR-oriented git operations: fetch both branches into private refs, compute the
//! merge-base, list changed files and produce per-file patches.

use crate::runner::{Git, GitAuth, GitError, RunOpts, LAZY_FETCH_TIMEOUT, NETWORK_TIMEOUT};
use crate::{validate_url, LocalRepo};
use prcore::diff::{
    parse_diff_index, parse_diff_raw, parse_numstat, DiffFile, FileCounts, FileStatus,
};

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
mod tests {
    use super::*;
    use crate::RepoKind;
    use prcore::diff::parse_patch;
    use std::path::Path;

    #[test]
    fn utf8_output_is_kept_without_a_copy_and_bad_bytes_are_replaced() {
        let valid = "+héllo ✓\n".as_bytes().to_vec();
        let at = valid.as_ptr();
        let s = utf8_owned(valid);
        assert_eq!(s, "+héllo ✓\n");
        assert_eq!(s.as_ptr(), at, "valid UTF-8 is not copied");
        let bad = b"+a\xff\xfeb\n+c\xe2\x82\n".to_vec();
        assert_eq!(utf8_owned(bad.clone()), String::from_utf8_lossy(&bad));
    }

    fn git() -> Git {
        Git::shared().unwrap()
    }

    fn g(dir: &Path, args: &[&str]) -> String {
        let mut full = vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ];
        full.extend(args);
        String::from_utf8_lossy(&git().run(dir, &full, None).unwrap()).into_owned()
    }

    fn commit_all(dir: &Path, msg: &str) {
        g(dir, &["add", "-A"]);
        g(dir, &["commit", "-qm", msg]);
    }

    fn auth() -> GitAuth {
        GitAuth::bitbucket("tok")
    }

    fn file_url(p: &Path) -> String {
        format!("file:///{}", p.to_string_lossy().replace('\\', "/"))
    }

    fn lines(prefix: &str, n: usize) -> String {
        (0..n).map(|i| format!("{prefix} line {i}\n")).collect()
    }

    /// Remote with `main` (c1, then c2 after branching) and `feature` (branched at c1).
    struct Fixture {
        remote: tempfile::TempDir,
        _local: tempfile::TempDir,
        local_repo: LocalRepo,
        c1: String,
    }

    fn fixture() -> Fixture {
        let remote = tempfile::tempdir().unwrap();
        let r = remote.path();
        g(r, &["init", "-q", "-b", "main"]);
        std::fs::write(r.join("mod.txt"), lines("mod", 10)).unwrap();
        std::fs::write(r.join("del.txt"), lines("del", 5)).unwrap();
        std::fs::write(r.join("old_name.txt"), lines("rename me", 20)).unwrap();
        std::fs::write(r.join("bin.dat"), [0u8, 1, 2, 3, 0, 255]).unwrap();
        commit_all(r, "c1");
        let c1 = g(r, &["rev-parse", "HEAD"]).trim().to_string();
        g(r, &["checkout", "-q", "-b", "feature"]);
        let m = lines("mod", 10).replace("mod line 3\n", "mod line 3 changed\n");
        std::fs::write(r.join("mod.txt"), m).unwrap();
        std::fs::remove_file(r.join("del.txt")).unwrap();
        std::fs::rename(r.join("old_name.txt"), r.join("new_name.txt")).unwrap();
        std::fs::write(r.join("added.txt"), "new\n").unwrap();
        std::fs::write(r.join("bin.dat"), [0u8, 9, 9, 0, 0, 255, 7]).unwrap();
        commit_all(r, "feature work");
        g(r, &["checkout", "-q", "main"]);
        std::fs::write(r.join("main_only.txt"), "main moved on\n").unwrap();
        commit_all(r, "c2 on main");

        let local = tempfile::tempdir().unwrap();
        g(local.path(), &["init", "-q", "-b", "trunk"]);
        std::fs::write(local.path().join("user.txt"), "mine\n").unwrap();
        commit_all(local.path(), "user commit");
        let local_repo = LocalRepo {
            path: local.path().to_path_buf(),
            kind: RepoKind::Clone,
        };
        Fixture {
            remote,
            _local: local,
            local_repo,
            c1,
        }
    }

    fn branches(src: &str, dst: &str) -> FetchSpec {
        FetchSpec::Branches {
            src: src.into(),
            dst: dst.into(),
        }
    }

    fn fetch(f: &Fixture, id: u64, src: &str, dst: &str) -> Result<PrRefs, GitError> {
        fetch_spec(f, "bitbucket.org", id, &branches(src, dst))
    }

    fn fetch_spec(f: &Fixture, host: &str, id: u64, spec: &FetchSpec) -> Result<PrRefs, GitError> {
        fetch_pr_inner(
            &git(),
            &f.local_repo,
            &file_url(f.remote.path()),
            &auth(),
            &ref_prefix(host),
            id,
            spec,
            true,
        )
    }

    #[test]
    fn bitbucket_refs_namespace_unchanged() {
        assert_eq!(ref_prefix("bitbucket.org"), "refs/nit");
        assert_eq!(ref_prefix("Bitbucket.org"), "refs/nit");
        assert_eq!(ref_prefix("github.com"), "refs/nit/github.com");
        assert_eq!(ref_prefix("ghe.corp:8443"), "refs/nit/ghe.corp_8443");
        assert_eq!(
            ref_prefix("git.corp.example/gitlab"),
            "refs/nit/git.corp.example_gitlab"
        );
    }

    #[test]
    fn pull_ref_fetch_lands_in_host_namespace() {
        let f = fixture();
        // the forge keeps the PR head under its own ref, not a branch
        let head = g(f.remote.path(), &["rev-parse", "feature"])
            .trim()
            .to_string();
        g(f.remote.path(), &["update-ref", "refs/pull/7/head", &head]);
        let spec = FetchSpec::PullRef {
            src_ref: "refs/pull/7/head".into(),
            dst: "main".into(),
        };
        let refs = fetch_spec(&f, "github.com", 7, &spec).unwrap();
        assert_eq!(refs.head, head);
        assert_eq!(refs.base, f.c1);
        let p = &f.local_repo.path;
        let all = g(p, &["for-each-ref", "--format=%(refname)", "refs/nit"]);
        assert!(all.contains("refs/nit/github.com/pr-7/src"), "{all}");
        assert!(all.contains("refs/nit/github.com/pr-7/dst"), "{all}");
        assert!(!all.contains("refs/nit/pr-7/"), "{all}");
    }

    #[test]
    fn two_forges_in_one_clone_do_not_collide() {
        let f = fixture();
        let feature = g(f.remote.path(), &["rev-parse", "feature"])
            .trim()
            .to_string();
        let main = g(f.remote.path(), &["rev-parse", "main"])
            .trim()
            .to_string();
        // PR 7 on Bitbucket is `feature`; MR 7 on a GitLab host is `main`'s tip
        g(
            f.remote.path(),
            &["update-ref", "refs/merge-requests/7/head", &main],
        );
        let bb = fetch(&f, 7, "feature", "main").unwrap();
        let spec = FetchSpec::PullRef {
            src_ref: "refs/merge-requests/7/head".into(),
            dst: "main".into(),
        };
        let gl = fetch_spec(&f, "gitlab.example", 7, &spec).unwrap();
        assert_eq!(bb.head, feature);
        assert_eq!(gl.head, main);
        let p = &f.local_repo.path;
        assert_eq!(g(p, &["rev-parse", "refs/nit/pr-7/src"]).trim(), feature);
        assert_eq!(
            g(p, &["rev-parse", "refs/nit/gitlab.example/pr-7/src"]).trim(),
            main
        );
    }

    #[test]
    fn pull_ref_and_prefix_are_validated_before_running_git() {
        let dead = Git::from_exe("definitely-not-a-git-binary");
        let local = LocalRepo {
            path: std::env::temp_dir(),
            kind: RepoKind::Clone,
        };
        let run = |prefix: &str, spec: &FetchSpec| {
            fetch_pr_inner(
                &dead,
                &local,
                "https://h/x",
                &auth(),
                prefix,
                1,
                spec,
                false,
            )
        };
        for bad in ["pull/1/head", "refs/../x", "refs/a b", "refs/"] {
            let spec = FetchSpec::PullRef {
                src_ref: bad.into(),
                dst: "main".into(),
            };
            let err = run("refs/nit", &spec).unwrap_err();
            assert!(matches!(err, GitError::InvalidRef(_)), "{bad:?}: {err:?}");
        }
        for bad in [
            "refs/heads",
            "refs/nit/",
            "refs/nit/a:b",
            "refs/nit/a/b",
            "refs/nitx",
        ] {
            let err = run(bad, &branches("a", "b")).unwrap_err();
            assert!(matches!(err, GitError::InvalidRef(_)), "{bad:?}: {err:?}");
        }
    }

    #[test]
    fn fetch_pr_returns_merge_base_not_dest_tip() {
        let f = fixture();
        let refs = fetch(&f, 7, "feature", "main").unwrap();
        assert_eq!(refs.base, f.c1);
        assert_ne!(refs.dest, f.c1);
        assert_eq!(
            refs.head,
            g(f.remote.path(), &["rev-parse", "feature"]).trim()
        );
        assert_eq!(refs.dest, g(f.remote.path(), &["rev-parse", "main"]).trim());
        let p = &f.local_repo.path;
        assert_eq!(g(p, &["rev-parse", "refs/nit/pr-7/src"]).trim(), refs.head);
        assert_eq!(g(p, &["rev-parse", "refs/nit/pr-7/dst"]).trim(), refs.dest);
    }

    #[test]
    fn file_index_lists_add_modify_delete_rename_binary() {
        let f = fixture();
        let refs = fetch(&f, 1, "feature", "main").unwrap();
        let files = file_index(&git(), &f.local_repo, &refs, None).unwrap();
        let by = |p: &str| {
            files
                .iter()
                .find(|d| d.path == p)
                .unwrap_or_else(|| panic!("{p}: {files:?}"))
        };
        assert_eq!(files.len(), 5, "{files:?}");
        assert_eq!(by("added.txt").status, FileStatus::Added);
        assert_eq!(by("added.txt").additions, 1);
        assert_eq!(by("mod.txt").status, FileStatus::Modified);
        assert_eq!((by("mod.txt").additions, by("mod.txt").deletions), (1, 1));
        let del = by("del.txt");
        assert_eq!(del.status, FileStatus::Deleted);
        assert!(del.is_deleted_oid(), "{}", del.new_oid);
        assert_eq!(del.new_oid.len(), 40);
        let ren = by("new_name.txt");
        assert_eq!(ren.status, FileStatus::Renamed);
        assert_eq!(ren.old_path.as_deref(), Some("old_name.txt"));
        assert!(by("bin.dat").binary);
        assert!(!by("mod.txt").binary);
        assert_eq!(by("mod.txt").new_oid.len(), 40);
        // dest-only change must not appear (three-dot semantics)
        assert!(files.iter().all(|d| d.path != "main_only.txt"));
    }

    #[test]
    fn file_list_then_counts_equals_the_joined_index() {
        let f = fixture();
        let refs = fetch(&f, 1, "feature", "main").unwrap();
        let joined = file_index(&git(), &f.local_repo, &refs, None).unwrap();
        let mut list = file_list(&git(), &f.local_repo, &refs, None).unwrap();
        assert_eq!(list.len(), joined.len());
        assert!(list.iter().all(|d| !d.counts_known && !d.binary));
        let counts = file_counts(&git(), &f.local_repo, &refs, None, &|| false).unwrap();
        list.reverse(); // matched by path, whatever the order
        prcore::diff::apply_counts(&mut list, &counts);
        list.reverse();
        assert_eq!(list, joined);
    }

    #[test]
    fn file_counts_stops_when_cancelled() {
        let f = fixture();
        let refs = fetch(&f, 1, "feature", "main").unwrap();
        let err = file_counts(&git(), &f.local_repo, &refs, None, &|| true).unwrap_err();
        assert!(matches!(err, GitError::Cancelled), "{err:?}");
    }

    #[test]
    fn file_patch_matches_expected_lines() {
        let f = fixture();
        let refs = fetch(&f, 1, "feature", "main").unwrap();
        let files = file_index(&git(), &f.local_repo, &refs, None).unwrap();
        let get = |p: &str| {
            let d = files.iter().find(|d| d.path == p).unwrap();
            file_patch(&git(), &f.local_repo, &refs, d, None).unwrap()
        };
        let m = get("mod.txt");
        assert!(m.contains("-mod line 3\n"), "{m}");
        assert!(m.contains("+mod line 3 changed\n"), "{m}");
        assert!(m.contains("@@"), "{m}");
        let a = get("added.txt");
        assert!(a.contains("+new\n"), "{a}");
        let r = get("new_name.txt");
        assert!(r.contains("rename from old_name.txt"), "{r}");
        assert!(r.contains("rename to new_name.txt"), "{r}");
        assert!(get("bin.dat").contains("Binary files"));
        let t = file_text(&git(), &f.local_repo, &refs.head, "added.txt", None).unwrap();
        assert_eq!(t, "new\n");
    }

    #[test]
    fn fetch_does_not_touch_user_branches_or_worktree() {
        let f = fixture();
        let p = &f.local_repo.path;
        std::fs::write(p.join("dirty.txt"), "untracked\n").unwrap();
        std::fs::write(p.join("user.txt"), "mine edited\n").unwrap();
        g(p, &["branch", "side"]);
        let snap = |p: &Path| {
            (
                g(p, &["rev-parse", "HEAD"]),
                g(p, &["symbolic-ref", "HEAD"]),
                g(p, &["branch", "--format=%(refname) %(objectname)"]),
                g(p, &["status", "--porcelain"]),
                g(p, &["ls-files", "-s"]),
                std::fs::read_to_string(p.join("user.txt")).unwrap(),
            )
        };
        let before = snap(p);
        fetch(&f, 3, "feature", "main").unwrap();
        assert_eq!(snap(p), before);
        let refs = g(p, &["for-each-ref", "--format=%(refname)"]);
        assert!(refs.contains("refs/nit/pr-3/src"), "{refs}");
        assert!(refs.contains("refs/nit/pr-3/dst"), "{refs}");
        assert!(!refs.contains("refs/remotes"), "{refs}");
        assert!(!refs.contains("refs/tags"), "{refs}");
    }

    /// A clone or mirror fetched into by prview (before the rename) keeps its old refs; the
    /// next fetch goes to the new namespace, works the same and leaves the user's state alone.
    #[test]
    fn old_prview_refs_stay_and_the_fetch_goes_to_the_new_namespace() {
        let f = fixture();
        let p = &f.local_repo.path;
        let user_tip = g(p, &["rev-parse", "HEAD"]).trim().to_string();
        g(p, &["update-ref", "refs/prview/pr-3/src", &user_tip]);
        g(p, &["update-ref", "refs/prview/pr-3/dst", &user_tip]);
        let snap = |p: &Path| {
            (
                g(p, &["symbolic-ref", "HEAD"]),
                g(p, &["branch", "--format=%(refname) %(objectname)"]),
                g(p, &["status", "--porcelain"]),
                g(p, &["ls-files", "-s"]),
            )
        };
        let before = snap(p);
        let refs = fetch(&f, 3, "feature", "main").unwrap();
        assert_eq!(snap(p), before);
        assert_eq!(refs.base, f.c1);
        assert_eq!(g(p, &["rev-parse", "refs/nit/pr-3/src"]).trim(), refs.head);
        for old in ["refs/prview/pr-3/src", "refs/prview/pr-3/dst"] {
            assert_eq!(g(p, &["rev-parse", old]).trim(), user_tip, "{old}");
        }
    }

    #[test]
    fn bad_branch_names_rejected() {
        let dead = Git::from_exe("definitely-not-a-git-binary");
        let local = LocalRepo {
            path: std::env::temp_dir(),
            kind: RepoKind::Clone,
        };
        for bad in [
            "",
            "-x",
            "--upload-pack=evil",
            "a..b",
            "a b",
            "a\tb",
            "a\nb",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[0]",
            "a\\b",
            "/a",
            "a/",
            "a//b",
            "x.lock",
            "a@{1}",
            "a.",
            ".hidden",
        ] {
            for (s, d) in [(bad, "main"), ("main", bad)] {
                let err = fetch_pr_inner(
                    &dead,
                    &local,
                    "https://h/x",
                    &auth(),
                    "refs/nit",
                    1,
                    &branches(s, d),
                    false,
                )
                .unwrap_err();
                assert!(matches!(err, GitError::InvalidRef(_)), "{bad:?}: {err:?}");
            }
        }
        // non-https url is rejected too
        let err = fetch_pr(
            &dead,
            &local,
            "http://h/x",
            &auth(),
            "refs/nit",
            1,
            &branches("a", "b"),
        )
        .unwrap_err();
        assert!(matches!(err, GitError::InvalidUrl(_)), "{err:?}");
        let err = fetch_pr(
            &dead,
            &local,
            "file:///x",
            &auth(),
            "refs/nit",
            1,
            &branches("a", "b"),
        )
        .unwrap_err();
        assert!(matches!(err, GitError::InvalidUrl(_)), "{err:?}");
        // good names pass validation
        for ok in ["main", "feature/x-1", "release_2.0", "ünï"] {
            assert!(validate_branch(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn deleted_source_branch_gives_failed_error() {
        let f = fixture();
        let err = fetch(&f, 2, "no-such-branch", "main").unwrap_err();
        assert!(matches!(err, GitError::Failed { .. }), "{err:?}");
    }

    #[test]
    fn failure_text_is_scrubbed_of_the_token() {
        let f = fixture();
        let a = GitAuth::bitbucket("s3cr3tTOKEN");
        let url = file_url(&f.remote.path().join("missing-s3cr3tTOKEN"));
        let err = fetch_pr_inner(
            &git(),
            &f.local_repo,
            &url,
            &a,
            "refs/nit",
            9,
            &branches("a", "b"),
            true,
        )
        .unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(shown.contains("***"), "{shown}");
        assert!(!shown.contains("s3cr3tTOKEN"), "{shown}");
    }

    #[test]
    fn unrelated_histories_give_no_merge_base() {
        let f = fixture();
        let r = f.remote.path();
        g(r, &["checkout", "-q", "--orphan", "island"]);
        g(r, &["rm", "-rfq", "."]);
        std::fs::write(r.join("island.txt"), "x\n").unwrap();
        commit_all(r, "island");
        let err = fetch(&f, 4, "island", "main").unwrap_err();
        assert_eq!(err, GitError::NoMergeBase);
        assert!(err.to_string().contains("unrelated"));
    }

    #[test]
    fn oversized_output_is_too_large() {
        let f = fixture();
        let p = &f.local_repo.path;
        let args = ["log", "--format=%H"];
        let err = git().run_limited(p, &args, None, 10).unwrap_err();
        assert_eq!(err, GitError::TooLarge { limit: 10 });
        assert!(git().run_limited(p, &args, None, 1 << 20).is_ok());
    }

    #[test]
    fn patch_for_path_with_glob_chars_is_literal() {
        let f = fixture();
        let r = f.remote.path();
        g(r, &["checkout", "-q", "feature"]);
        std::fs::write(r.join("a[1].txt"), "x\n").unwrap();
        std::fs::write(r.join("a1.txt"), "y\n").unwrap();
        commit_all(r, "globs");
        let refs = fetch(&f, 5, "feature", "main").unwrap();
        let files = file_index(&git(), &f.local_repo, &refs, None).unwrap();
        let d = files.iter().find(|d| d.path == "a[1].txt").unwrap();
        let p = file_patch(&git(), &f.local_repo, &refs, d, None).unwrap();
        assert!(p.contains("+x\n") && !p.contains("+y\n"), "{p}");
    }

    fn fixture_for(src: &Path) -> Fixture {
        let remote = tempfile::tempdir().unwrap();
        g(
            remote.path(),
            &["clone", "-q", "--bare", &file_url(src), "."],
        );
        let local = tempfile::tempdir().unwrap();
        g(local.path(), &["init", "-q", "-b", "trunk"]);
        let local_repo = LocalRepo {
            path: local.path().to_path_buf(),
            kind: RepoKind::Clone,
        };
        Fixture {
            remote,
            _local: local,
            local_repo,
            c1: String::new(),
        }
    }

    #[test]
    fn renamed_and_modified_file_has_correct_counts_and_dash_path_reads() {
        let r = tempfile::tempdir().unwrap();
        let p = r.path();
        g(p, &["init", "-q", "-b", "main"]);
        std::fs::write(p.join("a.txt"), lines("x", 20)).unwrap();
        commit_all(p, "c1");
        g(p, &["checkout", "-q", "-b", "feature"]);
        std::fs::remove_file(p.join("a.txt")).unwrap();
        let m = lines("x", 20).replace("x line 5\n", "x line five\n") + "extra\n";
        std::fs::write(p.join("b.txt"), m).unwrap();
        std::fs::write(p.join("-x.txt"), "dash\n").unwrap();
        commit_all(p, "rename+edit");
        g(p, &["checkout", "-q", "main"]);
        let f = fixture_for(p);
        let refs = fetch(&f, 1, "feature", "main").unwrap();
        let files = file_index(&git(), &f.local_repo, &refs, None).unwrap();
        let d = files.iter().find(|d| d.path == "b.txt").unwrap();
        assert_eq!(d.status, FileStatus::Renamed);
        assert_eq!(d.old_path.as_deref(), Some("a.txt"));
        assert_eq!((d.additions, d.deletions), (2, 1), "{d:?}");
        let t = file_text(&git(), &f.local_repo, &refs.head, "-x.txt", None).unwrap();
        assert_eq!(t, "dash\n");
    }

    #[test]
    fn resolve_commits_returns_both_refs_with_one_spawn() {
        let f = fixture();
        let refs = fetch(&f, 11, "feature", "main").unwrap();
        let counted = Git::locate().unwrap();
        let got = resolve_commits(
            &counted,
            &f.local_repo,
            ["refs/nit/pr-11/src", "refs/nit/pr-11/dst"],
        )
        .unwrap();
        assert_eq!(got, [refs.head.clone(), refs.dest.clone()]);
        assert_eq!(counted.spawn_count(), 1);
        let err = resolve_commits(
            &counted,
            &f.local_repo,
            ["refs/nit/pr-11/src", "refs/nit/none"],
        )
        .unwrap_err();
        assert!(
            matches!(err, GitError::Failed { .. } | GitError::InvalidRef(_)),
            "{err:?}"
        );
        assert!(matches!(
            resolve_commits(&counted, &f.local_repo, ["-x", "HEAD"]),
            Err(GitError::InvalidRef(_))
        ));
    }

    /// Every kind of change a patch can carry, between `main` and `feature`.
    fn matrix_repo() -> tempfile::TempDir {
        let r = tempfile::tempdir().unwrap();
        let p = r.path();
        let w = |name: &str, data: &[u8]| {
            let path = p.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, data).unwrap();
        };
        g(p, &["init", "-q", "-b", "main"]);
        g(p, &["config", "core.autocrlf", "false"]);
        // the mode change of mode.sh is made in the index only (`update-index --chmod`), so
        // the work tree must not be compared by mode: Git for Windows sets this at init,
        // elsewhere the 644 file on disk would block the checkout back to main
        g(p, &["config", "core.fileMode", "false"]);
        let long_line = "x".repeat(100_000);
        w("mod.txt", lines("mod", 30).as_bytes());
        w("del.txt", lines("del", 5).as_bytes());
        w("old name ü.txt", lines("rename me", 20).as_bytes());
        w("plain_old.txt", lines("moved as is", 12).as_bytes());
        w("bin.dat", &[0, 1, 2, 3, 0, 255]);
        w("mode.sh", b"echo hi\n");
        w(
            "crlf.txt",
            lines("crlf", 8).replace('\n', "\r\n").as_bytes(),
        );
        w("noeol.txt", b"a\nb\nc");
        w("long.txt", format!("short\n{long_line}\nend\n").as_bytes());
        w("with space.txt", lines("space", 6).as_bytes());
        w("a[1].txt", b"bracket\n");
        w("a1.txt", b"no bracket\n");
        w("ünïcödé/файл.txt", lines("utf", 6).as_bytes());
        w("tab\u{e9}\u{301}.txt", b"combining\n");
        w("unchanged.txt", b"same\n");
        commit_all(p, "c1");
        g(p, &["checkout", "-q", "-b", "feature"]);
        w(
            "mod.txt",
            lines("mod", 30)
                .replace("mod line 3\n", "mod line 3 changed\n")
                .replace("mod line 25\n", "")
                .as_bytes(),
        );
        std::fs::remove_file(p.join("del.txt")).unwrap();
        std::fs::remove_file(p.join("old name ü.txt")).unwrap();
        w(
            "new name ü [x].txt",
            (lines("rename me", 20).replace("rename me line 7\n", "edited\n") + "more\n")
                .as_bytes(),
        );
        std::fs::remove_file(p.join("plain_old.txt")).unwrap();
        w("dir/plain_new.txt", lines("moved as is", 12).as_bytes());
        w("bin.dat", &[0, 9, 9, 0, 0, 255, 7]);
        w(
            "crlf.txt",
            lines("crlf", 8)
                .replace("crlf line 2\n", "crlf line two\n")
                .replace('\n', "\r\n")
                .as_bytes(),
        );
        w("noeol.txt", b"a\nB\nc");
        w(
            "long.txt",
            format!("short\n{}y\nend\n", &long_line[1..]).as_bytes(),
        );
        w("with space.txt", lines("space", 7).as_bytes());
        w("a[1].txt", b"bracket changed\n");
        w("ünïcödé/файл.txt", lines("utf", 3).as_bytes());
        w("tab\u{e9}\u{301}.txt", b"combining changed\n");
        w("empty.txt", b"");
        w("added.txt", b"new\n");
        w("noeol_add.txt", b"no newline");
        g(p, &["add", "-A"]);
        g(p, &["update-index", "--chmod=+x", "mode.sh"]);
        g(p, &["commit", "-qm", "every kind of change"]);
        g(p, &["checkout", "-q", "main"]);
        r
    }

    fn assert_batch_matches_per_file(f: &Fixture, refs: &PrRefs, files: &[DiffFile]) {
        let local = &f.local_repo;
        let per: Vec<String> = files
            .iter()
            .map(|d| file_patch(&git(), local, refs, d, None).unwrap())
            .collect();
        let all: Vec<&DiffFile> = files.iter().collect();
        let batch = batch_patches(&git(), local, refs, &all, None).unwrap();
        let missing: Vec<&str> = batch
            .missing
            .iter()
            .map(|&i| files[i].path.as_str())
            .collect();
        assert!(missing.is_empty(), "not attributed: {missing:?}");
        assert_eq!(batch.found.len(), files.len());
        for (i, text) in &batch.found {
            let d = &files[*i];
            assert_eq!(text, &per[*i], "bytes differ for {:?}", d.path);
            assert_eq!(
                parse_patch(text),
                parse_patch(&per[*i]),
                "rows differ for {:?}",
                d.path
            );
            assert!(!per[*i].is_empty(), "{:?} has no patch", d.path);
        }
        // file_patches gives the same, in request order
        let pairs = file_patches(&git(), local, refs, &all, None).unwrap();
        for ((path, text), (d, want)) in pairs.iter().zip(files.iter().zip(&per)) {
            assert_eq!(path, &d.path);
            assert_eq!(text, want, "{path:?}");
        }
        // any neighbourhood of the list gives the same sections
        for w in all.windows(4).step_by(3) {
            let part = batch_patches(&git(), local, refs, w, None).unwrap();
            assert!(part.missing.is_empty(), "{w:?}");
            for (i, text) in &part.found {
                let at = files.iter().position(|d| d.path == w[*i].path).unwrap();
                assert_eq!(text, &per[at]);
            }
        }
    }

    #[test]
    fn batch_sections_equal_per_file_patches_for_every_kind_of_change() {
        let src = matrix_repo();
        let f = fixture_for(src.path());
        let refs = fetch(&f, 1, "feature", "main").unwrap();
        let files = file_index(&git(), &f.local_repo, &refs, None).unwrap();
        let by = |p: &str| {
            files
                .iter()
                .find(|d| d.path == p)
                .unwrap_or_else(|| panic!("{p}: {files:?}"))
        };
        // the matrix really has what it claims
        assert_eq!(by("new name ü [x].txt").status, FileStatus::Renamed);
        assert_eq!(by("dir/plain_new.txt").status, FileStatus::Renamed);
        assert_eq!(by("del.txt").status, FileStatus::Deleted);
        assert_eq!(by("empty.txt").status, FileStatus::Added);
        assert!(by("bin.dat").binary);
        assert_eq!((by("mode.sh").additions, by("mode.sh").deletions), (0, 0));
        assert!(files.iter().all(|d| d.path != "unchanged.txt"));
        assert_eq!(files.len(), 16, "{files:?}");
        assert_batch_matches_per_file(&f, &refs, &files);
        // the same with unquoted non-ASCII headers
        g(&f.local_repo.path, &["config", "core.quotepath", "off"]);
        assert_batch_matches_per_file(&f, &refs, &files);
        // and when the repository asks for headers without prefixes
        g(&f.local_repo.path, &["config", "diff.noprefix", "true"]);
        assert_batch_matches_per_file(&f, &refs, &files);
    }

    fn file(path: &str, old: Option<&str>) -> DiffFile {
        DiffFile {
            path: path.into(),
            old_path: old.map(Into::into),
            status: if old.is_some() {
                FileStatus::Renamed
            } else {
                FileStatus::Modified
            },
            additions: 1,
            deletions: 1,
            binary: false,
            counts_known: true,
            new_oid: String::new(),
        }
    }

    #[test]
    fn attribution_never_guesses() {
        let sec = |p: &str| {
            format!("diff --git a/{p} b/{p}\nindex 1..2 100644\n--- a/{p}\n+++ b/{p}\n@@ -1 +1 @@\n-x\n+y\n")
        };
        let (a, b, c) = (file("a", None), file("b c", None), file("n", Some("o")));
        let all = [&a, &b, &c];
        let ren = "diff --git a/o b/n\nsimilarity index 90%\nrename from o\nrename to n\n";
        let ok = format!("{}{}{ren}", sec("a"), sec("b c"));
        let got = attribute(ok.as_bytes(), &all);
        assert_eq!(got[0].as_deref(), Some(sec("a").as_str()));
        assert_eq!(got[1].as_deref(), Some(sec("b c").as_str()));
        assert_eq!(got[2].as_deref(), Some(ren));
        // a missing section: only that file is unattributed
        let got = attribute(sec("a").as_bytes(), &all);
        assert!(got[0].is_some() && got[1].is_none() && got[2].is_none());
        // a different rename pairing than the one requested is not taken
        let other = "diff --git a/x b/n\nrename from x\nrename to n\n";
        assert!(attribute(other.as_bytes(), &[&c])[0].is_none());
        // two sections for one path (a type change): neither is taken
        let twice = format!("{}{}", sec("a"), sec("a"));
        assert!(attribute(twice.as_bytes(), &[&a])[0].is_none());
        // unreadable header or stray text: nothing of that output is trusted
        let bad = format!("{}diff --git a/one b/two\n@@ -1 +1 @@\n", sec("a"));
        assert!(attribute(bad.as_bytes(), &[&a]).iter().all(Option::is_none));
        let stray = format!("warning: x\n{}", sec("a"));
        assert!(attribute(stray.as_bytes(), &[&a])[0].is_none());
        // quoted headers and rename lines are unquoted
        let q = "diff --git \"a/\\303\\274 \\\"q\\\"\" \"b/\\303\\274 \\\"q\\\"\"\n@@ -1 +1 @@\n";
        assert!(attribute(q.as_bytes(), &[&file("ü \"q\"", None)])[0].is_some());
        let qr = "diff --git \"a/\\303\\274\" b/n\nrename from \"\\303\\274\"\nrename to n\n";
        assert!(attribute(qr.as_bytes(), &[&file("n", Some("ü"))])[0].is_some());
        // an unquoted header that splits two ways is only read as the same path twice
        let sp = "diff --git a/x b/x b/x b/x\n@@ -1 +1 @@\n";
        assert!(attribute(sp.as_bytes(), &[&file("x b/x", None)])[0].is_some());
        assert_eq!(unquote(b"\"\\q\""), None);
        assert_eq!(unquote(b"\"open"), None);
    }

    #[test]
    fn long_pathspec_lists_are_split_under_the_command_line_budget() {
        let r = tempfile::tempdir().unwrap();
        let p = r.path();
        g(p, &["init", "-q", "--bare"]);
        let dir = format!("d/{}", "long_directory_name_".repeat(5));
        let n = 2000;
        let name = |i: usize| format!("{dir}/f{i:04}_{}.txt", "p".repeat(20));
        // two commits of 2000 files, written straight into the object store
        let mut stream = String::new();
        for (branch, v, from) in [("main", 1, ""), ("feature", 2, "from refs/heads/main\n")] {
            stream += &format!(
                "commit refs/heads/{branch}\ncommitter t <t@t> 0 +0000\ndata 1\nc\n{from}"
            );
            for i in 0..n {
                let data = format!("v{v} {i}\n");
                stream += &format!("M 100644 inline {}\ndata {}\n{data}", name(i), data.len());
            }
            stream += "\n";
        }
        let mut child = std::process::Command::new(git().exe())
            .args(["fast-import", "--quiet"])
            .current_dir(p)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stream.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        let f = fixture_for(p);
        let refs = fetch(&f, 1, "feature", "main").unwrap();
        let files = file_index(&git(), &f.local_repo, &refs, None).unwrap();
        assert_eq!(files.len(), n);
        let all: Vec<&DiffFile> = files.iter().collect();
        let total: usize = all.iter().map(|d| arg_cost(&d.path)).sum();
        assert!(total > 8 * MAX_PATHSPEC_BYTES, "{total}");
        let chunks = pathspec_chunks(&all, MAX_PATHSPEC_BYTES);
        for c in &chunks {
            let cost: usize = c.iter().map(|&i| arg_cost(&all[i].path)).sum();
            assert!(cost <= MAX_PATHSPEC_BYTES, "{cost}");
        }
        assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), n);
        let counted = Git::locate().unwrap();
        let batch = batch_patches(&counted, &f.local_repo, &refs, &all, None).unwrap();
        assert_eq!(counted.spawn_count(), chunks.len());
        assert!(chunks.len() <= total / MAX_PATHSPEC_BYTES + 2);
        assert!(batch.missing.is_empty());
        assert_eq!(batch.found.len(), n);
        for (i, text) in batch.found.iter().step_by(97) {
            let want = file_patch(&git(), &f.local_repo, &refs, all[*i], None).unwrap();
            assert_eq!(text, &want);
        }
        // a rename's two paths stay in one run, and a lone huge file gets its own run
        let ren = file(&"n".repeat(30), Some(&"o".repeat(30)));
        let big = file(&"b".repeat(100), None);
        let small = file("s", None);
        let chunks = pathspec_chunks(&[&small, &ren, &big, &small], 70);
        assert_eq!(chunks, vec![vec![0, 1], vec![2], vec![3]]);
    }

    /// `cargo test -p gitlocal --release -- --ignored --nocapture bench_batch`
    /// The audit's fixture: 1000 files of 40 lines with 10 changed, one patch each.
    #[test]
    #[ignore]
    fn bench_batch_against_per_file() {
        use std::time::Instant;
        let r = tempfile::tempdir().unwrap();
        let p = r.path();
        g(p, &["init", "-q", "-b", "main"]);
        let n = 1000;
        let body = |i: usize, v: &str| -> String {
            (0..40)
                .map(|l| {
                    if l % 4 == 0 {
                        format!("fn f{i}_{l}() {{ {v} }}\n")
                    } else {
                        format!("// file {i} line {l}\n")
                    }
                })
                .collect()
        };
        for i in 0..n {
            let d = p.join(format!("src/m{:02}", i / 50));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(format!("file_{i:04}.rs")), body(i, "1")).unwrap();
        }
        commit_all(p, "c1");
        g(p, &["checkout", "-q", "-b", "feature"]);
        for i in 0..n {
            let d = p.join(format!("src/m{:02}", i / 50));
            std::fs::write(d.join(format!("file_{i:04}.rs")), body(i, "2")).unwrap();
        }
        commit_all(p, "c2");
        g(p, &["checkout", "-q", "main"]);
        let f = fixture_for(p);
        let refs = fetch(&f, 1, "feature", "main").unwrap();
        let files = file_index(&git(), &f.local_repo, &refs, None).unwrap();
        let all: Vec<&DiffFile> = files.iter().collect();
        println!("git: {}", git().exe().display());
        // spawn cost of the launcher against the real binary (what `locate` now picks)
        let launcher = std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("git.exe"))
                .find(|e| e.ends_with("cmd\\git.exe") && e.is_file())
        });
        if let Some(l) = launcher {
            for exe in [l.as_path(), git().exe()] {
                let g = Git::from_exe(exe);
                let t = Instant::now();
                for _ in 0..20 {
                    g.run(p, &["--version"], None).unwrap();
                }
                println!("{}: {:?} per spawn", exe.display(), t.elapsed() / 20);
            }
        }
        let t = Instant::now();
        let seq: Vec<Vec<prcore::diff::Row>> = all
            .iter()
            .map(|d| parse_patch(&file_patch(&git(), &f.local_repo, &refs, d, None).unwrap()))
            .collect();
        println!("per-file, sequential: {:?}", t.elapsed());
        let t = Instant::now();
        std::thread::scope(|s| {
            for part in all.chunks(n / 4) {
                let (local, refs) = (&f.local_repo, &refs);
                s.spawn(move || {
                    for d in part {
                        parse_patch(&file_patch(&git(), local, refs, d, None).unwrap());
                    }
                });
            }
        });
        println!("per-file, 4 threads: {:?}", t.elapsed());
        let t = Instant::now();
        let batch = batch_patches(&git(), &f.local_repo, &refs, &all, None).unwrap();
        let rows: Vec<_> = batch.found.iter().map(|(_, t)| parse_patch(t)).collect();
        println!(
            "batch: {:?} ({} found, {} missing)",
            t.elapsed(),
            batch.found.len(),
            batch.missing.len()
        );
        for ((i, _), r) in batch.found.iter().zip(&rows) {
            assert_eq!(r, &seq[*i]);
        }
    }

    fn missing_objects(p: &Path) -> String {
        g(p, &["rev-list", "--objects", "--all", "--missing=print"])
            .lines()
            .filter(|l| l.starts_with('?'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn blobless_mirror_lazily_fetches_blobs_with_auth() {
        let f = fixture();
        let r = f.remote.path();
        g(r, &["config", "uploadpack.allowFilter", "true"]);
        g(r, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        let base_blob = g(r, &["rev-parse", &format!("{}:mod.txt", f.c1)])
            .trim()
            .to_string();
        let mirror_dir = tempfile::tempdir().unwrap();
        let mp = mirror_dir.path().join("m.git");
        g(
            mirror_dir.path(),
            &[
                "clone",
                "-q",
                "--bare",
                "--filter=blob:none",
                &file_url(r),
                mp.to_str().unwrap(),
            ],
        );
        let mirror = LocalRepo {
            path: mp.clone(),
            kind: RepoKind::Mirror,
        };
        assert!(
            missing_objects(&mp).contains(&base_blob),
            "blob present before"
        );
        let refs = fetch_pr_inner(
            &git(),
            &mirror,
            &file_url(r),
            &auth(),
            "refs/nit",
            1,
            &branches("feature", "main"),
            true,
        )
        .unwrap();
        let a = auth();
        let files = file_index(&git(), &mirror, &refs, Some(&a)).unwrap();
        let d = files.iter().find(|d| d.path == "mod.txt").unwrap();
        assert_eq!((d.additions, d.deletions), (1, 1));
        let patch = file_patch(&git(), &mirror, &refs, d, Some(&a)).unwrap();
        assert!(patch.contains("+mod line 3 changed\n"), "{patch}");
        let text = file_text(&git(), &mirror, &refs.base, "mod.txt", Some(&a)).unwrap();
        assert!(text.contains("mod line 3\n"));
        assert!(
            !missing_objects(&mp).contains(&base_blob),
            "blob was not lazily fetched"
        );
    }
}
