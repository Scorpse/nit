mod discover;
mod pr;
mod probe;
mod runner;

pub use discover::{
    discover_clones, discover_clones_limited, normalize_remote, LocalClone, SkippedRepo,
    MAX_VISITED_DIRS, NO_HTTPS_REMOTE,
};
pub use pr::{
    batch_patches, batch_patches_cancellable, fetch_pr, file_counts, file_index, file_list,
    file_patch, file_patches, file_text, host_slug, ref_prefix, resolve_commits, FetchSpec,
    PatchBatch, PrRefs, MAX_PATCH_BYTES, MAX_PATHSPEC_BYTES,
};
pub use probe::{ls_remote, LS_REMOTE_TIMEOUT};
use runner::RunOpts;
pub use runner::{
    auth_for_remote, parse_progress, secret_forms, Git, GitAuth, GitError, GitProgress, Progress,
    BITBUCKET_GIT_USER, GITHUB_GIT_USER, GITLAB_GIT_USER, LAZY_FETCH_TIMEOUT, NETWORK_TIMEOUT,
};

use prcore::model::RepoRef;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoKind {
    Clone,
    Mirror,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalRepo {
    pub path: PathBuf,
    pub kind: RepoKind,
}

/// Windows device names: unusable as a file or directory name, with any extension.
fn is_reserved_device_name(s: &str) -> bool {
    let stem = s.split('.').next().unwrap_or("").to_ascii_lowercase();
    match stem.as_str() {
        "con" | "prn" | "aux" | "nul" => true,
        _ => {
            let b = stem.as_bytes();
            b.len() == 4
                && (stem.starts_with("com") || stem.starts_with("lpt"))
                && (b'1'..=b'9').contains(&b[3])
        }
    }
}

/// One path component (workspace or slug) of a mirror path.
fn valid_component(s: &str) -> Result<(), GitError> {
    let ok = s != "."
        && s != ".."
        && !s.is_empty()
        && !s.starts_with('-')
        && !s.ends_with(['.', ' '])
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(GitError::InvalidName(s.to_string()));
    }
    if is_reserved_device_name(s) {
        return Err(GitError::ReservedName(s.to_string()));
    }
    Ok(())
}

/// `<download_dir>/<host>/<namespace...>/<slug>.git` (host as `host_slug`; a GitLab
/// namespace may have several segments), after checking every name.
fn mirror_path(download_dir: &Path, repo: &RepoRef) -> Result<PathBuf, GitError> {
    let named = |e: GitError| match e {
        GitError::InvalidName(_) => GitError::InvalidName(repo.full_name()),
        GitError::ReservedName(_) => GitError::ReservedName(repo.full_name()),
        other => other,
    };
    let host = host_slug(&repo.host);
    valid_component(&host).map_err(named)?;
    let mut path = download_dir.join(host);
    for segment in repo.namespace.split('/') {
        valid_component(segment).map_err(named)?;
        path.push(segment);
    }
    valid_component(&repo.slug).map_err(named)?;
    path.push(format!("{}.git", repo.slug));
    Ok(path)
}

/// Where a Bitbucket mirror lived before mirrors had a host folder:
/// `<download_dir>/<workspace>/<slug>.git`. `None` for other hosts. A workspace slug never
/// contains a dot, so this layout cannot collide with a host folder.
fn legacy_mirror_path(download_dir: &Path, repo: &RepoRef) -> Option<PathBuf> {
    if !repo.host.eq_ignore_ascii_case(LEGACY_MIRROR_HOST) || repo.namespace.contains('/') {
        return None;
    }
    Some(
        download_dir
            .join(&repo.namespace)
            .join(format!("{}.git", repo.slug)),
    )
}

const LEGACY_MIRROR_HOST: &str = "bitbucket.org";

