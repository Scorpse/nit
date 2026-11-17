//! My last review of a PR, from GitHub's `viewerLatestReview`.

use jiff::Timestamp;
use prcore::history::ReviewMark;
use prcore::model::RepoRef;
use serde_json::{json, Value};

use super::{graphql, GhClient};
use crate::ProviderError;

pub(super) async fn my_last_review(
    client: &GhClient,
    repo: &RepoRef,
    id: u64,
) -> Result<Option<ReviewMark>, ProviderError> {
    let vars = json!({"owner": repo.namespace, "name": repo.slug, "number": id});
    let data = client.graphql(graphql::MY_REVIEW, vars).await?;
    Ok(mark_of(
        &data["repository"]["pullRequest"]["viewerLatestReview"],
    ))
}

/// A submitted review's mark; a pending (unsent) one is no review yet.
fn mark_of(review: &Value) -> Option<ReviewMark> {
    if review["state"].as_str()? == "PENDING" {
        return None;
    }
    let at = review["submittedAt"].as_str()?.parse::<Timestamp>().ok()?;
    let commit = review["commit"]["oid"].as_str().map(str::to_string);
    Some(ReviewMark { at, commit })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_submitted_review_gives_its_time_and_commit() {
        let m = mark_of(
            &json!({"state": "APPROVED", "submittedAt": "2026-11-14T00:02:51Z",
            "commit": {"oid": "7094b61de42571afe2541aba7aed7d9fe2b41846"}}),
        )
        .unwrap();
        assert_eq!(m.at.to_string(), "2026-11-14T00:02:51Z");
        assert_eq!(
            m.commit.as_deref(),
            Some("7094b61de42571afe2541aba7aed7d9fe2b41846")
        );
    }

    #[test]
    fn no_review_or_a_pending_one_gives_nothing() {
        assert_eq!(mark_of(&Value::Null), None);
        assert_eq!(
            mark_of(&json!({"state": "PENDING", "submittedAt": null, "commit": null})),
            None
        );
    }

    #[test]
    fn a_review_without_a_commit_still_has_its_time() {
        let m = mark_of(
            &json!({"state": "COMMENTED", "submittedAt": "2026-11-14T00:02:51Z",
            "commit": null}),
        )
        .unwrap();
        assert_eq!(m.commit, None);
    }
}
