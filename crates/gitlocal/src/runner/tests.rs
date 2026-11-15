use super::*;
use std::time::Instant;

#[test]
fn locate_finds_system_git() {
    assert!(Git::locate().is_ok());
}

#[test]
fn run_error_never_contains_token() {
    let git = Git::locate().unwrap();
    let dir = tempfile::tempdir().unwrap();
    git.run(dir.path(), &["init", "-q"], None).unwrap();
    let auth = GitAuth::bitbucket("s3cr3t-T0KEN");
    let header = auth_header_b64(&auth);
    for needle in [auth.token.as_str(), header.as_str()] {
        let err = git
            .run(dir.path(), &["checkout", needle], Some(&auth))
            .unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(shown.contains("***"), "{shown}");
        assert!(!shown.contains(&auth.token), "{shown}");
        assert!(!shown.contains(&header), "{shown}");
    }
    let dbg = format!("{auth:?}");
    assert!(!dbg.contains(&auth.token) && !dbg.contains("me@example.com"));
}

#[test]
fn run_limited_bounds_stderr_and_returns_promptly_when_too_large() {
    let git = Git::locate().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let alias = "alias.big=!yes x | head -c 3000000 >&2; yes y | head -c 100000; exit 1";
    let args = ["-c", alias, "big"];
    let err = git
        .run_limited(dir.path(), &args, None, 1 << 20)
        .unwrap_err();
    match err {
        GitError::Failed { stderr, .. } => {
            assert!(stderr.len() <= 1 << 20, "{}", stderr.len())
        }
        e => panic!("{e:?}"),
    }
    let err = git.run_limited(dir.path(), &args, None, 10).unwrap_err();
    assert_eq!(err, GitError::TooLarge { limit: 10 });
}

#[test]
fn check_version_rejects_old_git() {
    assert!(matches!(
        check_version("git version 2.30.0\n"),
        Err(GitError::TooOld(_))
    ));
    assert!(matches!(check_version("garbage"), Err(GitError::TooOld(_))));
    assert!(check_version("git version 2.31.0").is_ok());
    assert!(check_version("git version 3.0.1").is_ok());
}

#[test]
fn scrub_redacts_all_encodings_on_every_line() {
    let auth = GitAuth::bitbucket("tok/+=en?x");
    let std = auth_header_b64(&auth);
    let unpadded = std.trim_end_matches('=').to_string();
    let urlsafe = std.replace('+', "-").replace('/', "_");
    let urlsafe_unpadded = urlsafe.trim_end_matches('=').to_string();
    let pct = percent_encode(&std);
    let tok_pct = percent_encode(&auth.token);
    let forms = [
        std.clone(),
        unpadded,
        urlsafe,
        urlsafe_unpadded,
        pct,
        auth.token.clone(),
        tok_pct,
    ];
    let text = forms
        .iter()
        .map(|f| format!("fatal: bad {f} here"))
        .collect::<Vec<_>>()
        .join("\r\n");
    let out = scrub(&text, Some(&auth));
    for f in &forms {
        assert!(!out.contains(f.as_str()), "{f} in {out}");
    }
    assert_eq!(out.matches("***").count(), forms.len());
    assert_eq!(out.lines().count(), forms.len());
}

fn b64_decode(s: &str) -> String {
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).unwrap();
    String::from_utf8(bytes).unwrap()
}

fn envs(cmd: &Command) -> Vec<(String, Option<String>)> {
    cmd.get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

fn env_of(cmd: &Command, key: &str) -> Option<String> {
    envs(cmd)
        .into_iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v)
}

#[test]
fn git_header_uses_static_token_user_not_email() {
    let auth = GitAuth::bitbucket("s3cr3t");
    assert_eq!(
        b64_decode(&auth_header_b64(&auth)),
        "x-bitbucket-api-token-auth:s3cr3t"
    );
    let git = Git::locate().unwrap();
    let cmd = git.prepared(
        Path::new("."),
        &["fetch"],
        Some(&auth),
        Some("https://bitbucket.org/ws/r.git"),
    );
    let value = env_of(&cmd, "GIT_CONFIG_VALUE_0").unwrap();
    let b64 = value.strip_prefix("Authorization: Basic ").unwrap();
    assert_eq!(b64_decode(b64), "x-bitbucket-api-token-auth:s3cr3t");
    for (k, v) in envs(&cmd) {
        let v = v.unwrap_or_default();
        assert!(!v.contains("me@example.com"), "{k}={v}");
    }
}