/// Moves a valid legacy Bitbucket mirror to `path` (never re-cloned). When the move fails
/// (a file in it is open, permissions) the legacy folder is used where it is. `None` when
/// there is no legacy mirror; an invalid legacy folder is reported, never touched.
fn adopt_legacy_mirror(
    git: &Git,
    download_dir: &Path,
    repo: &RepoRef,
    path: &Path,
) -> Option<Result<PathBuf, GitError>> {
    let old = legacy_mirror_path(download_dir, repo)?;
    if !old.exists() {
        return None;
    }
    if let Err(e) = check_mirror(git, &old) {
        return Some(Err(e));
    }
    let moved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::rename(&old, path));
    Some(match moved {
        Ok(()) => {
            // the workspace folder is left behind empty; remove it only if it is
            if let Some(ws) = old.parent() {
                let _ = std::fs::remove_dir(ws);
            }
            check_mirror(git, path).map(|_| path.to_path_buf())
        }
        Err(_) => Ok(old),
    })
}

fn validate_url(url: &str, allow_file: bool) -> Result<(), GitError> {
    let bad = |m: &str| Err(GitError::InvalidUrl(m.into()));
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return bad("contains whitespace or control characters");
    }
    let is_file =
        url.len() >= 7 && url.is_char_boundary(7) && url[..7].eq_ignore_ascii_case("file://");
    if allow_file && is_file {
        return Ok(());
    }
    let is_http =
        url.len() >= 7 && url.is_char_boundary(7) && url[..7].eq_ignore_ascii_case("http://");
    if is_http {
        // plain http only to a private address, with no user info (see `url_scope`)
        return match runner::url_scope(url) {
            Some(_) => Ok(()),
            None => bad("plain http:// remotes are supported only on a private address"),
        };
    }
    if url.len() < 8 || !url.is_char_boundary(8) || !url[..8].eq_ignore_ascii_case("https://") {
        return bad("only https:// remotes are supported");
    }
    let authority = url[8..].split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return bad("must not contain credentials");
    }
    Ok(())
}

/// Returns the bare, blobless mirror of `repo` under `download_dir`, cloning it on first use.
pub fn ensure_mirror(
    git: &Git,
    download_dir: &Path,
    repo: &RepoRef,
    https_url: &str,
    auth: &GitAuth,
) -> Result<LocalRepo, GitError> {
    ensure_mirror_inner(git, download_dir, repo, https_url, auth, false)
}

pub(crate) fn ensure_mirror_inner(
    git: &Git,
    download_dir: &Path,
    repo: &RepoRef,
    url: &str,
    auth: &GitAuth,
    allow_file: bool,
) -> Result<LocalRepo, GitError> {
    clone_mirror(git, download_dir, repo, url, auth, allow_file, None)
}

/// `ensure_mirror_inner`, handing git's clone progress to `progress` while it clones.
pub(crate) fn clone_mirror(
    git: &Git,
    download_dir: &Path,
    repo: &RepoRef,
    url: &str,
    auth: &GitAuth,
    allow_file: bool,
    progress: Option<Progress<'_>>,
) -> Result<LocalRepo, GitError> {
    let path = mirror_path(download_dir, repo)?;
    validate_url(url, allow_file)?;
    let ws_dir = path
        .parent()
        .expect("a mirror path has a parent")
        .to_path_buf();
    let mirror = LocalRepo {
        path: path.clone(),
        kind: RepoKind::Mirror,
    };
    // one clone per mirror path at a time within this process
    let lock = mirror_lock(&path);
    let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());
    if path.exists() {
        return check_mirror(git, &path).map(|_| mirror);
    }
    if let Some(found) = adopt_legacy_mirror(git, download_dir, repo, &path) {
        return found.map(|path| LocalRepo {
            path,
            kind: RepoKind::Mirror,
        });
    }
    std::fs::create_dir_all(&ws_dir).map_err(|e| GitError::Io(e.to_string()))?;
    remove_stale_tmp(&ws_dir, &repo.slug);
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp_name = format!(
        "{}.git.tmp-{}-{}-{nanos}",
        repo.slug,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = ws_dir.join(&tmp_name);
    // relative to the download folder, '/'-separated like the names it is built from
    let target = format!("{}/{}/{}", host_slug(&repo.host), repo.namespace, tmp_name);
    let mut args = vec!["clone", "--bare", "--filter=blob:none"];
    if progress.is_some() {
        // progress goes to stderr even though it is not a terminal
        args.push("--progress");
    }
    args.extend(["--", url, &target]);
    let opts = RunOpts {
        auth: Some(auth),
        url: Some(url),
        timeout: Some(NETWORK_TIMEOUT),
        progress,
        ..RunOpts::default()
    };
    let result = git
        .run_with(download_dir, &args, opts)
        .and_then(|_| std::fs::rename(&tmp, &path).map_err(|e| GitError::Io(e.to_string())));
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp); // only our own temp dir
                                               // another process may have won the race
        return match check_mirror(git, &path) {
            Ok(()) => Ok(mirror),
            Err(_) => Err(e),
        };
    }
    Ok(mirror)
}

