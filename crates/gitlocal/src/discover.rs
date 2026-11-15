use crate::runner::{Git, GitError};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalClone {
    pub path: PathBuf,
    pub remote_key: String,
}

/// Canonical `host/workspace/repo` key for an HTTPS remote URL, or a plain-HTTP one on a
/// private address; `None` for anything else.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let has = |scheme: &str| {
        url.len() > scheme.len()
            && url.is_char_boundary(scheme.len())
            && url[..scheme.len()].eq_ignore_ascii_case(scheme)
    };
    let (scheme_len, default_port) = if has("https://") {
        ("https://".len(), ":443")
    } else if has("http://") {
        ("http://".len(), ":80")
    } else {
        return None;
    };
    let rest = url[scheme_len..].split(['?', '#']).next()?;
    let (authority, path) = rest.split_once('/')?;
    if scheme_len == "http://".len()
        && !prcore::origin::split_authority(authority)
            .is_some_and(|(h, _)| prcore::origin::is_private_host(h))
    {
        return None;
    }
    let host = authority.rsplit('@').next()?;
    let host = host.strip_suffix(default_port).unwrap_or(host);
    let key = format!("{host}/{}", path.trim_end_matches('/')).to_lowercase();
    let key = key.strip_suffix(".git").unwrap_or(&key);
    let key = key.trim_end_matches('/');
    if host.is_empty() || key.len() <= host.len() + 1 {
        return None;
    }
    Some(key.to_string())
}

/// Reason given for a repository whose remotes are all non-HTTPS (e.g. SSH-only).
pub const NO_HTTPS_REMOTE: &str = "no HTTPS remote";

const SKIP_DIRS: [&str; 3] = ["node_modules", "target", ".git"];

/// Default cap on directories visited by one `discover_clones` walk.
pub const MAX_VISITED_DIRS: usize = 5000;

/// A repository that was found but cannot be used, with a human-readable reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedRepo {
    pub path: PathBuf,
    pub reason: String,
}

fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists() || (dir.join("HEAD").is_file() && dir.join("objects").is_dir())
}

fn remote_urls(git: &Git, repo: &Path) -> Result<Vec<String>, String> {
    let args = ["config", "--get-regexp", r"^remote\..*\.url$"];
    match git.run(repo, &args, None) {
        Ok(out) => Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| l.split_once(char::is_whitespace))
            .map(|(_, url)| url.trim().to_string())
            .collect()),
        // exit 1 means "no matching key", i.e. no remotes
        Err(GitError::Failed { code: Some(1), .. }) => Ok(Vec::new()),
        Err(e) => Err(e.to_string()),
    }
}

/// Walks `root` (at most `max_depth` levels below it, visiting at most `MAX_VISITED_DIRS`
/// directories) looking for git repositories. A clone with several HTTPS remotes yields one
/// entry per distinct key. The second list holds repositories that were found but are
/// unusable (only non-HTTPS remotes, or git could not read them), each with a reason.
pub fn discover_clones(
    git: &Git,
    root: &Path,
    max_depth: usize,
) -> (Vec<LocalClone>, Vec<SkippedRepo>) {
    discover_clones_limited(git, root, max_depth, MAX_VISITED_DIRS)
}

pub fn discover_clones_limited(
    git: &Git,
    root: &Path,
    max_depth: usize,
    max_visited: usize,
) -> (Vec<LocalClone>, Vec<SkippedRepo>) {
    let mut clones = Vec::new();
    let mut skipped = Vec::new();
    let mut visited = 0usize;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        visited += 1;
        if visited > max_visited {
            skipped.push(SkippedRepo {
                path: root.to_path_buf(),
                reason: "scan limit reached".into(),
            });
            break;
        }
        if is_repo(&dir) {
            match remote_urls(git, &dir) {
                Err(reason) => skipped.push(SkippedRepo { path: dir, reason }),
                Ok(urls) => {
                    let mut keys: Vec<String> =
                        urls.iter().filter_map(|u| normalize_remote(u)).collect();
                    keys.sort();
                    keys.dedup();
                    if keys.is_empty() {
                        if !urls.is_empty() {
                            skipped.push(SkippedRepo {
                                path: dir,
                                reason: NO_HTTPS_REMOTE.into(),
                            });
                        }
                    } else {
                        for remote_key in keys {
                            clones.push(LocalClone {
                                path: dir.clone(),
                                remote_key,
                            });
                        }
                    }
                }
            }
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() || SKIP_DIRS.iter().any(|s| name == *s) {
                continue;
            }
            stack.push((entry.path(), depth + 1));
        }
    }
    clones.sort_by(|a, b| a.path.cmp(&b.path));
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    (clones, skipped)
}

#[cfg(test)]
mod tests {
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
}
