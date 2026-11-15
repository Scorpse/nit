//! Opt-in live checks of the GitHub provider against a real account (plan Task 12, 14, 15).
//! Never run by default (`#[ignore]`). The token is read from the environment only and never
//! printed; nothing is written to GitHub.
//!
//! ```text
//! set GITHUB_TOKEN=<read-only PAT>
//! set NIT_GITHUB_LIVE_REPO=<owner>/<repo>          (has at least one open PR)
//! set NIT_GITHUB_LIVE_PR=<number>                  (optional: a PR with review threads)
//! set NIT_GITHUB_LIVE_API=https://HOST/api/v3      (optional, GHES; default api.github.com)
//! set NIT_GITHUB_LIVE_GRAPHQL=https://HOST/api/graphql   (optional, with the line above)
//! set NIT_GITHUB_LIVE_HOST=HOST                    (optional, with the lines above)
//! set NIT_GITHUB_RECORD_DIR=<empty folder>         (optional: save raw answers)
//! cargo test -p providers --test github_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Recorded answers have email addresses rewritten to `@example.com` and nothing else
//! changed: read them before turning any into a fixture.

use prcore::model::{PrState, RepoRef};
use providers::github::{graphql, GhClient, GitHubProvider};
use providers::provider::Provider;
use serde_json::{json, Value};

struct Live {
    token: String,
    repo: RepoRef,
    pr: Option<u64>,
    api: String,
    graphql: String,
    host: String,
}

fn live() -> Option<Live> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let (Some(token), Some(repo)) = (env("GITHUB_TOKEN"), env("NIT_GITHUB_LIVE_REPO")) else {
        eprintln!("skipped: set GITHUB_TOKEN and NIT_GITHUB_LIVE_REPO");
        return None;
    };
    let host = env("NIT_GITHUB_LIVE_HOST").unwrap_or_else(|| "github.com".into());
    let api = env("NIT_GITHUB_LIVE_API").unwrap_or_else(|| "https://api.github.com".into());
    let graphql = env("NIT_GITHUB_LIVE_GRAPHQL").unwrap_or_else(|| format!("{api}/graphql"));
    let repo = RepoRef::parse(&host, &repo).expect("NIT_GITHUB_LIVE_REPO is <owner>/<repo>");
    let pr = env("NIT_GITHUB_LIVE_PR").and_then(|p| p.parse().ok());
    Some(Live {
        token,
        repo,
        pr,
        api,
        graphql,
        host,
    })
}

fn provider(l: &Live) -> GitHubProvider {
    GitHubProvider::new(&l.api, &l.graphql, &l.host, &l.token)
}