fn mirror_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut map = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    map.entry(path.to_path_buf()).or_default().clone()
}

/// Existing path must be a bare repository; the error says whether git failed or it is not bare.
fn check_mirror(git: &Git, path: &Path) -> Result<(), GitError> {
    if !path.is_dir() {
        return Err(GitError::InvalidMirror(format!(
            "{} exists but is not a directory",
            path.display()
        )));
    }
    match git.run(path, &["rev-parse", "--is-bare-repository"], None) {
        Ok(o) if String::from_utf8_lossy(&o).trim() == "true" => Ok(()),
        Ok(_) => Err(GitError::InvalidMirror(format!(
            "{} exists but is not a bare repository",
            path.display()
        ))),
        Err(e) => Err(GitError::InvalidMirror(format!(
            "{} could not be inspected by git: {e}",
            path.display()
        ))),
    }
}

/// Best effort: delete `<slug>.git.tmp-*` leftovers older than an hour.
fn remove_stale_tmp(ws_dir: &Path, slug: &str) {
    let prefix = format!("{slug}.git.tmp-");
    let Ok(rd) = std::fs::read_dir(ws_dir) else {
        return;
    };
    for e in rd.flatten() {
        if !e.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(3600));
        if old {
            let p = e.path();
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(p);
            } else {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// The discovered clone of `https_url`'s repository that still exists as a directory (a clone
/// moved or deleted since the scan is skipped).
pub fn find_clone<'a>(clones: &'a [LocalClone], https_url: &str) -> Option<&'a LocalClone> {
    let key = normalize_remote(https_url)?;
    clones
        .iter()
        .find(|c| c.remote_key == key && c.path.is_dir())
}

fn matching_clone<'a>(clones: &'a [LocalClone], https_url: &str) -> Option<&'a LocalClone> {
    find_clone(clones, https_url)
}

/// True when `pick_local` would have to clone a new mirror (slow on first use): no matching
/// clone and nothing at the mirror path yet. Invalid names are false (`pick_local` fails fast).
pub fn needs_clone(
    clones: &[LocalClone],
    repo: &RepoRef,
    download_dir: &Path,
    https_url: &str,
) -> bool {
    matching_clone(clones, https_url).is_none()
        && mirror_path(download_dir, repo).is_ok_and(|p| !p.exists())
        && legacy_mirror_path(download_dir, repo).is_none_or(|p| !p.exists())
}

/// Prefers an already-discovered clone of the repository, falling back to a mirror. A first
/// clone of the mirror hands git's progress to `progress`.
pub fn pick_local(
    clones: &[LocalClone],
    repo: &RepoRef,
    download_dir: &Path,
    git: &Git,
    https_url: &str,
    auth: &GitAuth,
    progress: Option<Progress<'_>>,
) -> Result<LocalRepo, GitError> {
    if let Some(c) = matching_clone(clones, https_url) {
        return Ok(LocalRepo {
            path: c.path.clone(),
            kind: RepoKind::Clone,
        });
    }
    clone_mirror(git, download_dir, repo, https_url, auth, false, progress)
}

#[cfg(test)]
mod tests;
