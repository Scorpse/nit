use super::*;
use std::path::Path;

fn auth() -> GitAuth {
    GitAuth::bitbucket("s3cr3t-T0KEN")
}

fn file_url(p: &Path) -> String {
    format!("file:///{}", p.display().to_string().replace('\\', "/"))
}

#[test]
fn lists_a_local_repository_with_either_user_name() {
    let git = Git::locate().unwrap();
    let dir = tempfile::tempdir().unwrap();
    git.run(dir.path(), &["init", "-q", "--bare"], None)
        .unwrap();
    let url = file_url(dir.path());
    ls_remote(&git, &url, &auth(), None).unwrap();
    ls_remote(&git, &url, &auth(), Some("me@example.com")).unwrap();
}

#[test]
fn a_failure_never_shows_the_token_or_the_header() {
    let git = Git::locate().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let url = file_url(&dir.path().join("missing"));
    let a = auth();
    for user in [None, Some("me@example.com")] {
        let err = ls_remote(&git, &url, &a, user).unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains(&a.token), "{shown}");
        assert!(
            !shown.contains(&basic("me@example.com", &a.token)),
            "{shown}"
        );
    }
}

/// A fake `git` that records its arguments and environment, then fails with the
/// configured header on stderr (as a verbose git would), so the whole auth path for a
/// github.com remote can be checked without a network or a real token.
fn fake_git(dir: &Path) -> Git {
    #[cfg(windows)]
    {
        let script = dir.join("git.cmd");
        std::fs::write(
            &script,
            "@echo %*> \"%~dp0args.txt\"\r\n@set > \"%~dp0env.txt\"\r\n@echo fatal: header %GIT_CONFIG_VALUE_0% 1>&2\r\n@exit /b 128\r\n",
        )
        .unwrap();
        Git::from_exe(script)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("git");
        std::fs::write(
            &script,
            "#!/bin/sh\nd=$(dirname \"$0\")\necho \"$@\" > \"$d/args.txt\"\nenv > \"$d/env.txt\"\necho \"fatal: header $GIT_CONFIG_VALUE_0\" >&2\nexit 128\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Git::from_exe(script)
    }
}

#[test]
fn github_token_goes_only_into_a_scoped_env_header_as_x_access_token() {
    use crate::runner::GITHUB_GIT_USER;
    let dir = tempfile::tempdir().unwrap();
    let git = fake_git(dir.path());
    let token = "fake-github-T0KEN";
    let auth = GitAuth::new(GITHUB_GIT_USER, token, "github.com");
    let header = basic("x-access-token", token);
    let err = ls_remote(&git, "https://github.com/octo-org/api.git", &auth, None).unwrap_err();

    // argv: the remote and the ref, never the token or the header
    let args = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
    assert!(
        args.contains("ls-remote https://github.com/octo-org/api.git HEAD"),
        "{args}"
    );
    assert!(!args.contains(token) && !args.contains(&header), "{args}");
    assert!(args.contains("credential.helper=") && args.contains("core.askPass="));

    // env: one header, scoped to https://github.com/, user name per GitHub's docs
    let env = std::fs::read_to_string(dir.path().join("env.txt")).unwrap();
    let line = |k: &str| {
        env.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .map(str::to_string)
    };
    assert_eq!(
        line("GIT_CONFIG_KEY_0").as_deref(),
        Some("http.https://github.com/.extraheader")
    );
    assert_eq!(
        line("GIT_CONFIG_VALUE_0"),
        Some(format!("Authorization: Basic {header}"))
    );
    assert_eq!(line("GIT_CONFIG_VALUE_1").as_deref(), Some("false"));
    assert_eq!(line("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
    assert_eq!(line("GIT_ASKPASS"), None);

    // the failure text (what logs and reports show) is scrubbed
    let shown = format!("{err} {err:?}");
    assert!(shown.contains("Authorization: Basic ***"), "{shown}");
    assert!(
        !shown.contains(token) && !shown.contains(&header),
        "{shown}"
    );
}

#[test]
fn github_token_is_refused_for_another_host_before_git_runs() {
    use crate::runner::GITHUB_GIT_USER;
    let dir = tempfile::tempdir().unwrap();
    let git = fake_git(dir.path());
    let auth = GitAuth::new(GITHUB_GIT_USER, "tok", "github.com");
    for url in [
        "https://gist.github.com/o/r.git",
        "https://github.com.evil.example/o/r.git",
        "https://github.com:444/o/r.git",
    ] {
        let err = ls_remote(&git, url, &auth, None).unwrap_err();
        assert!(matches!(err, GitError::InvalidUrl(_)), "{url}: {err:?}");
    }
    assert!(!dir.path().join("args.txt").exists(), "git ran");
}

/// One env line of the fake git's recorded environment.
fn env_line(dir: &Path, k: &str) -> Option<String> {
    let env = std::fs::read_to_string(dir.join("env.txt")).unwrap();
    env.lines()
        .find_map(|l| l.strip_prefix(&format!("{k}=")))
        .map(str::to_string)
}

/// GitLab (spec 7.2): user `oauth2`, the PAT as the password, in a header scoped to the
/// account's host; never in argv; failure text scrubbed.
#[test]
fn gitlab_token_goes_only_into_a_scoped_env_header_as_oauth2() {
    use crate::runner::GITLAB_GIT_USER;
    let dir = tempfile::tempdir().unwrap();
    let git = fake_git(dir.path());
    let token = "glpat-fake-T0KEN-not-real";
    let auth = GitAuth::new(GITLAB_GIT_USER, token, "gitlab.com");
    let header = basic("oauth2", token);
    let url = "https://gitlab.com/my-group/sub/my-project.git";
    let err = ls_remote(&git, url, &auth, None).unwrap_err();

    let args = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
    assert!(args.contains(&format!("ls-remote {url} HEAD")), "{args}");
    assert!(!args.contains(token) && !args.contains(&header), "{args}");
    assert_eq!(
        env_line(dir.path(), "GIT_CONFIG_KEY_0").as_deref(),
        Some("http.https://gitlab.com/.extraheader")
    );
    assert_eq!(
        env_line(dir.path(), "GIT_CONFIG_VALUE_0"),
        Some(format!("Authorization: Basic {header}"))
    );
    assert_eq!(
        env_line(dir.path(), "GIT_TERMINAL_PROMPT").as_deref(),
        Some("0")
    );
    let shown = format!("{err} {err:?}");
    assert!(shown.contains("Authorization: Basic ***"), "{shown}");
    assert!(
        !shown.contains(token) && !shown.contains(&header),
        "{shown}"
    );
}

/// A self-managed GitLab under a relative URL root (`https://git.corp.example/gitlab`):
/// the account's host carries the path prefix, the header scope is the host only, so the
/// project's clone URL under the prefix gets it.
#[test]
fn gitlab_relative_url_root_scopes_the_header_to_the_host() {
    use crate::runner::GITLAB_GIT_USER;
    let dir = tempfile::tempdir().unwrap();
    let git = fake_git(dir.path());
    let auth = GitAuth::new(GITLAB_GIT_USER, "tok", "Git.Corp.Example/gitlab");
    assert_eq!(auth.scope, "https://git.corp.example/");
    let url = "https://git.corp.example/gitlab/grp/proj.git";
    let err = ls_remote(&git, url, &auth, None).unwrap_err();
    assert!(matches!(err, GitError::Failed { .. }), "{err:?}");
    assert_eq!(
        env_line(dir.path(), "GIT_CONFIG_KEY_0").as_deref(),
        Some("http.https://git.corp.example/.extraheader")
    );
    // with a port: kept in the scope
    let p = GitAuth::new(GITLAB_GIT_USER, "tok", "git.corp.example:8443/gitlab");
    assert_eq!(p.scope, "https://git.corp.example:8443/");
}

#[test]
fn gitlab_token_is_refused_for_another_host_before_git_runs() {
    use crate::runner::GITLAB_GIT_USER;
    let dir = tempfile::tempdir().unwrap();
    let git = fake_git(dir.path());
    let auth = GitAuth::new(GITLAB_GIT_USER, "tok", "gitlab.com");
    for url in [
        "https://gitlab.com.evil.example/g/p.git",
        "https://registry.gitlab.com/g/p.git",
        "https://gitlab.com:8443/g/p.git",
    ] {
        let err = ls_remote(&git, url, &auth, None).unwrap_err();
        assert!(matches!(err, GitError::InvalidUrl(_)), "{url}: {err:?}");
    }
    assert!(!dir.path().join("args.txt").exists(), "git ran");
}

#[test]
fn scrub_header_removes_every_form() {
    let h = basic("me@example.com", "s3cr3t-T0KEN");
    let e = GitError::Failed {
        code: Some(1),
        stderr: format!("x {h} y {}", h.trim_end_matches('=')),
    };
    let shown = format!("{:?}", scrub_header(e, &h));
    assert!(!shown.contains(h.trim_end_matches('=')), "{shown}");
}
