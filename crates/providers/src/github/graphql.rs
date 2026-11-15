//! GraphQL documents, kept next to their decoders and pinned by the fixtures under
//! `tests/fixtures/github/` (field names from the schema reference: UNVERIFIED live, see
//! `tests/fixtures/github/SOURCES.md`). Every connection asks for at most 100 nodes.

/// Open PRs of one repository with sizes and review state (spec 8.1).
pub const OPEN_PRS: &str = r#"query($owner: String!, $name: String!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: 50, after: $cursor, orderBy: {field: UPDATED_AT, direction: DESC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        id number title url isDraft locked isCrossRepository createdAt updatedAt
        additions deletions changedFiles
        headRefName headRefOid baseRefName baseRefOid
        headRepository { nameWithOwner }
        author { __typename login ... on User { databaseId name } ... on Bot { databaseId } }
        comments { totalCount }
        reviewThreads { totalCount }
        reviewRequests(first: 20) {
          nodes { requestedReviewer {
            __typename
            ... on User { login databaseId name }
            ... on Bot { login databaseId }
            ... on Mannequin { login databaseId }
            ... on Team { slug name organization { login } }
          } }
        }
        latestReviews(first: 20) {
          nodes { state submittedAt commit { oid }
            author { __typename login ... on User { databaseId name } ... on Bot { databaseId } } }
        }
      }
    }
  }
}"#;

const COMMENT_FIELDS: &str = "id databaseId body createdAt updatedAt outdated \
     commit { oid } originalCommit { oid } \
     author { __typename login ... on User { databaseId name } ... on Bot { databaseId } }";

/// Review threads of one PR with their first 100 comments each (spec 7.4).
pub fn review_threads() -> String {
    format!(
        r#"query($owner: String!, $name: String!, $number: Int!, $cursor: String) {{
  repository(owner: $owner, name: $name) {{
    pullRequest(number: $number) {{
      reviewThreads(first: 50, after: $cursor) {{
        pageInfo {{ hasNextPage endCursor }}
        nodes {{
          id isResolved isOutdated viewerCanResolve viewerCanUnresolve
          path line originalLine startLine originalStartLine diffSide startDiffSide subjectType
          comments(first: 100) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ {COMMENT_FIELDS} }} }}
        }}
      }}
    }}
  }}
}}"#
    )
}

/// The next page of one thread's comments.
pub fn thread_comments() -> String {
    format!(
        r#"query($id: ID!, $cursor: String) {{
  node(id: $id) {{
    ... on PullRequestReviewThread {{
      comments(first: 100, after: $cursor) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ {COMMENT_FIELDS} }} }}
    }}
  }}
}}"#
    )
}

/// Submitted reviews of one PR (their summary bodies are shown as PR-level comments).
pub const REVIEWS: &str = r#"query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviews(first: 100, after: $cursor, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) {
        pageInfo { hasNextPage endCursor }
        nodes { id databaseId state body submittedAt updatedAt
          author { __typename login ... on User { databaseId name } ... on Bot { databaseId } } }
      }
    }
  }
}"#;

/// Resolves a review thread by its node id ([mutations]; a write: sent through
/// `GhClient::graphql_mutation`).
///
/// [mutations]: https://docs.github.com/en/graphql/reference/mutations#resolvereviewthread
pub const RESOLVE_THREAD: &str = "mutation($threadId: ID!) { resolveReviewThread(input: {threadId: $threadId}) { thread { id isResolved } } }";

/// Unresolves a review thread by its node id.
pub const UNRESOLVE_THREAD: &str = "mutation($threadId: ID!) { unresolveReviewThread(input: {threadId: $threadId}) { thread { id isResolved } } }";
