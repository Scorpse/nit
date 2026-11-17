use super::*;
use gitlocal::GitAuth;
use serde_json::json;
use std::cell::RefCell;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "glpat-very-secret-token-0123456789";

struct FakeGit {
    result: Result<(), String>,
    calls: RefCell<Vec<(String, String, String)>>,
}

impl FakeGit {
    fn ok() -> FakeGit {
        FakeGit {
            result: Ok(()),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl GitSide for FakeGit {
    fn version(&self) -> Result<String, String> {
        Ok("git version 2.47.1.windows.1".into())
    }
    fn ls_remote(&self, url: &str, user: Option<&str>, auth: &GitAuth) -> Result<(), String> {
        assert_eq!(auth.token, TOKEN);
        assert_eq!(user, None);
        self.calls
            .borrow_mut()
            .push((url.into(), auth.user.clone(), auth.scope.clone()));
        self.result.clone()
    }
}

fn today() -> Date {
    Date::new(2026, 11, 14).unwrap()
}

fn bases(s: &MockServer, git_host: &str) -> GitLabBases {
    GitLabBases {
        web_base: s.uri(),
        api_base: s.uri(),
        git_host: git_host.into(),
    }
}

fn args(repo: Option<&str>) -> CheckArgs {
    CheckArgs {
        account: None,
        repo: repo.and_then(|r| RepoRef::parse("gitlab.com", r)),
        pr: None,
        write_test: false,
    }
}

async fn server(scopes: &[&str], expires: Option<&str>) -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": 1, "username": "root", "name": "Administrator"})),
        )
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/personal_access_tokens/self"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id": 4, "scopes": scopes, "active": true, "expires_at": expires}),
        ))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 3, "path_with_namespace": "grp/sub/proj", "name": "proj"}
        ])))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"/merge_requests$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&s)
        .await;
    s
}

async fn run_with(s: &MockServer, git: &FakeGit, a: &CheckArgs, host: &str) -> Report {
    let b = bases(s, host);
    let inp = Inputs {
        args: a,
        bases: &b,
        git,
        proxy_var: None,
        today: today(),
    };
    run(&inp, Ok((TOKEN.into(), Source::EnvGitLab))).await
}

#[tokio::test]
async fn read_api_only_token_warns_comments_disabled() {
    let s = server(&["read_api", "read_repository"], Some("2027-06-01")).await;
    let git = FakeGit::ok();
    let r = run_with(&s, &git, &args(Some("grp/sub/proj")), "gitlab.com").await;
    let c = &r.checks[&6];
    assert_eq!(c.status, Status::Pass, "{}", c.detail);
    assert!(
        c.detail
            .contains("comments and approvals need the api scope"),
        "{}",
        c.detail
    );
    assert_eq!(r.checks[&4].status, Status::Pass, "{}", r.checks[&4].detail);
    // git as oauth2, scoped to the host, URL with the nested namespace
    assert_eq!(r.checks[&7].status, Status::Pass);
    assert_eq!(
        git.calls.borrow()[0],
        (
            "https://gitlab.com/grp/sub/proj.git".to_string(),
            "oauth2".to_string(),
            "https://gitlab.com/".to_string()
        )
    );
    let text = r.render(&[TOKEN]);
    assert!(!text.contains(TOKEN));
    // read_api without read_repository: git cannot fetch
    let s = server(&["read_api"], None).await;
    let r = run_with(&s, &FakeGit::ok(), &args(None), "gitlab.com").await;
    assert_eq!(r.checks[&6].status, Status::Fail);
    assert!(r.checks[&6].detail.contains("read_repository"));
    // nothing but GETs were sent
    for req in s.received_requests().await.unwrap() {
        assert_eq!(req.method.as_str(), "GET", "{}", req.url);
    }
}

/// A plain-http account on a private address (the mock's own 127.0.0.1): git goes over
/// http to exactly that origin, and check 9 says the connection is not encrypted.
#[tokio::test]
async fn an_http_account_uses_plain_http_git_on_its_own_origin() {
    let s = server(&["api"], None).await;
    let git = FakeGit::ok();
    let host = s.address().to_string();
    let r = run_with(&s, &git, &args(Some("grp/proj")), &host).await;
    assert_eq!(r.checks[&7].status, Status::Pass, "{}", r.checks[&7].detail);
    assert_eq!(
        git.calls.borrow()[0],
        (
            format!("http://{host}/grp/proj.git"),
            "oauth2".to_string(),
            format!("http://{host}/")
        )
    );
    let c9 = &r.checks[&9];
    assert!(c9.detail.contains("plain HTTP"), "{}", c9.detail);
    assert!(!r.render(&[TOKEN]).contains(TOKEN));
}

#[test]
fn expiry_warning_within_14_days() {
    let d = |y, m, day| Date::new(y, m, day).ok();
    assert_eq!(expiry_warning(None, today()), None);
    assert_eq!(expiry_warning(d(2026, 12, 31), today()), None);
    assert_eq!(expiry_warning(d(2026, 11, 29), today()), None, "15 days");
    assert!(expiry_warning(d(2026, 11, 28), today())
        .unwrap()
        .contains("in 14 day(s)"));
    assert_eq!(
        expiry_warning(d(2026, 11, 14), today()).as_deref(),
        Some("the token expires today")
    );
    assert!(expiry_warning(d(2026, 11, 1), today())
        .unwrap()
        .contains("expired"));
}

