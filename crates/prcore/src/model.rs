use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const BB_MAX_FILES: u32 = 200;
pub const BB_MAX_TOTAL_LINES: u32 = 8000;
pub const BB_MAX_FILE_LINES: u32 = 2000;

/// Git and web host of Bitbucket Cloud; the host every pre-multi-forge `RepoRef` had.
pub const BITBUCKET_HOST: &str = "bitbucket.org";

/// The account every pre-multi-forge (single Bitbucket sign-in) row belongs to.
pub const DEFAULT_BITBUCKET_ACCOUNT: &str = "acc-bitbucket-default";

/// Which kind of server hosts a repository.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Forge {
    #[default]
    #[serde(rename = "bitbucket")]
    BitbucketCloud,
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "gitlab")]
    GitLab,
}

impl Forge {
    pub fn as_str(self) -> &'static str {
        match self {
            Forge::BitbucketCloud => "bitbucket",
            Forge::GitHub => "github",
            Forge::GitLab => "gitlab",
        }
    }

    pub fn parse(s: &str) -> Option<Forge> {
        [Forge::BitbucketCloud, Forge::GitHub, Forge::GitLab]
            .into_iter()
            .find(|f| f.as_str() == s)
    }

    /// The product name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Forge::BitbucketCloud => "Bitbucket Cloud",
            Forge::GitHub => "GitHub",
            Forge::GitLab => "GitLab",
        }
    }

    /// What the forge calls a change request in its UI.
    pub fn change_noun(self) -> &'static str {
        match self {
            Forge::GitLab => "MR",
            Forge::BitbucketCloud | Forge::GitHub => "PR",
        }
    }

    /// The change request spelled out, lower-case ("pull request", GitLab "merge request").
    pub fn change_name(self) -> &'static str {
        match self {
            Forge::GitLab => "merge request",
            Forge::BitbucketCloud | Forge::GitHub => "pull request",
        }
    }

    /// How the forge writes a change request's number ("#12", GitLab "!12").
    pub fn change_ref(self, id: u64) -> String {
        match self {
            Forge::GitLab => format!("!{id}"),
            Forge::BitbucketCloud | Forge::GitHub => format!("#{id}"),
        }
    }
}

/// Stable, local, non-secret id of one signed-in account.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId(pub String);

impl AccountId {
    pub fn default_bitbucket() -> AccountId {
        AccountId(DEFAULT_BITBUCKET_ACCOUNT.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for AccountId {
    /// Rows written before accounts existed belong to the single Bitbucket sign-in.
    fn default() -> Self {
        AccountId::default_bitbucket()
    }
}

fn bitbucket_host() -> String {
    BITBUCKET_HOST.into()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    /// Lower-case host plus optional path prefix ("bitbucket.org", "git.corp.example/gitlab").
    /// JSON cached before multi-forge has no host: it is Bitbucket's.
    #[serde(default = "bitbucket_host")]
    pub host: String,
    /// Bitbucket workspace, GitHub owner, GitLab namespace path (may contain '/').
    #[serde(alias = "workspace")]
    pub namespace: String,
    pub slug: String,
}

impl RepoRef {
    /// A Bitbucket Cloud repository.
    pub fn bitbucket(workspace: &str, slug: &str) -> RepoRef {
        RepoRef {
            host: BITBUCKET_HOST.into(),
            namespace: workspace.into(),
            slug: slug.into(),
        }
    }

    /// `namespace/slug`, for display and for the forge's own API paths.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.namespace, self.slug)
    }

    /// `host/namespace/slug`, lower-case: the store key and the clone-matching key
    /// (same shape as `gitlocal::normalize_remote`).
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.host, self.namespace, self.slug).to_lowercase()
    }

    /// Parses `namespace/slug` on `host`. Splits on the LAST '/', so nested GitLab groups
    /// round-trip. The host never comes from the string (ambiguous with a path prefix).
    pub fn parse(host: &str, full_name: &str) -> Option<RepoRef> {
        let (namespace, slug) = full_name.rsplit_once('/')?;
        let bad = |seg: &str| {
            seg.is_empty() || seg == "." || seg == ".." || seg.contains(['?', '#', '\\'])
        };
        if bad(slug) || namespace.split('/').any(bad) {
            return None;
        }
        Some(RepoRef {
            host: host.into(),
            namespace: namespace.into(),
            slug: slug.into(),
        })
    }