#[test]
fn network_command_scopes_header_and_blocks_prompts_and_redirects() {
    let auth = GitAuth::bitbucket("tok");
    let git = Git::locate().unwrap();
    let cmd = git.prepared(
        Path::new("."),
        &["fetch"],
        Some(&auth),
        Some("https://Bitbucket.org/ws/r.git"),
    );
    assert_eq!(env_of(&cmd, "GIT_CONFIG_COUNT").as_deref(), Some("4"));
    // a stalled transfer is ended by curl's low-speed check, not by the overall cap
    assert_eq!(
        env_of(&cmd, "GIT_CONFIG_KEY_2").as_deref(),
        Some("http.lowSpeedLimit")
    );
    assert_eq!(
        env_of(&cmd, "GIT_CONFIG_VALUE_2").as_deref(),
        Some(LOW_SPEED_LIMIT.to_string().as_str())
    );
    assert_eq!(
        env_of(&cmd, "GIT_CONFIG_KEY_3").as_deref(),
        Some("http.lowSpeedTime")
    );
    assert_eq!(
        env_of(&cmd, "GIT_CONFIG_VALUE_3").as_deref(),
        Some(LOW_SPEED_TIME.as_secs().to_string().as_str())
    );
    assert_eq!(
        env_of(&cmd, "GIT_CONFIG_KEY_0").as_deref(),
        Some("http.https://bitbucket.org/.extraheader")
    );
    assert_eq!(
        env_of(&cmd, "GIT_CONFIG_KEY_1").as_deref(),
        Some("http.followRedirects")
    );
    assert_eq!(env_of(&cmd, "GIT_CONFIG_VALUE_1").as_deref(), Some("false"));
    let removed: Vec<String> = envs(&cmd)
        .into_iter()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.to_ascii_uppercase())
        .collect();
    for var in ["GIT_ASKPASS", "SSH_ASKPASS", "SSH_ASKPASS_REQUIRE"] {
        assert!(removed.iter().any(|k| k == var), "{var} not removed");
    }
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let pos = |s: &str| args.iter().position(|a| a == s);
    let fetch = pos("fetch").unwrap();
    assert!(pos("core.askPass=").is_some_and(|p| p < fetch), "{args:?}");
    assert!(
        pos("credential.helper=").is_some_and(|p| p < fetch),
        "{args:?}"
    );
    assert!(args.iter().all(|a| !a.contains("Basic")), "{args:?}");
    // the scope is always the account's: another host gets no header at all, and a
    // non-https url keeps the account's scope (never a guessed one)
    let other = git.prepared(
        Path::new("."),
        &["fetch"],
        Some(&auth),
        Some("https://git.example.com:8443/x/y.git"),
    );
    assert_eq!(
        env_of(&other, "GIT_CONFIG_KEY_0").as_deref(),
        Some("http.https://bitbucket.org/.extraheader")
    );
    assert_eq!(env_of(&other, "GIT_CONFIG_VALUE_0").as_deref(), Some(""));
    let file = git.prepared(Path::new("."), &["fetch"], Some(&auth), Some("file:///x"));
    assert_eq!(
        env_of(&file, "GIT_CONFIG_KEY_0").as_deref(),
        Some("http.https://bitbucket.org/.extraheader")
    );
}

#[test]
fn no_scope_means_no_header() {
    // a command without a remote (lazy fetch of a mirror) uses the account's scope,
    // never bitbucket.org by default
    let gl = GitAuth::new(GITLAB_GIT_USER, "tok", "Git.Corp.Example:8443");
    assert_eq!(gl.scope, "https://git.corp.example:8443/");
    let git = Git::locate().unwrap();
    let cmd = git.prepared(Path::new("."), &["log"], Some(&gl), None);
    assert_eq!(
        env_of(&cmd, "GIT_CONFIG_KEY_0").as_deref(),
        Some("http.https://git.corp.example:8443/.extraheader")
    );
    assert!(envs(&cmd)
        .iter()
        .all(|(_, v)| !v.as_deref().unwrap_or("").contains("bitbucket.org")));
    // and without credentials there is no header configuration at all
    let bare = git.prepared(Path::new("."), &["log"], None, None);
    assert_eq!(env_of(&bare, "GIT_CONFIG_COUNT"), None);
}

