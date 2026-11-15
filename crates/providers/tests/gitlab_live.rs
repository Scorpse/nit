//! Opt-in live checks of the GitLab provider against a real instance (plan Tasks 21, 23,
//! 24). Never run by default (`#[ignore]`). The token is read from the environment only and
//! never printed; nothing is written to GitLab.
//!
//! ```text
//! set NIT_LIVE_GITLAB_URL=https://gitlab.com         (or a self-managed web address)
//! set NIT_LIVE_GITLAB_TOKEN=<PAT with read_api>
//! set NIT_LIVE_GITLAB_PROJECT=<group/sub/project>    (has at least one open MR)
//! set NIT_LIVE_GITLAB_MR=<iid>                       (optional: an MR with discussions)
//! cargo test -p providers --test gitlab_live -- --ignored --nocapture --test-threads=1
//! ```

use prcore::model::{PrState, RepoRef};
use providers::gitlab::{bases_for, GitLabProvider};
use providers::provider::Provider;

struct Live {
    p: GitLabProvider,
    repo: RepoRef,
    mr: Option<u64>,
}

fn live() -> Option<Live> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let (Some(url), Some(token), Some(project)) = (
        env("NIT_LIVE_GITLAB_URL"),
        env("NIT_LIVE_GITLAB_TOKEN"),
        env("NIT_LIVE_GITLAB_PROJECT"),
    ) else {
        eprintln!(
            "skipped: set NIT_LIVE_GITLAB_URL, NIT_LIVE_GITLAB_TOKEN and NIT_LIVE_GITLAB_PROJECT"
        );
        return None;
    };
    let b = bases_for(&url)
        .expect("NIT_LIVE_GITLAB_URL is an https web address (or http on a private one)");
    let repo = RepoRef::parse(&b.git_host, &project)
        .expect("NIT_LIVE_GITLAB_PROJECT is <group>/<project>");
    Some(Live {
        p: GitLabProvider::new(&b.api_base, &b.git_host, &token),
        repo,
        mr: env("NIT_LIVE_GITLAB_MR").and_then(|m| m.parse().ok()),
    })
}

#[tokio::test]
#[ignore]
async fn live_me_token_and_discovery_decode() {
    let Some(l) = live() else { return };
    let me = l.p.me().await.expect("GET /user");
    eprintln!("me: id {} login {:?}", me.id, me.login);
    match l.p.token_info().await {
        Ok(t) => eprintln!(
            "token: scopes {:?}, expires {:?}, active {}",
            t.scopes, t.expires_at, t.active
        ),
        Err(e) => eprintln!("token info failed: {}", e.user_message()),
    }
    let d = l.p.discover().await.expect("GET /projects");
    eprintln!(
        "{} projects in {} namespaces",
        d.repos.len(),
        d.namespaces.len()
    );
    assert!(
        d.repos.iter().any(|r| r.repo.key() == l.repo.key()),
        "project not listed"
    );
}

#[tokio::test]
#[ignore]
async fn live_open_mrs_sizes_and_revision_decode() {
    let Some(l) = live() else { return };
    let list =
        l.p.prs(&l.repo, PrState::Open, usize::MAX)
            .await
            .expect("open MRs");
    eprintln!(
        "{} open MRs, {} skipped {:?}",
        list.prs.len(),
        list.skipped.len(),
        list.skipped
    );
    assert!(list.skipped.is_empty());
    let again =
        l.p.prs(&l.repo, PrState::Open, usize::MAX)
            .await
            .expect("updated_after");
    assert_eq!(again.prs.len(), list.prs.len());
    for pr in list.prs.iter().take(5) {
        let stat = l.p.diffstat(&l.repo, pr.id).await;
        let rev = l.p.revision(&l.repo, pr.id).await.expect("revision");
        eprintln!(
            "!{} needs me {} files {:?} head {} base {:?}",
            pr.id,
            pr.review_requested_from_me,
            stat.map(|s| s.files),
            rev.head,
            rev.base
        );
    }
    eprintln!(
        "requests: {}, rate {:?}",
        l.p.client().request_count(),
        l.p.client().rate_limit()
    );
}

#[tokio::test]
#[ignore]
async fn live_discussions_decode() {
    let Some(l) = live() else { return };
    let Some(mr) = l.mr else {
        eprintln!("skipped: set NIT_LIVE_GITLAB_MR");
        return;
    };
    let c = l.p.comments(&l.repo, mr).await.expect("discussions");
    let outdated = c
        .comments
        .iter()
        .filter(|c| c.anchor.as_ref().is_some_and(|a| a.outdated))
        .count();
    eprintln!(
        "{} notes, {} skipped, {outdated} outdated",
        c.comments.len(),
        c.skipped.len()
    );
    assert!(c.skipped.is_empty(), "{:?}", c.skipped);
}
