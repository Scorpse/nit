//! `git ls-remote` with the app's own git authentication, for the token self-test
//! (`nit --check-token`).

use std::time::Duration;

use base64::Engine;

use crate::runner::{auth_for_remote, execute_reporting, Git, GitAuth, GitError, RunOpts};

/// Longest one self-test `ls-remote` may run.
pub const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

/// `git ls-remote <url> HEAD`, authenticated exactly like the app's fetches: the token goes
/// into a host-scoped `http.extraheader` through environment variables of the child only,
/// with credential helpers and prompts off and redirects not followed.
///
/// `user` replaces the account's Basic-auth user name (`auth.user`); it exists only to diagnose
/// which user name a token is accepted with. Error text is scrubbed of the token, including
/// the header built with `user`.
pub fn ls_remote(git: &Git, url: &str, auth: &GitAuth, user: Option<&str>) -> Result<(), GitError> {
    let dir = std::env::temp_dir();
    let args = ["ls-remote", url, "HEAD"];
    let opts = RunOpts {
        auth: Some(auth),
        url: Some(url),
        max_bytes: Some(1 << 20),
        timeout: Some(LS_REMOTE_TIMEOUT),
        cancel: None,
        progress: None,
    };
    // http(s) only to the account's own origin: never a credential (or an override of the
    // header slot) for another host, port or scheme
    let web = url
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        || url
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"));
    if web && auth_for_remote(url, auth).is_none() {
        return Err(GitError::InvalidUrl(
            "the remote is not on the account's git host".into(),
        ));
    }
    let Some(user) = user.filter(|u| *u != auth.user) else {
        return git.run_with(&dir, &args, opts).map(|_| ());
    };
    let header = basic(user, &auth.token);
    let mut cmd = git.prepared(&dir, &args, Some(auth), Some(url));
    // `prepared` put the app's header in slot 0; the same slot now carries `user`
    cmd.env(
        "GIT_CONFIG_VALUE_0",
        format!("Authorization: Basic {header}"),
    );
    execute_reporting(
        cmd,
        Some(auth),
        opts.max_bytes,
        opts.timeout,
        opts.cancel,
        None,
    )
    .map(|_| ())
    .map_err(|e| scrub_header(e, &header))
}

fn basic(user: &str, token: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"))
}

/// Removes `header` (and its unpadded and URL-safe forms) from the error text; the token
/// itself was already scrubbed by `execute`.
fn scrub_header(e: GitError, header: &str) -> GitError {
    let urlsafe = header.replace('+', "-").replace('/', "_");
    let mut forms = vec![
        header.to_string(),
        header.trim_end_matches('=').to_string(),
        urlsafe.clone(),
        urlsafe.trim_end_matches('=').to_string(),
    ];
    forms.retain(|f| !f.is_empty());
    forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
    let clean = |s: String| {
        forms
            .iter()
            .fold(s, |acc, f| acc.replace(f.as_str(), "***"))
    };
    match e {
        GitError::Failed { code, stderr } => GitError::Failed {
            code,
            stderr: clean(stderr),
        },
        GitError::Io(m) => GitError::Io(clean(m)),
        other => other,
    }
}

#[cfg(test)]
mod tests;
