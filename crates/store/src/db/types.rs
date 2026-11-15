use super::*;

/// One repository of the discovery cache and whether the user chose to sync it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoChoice {
    pub repo: RepoRef,
    pub name: Option<String>,
    pub private: Option<bool>,
    pub selected: bool,
    /// When a discovery last listed it (UTC, `YYYY-MM-DD HH:MM:SS`); `None` before any did.
    pub discovered_at: Option<String>,
    /// A later discovery no longer listed it; kept (and no longer synced) only while selected.
    pub gone: bool,
    /// The account that listed it (and syncs it when selected).
    pub account: AccountId,
}

/// One signed-in account: where it lives and who it is. Holds no secret (tokens live in the
/// credential store under keys derived from `id`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: AccountId,
    pub forge: Forge,
    /// `https://host[/prefix]`, no trailing slash.
    pub web_base: String,
    pub api_base: String,
    pub graphql_base: Option<String>,
    /// The only host git credentials of this account are sent to.
    pub git_host: String,
    pub user_id: String,
    pub login: String,
    pub display_name: String,
    /// How the account signs in ("api_token" for Bitbucket, "pat" later).
    pub auth_kind: String,
    pub enabled: bool,
}

impl Account {
    /// The single Bitbucket Cloud sign-in every pre-multi-forge cache belongs to.
    pub fn default_bitbucket(user_id: String, display_name: String) -> Account {
        Account {
            id: AccountId::default_bitbucket(),
            forge: Forge::BitbucketCloud,
            web_base: "https://bitbucket.org".into(),
            api_base: "https://api.bitbucket.org/2.0".into(),
            graphql_base: None,
            git_host: BITBUCKET_HOST.into(),
            user_id,
            login: String::new(),
            display_name,
            auth_kind: "api_token".into(),
            enabled: true,
        }
    }
}

/// What `Store::remove_account` deleted, and the drafts it kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemovedCounts {
    pub prs: usize,
    pub diffstats: usize,
    pub repo_choices: usize,
    pub repo_errors: usize,
    pub cursors: usize,
    /// Drafts of the account stay (shown as "account removed", never posted).
    pub drafts_kept: usize,
}

/// A sync position of one repository: the list request's ETag and the newest `updated_at`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncCursorRow {
    pub etag: Option<String>,
    pub last_updated: Option<String>,
}

/// User-chosen folders. `download_dir` falls back to a caller-supplied default when unset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub clone_root: Option<PathBuf>,
    pub download_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored data could not be serialized or deserialized")]
    Json(#[from] serde_json::Error),
    #[error("database schema version {found} is newer than supported version {supported}")]
    NewerSchema { found: i64, supported: i64 },
    #[error("stored data is malformed")]
    Corrupt,
    #[error("record not found")]
    NotFound,
    #[error(
        "The cache folder does not support WAL mode (network drive?); the cache uses a slower \
         rollback journal instead"
    )]
    WalUnavailable,
}

pub type StoreResult<T> = Result<T, StoreError>;

/// A diffstat as the sync stores it: PR id, source commit it was computed for, the stat.
pub type DiffstatRow = (u64, String, DiffStat);

/// What the inbox needs, read in one read transaction (one joined query for PRs and their
/// diffstats).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CacheSnapshot {
    /// Every stored PR of a selected repository (ordered by repo and id) with its diffstat and
    /// the commit it is for. PRs of unselected repositories stay cached but are left out.
    pub prs: Vec<(PullRequest, Option<(String, DiffStat)>)>,
    /// Errors of selected repositories and of workspace listings (slug `*`).
    pub errors: Vec<(RepoRef, String)>,
    /// Selected repositories that are still accessible (what a sync covers).
    pub repo_count: usize,
    /// Repositories in the discovery cache that are still accessible.
    pub known_repos: usize,
    /// `META_REPOS_PRESELECTED`: repositories selected by the upgrade, until dismissed.
    pub preselected: Option<usize>,
    /// The user the default account was last signed in as (`adopt_user`); `None` before the first
    /// sign-in that recorded one.
    pub account_id: Option<String>,
    /// Every account's own user id (accounts that recorded one), for per-account "Mine"
    /// and "Needs me".
    pub account_users: Vec<(AccountId, String)>,
}
