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
mod tests;