#[test]
fn header_not_sent_to_other_host_or_port() {
    let a = GitAuth::new(GITHUB_GIT_USER, "tok", "ghe.corp.example");
    let ok = "https://GHE.corp.example/o/r.git";
    assert!(auth_for_remote(ok, &a).is_some());
    for url in [
        "https://ghe.corp.example:8443/o/r.git",
        "https://ghe.corp.example.evil/o/r.git",
        "https://user@ghe.corp.example/o/r.git",
        "http://ghe.corp.example/o/r.git",
        "https://github.com/o/r.git",
        "file:///ghe.corp.example/o/r.git",
    ] {
        assert!(auth_for_remote(url, &a).is_none(), "{url}");
    }
    // git itself only matches the scope: run with the account's url, ask for others
    let git = Git::locate().unwrap();
    let dir = tempfile::tempdir().unwrap();
    git.run(dir.path(), &["init", "-q"], None).unwrap();
    let opts = RunOpts {
        auth: Some(&a),
        url: Some(ok),
        ..RunOpts::default()
    };
    let get = |url: &str| {
        let args = ["config", "--get-urlmatch", "http.extraheader", url];
        git.run_with(dir.path(), &args, opts)
    };
    assert!(get("https://ghe.corp.example/o/r.git").is_ok());
    for url in [
        "https://ghe.corp.example:8443/o/r.git",
        "https://bitbucket.org/o/r.git",
    ] {
        assert!(get(url).is_err(), "{url}");
    }
}

#[test]
fn cleartext_auth_only_for_a_private_host() {
    let a = GitAuth::cleartext(GITLAB_GIT_USER, "tok", "192.168.1.20:8080/GitLab").unwrap();
    assert_eq!(a.scope, "http://192.168.1.20:8080/");
    assert_eq!(a.scheme(), "http");
    assert_eq!(
        GitAuth::new(GITLAB_GIT_USER, "tok", "gitlab.com").scheme(),
        "https"
    );
    assert!(GitAuth::cleartext(GITLAB_GIT_USER, "tok", "git.localhost").is_some());
    assert!(GitAuth::cleartext(GITLAB_GIT_USER, "tok", "[::1]:3000").is_some());
    for bad in [
        "gitlab.com",
        "8.8.8.8",
        "192.168.1.1.evil.com",
        "192.168.1.1@evil.com",
        "3232235777",
        "0300.0250.1.1",
        "[::ffff:192.168.1.1]",
        "192.168.1.1:x",
        "",
    ] {
        assert!(
            GitAuth::cleartext(GITLAB_GIT_USER, "tok", bad).is_none(),
            "{bad}"
        );
    }
}

