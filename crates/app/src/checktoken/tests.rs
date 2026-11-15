use super::*;
use serde_json::json;
use std::cell::RefCell;
use store::MemorySecrets;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "ATATT-very-secret-token-0123456789";
const EMAIL: &str = "ana@example.com";

fn argv(a: &[&str]) -> Vec<String> {
    std::iter::once("nit")
        .chain(a.iter().copied())
        .map(String::from)
        .collect()
}

fn creds() -> Result<Creds, String> {
    Ok(Creds {
        email: EMAIL.into(),
        token: TOKEN.into(),
        source: Source::Env,
    })
}

/// A git that answers from a table: `ok_users` are the user names ls-remote accepts
/// (`None` is the app's own).
struct FakeGit {
    version: Result<String, String>,
    ok_users: Vec<Option<String>>,
    error: String,
    calls: RefCell<Vec<Option<String>>>,
}

impl FakeGit {
    fn ok() -> FakeGit {
        FakeGit {
            version: Ok("git version 2.47.1.windows.1".into()),
            ok_users: vec![None],
            error: String::new(),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl GitSide for FakeGit {
    fn version(&self) -> Result<String, String> {
        self.version.clone()
    }
    fn ls_remote(&self, _url: &str, user: Option<&str>, auth: &GitAuth) -> Result<(), String> {
        assert_eq!(auth.token, TOKEN);
        let user = user.map(String::from);
        self.calls.borrow_mut().push(user.clone());
        if self.ok_users.contains(&user) {
            Ok(())
        } else {
            Err(self.error.clone())
        }
    }
}

fn args(repo: Option<&str>, pr: Option<u64>, write_test: bool) -> CheckArgs {
    CheckArgs {
        account: None,
        repo: repo.and_then(RepoRef::parse_bitbucket),
        pr,
        write_test,
    }
}

async fn get(s: &MockServer, p: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(s)
        .await;
}

fn pr_json(id: u64) -> Value {
    json!({
        "id": id, "title": "Add things", "state": "OPEN",
        "author": {"uuid": "{bo}", "display_name": "Bo"},
        "source": {"branch": {"name": "feature"}, "commit": {"hash": "abc"},
                   "repository": {"full_name": "acme/api"}},
        "destination": {"branch": {"name": "main"}, "commit": {"hash": "def"},
                        "repository": {"full_name": "acme/api"}},
        "participants": [
            {"user": {"uuid": "{ana}", "display_name": "Ana"}, "role": "REVIEWER",
             "approved": true, "state": "approved"},
            {"user": {"uuid": "{cy}", "display_name": "Cy"}, "role": "REVIEWER",
             "approved": false, "state": null}
        ],
        "created_on": "2026-01-01T00:00:00Z", "updated_on": "2026-01-02T00:00:00Z"
    })
}

fn comment_json(id: u64, text: &str, bot: bool) -> Value {
    let user = if bot {
        json!({"type": "app_user", "display_name": "Sonar", "uuid": "{app}"})
    } else {
        json!({"type": "user", "display_name": "Bo", "uuid": "{bo}"})
    };
    json!({"id": id, "user": user, "content": {"raw": text},
           "created_on": "2026-01-01T00:00:00Z", "updated_on": "2026-01-01T00:00:00Z"})
}

/// A healthy account: one workspace `acme`, repos `empty` (no PRs) and `api` (PR 7).
async fn healthy(display_name: &str) -> MockServer {
    let s = MockServer::start().await;
    get(
        &s,
        "/user",
        json!({"uuid": "{ana}", "display_name": display_name, "username": "ana"}),
    )
    .await;
    get(
        &s,
        "/user/workspaces",
        json!({"values": [{"workspace": {"slug": "acme"}}]}),
    )
    .await;
    get(
        &s,
        "/repositories/acme",
        json!({"values": [{"full_name": "acme/empty"}, {"full_name": "acme/api"}]}),
    )
    .await;
    get(
        &s,
        "/repositories/acme/empty/pullrequests",
        json!({"values": []}),
    )
    .await;
    get(
        &s,
        "/repositories/acme/api/pullrequests",
        json!({"values": [pr_json(7)]}),
    )
    .await;
    get(&s, "/repositories/acme/api/pullrequests/7", pr_json(7)).await;
    get(
        &s,
        "/repositories/acme/api/pullrequests/7/diffstat",
        json!({"values": [{"lines_added": 3, "lines_removed": 1}]}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/repositories/acme/api/pullrequests/7/diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string("diff --git a/x b/x\n"))
        .mount(&s)
        .await;
    get(
        &s,
        "/repositories/acme/api/pullrequests/7/tasks",
        json!({"values": []}),
    )
    .await;
    get(
        &s,
        "/user/permissions/repositories",
        json!({"values": [{"permission": "write"}]}),
    )
    .await;
    s
}

async fn mount_comments(s: &MockServer, values: Value) {
    get(s, "/repositories/acme/api/pullrequests/7/comments", values).await;
}

async fn check(s: &MockServer, a: &CheckArgs, git: &FakeGit) -> Report {
    let base = s.uri();
    let inputs = Inputs {
        args: a,
        api_base: &base,
        git,
        proxy_var: None,
    };
    run(&inputs, creds()).await
}

fn statuses(r: &Report) -> Vec<(u8, Status)> {
    r.checks.iter().map(|(n, c)| (*n, c.status)).collect()
}

#[test]
fn check_token_flags_parse_and_refuse_bad_combinations() {
    assert_eq!(parse_args(&argv(&[])).unwrap(), None);
    assert_eq!(
        parse_args(&argv(&["--check-token"])).unwrap(),
        Some(args(None, None, false))
    );
    let a = parse_args(&argv(&["--check-token", "--repo", "acme/api", "--pr", "7"])).unwrap();
    assert_eq!(a, Some(args(Some("acme/api"), Some(7), false)));
    let w = parse_args(&argv(&[
        "--check-token",
        "--write-test",
        "--pr",
        "7",
        "--repo",
        "acme/api",
        "--i-understand-this-posts",
    ]))
    .unwrap();
    assert!(w.unwrap().write_test);
    for bad in [
        vec!["--repo", "acme/api"],
        vec!["--pr", "7"],
        vec!["--write-test"],
        vec!["--check-token", "--pr", "7"],
        vec!["--check-token", "--repo"],
        vec!["--check-token", "--repo", "acme"],
        vec!["--check-token", "--repo", "acme/../x"],
        vec!["--check-token", "--repo", "acme/a?b"],
        vec!["--check-token", "--repo", "acme/api", "--pr", "x"],
        vec!["--check-token", "--repo", "acme/api", "--pr", "0"],
        vec!["--check-token", "--demo"],
        vec![
            "--check-token",
            "--write-test",
            "--repo",
            "acme/api",
            "--pr",
            "7",
        ],
        vec![
            "--check-token",
            "--write-test",
            "--repo",
            "acme/api",
            "--i-understand-this-posts",
        ],
        vec!["--check-token", "--i-understand-this-posts"],
    ] {
        assert!(parse_args(&argv(&bad)).is_err(), "{bad:?}");
    }
}

fn account(id: &str, forge: Forge, host: &str) -> Account {
    Account {
        id: AccountId(id.into()),
        forge,
        web_base: format!("https://{host}"),
        git_host: host.into(),
        ..Account::default_bitbucket(String::new(), String::new())
    }
}

#[test]
fn transport_rule_allows_https_and_warns_on_private_http() {
    assert_eq!(transport_note("https://gitlab.com/api/v4"), Ok(None));
    assert_eq!(
        transport_note("http://192.168.1.20:8080/api/v4"),
        Ok(Some(prcore::origin::CLEARTEXT_WARNING))
    );
    for bad in [
        "http://gitlab.com/api/v4",
        "http://8.8.8.8/api/v4",
        "http://192.168.1.1.evil.com/api/v4",
        "ftp://x",
    ] {
        assert!(transport_note(bad).is_err(), "{bad}");
    }
}

#[test]
fn account_flag_selects_by_id_or_host() {
    let accounts = vec![
        Account::default_bitbucket("{ana}".into(), "Ana".into()),
        account("acc-2", Forge::BitbucketCloud, "bitbucket.org"),
        account("acc-gl", Forge::GitLab, "git.corp.example/gitlab"),
    ];
    let id = |r: Result<Account, String>| r.map(|a| a.id.as_str().to_string());
    assert_eq!(
        id(select_account(&accounts, None)),
        Ok("acc-bitbucket-default".into())
    );
    assert_eq!(
        id(select_account(&accounts, Some("acc-2"))),
        Ok("acc-2".into())
    );
    assert_eq!(
        id(select_account(&accounts, Some("GIT.corp.example/gitlab"))),
        Ok("acc-gl".into())
    );
    let two = select_account(&accounts, Some("bitbucket.org")).unwrap_err();
    assert!(
        two.contains("acc-2") && two.contains("acc-bitbucket-default"),
        "{two}"
    );
    assert!(select_account(&accounts, Some("github.com")).is_err());
    // no cache yet: the default sign-in
    assert_eq!(
        id(select_account(&[], None)),
        Ok("acc-bitbucket-default".into())
    );
    let parsed = parse_args(&argv(&["--check-token", "--account", "acc-2"]))
        .unwrap()
        .unwrap();
    assert_eq!(parsed.account.as_deref(), Some("acc-2"));
    assert!(parse_args(&argv(&["--account", "acc-2"])).is_err());
    assert!(parse_args(&argv(&["--check-token", "--account"])).is_err());
}

#[test]
fn nested_repo_rejected_for_bitbucket_accepted_for_gitlab() {
    let a = parse_args(&argv(&["--check-token", "--repo", "group/sub/proj"]))
        .unwrap()
        .unwrap();
    let nested = a.repo.unwrap();
    let bb = account("acc-2", Forge::BitbucketCloud, "bitbucket.org");
    let gl = account("acc-gl", Forge::GitLab, "git.corp.example/gitlab");
    assert!(repo_for(&bb, &nested).unwrap_err().contains("nested"));
    let r = repo_for(&gl, &nested).unwrap();
    assert_eq!(
        (r.host.as_str(), r.namespace.as_str(), r.slug.as_str()),
        ("git.corp.example/gitlab", "group/sub", "proj")
    );
    let flat = RepoRef::parse_bitbucket("acme/api").unwrap();
    assert_eq!(repo_for(&bb, &flat).unwrap(), flat);
    assert!(parse_args(&argv(&["--check-token", "--repo", "group/../proj"])).is_err());
}

#[test]
fn report_scrubs_every_account_token() {
    let s = MemorySecrets::new();
    let (two, three) = (
        account("acc-2", Forge::BitbucketCloud, "bitbucket.org"),
        account("acc-3", Forge::BitbucketCloud, "bitbucket.org"),
    );
    s.set(&email_key(&two.id), "bo@example.com").unwrap();
    s.set(&token_key(&two.id), "token-of-account-two").unwrap();
    s.set(&token_key(&three.id), "token-of-account-three")
        .unwrap();
    let creds = load_account_credentials(&s, &two, |_| None);
    assert_eq!(creds.as_ref().unwrap().source, Source::Stored);
    // a half stored account is no sign-in, and other forges are not checked yet
    assert!(load_account_credentials(&s, &three, |_| None).is_err());
    let gh = account("acc-gh", Forge::GitHub, "github.com");
    assert!(load_account_credentials(&s, &gh, |_| None)
        .unwrap_err()
        .contains("not checked"));
    let env = |k: &str| (k == ENV_TOKEN).then(|| "token-from-the-env".to_string());
    let scrub = tokens_to_scrub(&s, &[two, three], &creds, env);
    let mut r = Report {
        account: Some("Bitbucket Cloud bitbucket.org (acc-2)".into()),
        ..Report::default()
    };
    r.pass(
        2,
        "token-of-account-two token-of-account-three token-from-the-env",
    );
    let refs: Vec<&str> = scrub.iter().map(String::as_str).collect();
    let text = r.render(&refs);
    assert!(text.contains("Account: Bitbucket Cloud bitbucket.org (acc-2)"));
    for t in [
        "token-of-account-two",
        "token-of-account-three",
        "token-from-the-env",
    ] {
        assert!(!text.contains(t), "{text}");
    }
}

#[test]
fn stored_credentials_win_and_the_environment_is_the_fallback() {
    let s = MemorySecrets::new();
    let env = |k: &str| match k {
        ENV_EMAIL => Some("env@example.com".to_string()),
        ENV_TOKEN => Some("env-token-value".to_string()),
        _ => None,
    };
    assert_eq!(load_credentials(&s, env).unwrap().source, Source::Env);
    let (ek, tk) = crate::accounts::bitbucket_keys();
    s.set(&ek, EMAIL).unwrap();
    s.set(&tk, TOKEN).unwrap();
    let c = load_credentials(&s, env).unwrap();
    assert_eq!((c.source, c.email.as_str()), (Source::Stored, EMAIL));
    assert!(!format!("{c:?}").contains(TOKEN));
    let none = load_credentials(&MemorySecrets::new(), |_| None).unwrap_err();
    assert!(none.contains("NIT_EMAIL"), "{none}");
    let half = load_credentials(&MemorySecrets::new(), |k| {
        (k == ENV_EMAIL).then(|| "x@example.com".to_string())
    })
    .unwrap_err();
    assert!(half.contains("NIT_TOKEN is not"), "{half}");
}

#[test]
fn the_deprecated_prview_variables_still_work_and_the_new_ones_win() {
    let old = |k: &str| match k {
        LEGACY_ENV_EMAIL => Some("old@example.com".to_string()),
        LEGACY_ENV_TOKEN => Some("old-token-value".to_string()),
        _ => None,
    };
    let c = load_credentials(&MemorySecrets::new(), old).unwrap();
    assert_eq!(
        (c.source, c.email.as_str()),
        (Source::Env, "old@example.com")
    );
    let both = |k: &str| match k {
        ENV_EMAIL => Some("new@example.com".to_string()),
        ENV_TOKEN => Some("new-token-value".to_string()),
        _ => old(k),
    };
    let c = load_credentials(&MemorySecrets::new(), both).unwrap();
    assert_eq!(
        (c.email.as_str(), c.token.as_str()),
        ("new@example.com", "new-token-value")
    );
}

#[tokio::test]
async fn a_healthy_account_is_all_ok_and_nothing_is_posted() {
    let s = healthy("Ana Souza").await;
    mount_comments(
        &s,
        json!({"values": [comment_json(1, "looks good", false), comment_json(2, "Quality gate passed", true)]}),
    )
    .await;
    for m in ["POST", "DELETE", "PUT"] {
        Mock::given(method(m))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&s)
            .await;
    }
    let git = FakeGit::ok();
    let r = check(&s, &args(None, None, false), &git).await;
    let text = r.render(&[TOKEN]);
    assert!(
        statuses(&r).iter().all(|(_, st)| *st == Status::Pass),
        "{text}"
    );
    assert!(text.trim_end().ends_with("ALL OK"), "{text}");
    assert!(text.contains("signed in as Ana Souza"), "{text}");
    assert!(text.contains("picked acme/api"), "{text}");
    assert!(
        text.contains("2 comment(s), 1 of them from bots / apps"),
        "{text}"
    );
    assert!(
        text.contains("1 approved") && text.contains("write"),
        "{text}"
    );
    assert!(text.contains(GIT_TOKEN_USER), "{text}");
    // only the app's own user name was tried
    assert_eq!(*git.calls.borrow(), vec![None]);
    assert!(
        !text.contains("ana@example.com") && !text.contains("{ana}"),
        "{text}"
    );
}

#[tokio::test]
async fn the_token_never_reaches_the_report() {
    // a hostile server and git echo the token back in every place that is shown
    let s = healthy(&format!("Ana {TOKEN}")).await;
    mount_comments(&s, json!({"values": [comment_json(1, TOKEN, false)]})).await;
    let git = FakeGit {
        version: Ok(format!("git version 2.47 {TOKEN}")),
        ok_users: vec![],
        error: format!("fatal: Authentication failed for {TOKEN}"),
        calls: RefCell::new(Vec::new()),
    };
    let r = check(&s, &args(Some("acme/api"), Some(7), false), &git).await;
    let text = r.render(&[TOKEN]);
    assert!(!text.contains(TOKEN), "{text}");
    assert!(text.contains("***"), "{text}");
    // no "ALL OK" with a failed git check
    assert!(text.trim_end().ends_with("1 problem"), "{text}");
}

#[tokio::test]
async fn a_refused_token_fails_check_2_and_skips_what_depends_on_it() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&s)
        .await;
    let git = FakeGit {
        ok_users: vec![],
        error: "remote: Invalid credentials\nfatal: Authentication failed".into(),
        ..FakeGit::ok()
    };
    let r = check(&s, &args(Some("acme/api"), None, false), &git).await;
    assert_eq!(r.status(1), Some(Status::Pass));
    assert_eq!(r.status(2), Some(Status::Fail));
    for n in [3, 4, 5, 6] {
        assert_eq!(r.status(n), Some(Status::Skip), "{n}");
    }
    assert_eq!(r.status(9), Some(Status::Pass), "the server answered");
    assert_eq!(r.status(7), Some(Status::Fail));
    let text = r.render(&[TOKEN]);
    assert!(
        text.contains("HTTP 401") && text.contains("API token"),
        "{text}"
    );
    assert!(text.trim_end().ends_with("2 problems"), "{text}");
}

#[tokio::test]
async fn git_reports_which_user_name_works() {
    let s = healthy("Ana").await;
    mount_comments(&s, json!({"values": []})).await;
    let git = FakeGit {
        ok_users: vec![Some(EMAIL.into())],
        error: "fatal: Authentication failed for 'https://bitbucket.org/acme/api.git/'".into(),
        ..FakeGit::ok()
    };
    let r = check(&s, &args(Some("acme/api"), Some(7), false), &git).await;
    let c = &r.checks[&7];
    assert_eq!(c.status, Status::Fail);
    assert!(
        c.detail.contains("your account email: accepted"),
        "{}",
        c.detail
    );
    assert!(
        c.detail.contains("your Bitbucket username: refused"),
        "{}",
        c.detail
    );
    assert!(
        c.detail.contains("your account UUID: refused"),
        "{}",
        c.detail
    );
    let hint = c.hint.as_deref().unwrap();
    assert!(hint.contains("only with your account email"), "{hint}");
    assert_eq!(
        *git.calls.borrow(),
        vec![
            None,
            Some(EMAIL.into()),
            Some("ana".into()),
            Some("{ana}".into())
        ]
    );
}

#[tokio::test]
async fn git_network_and_certificate_failures_try_no_other_user_name() {
    let s = healthy("Ana").await;
    mount_comments(&s, json!({"values": []})).await;
    for (err, word) in [
        ("fatal: unable to access: SSL certificate problem: unable to get local issuer certificate", "certificate"),
        ("fatal: unable to access: Could not resolve host: bitbucket.org", "reach"),
    ] {
        let git = FakeGit {
            ok_users: vec![],
            error: err.into(),
            ..FakeGit::ok()
        };
        let r = check(&s, &args(Some("acme/api"), Some(7), false), &git).await;
        let c = &r.checks[&7];
        assert!(c.status == Status::Fail && c.detail.contains(word), "{}", c.detail);
        assert_eq!(*git.calls.borrow(), vec![None]);
    }
}

#[tokio::test]
async fn an_unreachable_api_fails_check_9_not_check_2() {
    let inputs_args = args(None, None, false);
    let git = FakeGit::ok();
    let inputs = Inputs {
        args: &inputs_args,
        api_base: "http://127.0.0.1:1",
        git: &git,
        proxy_var: Some("HTTPS_PROXY"),
    };
    let r = run(&inputs, creds()).await;
    assert_eq!(r.status(9), Some(Status::Fail));
    assert_eq!(r.status(2), Some(Status::Skip));
    let text = r.render(&[TOKEN]);
    assert!(text.contains("could not reach 127.0.0.1:1"), "{text}");
    assert!(text.contains("HTTPS_PROXY"), "{text}");
}

#[test]
fn a_certificate_error_is_told_apart_from_the_network() {
    let mut r = Report::default();
    let cert = Err(ProviderError::Network(CERT_UNTRUSTED.into()));
    check_reach(
        &mut r,
        &cert,
        "https://api.bitbucket.org",
        "api.bitbucket.org",
        None,
    );
    let c = &r.checks[&9];
    assert!(c.detail.contains("certificate") && c.hint.as_deref().unwrap().contains("root"));
    let net = Err(ProviderError::Network("connection failed".into()));
    check_reach(
        &mut r,
        &net,
        "https://api.bitbucket.org",
        "api.bitbucket.org",
        None,
    );
    assert!(r.checks[&9].detail.contains("could not reach"));
    check_reach(
        &mut r,
        &Err(ProviderError::Auth),
        "https://api.bitbucket.org",
        "api.bitbucket.org",
        None,
    );
    assert_eq!(r.status(9), Some(Status::Pass));
}

#[tokio::test]
async fn missing_credentials_fail_check_1_and_still_check_git() {
    let a = args(None, None, false);
    let git = FakeGit::ok();
    let inputs = Inputs {
        args: &a,
        api_base: "http://127.0.0.1:1",
        git: &git,
        proxy_var: None,
    };
    let r = run(&inputs, Err("nothing saved".into())).await;
    assert_eq!(r.status(1), Some(Status::Fail));
    assert_eq!(r.status(8), Some(Status::Pass));
    assert_eq!(r.status(2), Some(Status::Skip));
    assert!(r.render(&[]).trim_end().ends_with("1 problem"));
}

#[tokio::test]
async fn the_write_test_posts_reads_back_and_deletes() {
    let s = healthy("Ana").await;
    let mine = comment_json(55, WRITE_TEST_TEXT, false);
    // listed once (read back), then gone (after the delete)
    Mock::given(method("GET"))
        .and(path("/repositories/acme/api/pullrequests/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"values": [mine]})))
        .up_to_n_times(2)
        .mount(&s)
        .await;
    mount_comments(&s, json!({"values": []})).await;
    Mock::given(method("POST"))
        .and(path("/repositories/acme/api/pullrequests/7/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mine.clone()))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/repositories/acme/api/pullrequests/7/comments/55"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&s)
        .await;
    let r = check(&s, &args(Some("acme/api"), Some(7), true), &FakeGit::ok()).await;
    let text = r.render(&[TOKEN]);
    assert_eq!(r.write.len(), 4, "{text}");
    assert!(r.write.iter().all(|c| c.status == Status::Pass), "{text}");
    assert!(text.contains("Write test on acme/api PR #7"), "{text}");
    assert!(text.trim_end().ends_with("ALL OK"), "{text}");
}

#[tokio::test]
async fn a_refused_post_is_reported_with_the_write_scope_and_nothing_is_deleted() {
    let s = healthy("Ana").await;
    mount_comments(&s, json!({"values": []})).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&s)
        .await;
    let r = check(&s, &args(Some("acme/api"), Some(7), true), &FakeGit::ok()).await;
    assert_eq!(r.write[0].status, Status::Fail);
    assert!(r.write[0]
        .hint
        .as_deref()
        .unwrap()
        .contains("pull request: write"));
    assert!(r.write[1..].iter().all(|c| c.status == Status::Skip));
    assert_eq!(r.problems(), 1);
}

#[test]
fn git_errors_are_classified() {
    assert_eq!(
        classify_git("fatal: Authentication failed for 'x'"),
        GitFailure::Auth
    );
    assert_eq!(
        classify_git("remote: Repository not found\nfatal: repository 'x' not found"),
        GitFailure::NoAccess
    );
    assert_eq!(
        classify_git("SSL certificate problem: self-signed certificate in certificate chain"),
        GitFailure::Cert
    );
    assert_eq!(
        classify_git("Failed to connect to bitbucket.org port 443"),
        GitFailure::Network
    );
    assert_eq!(
        classify_git("git did not finish within 60 seconds and was stopped"),
        GitFailure::Timeout
    );
}
