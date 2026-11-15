use super::*;

pub(super) async fn write_test(
    r: &mut Report,
    provider: &BitbucketProvider,
    client: &BbClient,
    repo: &RepoRef,
    id: u64,
) {
    r.write_target = Some(format!("{} PR #{id}", repo.full_name()));
    let skip = |r: &mut Report, from: usize| {
        let steps = [
            (2, "Read the comment back"),
            (3, "Delete the comment"),
            (4, "Check it is gone"),
        ];
        for (n, t) in steps.into_iter().filter(|(n, _)| *n >= from) {
            r.step(
                n,
                t,
                Status::Skip,
                "not attempted (an earlier step failed)".into(),
                None,
            );
        }
    };
    let new = NewComment::Pr {
        text: WRITE_TEST_TEXT.into(),
    };
    // Bitbucket anchors to the current diff: no revision is pinned
    let at = providers::provider::PostRevision::default();
    let posted = match provider.post_comment(repo, id, &at, &new).await {
        Ok(c) => {
            r.step(
                1,
                "Post a PR comment",
                Status::Pass,
                format!("posted comment #{}", c.id),
                None,
            );
            c
        }
        Err(e) => {
            let hint = match &e {
                ProviderError::Unconfirmed | ProviderError::PostedUnreadable => Some(
                    "the comment may exist: look at the PR in Bitbucket and delete the test \
                     comment by hand"
                        .into(),
                ),
                e => api_hint(
                    e,
                    "Bitbucket write pull request access (pull request: write)",
                ),
            };
            r.step(
                1,
                "Post a PR comment",
                Status::Fail,
                format!("could not post: {}", e.user_message()),
                hint,
            );
            return skip(r, 2);
        }
    };
    match provider.comments(repo, id).await {
        Ok(list) => match list.comments.iter().find(|c| c.id == posted.id) {
            Some(c) if c.body.trim() == WRITE_TEST_TEXT => r.step(
                2,
                "Read the comment back",
                Status::Pass,
                format!("comment #{} is listed with the posted text", c.id),
                None,
            ),
            Some(c) => r.step(
                2,
                "Read the comment back",
                Status::Fail,
                format!("comment #{} is listed but its text differs", c.id),
                None,
            ),
            None => r.step(
                2,
                "Read the comment back",
                Status::Fail,
                format!("comment #{} is not in the PR's comments", posted.id),
                None,
            ),
        },
        Err(e) => r.step(
            2,
            "Read the comment back",
            Status::Fail,
            format!("could not read the comments: {}", e.user_message()),
            api_hint(&e, SCOPE_READ_PR),
        ),
    }
    let path = format!("{}/comments/{}", pr_path(repo, id), posted.id);
    if let Err(e) = client.delete_checked(&path).await {
        r.step(
            3,
            "Delete the comment",
            Status::Fail,
            format!("could not delete: {}", e.user_message()),
            Some(format!(
                "delete the test comment #{} on {} PR #{id} by hand in Bitbucket",
                posted.id,
                repo.full_name()
            )),
        );
        return skip(r, 4);
    }
    r.step(
        3,
        "Delete the comment",
        Status::Pass,
        format!("deleted comment #{}", posted.id),
        None,
    );
    match provider.comments(repo, id).await {
        Ok(list) => {
            let left = list
                .comments
                .iter()
                .any(|c| c.id == posted.id && !c.deleted);
            if left {
                r.step(
                    4,
                    "Check it is gone",
                    Status::Fail,
                    format!("comment #{} is still listed", posted.id),
                    Some(format!(
                        "delete the test comment #{} by hand in Bitbucket",
                        posted.id
                    )),
                );
            } else {
                r.step(
                    4,
                    "Check it is gone",
                    Status::Pass,
                    "no longer listed (or marked deleted)".into(),
                    None,
                );
            }
        }
        Err(e) => r.step(
            4,
            "Check it is gone",
            Status::Fail,
            format!("could not read the comments: {}", e.user_message()),
            None,
        ),
    }
}