#[test]
fn cleartext_header_only_for_the_exact_http_origin() {
    let a = GitAuth::cleartext(GITLAB_GIT_USER, "tok", "192.168.1.20:8080").unwrap();
    let ok = "http://192.168.1.20:8080/root/nit-live-test.git";
    assert!(auth_for_remote(ok, &a).is_some());
    for url in [
        // https on the same host and port is another origin
        "https://192.168.1.20:8080/root/x.git",
        "https://192.168.1.20/root/x.git",
        "http://192.168.1.20/root/x.git",
        "http://192.168.1.20:8081/root/x.git",
        "http://192.168.1.88:8080/root/x.git",
        "http://192.168.1.20:8080.evil.com/root/x.git",
        "http://user@192.168.1.20:8080/root/x.git",
        "http://192.168.1.20:8080@evil.com/root/x.git",
        "file:///192.168.1.20:8080/root/x.git",
    ] {
        assert!(auth_for_remote(url, &a).is_none(), "{url}");
    }
    // an https account never matches the http form of its own host
    let s = GitAuth::new(GITLAB_GIT_USER, "tok", "192.168.1.20:8080");
    assert!(auth_for_remote(ok, &s).is_none());
    // a public http remote is never a scope, whatever the account
    assert_eq!(url_scope("http://gitlab.com/x.git"), None);
    assert_eq!(url_scope("http://8.8.8.8/x.git"), None);

    let git = Git::locate().unwrap();
    let cmd = git.prepared(Path::new("."), &["fetch"], Some(&a), Some(ok));
    assert_eq!(
        env_of(&cmd, "GIT_CONFIG_KEY_0").as_deref(),
        Some("http.http://192.168.1.20:8080/.extraheader")
    );
    assert!(
        env_of(&cmd, "GIT_CONFIG_VALUE_0").is_some_and(|v| v.starts_with("Authorization: Basic "))
    );
    for other in [
        "https://192.168.1.20:8080/root/x.git",
        "http://192.168.1.20:8081/root/x.git",
        "http://10.0.0.1:8080/root/x.git",
        "http://gitlab.com/root/x.git",
    ] {
        let cmd = git.prepared(Path::new("."), &["fetch"], Some(&a), Some(other));
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_VALUE_0").as_deref(),
            Some(""),
            "{other}"
        );
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_KEY_0").as_deref(),
            Some("http.http://192.168.1.20:8080/.extraheader")
        );
    }
    // git itself matches the header only on the exact origin (scheme, host, port)
    let dir = tempfile::tempdir().unwrap();
    git.run(dir.path(), &["init", "-q"], None).unwrap();
    let opts = RunOpts {
        auth: Some(&a),
        url: Some(ok),
        ..RunOpts::default()
    };
    let get = |url: &str| {
        let args = ["config", "--get-urlmatch", "http.extraheader", url];
        git.run_with(dir.path(), &args, opts)
    };
    assert!(get("http://192.168.1.20:8080/other/repo.git").is_ok());
    for url in [
        "https://192.168.1.20:8080/root/x.git",
        "http://192.168.1.20/root/x.git",
        "http://192.168.1.20:8081/root/x.git",
        "http://192.168.1.88:8080/root/x.git",
    ] {
        assert!(get(url).is_err(), "{url}");
    }
    let dbg = format!("{a:?}");
    assert!(
        !dbg.contains("tok\"") && dbg.contains("http://192.168.1.20:8080/"),
        "{dbg}"
    );
}

#[test]
fn user_name_per_forge_in_header() {
    for (user, host) in [
        (BITBUCKET_GIT_USER, "bitbucket.org"),
        (GITHUB_GIT_USER, "github.com"),
        (GITLAB_GIT_USER, "gitlab.com"),
    ] {
        let a = GitAuth::new(user, "s3cr3t", host);
        assert_eq!(b64_decode(&auth_header_b64(&a)), format!("{user}:s3cr3t"));
        let dbg = format!("{a:?}");
        assert!(!dbg.contains("s3cr3t") && dbg.contains(host), "{dbg}");
    }
    assert_eq!(GITHUB_GIT_USER, "x-access-token");
    assert_eq!(GITLAB_GIT_USER, "oauth2");
}

#[test]
fn header_is_only_sent_to_its_own_host() {
    let git = Git::locate().unwrap();
    let dir = tempfile::tempdir().unwrap();
    git.run(dir.path(), &["init", "-q"], None).unwrap();
    let auth = GitAuth::bitbucket("tok");
    let opts = RunOpts {
        auth: Some(&auth),
        url: Some("https://bitbucket.org/ws/r.git"),
        ..RunOpts::default()
    };
    let get = |url: &str| {
        let args = ["config", "--get-urlmatch", "http.extraheader", url];
        git.run_with(dir.path(), &args, opts)
    };
    let ours = get("https://bitbucket.org/ws/r.git").unwrap();
    assert!(String::from_utf8_lossy(&ours).starts_with("Authorization: Basic "));
    for url in [
        "https://evil.example/ws/r.git",
        "https://bitbucket.org.evil.example/ws/r.git",
        "http://bitbucket.org/ws/r.git",
    ] {
        let err = get(url).unwrap_err();
        assert!(
            matches!(err, GitError::Failed { code: Some(1), .. }),
            "{url}: {err:?}"
        );
    }
    let redirects = git
        .run_with(
            dir.path(),
            &["config", "--get", "http.followRedirects"],
            opts,
        )
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&redirects).trim(), "false");
    let low = git
        .run_with(dir.path(), &["config", "--get", "http.lowSpeedTime"], opts)
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&low).trim(), "300");
}

