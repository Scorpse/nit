use super::*;
use gitlocal::GitAuth;
use serde_json::json;
use std::cell::RefCell;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "gh-very-secret-token-0123456789";

struct FakeGit {
    ok: bool,
    error: String,
    urls: RefCell<Vec<(String, String, String)>>,
}

impl FakeGit {
    fn ok() -> FakeGit {
        FakeGit {
            ok: true,
            error: String::new(),
            urls: RefCell::new(Vec::new()),
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
        self.urls
            .borrow_mut()
            .push((url.into(), auth.user.clone(), auth.scope.clone()));
        if self.ok {
            Ok(())
        } else {
            Err(self.error.clone())
        }
    }
}

fn bases(s: &MockServer) -> GitHubBases {
    GitHubBases {
        web_base: s.uri(),
        api_base: s.uri(),
        graphql_url: format!("{}/graphql", s.uri()),
        git_host: "github.com".into(),
    }
}

fn args(repo: Option<&str>, pr: Option<u64>) -> CheckArgs {
    CheckArgs {
        account: None,
        repo: repo.and_then(|r| RepoRef::parse("github.com", r)),
        pr,
        write_test: false,
    }
}

async fn user(s: &MockServer, scopes: Option<&str>) {
    let mut resp = ResponseTemplate::new(200)
        .set_body_json(json!({"login": "octocat", "id": 1, "name": "Mona"}))
        .insert_header("x-ratelimit-remaining", "4990")
        .insert_header("x-ratelimit-limit", "5000");
    if let Some(sc) = scopes {
        resp = resp.insert_header("x-oauth-scopes", sc);
    }
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(resp)
        .mount(s)
        .await;
}

async fn repos(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!([{"full_name": "octo-org/api", "name": "api", "private": true}]),
            ),
        )
        .mount(s)
        .await;
}

async fn run_with(s: &MockServer, git: &FakeGit, a: &CheckArgs) -> Report {
    let b = bases(s);
    let inp = Inputs {
        args: a,
        bases: &b,
        git,
        proxy_var: None,
    };
    run(&inp, Ok((TOKEN.into(), Source::EnvGitHub))).await
}

#[tokio::test]
async fn classic_without_repo_scope_fails_with_hint() {
    let s = MockServer::start().await;
    user(&s, Some("read:org, gist")).await;
    repos(&s).await;
    Mock::given(method("GET"))
        .and(path("/repos/octo-org/api/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&s)
        .await;
    let r = run_with(&s, &FakeGit::ok(), &args(None, None)).await;
    let c = &r.checks[&2];
    assert_eq!(c.status, Status::Fail);
    assert!(c.detail.contains("without the repo scope"), "{}", c.detail);
    assert!(c.hint.as_deref().unwrap().contains("repo scope"));
    // the other checks still ran
    assert_eq!(r.checks[&3].status, Status::Pass);
    assert_eq!(r.checks[&6].status, Status::Pass);
    assert_eq!(r.checks[&6].detail, "4990 of 5000 requests left this hour");
}

#[tokio::test]
async fn fine_grained_token_passes_and_git_uses_x_access_token_on_the_host() {
    let s = MockServer::start().await;
    user(&s, None).await;
    repos(&s).await;
    Mock::given(method("GET"))
        .and(path("/repos/octo-org/api/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&s)
        .await;
    let git = FakeGit::ok();
    let r = run_with(&s, &git, &args(Some("octo-org/api"), None)).await;
    assert_eq!(r.checks[&2].status, Status::Pass);
    assert!(r.checks[&2].detail.contains("fine-grained"));
    assert_eq!(r.checks[&4].status, Status::Pass, "{}", r.checks[&4].detail);
    assert_eq!(r.checks[&7].status, Status::Pass);
    assert_eq!(
        git.urls.borrow()[0],
        (
            "https://github.com/octo-org/api.git".to_string(),
            "x-access-token".to_string(),
            "https://github.com/".to_string()
        )
    );
    let text = r.render(&[TOKEN]);
    assert!(!text.contains(TOKEN));
    assert!(text.contains("List open pull requests"));
}

#[tokio::test]
async fn fine_grained_403_shows_accepted_permissions() {
    let s = MockServer::start().await;
    user(&s, None).await;
    repos(&s).await;
    Mock::given(method("GET"))
        .and(path("/repos/octo-org/api/pulls"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(
                    json!({"message": "Resource not accessible by personal access token"}),
                )
                .insert_header("x-accepted-github-permissions", "pull_requests=read"),
        )
        .mount(&s)
        .await;
    let r = run_with(&s, &FakeGit::ok(), &args(Some("octo-org/api"), None)).await;
    let c = &r.checks[&4];
    assert_eq!(c.status, Status::Fail);
    assert!(
        c.hint.as_deref().unwrap().contains("pull_requests=read"),
        "{:?}",
        c.hint
    );
}

#[tokio::test]
async fn sso_hint_includes_org_not_url_in_report_file() {
    let s = MockServer::start().await;
    user(&s, Some("repo")).await;
    repos(&s).await;
    Mock::given(method("GET"))
        .and(path("/repos/octo-org/api/pulls"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"message": "Resource protected by organization SAML enforcement."}))
                .insert_header(
                    "x-github-sso",
                    "required; url=https://github.com/orgs/octo-org/sso?authorization_request=SECRETISH",
                ),
        )
        .mount(&s)
        .await;
    let r = run_with(&s, &FakeGit::ok(), &args(Some("octo-org/api"), None)).await;
    let text = r.render(&[TOKEN]);
    assert!(text.contains("the organization octo-org"), "{text}");
    assert!(!text.contains("authorization_request"), "{text}");
    assert!(!text.contains("https://github.com/orgs"), "{text}");
}

#[tokio::test]
async fn a_pr_is_read_with_its_comments_and_write_test_is_skipped() {
    let s = MockServer::start().await;
    user(&s, Some("repo")).await;
    repos(&s).await;
    Mock::given(method("GET"))
        .and(path("/repos/octo-org/api/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 7, "title": "Fix", "state": "open",
            "user": {"login": "hubot", "id": 2},
            "head": {"ref": "f", "sha": "a"}, "base": {"ref": "main", "sha": "b"},
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        })))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo-org/api/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviewThreads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"repository":
            {"pullRequest": {"reviewThreads": {"pageInfo": {"hasNextPage": false}, "nodes": []}}}}})))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviews("))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"repository":
            {"pullRequest": {"reviews": {"pageInfo": {"hasNextPage": false}, "nodes": []}}}}})),
        )
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octo-org/api/issues/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "body": "hi", "user": {"login": "hubot", "id": 2},
             "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"}
        ])))
        .mount(&s)
        .await;
    let mut a = args(Some("octo-org/api"), Some(7));
    a.write_test = true;
    let r = run_with(&s, &FakeGit::ok(), &a).await;
    assert_eq!(r.checks[&5].status, Status::Pass, "{}", r.checks[&5].detail);
    assert!(r.checks[&5].detail.contains("1 comment(s)"));
    assert_eq!(r.write[0].status, Status::Skip);
    assert!(r.write[0].detail.contains("not available"));
    // nothing but GETs and GraphQL reads were sent
    for req in s.received_requests().await.unwrap() {
        let read = req.method.as_str() == "GET" || req.url.path() == "/graphql";
        assert!(read, "{} {}", req.method, req.url);
    }
}

