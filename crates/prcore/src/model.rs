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

/// Result of the CI checks (builds, pipelines) on a PR's head commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckState {
    #[default]
    Unknown,
    Passing,
    Failing,
    Running,
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
    /// Build/pipeline result of the head commit; `Unknown` when the forge did not say.
    #[serde(default)]
    pub checks: CheckState,
    /// The forge reports the PR cannot merge cleanly (a conflict with the target).
    #[serde(default)]
    pub has_conflicts: bool,
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
mod tests;