#[test]
fn scrub_removes_the_new_header_from_stderr() {
    let git = Git::locate().unwrap();
    let dir = tempfile::tempdir().unwrap();
    git.run(dir.path(), &["init", "-q"], None).unwrap();
    let auth = GitAuth::bitbucket("s3cr3t-T0KEN");
    let header = auth_header_b64(&auth);
    // the alias echoes the configured header value to stderr
    let alias = "alias.leak=!git config --get-urlmatch http.extraheader https://bitbucket.org/x >&2; exit 3";
    let opts = RunOpts {
        auth: Some(&auth),
        url: Some("https://bitbucket.org/ws/r.git"),
        ..RunOpts::default()
    };
    let err = git
        .run_with(dir.path(), &["-c", alias, "leak"], opts)
        .unwrap_err();
    let shown = format!("{err} {err:?}");
    assert!(shown.contains("Authorization: Basic ***"), "{shown}");
    assert!(!shown.contains(&header), "{shown}");
}

/// A child that would write `marker` after about two seconds unless it is killed first.
fn slow_child(dir: &Path) -> Command {
    #[cfg(windows)]
    {
        let script = dir.join("slow.cmd");
        std::fs::write(
            &script,
            "@ping -n 3 127.0.0.1 >nul\r\n@echo done>\"%~dp0marker\"\r\n",
        )
        .unwrap();
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(&script);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!(
            "sleep 2; echo done > '{}'",
            dir.join("marker").display()
        ));
        cmd
    }
}

#[test]
fn timeout_kills_the_child_and_returns_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let limit = Duration::from_millis(300);
    let start = Instant::now();
    let err = execute(slow_child(dir.path()), None, None, Some(limit), None).unwrap_err();
    assert_eq!(err, GitError::Timeout { after: limit });
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
    assert!(err.to_string().contains("stopped"), "{err}");
    // had the child survived it would have written the marker by now
    std::thread::sleep(Duration::from_secs(4));
    assert!(!dir.path().join("marker").exists(), "child was not killed");
    // without a deadline the same child runs to completion
    execute(slow_child(dir.path()), None, None, None, None).unwrap();
    assert!(dir.path().join("marker").exists());
}

/// A helper whose child (our grandchild) would write `marker` after about two seconds;
/// the helper itself keeps running longer than that.
#[cfg(windows)]
fn child_with_grandchild(dir: &Path) -> Command {
    let script = dir.join("parent.cmd");
    std::fs::write(
        &script,
        "@start \"\" /b cmd /c \"ping -n 3 127.0.0.1 >nul & echo done>\"%~dp0marker\"\"\r\n\
         @ping -n 8 127.0.0.1 >nul\r\n",
    )
    .unwrap();
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(&script);
    cmd
}

/// The same on macOS / Unix: a background subshell (the grandchild) that would write
/// `marker` after about two seconds, while the shell itself sleeps longer.
#[cfg(unix)]
fn child_with_grandchild(dir: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(format!(
        "(sleep 2; echo done > '{}') & sleep 8",
        dir.join("marker").display()
    ));
    cmd
}

/// The group is gone with the command: a grandchild that outlives a successful git run
/// is ended too (Windows: `KILL_ON_JOB_CLOSE`).
#[cfg(unix)]
#[test]
fn a_finished_command_leaves_no_grandchild_behind() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new("sh");
    // the grandchild closes the pipes so the command itself returns at once
    cmd.arg("-c").arg(format!(
        "(sleep 2; echo done > '{}') >/dev/null 2>&1 </dev/null & exit 0",
        dir.path().join("marker").display()
    ));
    execute(cmd, None, None, None, None).unwrap();
    std::thread::sleep(Duration::from_secs(4));
    assert!(!dir.path().join("marker").exists(), "grandchild survived");
}

