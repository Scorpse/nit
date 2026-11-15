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
mod tests {
    use super::*;

    #[test]
    fn plain_http_remote_only_on_a_private_address() {
        assert!(validate_url("http://192.168.1.20:8080/root/x.git", false).is_ok());
        assert!(validate_url("http://localhost/a/b.git", false).is_ok());
        for bad in [
            "http://gitlab.com/a/b.git",
            "http://8.8.8.8/a/b.git",
            "http://192.168.1.1.evil.com/a/b.git",
            "http://u@192.168.1.1/a/b.git",
            "http://192.168.1.1@evil.com/a/b.git",
            "http://3232235777/a/b.git",
        ] {
            assert!(validate_url(bad, false).is_err(), "{bad}");
        }
    }

    fn rr(w: &str, s: &str) -> RepoRef {
        RepoRef::bitbucket(w, s)
    }

    fn auth() -> GitAuth {
        GitAuth::bitbucket("tok")
    }

    /// The folder of workspace `ws`'s Bitbucket mirrors.
    fn bb_ws(dl: &Path) -> PathBuf {
        dl.join("bitbucket.org").join("ws")
    }

    fn source_repo() -> (tempfile::TempDir, String) {
        let src = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        git.run(src.path(), &["init", "-q"], None).unwrap();
        std::fs::write(src.path().join("f.txt"), "hi").unwrap();
        git.run(src.path(), &["add", "."], None).unwrap();
        git.run(
            src.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
            None,
        )
        .unwrap();
        let url = format!(
            "file:///{}",
            src.path().to_string_lossy().replace('\\', "/")
        );
        (src, url)
    }