    /// `parse` on Bitbucket Cloud, which has no nested workspaces: `a/b/c` is refused
    /// (the pre-multi-forge rule).
    pub fn parse_bitbucket(full_name: &str) -> Option<RepoRef> {
        RepoRef::parse(BITBUCKET_HOST, full_name).filter(|r| !r.namespace.contains('/'))
    }
}

/// A repository as a discovery lists it: its name and visibility when the provider said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    pub repo: RepoRef,
    pub name: Option<String>,
    pub private: Option<bool>,
}

impl From<RepoRef> for RepoInfo {
    fn from(repo: RepoRef) -> Self {
        RepoInfo {
            repo,
            name: None,
            private: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserRef {
    /// Opaque per forge: Bitbucket uuid, GitHub/GitLab numeric user id as a decimal string.
    /// Only ever compared within one account.
    #[serde(alias = "uuid")]
    pub id: String,
    pub display_name: String,
    /// The forge's user name where it has one (GitHub/GitLab); None on Bitbucket.
    #[serde(default)]
    pub login: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrState {
    Open,
    Merged,
    Declined,
    Superseded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    /// Reviewed without a verdict (GitHub `COMMENTED`, GitLab `reviewed`).
    Commented,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviewer {
    pub user: UserRef,
    pub state: ReviewState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub repo: RepoRef,
    pub id: u64,
    pub title: String,
    pub state: PrState,
    pub draft: bool,
    pub author: UserRef,
    pub source_branch: String,
    pub dest_branch: String,
    pub source_commit: String,
    pub dest_commit: String,
    pub source_repo: RepoRef,
    pub comment_count: u32,
    pub task_count: u32,
    pub reviewers: Vec<Reviewer>,
    pub created_on: DateTime<Utc>,
    pub updated_on: DateTime<Utc>,
    /// The fields below are absent from PR JSON cached before multi-forge: the serde
    /// defaults make such rows Bitbucket rows of the default account.
    #[serde(default)]
    pub forge: Forge,
    #[serde(default)]
    pub account: AccountId,
    /// Opened in the browser on a click, never fetched. Empty when unknown.
    #[serde(default)]
    pub web_url: String,
    /// GitHub GraphQL node id; GitLab project id. None on Bitbucket.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Set by providers whose forge drops a reviewer from the request list after a review.
    #[serde(default)]
    pub review_requested_from_me: bool,
    /// GitLab `locked`; display only.
    #[serde(default)]
    pub locked: bool,
}

impl PullRequest {
    pub fn is_fork(&self) -> bool {
        self.source_repo != self.repo
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStat {
    pub files: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
    pub max_file_lines: u32,
}

impl DiffStat {
    pub fn total_lines(&self) -> u32 {
        self.lines_added + self.lines_removed
    }

    /// True when the forge's web UI cannot show this diff in full (`web_ui_limits`).
    pub fn exceeds_web_ui(&self, forge: Forge) -> bool {
        let l = web_ui_limits(forge);
        over(self.files, l.max_files)
            || over(self.total_lines(), l.max_total_lines)
            || over(self.max_file_lines, l.max_file_lines)
    }
}

/// What a forge's web UI (or API, where that is the binding cap) can show of one diff.
/// None: no known limit of that kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebUiLimits {
    pub max_files: Option<u32>,
    pub max_total_lines: Option<u32>,
    pub max_file_lines: Option<u32>,
}

/// Bitbucket: today's measured limits. GitHub: the documented web diff caps (300 files,
/// 20,000 lines; the API lists at most 3000 files). GitLab: the documented instance defaults (1000 files, 50,000 lines; its
/// per-file limit is in bytes, not lines, so it is not modelled).
pub fn web_ui_limits(forge: Forge) -> WebUiLimits {
    match forge {
        Forge::BitbucketCloud => WebUiLimits {
            max_files: Some(BB_MAX_FILES),
            max_total_lines: Some(BB_MAX_TOTAL_LINES),
            max_file_lines: Some(BB_MAX_FILE_LINES),
        },
        Forge::GitHub => WebUiLimits {
            max_files: Some(300),
            max_total_lines: Some(20_000),
            max_file_lines: None,
        },
        Forge::GitLab => WebUiLimits {
            max_files: Some(1000),
            max_total_lines: Some(50_000),
            max_file_lines: None,
        },
    }
}

fn over(value: u32, max: Option<u32>) -> bool {
    max.is_some_and(|m| value > m)
}

#[cfg(test)]
mod tests {
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
        let again: PullRequest =
            serde_json::from_str(&serde_json::to_string(&pr).unwrap()).unwrap();
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
}
