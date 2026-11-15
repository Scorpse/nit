//! `nit --check-token [--account <id|host>] [--repo <workspace/repo>] [--pr <id>]`: a
//! read-only self-test of one account's credentials. Without `--account` it tests the
//! signed-in default Bitbucket account (stored credentials, else `NIT_EMAIL` / `NIT_TOKEN`;
//! the old names `PRVIEW_EMAIL` / `PRVIEW_TOKEN` still work, deprecated); `--account` picks
//! another account by its id or its host. The checks run per forge: Bitbucket Cloud here,
//! GitHub in `github` (read-only checks; `NIT_GITHUB_TOKEN` / `NIT_GITHUB_URL` stand in for an
//! account not added yet, with `--account github` or its host), GitLab in `gitlab` (read-only
//! checks incl. token scopes and expiry; `NIT_GITLAB_TOKEN` / `NIT_GITLAB_URL` with
//! `--account gitlab` or its host). Like the app, it first moves
//! a sign-in saved under the old name `prview` to `nit` (see `brand`).
//!
//! Every check ends PASS, FAIL or SKIP with a short reason and, on a failure, a hint. The
//! report goes to the parent console (when there is one) and always to
//! `token-check.txt` in the data folder (`brand::data_dir`; Windows `%LOCALAPPDATA%\nit`).
//! The token never appears in the report: it is
//! never formatted into it, and the rendered text is scrubbed of it once more.
//!
//! `--write-test --repo <w/r> --pr <id> --i-understand-this-posts` additionally posts one
//! PR-level comment through the provider's comment path, reads it back and deletes it.

mod args;
mod bitbucket;
mod creds;
mod github;
pub mod gitlab;
mod gitside;
mod report;
mod runner;
mod writetest;

#[cfg(test)]
mod tests;

pub use args::*;
pub use bitbucket::*;
pub use creds::*;
pub use gitside::*;
pub use report::*;
pub use runner::*;
use writetest::*;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use gitlocal::{GitAuth, BITBUCKET_GIT_USER as GIT_TOKEN_USER};
use prcore::comments::NewComment;
use prcore::model::{AccountId, Forge, PrState, RepoRef, BITBUCKET_HOST};
use providers::bitbucket::BitbucketProvider;
use providers::{BbClient, Provider, ProviderError, CERT_UNTRUSTED};
use serde_json::Value;
use store::{
    email_key, token_key, Account, SecretStore, CREDENTIAL_STORE, LEGACY_BB_EMAIL, LEGACY_BB_TOKEN,
};

pub const ENV_EMAIL: &str = "NIT_EMAIL";
pub const ENV_TOKEN: &str = "NIT_TOKEN";
/// Deprecated names from before the rename, still read when the new ones are unset.
pub const LEGACY_ENV_EMAIL: &str = "PRVIEW_EMAIL";
pub const LEGACY_ENV_TOKEN: &str = "PRVIEW_TOKEN";
const REPORT_FILE: &str = "token-check.txt";
/// At most this many repositories are asked for open PRs when no `--repo` is given.
const MAX_REPOS_SCANNED: usize = 25;
const WRITE_TEST_TEXT: &str = "nit token self-test: this comment was posted by \
    `nit --check-token --write-test` and is deleted again right away.";

const FLAG: &str = "--check-token";
const WRITE_FLAG: &str = "--write-test";
const CONSENT_FLAG: &str = "--i-understand-this-posts";
const ACCOUNT_FLAG: &str = "--account";
