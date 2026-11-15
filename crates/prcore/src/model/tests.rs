use super::*;

fn stat(files: u32, added: u32, removed: u32, max: u32) -> DiffStat {
    DiffStat {
        files,
        lines_added: added,
        lines_removed: removed,
        max_file_lines: max,
    }
}

fn repo(ws: &str, slug: &str) -> RepoRef {
    RepoRef::bitbucket(ws, slug)
}

const BB: Forge = Forge::BitbucketCloud;

#[test]
fn exceeds_ui_when_total_lines_over_8000() {
    assert!(stat(10, 5000, 3001, 100).exceeds_web_ui(BB));
}

#[test]
fn exact_8000_total_does_not_exceed() {
    assert!(!stat(10, 5000, 3000, 100).exceeds_web_ui(BB));
}

#[test]
fn exceeds_ui_when_files_over_200() {
    assert!(stat(201, 10, 10, 10).exceeds_web_ui(BB));
}

#[test]
fn exact_200_files_does_not_exceed() {
    assert!(!stat(200, 10, 10, 10).exceeds_web_ui(BB));
}

#[test]
fn exceeds_ui_when_one_file_over_2000() {
    assert!(stat(1, 10, 10, 2001).exceeds_web_ui(BB));
}

#[test]
fn within_limits_is_false() {
    assert!(!stat(5, 100, 50, 80).exceeds_web_ui(BB));
}

#[test]
fn github_limits_are_the_web_diff_caps() {
    let gh = Forge::GitHub;
    assert!(!stat(300, 10_000, 10_000, 90_000).exceeds_web_ui(gh));
    assert!(stat(301, 1, 1, 1).exceeds_web_ui(gh));
    assert!(stat(10, 10_000, 10_001, 1).exceeds_web_ui(gh));
}

#[test]
fn gitlab_limits_use_documented_defaults() {
    let gl = Forge::GitLab;
    assert!(!stat(1000, 25_000, 25_000, 40_000).exceeds_web_ui(gl));
    assert!(stat(1001, 1, 1, 1).exceeds_web_ui(gl));
    assert!(stat(10, 25_000, 25_001, 1).exceeds_web_ui(gl));
}

#[test]
fn repo_key_is_lowercase_host_namespace_slug() {
    let r = RepoRef::parse("Git.Corp.Example/gitlab", "Group/Sub/Proj").unwrap();
    assert_eq!(r.key(), "git.corp.example/gitlab/group/sub/proj");
    assert_eq!(r.full_name(), "Group/Sub/Proj");
    assert_eq!(repo("acme", "api").key(), "bitbucket.org/acme/api");
}

#[test]
fn parse_splits_on_last_slash_for_nested_groups() {
    let r = RepoRef::parse("gitlab.com", "a/b/c/proj").unwrap();
    assert_eq!((r.namespace.as_str(), r.slug.as_str()), ("a/b/c", "proj"));
    assert_eq!(r.full_name(), "a/b/c/proj");
    assert_eq!(r.host, "gitlab.com");
}

#[test]
fn parse_rejects_dot_dot_and_query_chars() {
    for bad in [
        "a/../b", "../b", "a/..", "a/b?x", "a/b#x", "a\\b/c", "a//b", "/b", "a/", "ab",
    ] {
        assert!(RepoRef::parse("h", bad).is_none(), "{bad}");
    }
}

#[test]
fn bitbucket_parse_keeps_refusing_nested_names() {
    assert!(RepoRef::parse_bitbucket("a/b/c").is_none());
    assert_eq!(
        RepoRef::parse_bitbucket("acme/api"),
        Some(repo("acme", "api"))
    );
}

#[test]
fn forge_round_trips_as_lower_case_strings() {
    for f in [Forge::BitbucketCloud, Forge::GitHub, Forge::GitLab] {
        assert_eq!(Forge::parse(f.as_str()), Some(f));
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, format!("\"{}\"", f.as_str()));
    }
    assert_eq!(Forge::GitLab.change_noun(), "MR");
    assert_eq!(Forge::GitHub.change_noun(), "PR");
}

#[test]
fn gitlab_says_merge_request_and_bang() {
    assert_eq!(Forge::GitLab.change_name(), "merge request");
    assert_eq!(Forge::GitLab.change_ref(12), "!12");
    assert_eq!(Forge::BitbucketCloud.change_name(), "pull request");
    assert_eq!(Forge::GitHub.change_ref(12), "#12");
}

#[test]
fn pr_json_from_v6_decodes_with_bitbucket_defaults() {
    // A PullRequest exactly as the v6 build serialized it into `prs.json`.
    let old = r#"{"repo":{"workspace":"acme","slug":"api"},"id":7,"title":"t",
        "state":"Open","draft":false,"author":{"uuid":"{u1}","display_name":"Ann"},
        "source_branch":"f","dest_branch":"main","source_commit":"a","dest_commit":"b",
        "source_repo":{"workspace":"acme","slug":"api"},"comment_count":2,"task_count":1,
        "reviewers":[{"user":{"uuid":"{u2}","display_name":"Bo"},"state":"Approved"}],
        "created_on":"2026-01-01T00:00:00Z","updated_on":"2026-01-02T00:00:00Z"}"#;
    let pr: PullRequest = serde_json::from_str(old).unwrap();
    assert_eq!(pr.repo, repo("acme", "api"));
    assert_eq!(pr.repo.key(), "bitbucket.org/acme/api");
    assert!(!pr.is_fork());
    assert_eq!(pr.author.id, "{u1}");
    assert_eq!(pr.author.login, None);
    assert_eq!(pr.reviewers[0].user.id, "{u2}");
    assert_eq!(pr.forge, Forge::BitbucketCloud);
    assert_eq!(pr.account, AccountId::default_bitbucket());
    assert_eq!(pr.web_url, "");
    assert_eq!(pr.node_id, None);
    assert!(!pr.review_requested_from_me && !pr.locked);
    // and the new shape round-trips
    let again: PullRequest = serde_json::from_str(&serde_json::to_string(&pr).unwrap()).unwrap();
    assert_eq!(again, pr);
}

#[test]
fn is_fork_true_when_source_repo_differs() {
    let user = UserRef {
        id: "{u}".into(),
        display_name: "U".into(),
        login: None,
    };
    let now = chrono::Utc::now();
    let mut pr = PullRequest {
        repo: repo("acme", "api"),
        id: 1,
        title: "t".into(),
        state: PrState::Open,
        draft: false,
        author: user,
        source_branch: "f".into(),
        dest_branch: "main".into(),
        source_commit: "a".into(),
        dest_commit: "b".into(),
        source_repo: repo("acme", "api"),
        comment_count: 0,
        task_count: 0,
        reviewers: vec![],
        created_on: now,
        updated_on: now,
        forge: Forge::BitbucketCloud,
        account: AccountId::default_bitbucket(),
        web_url: String::new(),
        node_id: None,
        review_requested_from_me: false,
        locked: false,
    };
    assert!(!pr.is_fork());
    pr.source_repo = repo("someone", "api");
    assert!(pr.is_fork());
}

#[test]
fn repo_ref_parse_round_trips() {
    let r = RepoRef::parse_bitbucket("acme/api").unwrap();
    assert_eq!(r, repo("acme", "api"));
    assert_eq!(r.full_name(), "acme/api");
}

#[test]
fn repo_ref_parse_rejects_no_slash() {
    assert!(RepoRef::parse_bitbucket("acmeapi").is_none());
}