#[tokio::test]
async fn a_refused_token_skips_the_rest_and_git_auth_failure_has_a_hint() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})),
        )
        .mount(&s)
        .await;
    let git = FakeGit {
        ok: false,
        error: "fatal: Authentication failed for 'https://github.com/octo-org/api.git/'".into(),
        urls: RefCell::new(Vec::new()),
    };
    let r = run_with(&s, &git, &args(Some("octo-org/api"), None)).await;
    assert_eq!(r.checks[&2].status, Status::Fail);
    assert_eq!(r.checks[&3].status, Status::Skip);
    assert_eq!(r.checks[&7].status, Status::Fail);
    assert!(r.checks[&7]
        .hint
        .as_deref()
        .unwrap()
        .contains("Contents: read"));
}

#[test]
fn the_env_account_needs_the_token_and_a_matching_name() {
    let env = |k: &str| match k {
        ENV_GITHUB_TOKEN => Some("t".to_string()),
        ENV_GITHUB_URL => Some("https://ghe.corp.example".to_string()),
        _ => None,
    };
    let a = env_account(Some("ghe.corp.example"), env).unwrap();
    assert_eq!(a.api_base, "https://ghe.corp.example/api/v3");
    assert!(env_account(Some("github"), env).is_some());
    assert!(env_account(Some("bitbucket.org"), env).is_none());
    assert!(env_account(Some("github.com"), |_| None).is_none());
    let plain = |k: &str| (k == ENV_GITHUB_TOKEN).then(|| "t".to_string());
    assert_eq!(
        env_account(Some("github.com"), plain).unwrap().git_host,
        "github.com"
    );
}

#[test]
fn the_token_comes_from_the_account_else_the_environment() {
    let secrets = store::MemorySecrets::new();
    let a = env_account(Some("github"), |k| {
        (k == ENV_GITHUB_TOKEN).then(|| "x".into())
    })
    .unwrap();
    let none = |_: &str| None;
    assert!(load_token(&secrets, &a, none)
        .unwrap_err()
        .contains(ENV_GITHUB_TOKEN));
    secrets.set(&token_key(&a.id), "stored").unwrap();
    assert_eq!(
        load_token(&secrets, &a, none).unwrap(),
        ("stored".into(), Source::Stored)
    );
}

#[test]
fn sso_org_is_read_from_the_path_only() {
    assert_eq!(
        sso_org(Some(
            "https://github.com/orgs/octo-org/sso?authorization_request=x"
        ))
        .as_deref(),
        Some("octo-org")
    );
    assert_eq!(sso_org(Some("https://github.com/sso")), None);
    assert_eq!(sso_org(None), None);
}
