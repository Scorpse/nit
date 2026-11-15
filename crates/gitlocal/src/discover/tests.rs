use super::*;

#[test]
fn normalize_table() {
    let cases = [
        (
            "https://bitbucket.org/ws/repo",
            Some("bitbucket.org/ws/repo"),
        ),
        (
            "https://user@Bitbucket.org/WS/Repo.git/",
            Some("bitbucket.org/ws/repo"),
        ),
        (
            "https://user:pw@bitbucket.org/ws/repo.git",
            Some("bitbucket.org/ws/repo"),
        ),
        (
            "HTTPS://BITBUCKET.ORG/ws/repo/",
            Some("bitbucket.org/ws/repo"),
        ),
        ("git@bitbucket.org:ws/r.git", None),
        ("ssh://git@bitbucket.org/ws/r.git", None),
        ("file:///tmp/x", None),
        ("/tmp/x", None),
        ("https://bitbucket.org", None),
        (
            "https://bitbucket.org:443/ws/repo.git",
            Some("bitbucket.org/ws/repo"),
        ),
        (
            "https://bitbucket.org:8443/ws/repo",
            Some("bitbucket.org:8443/ws/repo"),
        ),
        (
            "https://bitbucket.org/ws/repo.git?x=1#frag",
            Some("bitbucket.org/ws/repo"),
        ),
        (
            "https://bitbucket.org/WS/Repo.GIT",
            Some("bitbucket.org/ws/repo"),
        ),
        // plain http only on a private address
        (
            "http://192.168.1.20:8080/Root/Proj.git",
            Some("192.168.1.20:8080/root/proj"),
        ),
        ("http://localhost:80/a/b", Some("localhost/a/b")),
        ("http://gitlab.com/a/b.git", None),
        ("http://8.8.8.8/a/b.git", None),
        ("http://192.168.1.1.evil.com/a/b.git", None),
        ("http://192.168.1.1@evil.com/a/b.git", None),
    ];
    for (input, want) in cases {
        assert_eq!(normalize_remote(input).as_deref(), want, "{input}");
    }
}

fn g(dir: &Path, args: &[&str]) {
    Git::locate().unwrap().run(dir, args, None).unwrap();
}

fn make_repo(path: &Path, url: Option<&str>) {
    std::fs::create_dir_all(path).unwrap();
    g(path, &["init", "-q"]);
    if let Some(u) = url {
        g(path, &["remote", "add", "origin", u]);
    }
}

#[test]
fn discover_finds_clone_and_ignores_nested_and_node_modules() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("work").join("a");
    make_repo(&a, Some("https://user@bitbucket.org/WS/Repo.git"));
    make_repo(
        &a.join("vendor").join("nested"),
        Some("https://bitbucket.org/ws/nested"),
    );
    make_repo(
        &root.path().join("node_modules").join("m"),
        Some("https://bitbucket.org/ws/m"),
    );
    make_repo(&root.path().join("norem"), None);
    let git = Git::locate().unwrap();
    let (clones, skipped) = discover_clones(&git, root.path(), 4);
    assert_eq!(
        clones,
        vec![LocalClone {
            path: a,
            remote_key: "bitbucket.org/ws/repo".into()
        }]
    );
    assert!(skipped.is_empty());
}

#[test]
fn discover_reports_ssh_only_clone() {
    let root = tempfile::tempdir().unwrap();
    let s = root.path().join("s");
    make_repo(&s, Some("git@bitbucket.org:ws/r.git"));
    let git = Git::locate().unwrap();
    let (clones, skipped) = discover_clones(&git, root.path(), 3);
    assert!(clones.is_empty());
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].path, s);
    assert!(skipped[0].reason.contains("HTTPS"), "{:?}", skipped[0]);
}

fn commit_empty(dir: &Path) {
    g(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-qm",
            "i",
        ],
    );
}

#[test]
fn git_file_worktree_counts_as_repo() {
    let root = tempfile::tempdir().unwrap();
    let src = root.path().join("src");
    make_repo(&src, Some("https://bitbucket.org/ws/r"));
    commit_empty(&src);
    let wt = root.path().join("wt");
    g(&src, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    assert!(wt.join(".git").is_file());
    let git = Git::locate().unwrap();
    let (clones, _) = discover_clones(&git, root.path(), 2);
    let paths: Vec<_> = clones.iter().map(|c| c.path.clone()).collect();
    assert!(paths.contains(&wt), "{paths:?}");
    assert!(paths.contains(&src), "{paths:?}");
}

#[test]
fn bare_repo_is_discovered() {
    let root = tempfile::tempdir().unwrap();
    let bare = root.path().join("m.git");
    std::fs::create_dir_all(&bare).unwrap();
    g(&bare, &["init", "--bare", "-q"]);
    g(
        &bare,
        &["remote", "add", "origin", "https://bitbucket.org/ws/m"],
    );
    let git = Git::locate().unwrap();
    let (clones, _) = discover_clones(&git, root.path(), 2);
    assert_eq!(clones.len(), 1);
    assert_eq!(clones[0].path, bare);
}

#[test]
fn max_depth_boundary() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path().join("x").join("y");
    make_repo(&r, Some("https://bitbucket.org/ws/r"));
    let git = Git::locate().unwrap();
    assert!(discover_clones(&git, root.path(), 1).0.is_empty());
    assert_eq!(discover_clones(&git, root.path(), 2).0.len(), 1);
}

#[test]
fn unreadable_repo_is_reported_with_reason() {
    let root = tempfile::tempdir().unwrap();
    let bad = root.path().join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join(".git"), "gitdir: /definitely/not/here\n").unwrap();
    let git = Git::locate().unwrap();
    let (clones, skipped) = discover_clones(&git, root.path(), 2);
    assert!(clones.is_empty());
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].path, bad);
    assert!(!skipped[0].reason.is_empty());
}

#[test]
fn scan_limit_is_reported() {
    let root = tempfile::tempdir().unwrap();
    make_repo(&root.path().join("a"), Some("https://bitbucket.org/ws/r"));
    let git = Git::locate().unwrap();
    let (clones, skipped) = discover_clones_limited(&git, root.path(), 3, 1);
    assert!(clones.is_empty());
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].reason, "scan limit reached");
    let (_, skipped) = discover_clones_limited(&git, root.path(), 3, 100);
    assert!(skipped.is_empty());
}

#[test]
fn visit_limit_stops_the_walk() {
    let root = tempfile::tempdir().unwrap();
    for i in 0..6 {
        make_repo(
            &root.path().join(format!("r{i}")),
            Some("https://bitbucket.org/ws/r"),
        );
    }
    let git = Git::locate().unwrap();
    let (clones, _) = discover_clones_limited(&git, root.path(), 3, 3);
    assert!(clones.len() <= 2, "{}", clones.len());
    assert_eq!(discover_clones(&git, root.path(), 3).0.len(), 6);
}

#[test]
fn discover_matches_any_remote_and_respects_depth() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    make_repo(&a, Some("git@bitbucket.org:ws/r.git"));
    g(&a, &["remote", "add", "up", "https://bitbucket.org/ws/r"]);
    make_repo(
        &root.path().join("x").join("y").join("deep"),
        Some("https://bitbucket.org/ws/d"),
    );
    let git = Git::locate().unwrap();
    let (clones, skipped) = discover_clones(&git, root.path(), 2);
    assert_eq!(clones.len(), 1);
    assert_eq!(clones[0].remote_key, "bitbucket.org/ws/r");
    assert!(skipped.is_empty());
}