#[test]
fn cancel_kills_the_child_promptly_and_returns_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let stop = || start.elapsed() > Duration::from_millis(200);
    let err = execute(slow_child(dir.path()), None, None, None, Some(&stop)).unwrap_err();
    assert_eq!(err, GitError::Cancelled);
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
    std::thread::sleep(Duration::from_secs(4));
    assert!(!dir.path().join("marker").exists(), "child was not killed");
    // a token that never fires changes nothing
    let never = || false;
    execute(slow_child(dir.path()), None, None, None, Some(&never)).unwrap();
    assert!(dir.path().join("marker").exists());
}

#[test]
fn an_already_cancelled_command_is_never_started() {
    let git = Git::locate().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cancelled = || true;
    let opts = RunOpts {
        cancel: Some(&cancelled),
        ..RunOpts::default()
    };
    let before = git.spawn_count();
    // `init` would create .git at once if it ran
    let err = git.run_with(dir.path(), &["init", "-q"], opts).unwrap_err();
    assert_eq!(err, GitError::Cancelled);
    assert!(!dir.path().join(".git").exists(), "git ran anyway");
    assert_eq!(git.spawn_count(), before, "nothing was started");
    // the same through the low-level runner (the self-test's `ls-remote` path)
    let err = execute(slow_child(dir.path()), None, None, None, Some(&cancelled));
    assert_eq!(err.unwrap_err(), GitError::Cancelled);
    std::thread::sleep(Duration::from_secs(3));
    assert!(!dir.path().join("marker").exists(), "the child ran");
}

#[cfg(any(windows, unix))]
#[test]
fn cancel_kills_grandchildren_too() {
    let dir = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let stop = || start.elapsed() > Duration::from_millis(300);
    let err = execute(
        child_with_grandchild(dir.path()),
        None,
        None,
        None,
        Some(&stop),
    );
    assert_eq!(err.unwrap_err(), GitError::Cancelled);
    std::thread::sleep(Duration::from_secs(4));
    assert!(!dir.path().join("marker").exists(), "grandchild survived");
}

#[test]
fn exec_path_lookup_ignores_an_inherited_git_exec_path() {
    let cmd = exec_path_command(Path::new("git"));
    let removed = envs(&cmd)
        .into_iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("GIT_EXEC_PATH") && v.is_none());
    assert!(removed, "GIT_EXEC_PATH would redirect the lookup");
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(args, ["--exec-path"]);
}

#[cfg(any(windows, unix))]
#[test]
fn timeout_kills_grandchildren_too() {
    let dir = tempfile::tempdir().unwrap();
    let limit = Duration::from_millis(500);
    let err = execute(
        child_with_grandchild(dir.path()),
        None,
        None,
        Some(limit),
        None,
    )
    .unwrap_err();
    assert_eq!(err, GitError::Timeout { after: limit });
    // a surviving grandchild would have written the marker by now
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        !dir.path().join("marker").exists(),
        "grandchild survived the timeout"
    );
}

#[test]
fn launcher_layout_resolves_to_the_real_binary() {
    let root = tempfile::tempdir().unwrap();
    let cmd_dir = root.path().join("cmd");
    let real_dir = root.path().join("mingw64").join("bin");
    std::fs::create_dir_all(&cmd_dir).unwrap();
    std::fs::create_dir_all(&real_dir).unwrap();
    let launcher = cmd_dir.join(GIT_EXE);
    let real = real_dir.join(GIT_EXE);
    std::fs::write(&launcher, "shim").unwrap();
    std::fs::write(&real, "real").unwrap();
    assert_eq!(launcher_target(&launcher), Some(real.clone()));
    let never = |_: &Path| -> Option<PathBuf> { panic!("no exec-path spawn needed") };
    let ok = |_: &Path| Ok(());
    assert_eq!(resolve_exe(launcher.clone(), &ok, &never), Ok(real.clone()));
    // the real binary does not run: keep the launcher
    let only_launcher = |p: &Path| {
        if p == launcher.as_path() {
            Ok(())
        } else {
            Err(GitError::NotInstalled)
        }
    };
    assert_eq!(
        resolve_exe(launcher.clone(), &only_launcher, &never),
        Ok(launcher.clone())
    );
    // the real binary is missing: keep the launcher (exec-path finds nothing either)
    std::fs::remove_file(&real).unwrap();
    assert_eq!(launcher_target(&launcher), None);
    let nothing = |_: &Path| -> Option<PathBuf> { None };
    assert_eq!(
        resolve_exe(launcher.clone(), &ok, &nothing),
        Ok(launcher.clone())
    );
    // neither runs: the launcher's error
    let none = |_: &Path| Err(GitError::NotInstalled);
    assert_eq!(
        resolve_exe(launcher.clone(), &none, &nothing),
        Err(GitError::NotInstalled)
    );
    // not a launcher layout at all
    assert_eq!(launcher_target(&real_dir.join(GIT_EXE)), None);
}