    #[test]
    fn ensure_mirror_clones_bare_from_file_remote() {
        let (_src, url) = source_repo();
        let dl = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        let r =
            ensure_mirror_inner(&git, dl.path(), &rr("ws", "repo"), &url, &auth(), true).unwrap();
        assert_eq!(r.kind, RepoKind::Mirror);
        assert_eq!(r.path, bb_ws(dl.path()).join("repo.git"));
        let out = git
            .run(&r.path, &["rev-parse", "--is-bare-repository"], None)
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out).trim(), "true");
        // second call reuses without needing the remote
        let again = ensure_mirror_inner(
            &git,
            dl.path(),
            &rr("ws", "repo"),
            "file:///nonexistent",
            &auth(),
            true,
        )
        .unwrap();
        assert_eq!(again, r);
    }

    #[test]
    fn first_clone_reports_git_progress() {
        let (_src, url) = source_repo();
        let dl = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        let seen = Mutex::new(Vec::new());
        let report = |p: GitProgress| seen.lock().unwrap().push(p);
        let r = clone_mirror(
            &git,
            dl.path(),
            &rr("ws", "repo"),
            &url,
            &auth(),
            true,
            Some(&report),
        )
        .unwrap();
        assert_eq!(r.kind, RepoKind::Mirror);
        let seen = seen.into_inner().unwrap();
        assert!(
            seen.iter()
                .any(|p| p.phase == "Receiving objects" && p.percent == 100),
            "{seen:?}"
        );
    }

    fn dead_git() -> Git {
        Git::from_exe("definitely-not-a-git-binary")
    }

    #[test]
    fn ensure_mirror_rejects_bad_urls_before_running_anything() {
        let dl = tempfile::tempdir().unwrap();
        let git = dead_git(); // any process spawn would yield Io, not InvalidUrl
        for url in [
            "http://bitbucket.org/ws/r.git",
            "file:///tmp/x",
            "ssh://git@bitbucket.org/ws/r.git",
            "git@bitbucket.org:ws/r.git",
            "--upload-pack=evil",
            "https://evil.com@bitbucket.org/ws/r.git",
            "https://me:tok@bitbucket.org/ws/r.git",
            "https://me@bitbucket.org/ws/r.git",
        ] {
            let err = ensure_mirror(&git, dl.path(), &rr("ws", "r"), url, &auth()).unwrap_err();
            assert!(matches!(err, GitError::InvalidUrl(_)), "{url}: {err:?}");
            assert!(!err.to_string().contains("tok"), "{url}");
        }
    }

    #[test]
    fn mirror_config_has_no_userinfo() {
        let (_src, url) = source_repo();
        let dl = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        let a = GitAuth::bitbucket("s3cr3tTOKEN");
        let r = ensure_mirror_inner(&git, dl.path(), &rr("ws", "r"), &url, &a, true).unwrap();
        let cfg = std::fs::read_to_string(r.path.join("config")).unwrap();
        assert!(
            !cfg.contains("s3cr3tTOKEN") && !cfg.contains("e@x.io"),
            "{cfg}"
        );
    }

    #[test]
    fn existing_non_bare_dir_is_an_error() {
        let dl = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dl.path().join("ws").join("r.git")).unwrap();
        let git = Git::locate().unwrap();
        let err = ensure_mirror(
            &git,
            dl.path(),
            &rr("ws", "r"),
            "https://bitbucket.org/ws/r",
            &auth(),
        )
        .unwrap_err();
        assert!(matches!(err, GitError::InvalidMirror(_)), "{err:?}");
    }

    #[test]
    fn concurrent_ensure_mirror_clones_once() {
        let (_src, url) = source_repo();
        let dl = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        let results: Vec<_> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        ensure_mirror_inner(&git, dl.path(), &rr("ws", "r"), &url, &auth(), true)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for r in &results {
            assert!(r.is_ok(), "{r:?}");
        }
        let names: Vec<_> = std::fs::read_dir(bb_ws(dl.path()))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["r.git".to_string()]);
    }

    #[test]
    fn stale_tmp_siblings_are_removed_fresh_ones_kept() {
        let (_src, url) = source_repo();
        let dl = tempfile::tempdir().unwrap();
        let ws = bb_ws(dl.path());
        std::fs::create_dir_all(&ws).unwrap();
        let stale = ws.join("r.git.tmp-old");
        let fresh = ws.join("r.git.tmp-new");
        for p in [&stale, &fresh] {
            std::fs::write(p, "x").unwrap();
        }
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let git = Git::locate().unwrap();
        ensure_mirror_inner(&git, dl.path(), &rr("ws", "r"), &url, &auth(), true).unwrap();
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn non_bare_repo_error_says_so() {
        let dl = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        let p = dl.path().join("ws").join("r.git");
        std::fs::create_dir_all(&p).unwrap();
        git.run(&p, &["init", "-q"], None).unwrap();
        let err = ensure_mirror(
            &git,
            dl.path(),
            &rr("ws", "r"),
            "https://bitbucket.org/ws/r",
            &auth(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a bare"), "{err}");
    }

    #[test]
    fn failed_clone_leaves_nothing_behind() {
        let dl = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        let missing = dl.path().join("no-such-source");
        let url = format!("file:///{}", missing.to_string_lossy().replace('\\', "/"));
        let res = ensure_mirror_inner(&git, dl.path(), &rr("ws", "r"), &url, &auth(), true);
        assert!(res.is_err());
        let left: Vec<_> = std::fs::read_dir(bb_ws(dl.path()))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn slug_traversal_rejected() {
        let dl = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        for (w, s) in [
            ("ws", ".."),
            ("ws", "."),
            ("..", "r"),
            ("ws", "a/b"),
            ("ws", "a\\b"),
            ("", "r"),
            ("w s", "r"),
            ("ws", "-r"),
            ("ws", "r."),
            ("ws", "r "),
        ] {
            let err = ensure_mirror(&git, dl.path(), &rr(w, s), "https://h/x", &auth());
            assert!(matches!(err, Err(GitError::InvalidName(_))), "{w}/{s}");
        }
    }

    #[test]
    fn windows_reserved_names_rejected() {
        let dl = tempfile::tempdir().unwrap();
        let git = dead_git();
        for (w, s) in [
            ("ws", "nul"),
            ("ws", "NUL.txt"),
            ("Con", "r"),
            ("ws", "prn"),
            ("ws", "aux.git"),
            ("ws", "com1"),
            ("ws", "COM9.x.y"),
            ("lpt1", "r"),
            ("ws", "Lpt9"),
        ] {
            let err = ensure_mirror(&git, dl.path(), &rr(w, s), "https://h/x", &auth());
            assert!(
                matches!(&err, Err(GitError::ReservedName(_))),
                "{w}/{s}: {err:?}"
            );
            assert!(err.unwrap_err().to_string().contains("reserved"));
        }
        for ok in ["com10", "console", "nullable", "lpt", "auxiliary", "con-1"] {
            assert!(valid_component(ok).is_ok(), "{ok}");
        }
        for bad in ["r ", "r.", "r. "] {
            assert!(valid_component(bad).is_err(), "{bad:?}");
        }
    }

    /// A bare mirror at the pre-host location `<dl>/ws/<slug>.git`.
    fn legacy_mirror(git: &Git, dl: &Path, url: &str, slug: &str) -> PathBuf {
        let old = dl.join("ws").join(format!("{slug}.git"));
        std::fs::create_dir_all(dl.join("ws")).unwrap();
        let target = format!("ws/{slug}.git");
        git.run(dl, &["clone", "-q", "--bare", "--", url, &target], None)
            .unwrap();
        old
    }

    #[test]
    fn legacy_bitbucket_mirror_is_moved_not_recloned() {
        let (_src, url) = source_repo();
        let dl = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        let old = legacy_mirror(&git, dl.path(), &url, "r");
        let repo = rr("ws", "r");
        assert!(!needs_clone(&[], &repo, dl.path(), &url), "no clone needed");
        // a remote that cannot be cloned proves nothing is fetched again
        let gone = format!("{url}-missing");
        let r = ensure_mirror_inner(&git, dl.path(), &repo, &gone, &auth(), true).unwrap();
        assert_eq!(r.path, bb_ws(dl.path()).join("r.git"));
        assert!(!old.exists(), "moved");
        assert!(
            !dl.path().join("ws").exists(),
            "the empty folder is removed"
        );
        let again = ensure_mirror_inner(&git, dl.path(), &repo, &gone, &auth(), true).unwrap();
        assert_eq!(again.path, r.path);
    }

    #[test]
    fn legacy_folder_with_other_mirrors_stays() {
        let (_src, url) = source_repo();
        let dl = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        legacy_mirror(&git, dl.path(), &url, "a");
        let b = legacy_mirror(&git, dl.path(), &url, "b");
        ensure_mirror_inner(&git, dl.path(), &rr("ws", "a"), &url, &auth(), true).unwrap();
        assert!(b.exists(), "only the mirror in use moves");
    }

    #[cfg(windows)]
    #[test]
    fn legacy_mirror_is_used_in_place_when_it_cannot_move() {
        let (_src, url) = source_repo();
        let dl = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        let old = legacy_mirror(&git, dl.path(), &url, "r");
        // an open file inside makes Windows refuse to rename the folder
        let _held = std::fs::File::open(old.join("HEAD")).unwrap();
        let gone = format!("{url}-missing");
        let r = ensure_mirror_inner(&git, dl.path(), &rr("ws", "r"), &gone, &auth(), true).unwrap();
        assert_eq!(r.path, old);
        assert_eq!(r.kind, RepoKind::Mirror);
    }

    #[test]
    fn invalid_legacy_folder_is_reported_and_left_alone() {
        let dl = tempfile::tempdir().unwrap();
        let old = dl.path().join("ws").join("r.git");
        std::fs::create_dir_all(&old).unwrap();
        let err = ensure_mirror(
            &Git::locate().unwrap(),
            dl.path(),
            &rr("ws", "r"),
            "https://bitbucket.org/ws/r",
            &auth(),
        )
        .unwrap_err();
        assert!(matches!(err, GitError::InvalidMirror(_)), "{err:?}");
        assert!(old.exists());
        assert!(!bb_ws(dl.path()).join("r.git").exists());
    }

    #[test]
    fn other_hosts_get_their_own_folder_and_no_legacy_lookup() {
        let dl = tempfile::tempdir().unwrap();
        let gh = RepoRef {
            host: "github.com".into(),
            namespace: "ws".into(),
            slug: "r".into(),
        };
        let gl = RepoRef {
            host: "git.corp.example/gitlab".into(),
            namespace: "group/sub".into(),
            slug: "proj".into(),
        };
        assert_eq!(
            mirror_path(dl.path(), &gh).unwrap(),
            dl.path().join("github.com").join("ws").join("r.git")
        );
        assert_eq!(
            mirror_path(dl.path(), &gl).unwrap(),
            dl.path()
                .join("git.corp.example_gitlab")
                .join("group")
                .join("sub")
                .join("proj.git")
        );
        assert_eq!(legacy_mirror_path(dl.path(), &gh), None);
        // a legacy-looking folder never stands in for another host's repository
        std::fs::create_dir_all(dl.path().join("ws").join("r.git")).unwrap();
        assert!(needs_clone(
            &[],
            &gh,
            dl.path(),
            "https://github.com/ws/r.git"
        ));
        for bad in ["a/../b", "a//b", "a/con"] {
            let r = RepoRef {
                namespace: bad.into(),
                ..gl.clone()
            };
            assert!(mirror_path(dl.path(), &r).is_err(), "{bad}");
        }
    }

    #[test]
    fn needs_clone_only_without_clone_or_mirror() {
        let dl = tempfile::tempdir().unwrap();
        let mine = tempfile::tempdir().unwrap();
        let url = "https://bitbucket.org/ws/repo.git";
        let repo = rr("ws", "repo");
        assert!(needs_clone(&[], &repo, dl.path(), url));
        let clones = vec![LocalClone {
            path: mine.path().to_path_buf(),
            remote_key: "bitbucket.org/ws/repo".into(),
        }];
        assert!(!needs_clone(&clones, &repo, dl.path(), url));
        std::fs::create_dir_all(dl.path().join("ws").join("repo.git")).unwrap();
        assert!(!needs_clone(&[], &repo, dl.path(), url), "legacy layout");
        std::fs::remove_dir_all(dl.path().join("ws")).unwrap();
        std::fs::create_dir_all(bb_ws(dl.path()).join("repo.git")).unwrap();
        assert!(!needs_clone(&[], &repo, dl.path(), url));
        assert!(needs_clone(&[], &rr("ws", "other"), dl.path(), url));
    }

    #[test]
    fn pick_local_prefers_matching_clone() {
        let dl = tempfile::tempdir().unwrap();
        let mine = tempfile::tempdir().unwrap();
        let git = Git::locate().unwrap();
        let clones = vec![
            LocalClone {
                path: "/other".into(),
                remote_key: "bitbucket.org/ws/other".into(),
            },
            LocalClone {
                path: mine.path().to_path_buf(),
                remote_key: "bitbucket.org/ws/repo".into(),
            },
        ];
        let r = pick_local(
            &clones,
            &rr("ws", "repo"),
            dl.path(),
            &git,
            "https://u@Bitbucket.org/WS/repo.git",
            &auth(),
            None,
        )
        .unwrap();
        assert_eq!(
            r,
            LocalRepo {
                path: mine.path().to_path_buf(),
                kind: RepoKind::Clone
            }
        );
    }

    #[test]
    fn a_clone_that_was_moved_or_deleted_is_not_picked() {
        let dl = tempfile::tempdir().unwrap();
        let gone = dl.path().join("moved-away");
        let clones = vec![LocalClone {
            path: gone,
            remote_key: "bitbucket.org/ws/repo".into(),
        }];
        let url = "https://bitbucket.org/ws/repo.git";
        // no usable clone: a mirror is needed (and attempted: a dead git fails to clone it)
        assert!(needs_clone(&clones, &rr("ws", "repo"), dl.path(), url));
        let err = pick_local(
            &clones,
            &rr("ws", "repo"),
            dl.path(),
            &dead_git(),
            url,
            &auth(),
            None,
        )
        .unwrap_err();
        assert!(!matches!(err, GitError::InvalidUrl(_)), "{err:?}");
        assert!(find_clone(&clones, url).is_none());
    }
}
