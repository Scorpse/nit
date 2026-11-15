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
    let r = ensure_mirror_inner(&git, dl.path(), &rr("ws", "repo"), &url, &auth(), true).unwrap();
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