/// Rewrites every email address to `<local>@example.com`.
fn scrub_emails(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        let (before, after) = rest.split_at(at);
        out.push_str(before);
        let end = after[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
            .map_or(after.len(), |n| n + 1);
        let domain = &after[1..end];
        if domain.contains('.') {
            out.push_str("@example.com");
        } else {
            out.push_str(&after[..end]);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

fn record(name: &str, v: &Value) {
    let Ok(dir) = std::env::var("NIT_GITHUB_RECORD_DIR") else {
        return;
    };
    let text = serde_json::to_string_pretty(v).unwrap();
    let path = std::path::Path::new(&dir).join(format!("{name}.json"));
    std::fs::write(&path, scrub_emails(&text)).unwrap();
    eprintln!("recorded {}", path.display());
}

#[test]
fn email_scrub_keeps_mentions() {
    assert_eq!(
        scrub_emails("a@b.org and @octocat"),
        "a@example.com and @octocat"
    );
}

#[tokio::test]
#[ignore]
async fn live_me_discover_prs_and_comments_decode() {
    let Some(l) = live() else { return };
    let p = provider(&l);
    let me = p.me().await.expect("GET /user");
    eprintln!(
        "me: id present {}, login present {}",
        !me.id.is_empty(),
        me.login.is_some()
    );
    eprintln!(
        "token kind: {}",
        match p.client().oauth_scopes() {
            Some(s) => format!("classic, scopes [{s}]"),
            None => "fine-grained (no X-OAuth-Scopes)".into(),
        }
    );
    let d = p.discover().await.expect("discover");
    eprintln!(
        "discover: {} repos, {} owners",
        d.repos.len(),
        d.namespaces.len()
    );
    let open = p
        .prs(&l.repo, PrState::Open, usize::MAX)
        .await
        .expect("open PRs");
    eprintln!(
        "open PRs: {} decoded, {} skipped {:?}",
        open.prs.len(),
        open.skipped.len(),
        open.skipped
    );
    assert!(open.skipped.is_empty(), "a real PR did not decode");
    // the ETag gate: the same request again should be a 304 and not count
    let before = p.client().request_count();
    let again = p.prs(&l.repo, PrState::Open, usize::MAX).await.unwrap();
    eprintln!(
        "second list: {} counted requests (0 means the ETag gate held)",
        p.client().request_count() - before
    );
    assert_eq!(again, open);
    for pr in open.prs.iter().take(3) {
        let s = p.diffstat(&l.repo, pr.id).await.expect("diffstat");
        eprintln!(
            "#{}: {} files +{} -{}, needs me {}",
            pr.id, s.files, s.lines_added, s.lines_removed, pr.review_requested_from_me
        );
    }
    let id = l.pr.or(open.prs.first().map(|p| p.id));
    if let Some(id) = id {
        let c = p.comments(&l.repo, id).await.expect("comments");
        eprintln!(
            "PR #{id}: {} comments, {} skipped, {} outdated, {} resolved",
            c.comments.len(),
            c.skipped.len(),
            c.comments
                .iter()
                .filter(|c| c.anchor.as_ref().is_some_and(|a| a.outdated))
                .count(),
            c.comments.iter().filter(|c| c.resolved).count()
        );
        assert!(c.skipped.is_empty());
    }
    eprintln!(
        "primary-limit requests used: {}",
        p.client().request_count()
    );
}

/// Saves the raw answers the fixtures imitate (only with NIT_GITHUB_RECORD_DIR).
#[tokio::test]
#[ignore]
async fn live_record_raw_answers() {
    let Some(l) = live() else { return };
    if std::env::var("NIT_GITHUB_RECORD_DIR").is_err() {
        eprintln!("skipped: set NIT_GITHUB_RECORD_DIR");
        return;
    }
    let c = GhClient::new(&l.api, &l.graphql, &l.token);
    let (o, r) = (&l.repo.namespace, &l.repo.slug);
    record("user", &c.get_json("/user").await.unwrap());
    record(
        "user_repos_page1",
        &c.get_json("/user/repos?per_page=5").await.unwrap(),
    );
    record(
        "pulls_open",
        &c.get_json(&format!("/repos/{o}/{r}/pulls?state=open&per_page=5"))
            .await
            .unwrap(),
    );
    record(
        "graphql_prs",
        &c.graphql(
            graphql::OPEN_PRS,
            json!({"owner": o, "name": r, "cursor": null}),
        )
        .await
        .unwrap(),
    );
    if let Some(n) = l.pr {
        record(
            "pull_detail",
            &c.get_json(&format!("/repos/{o}/{r}/pulls/{n}"))
                .await
                .unwrap(),
        );
        let vars = json!({"owner": o, "name": r, "number": n, "cursor": null});
        record(
            "graphql_review_threads",
            &c.graphql(&graphql::review_threads(), vars.clone())
                .await
                .unwrap(),
        );
        record(
            "graphql_reviews",
            &c.graphql(graphql::REVIEWS, vars).await.unwrap(),
        );
        record(
            "issue_comments",
            &c.get_json(&format!("/repos/{o}/{r}/issues/{n}/comments"))
                .await
                .unwrap(),
        );
    }
}