#[cfg(windows)]
#[test]
fn other_launchers_resolve_through_exec_path() {
    let root = tempfile::tempdir().unwrap();
    let shim = root.path().join("shims").join(GIT_EXE);
    let prefix = root.path().join("app").join("mingw64");
    let exec = prefix.join("libexec").join("git-core");
    std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&exec).unwrap();
    std::fs::create_dir_all(prefix.join("bin")).unwrap();
    std::fs::write(&shim, "shim").unwrap();
    let real = prefix.join("bin").join(GIT_EXE);
    std::fs::write(&real, "real").unwrap();
    let ok = |_: &Path| Ok(());
    let e = exec.clone();
    let ask = move |_: &Path| Some(e.clone());
    assert_eq!(resolve_exe(shim.clone(), &ok, &ask), Ok(real.clone()));
    // the real binary itself is never asked again
    let never = |_: &Path| -> Option<PathBuf> { panic!("already the real binary") };
    assert_eq!(resolve_exe(real.clone(), &ok, &never), Ok(real));
}

#[test]
fn locate_skips_the_windows_launcher_when_installed() {
    let git = Git::locate().unwrap();
    assert!(git.exe().is_absolute() || git.exe() == Path::new("git"));
    if let Some(found) = find_on_path() {
        if launcher_target(&found).is_some() {
            assert_ne!(git.exe(), found.as_path(), "still runs the launcher");
            assert!(launcher_target(&found).as_deref() == Some(git.exe()));
        }
    }
    // shared: one lookup, clones share the counter
    let a = Git::shared().unwrap();
    let b = Git::shared().unwrap();
    assert!(Arc::ptr_eq(&a.spawns, &b.spawns));
    let before = a.spawn_count();
    a.run(Path::new("."), &["--version"], None).unwrap();
    assert!(b.spawn_count() > before);
}

#[test]
fn timeout_display_is_readable() {
    let e = GitError::Timeout {
        after: NETWORK_TIMEOUT,
    };
    assert!(e.to_string().contains("120 minutes"), "{e}");
}

#[test]
fn progress_lines_parse_to_phase_and_percent() {
    let p = |phase: &str, percent| {
        Some(GitProgress {
            phase: phase.into(),
            percent,
        })
    };
    assert_eq!(
        parse_progress("remote: Counting objects:  12% (1/8)"),
        p("Counting objects", 12)
    );
    assert_eq!(
        parse_progress("Receiving objects:  45% (450/1000), 1.20 MiB | 1.00 MiB/s"),
        p("Receiving objects", 45)
    );
    assert_eq!(
        parse_progress("Resolving deltas: 100% (2/2), done."),
        p("Resolving deltas", 100)
    );
    for other in [
        "remote: Enumerating objects: 123, done.",
        "Cloning into bare repository 'ws/r.git.tmp'...",
        "fatal: https://bitbucket.org/x: 50% nonsense",
        "Receiving objects: 150% (1/1)",
        "",
    ] {
        assert_eq!(parse_progress(other), None, "{other}");
    }
}

#[test]
fn parse_version_cases() {
    assert_eq!(parse_version("git version 2.53.0.windows.1"), Some((2, 53)));
    assert_eq!(parse_version("git version 2.31.0"), Some((2, 31)));
    assert_eq!(parse_version("nonsense"), None);
}
