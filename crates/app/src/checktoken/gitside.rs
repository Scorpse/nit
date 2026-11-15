use super::*;

// ---------------------------------------------------------------------------------------
// Git side (a trait so tests run without git or a network)

pub trait GitSide {
    /// `git --version` text (and where it is), or why git cannot be used.
    fn version(&self) -> Result<String, String>;
    /// `git ls-remote <url> HEAD` with the app's git auth; `user` overrides the user name.
    /// `Err` is git's (scrubbed) error text.
    fn ls_remote(&self, url: &str, user: Option<&str>, auth: &GitAuth) -> Result<(), String>;
}

pub struct SystemGit;

impl GitSide for SystemGit {
    fn version(&self) -> Result<String, String> {
        let git = gitlocal::Git::shared().map_err(|e| e.to_string())?;
        let out = git
            .run(&std::env::temp_dir(), &["--version"], None)
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "{} ({})",
            String::from_utf8_lossy(&out).trim(),
            git.exe().display()
        ))
    }

    fn ls_remote(&self, url: &str, user: Option<&str>, auth: &GitAuth) -> Result<(), String> {
        let git = gitlocal::Git::shared().map_err(|e| e.to_string())?;
        gitlocal::ls_remote(&git, url, auth, user).map_err(|e| e.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GitFailure {
    Auth,
    NoAccess,
    Cert,
    Network,
    Timeout,
    Other,
}

pub(super) fn classify_git(text: &str) -> GitFailure {
    let t = text.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if any(&["did not finish within"]) {
        GitFailure::Timeout
    } else if any(&[
        "ssl certificate",
        "certificate verify",
        "local issuer",
        "self signed",
        "self-signed",
        "certificate chain",
        "sec_e_untrusted_root",
        "revocation",
    ]) {
        GitFailure::Cert
    } else if any(&[
        "could not resolve",
        "failed to connect",
        "couldn't connect",
        "could not connect",
        "connection refused",
        "connection timed out",
        "timed out",
        "proxy",
        "connection was reset",
    ]) {
        GitFailure::Network
    } else if any(&[
        "authentication failed",
        "401",
        "invalid credentials",
        "invalid username or password",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
    ]) {
        GitFailure::Auth
    } else if any(&[
        "not found",
        "403",
        "does not exist",
        "access denied",
        "forbidden",
    ]) {
        GitFailure::NoAccess
    } else {
        GitFailure::Other
    }
}

/// First line of git's error text, shortened (it was already scrubbed of the token).
pub(super) fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    if line.chars().count() > 160 {
        format!("{}...", line.chars().take(157).collect::<String>())
    } else {
        line
    }
}