#[tokio::test]
async fn an_expiring_token_fails_check_6_with_a_hint() {
    let s = server(&["api"], Some("2026-11-20")).await;
    let r = run_with(&s, &FakeGit::ok(), &args(None), "gitlab.com").await;
    let c = &r.checks[&6];
    assert_eq!(c.status, Status::Fail);
    assert!(c.detail.contains("expires in 6 day(s)"), "{}", c.detail);
    assert!(c.hint.as_deref().unwrap().contains("before it expires"));
}

#[tokio::test]
async fn cert_untrusted_hint_shown_for_api_and_git_separately() {
    // the API answers (9 passes) while git distrusts the host's certificate (7 fails)
    let s = server(&["api"], None).await;
    let git = FakeGit {
        result: Err("fatal: unable to access 'https://git.corp.example/gitlab/grp/sub/proj.git/': SSL certificate problem: unable to get local issuer certificate".into()),
        calls: RefCell::new(Vec::new()),
    };
    let r = run_with(
        &s,
        &git,
        &args(Some("grp/sub/proj")),
        "git.corp.example/gitlab",
    )
    .await;
    assert_eq!(r.checks[&9].status, Status::Pass);
    let g = &r.checks[&7];
    assert_eq!(g.status, Status::Fail);
    assert!(g.hint.as_deref().unwrap().contains("sslBackend schannel"));
    assert!(g.hint.as_deref().unwrap().contains("check 9"));
    // relative URL root: clone URL under the prefix, header scoped to the host
    assert_eq!(
        git.calls.borrow()[0],
        (
            "https://git.corp.example/gitlab/grp/sub/proj.git".to_string(),
            "oauth2".to_string(),
            "https://git.corp.example/".to_string()
        )
    );
    // and the other way round: an API certificate failure is check 9's, with its hint
    let unreachable = GitLabBases {
        web_base: "https://127.0.0.1:9".into(),
        api_base: "https://127.0.0.1:9/api/v4".into(),
        git_host: "127.0.0.1:9".into(),
    };
    let a = args(None);
    let inp = Inputs {
        args: &a,
        bases: &unreachable,
        git: &FakeGit::ok(),
        proxy_var: None,
        today: today(),
    };
    let r = run(&inp, Ok((TOKEN.into(), Source::EnvGitLab))).await;
    assert_eq!(r.checks[&9].status, Status::Fail);
    assert_eq!(r.checks[&2].status, Status::Skip);
}

#[tokio::test]
async fn a_refused_token_skips_the_rest_and_write_test_is_skipped() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "401 Unauthorized"})),
        )
        .mount(&s)
        .await;
    let mut a = args(Some("grp/proj"));
    a.pr = Some(3);
    a.write_test = true;
    let r = run_with(&s, &FakeGit::ok(), &a, "gitlab.com").await;
    assert_eq!(r.checks[&2].status, Status::Fail);
    assert!(r.checks[&2].hint.as_deref().unwrap().contains("read_api"));
    assert_eq!(r.checks[&3].status, Status::Skip);
    assert_eq!(r.write[0].status, Status::Skip);
    assert!(r.write[0].detail.contains("no GitLab write test"));
}

#[test]
fn the_env_account_needs_the_token_and_a_matching_name() {
    let env = |k: &str| match k {
        ENV_GITLAB_TOKEN => Some("t".to_string()),
        ENV_GITLAB_URL => Some("https://git.corp.example/gitlab".to_string()),
        _ => None,
    };
    let a = env_account(Some("git.corp.example/gitlab"), env).unwrap();
    assert_eq!(a.api_base, "https://git.corp.example/gitlab/api/v4");
    assert_eq!(a.forge, Forge::GitLab);
    assert!(env_account(Some("gitlab"), env).is_some());
    assert!(env_account(Some("github"), env).is_none());
    let plain = |k: &str| (k == ENV_GITLAB_TOKEN).then(|| "t".to_string());
    assert_eq!(
        env_account(Some("gitlab.com"), plain).unwrap().git_host,
        "gitlab.com"
    );
    let secrets = store::MemorySecrets::new();
    assert!(load_token(&secrets, &a, |_| None)
        .unwrap_err()
        .contains(ENV_GITLAB_TOKEN));
    secrets.set(&token_key(&a.id), "stored").unwrap();
    assert_eq!(
        load_token(&secrets, &a, |_| None).unwrap(),
        ("stored".into(), Source::Stored)
    );
}

#[test]
fn scope_note_needs_read_api_and_git_access() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(scope_note(&s(&["read_user"])).is_err());
    assert!(scope_note(&s(&["read_api"]))
        .unwrap_err()
        .contains("read_repository"));
    assert!(scope_note(&s(&["api"])).is_ok());
    assert!(scope_note(&s(&["read_api", "read_repository"])).is_ok());
}
