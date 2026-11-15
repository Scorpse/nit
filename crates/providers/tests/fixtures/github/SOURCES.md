# github fixtures: where each one comes from

Fixtures were built from the forge's official API documentation unless a row says it was recorded from a live server (tokens and personal data removed). Every fixture in this folder must have a row here (checked by `fixture_hygiene`).

| Fixture | Source | Status | Notes |
|---|---|---|---|
| `user.json` | REST "Get the authenticated user" example ([users](https://docs.github.com/en/rest/users/users?apiVersion=2022-11-28#get-the-authenticated-user)) | from docs, not live-verified | `/user` as token check: documented endpoint for the token's user |
| `user.headers.json` | `X-OAuth-Scopes` ([scopes](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/scopes-for-oauth-apps)), `x-ratelimit-*` ([rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api)) | from docs, not live-verified | classic scope header name |
| `user_repos_page1.json` | REST "List repositories for the authenticated user" example ([repos](https://docs.github.com/en/rest/repos/repos?apiVersion=2022-11-28#list-repositories-for-the-authenticated-user)), trimmed | from docs, not live-verified | `/user/repos` shape (parameters `affiliation`, `per_page` still UNVERIFIED live) |
| `user_repos_page1.headers.json` | `link` header format ([pagination](https://docs.github.com/en/rest/using-the-rest-api/using-pagination-in-the-rest-api)); `{base}` is replaced by the test server | from docs, not live-verified | |
| `user_repos_page2.json` | as page 1 | from docs, not live-verified | |
| `user_teams.json` | REST "List teams for the authenticated user" example ([teams](https://docs.github.com/en/rest/teams/teams?apiVersion=2022-11-28#list-teams-for-the-authenticated-user)), trimmed | from docs, not live-verified | team review requests (needs `read:org` for classic tokens: UNVERIFIED for fine-grained) |
| `pulls_open.json` | REST "List pull requests" example ([pulls](https://docs.github.com/en/rest/pulls/pulls?apiVersion=2022-11-28#list-pull-requests)), trimmed | from docs, not live-verified | |
| `pulls_open.headers.json` | `etag` per [best practices](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api) (conditional requests) | from docs, not live-verified | whether the list ETag changes on a new review: UNVERIFIED |
| `pull_detail.json` | REST "Get a pull request" example, trimmed (`additions`, `deletions`, `changed_files` only on the full schema) | from docs, not live-verified | |
| `pulls_closed_head.json` | REST list shape with `merged_at` set / null; third entry is a deliberately broken entry (synthetic) for the skip test | from docs + synthetic, not live-verified | `head=owner:branch` filter format is documented on the same page |
| `graphql_prs.json` | composed from the GraphQL schema reference ([objects: PullRequest](https://docs.github.com/en/graphql/reference/objects#pullrequest)); the last node is deliberately broken (synthetic) | from schema, not live-verified | field names `additions`, `deletions`, `changedFiles`, `reviewRequests`, `latestReviews`, `isCrossRepository`, `headRepository`: UNVERIFIED live |
| `graphql_review_threads.json` | composed from [PullRequestReviewThread](https://docs.github.com/en/graphql/reference/objects#pullrequestreviewthread) and [PullRequestReviewComment](https://docs.github.com/en/graphql/reference/objects#pullrequestreviewcomment) | from schema, not live-verified | `isOutdated`, `line` null on outdated threads (assumed), `originalLine`, `startDiffSide`, `subjectType`, `viewerCanResolve` |
| `graphql_thread_comments_page2.json` | `node(id:)` query on a thread, same schema | from schema, not live-verified | nested comment paging |
| `graphql_reviews.json` | composed from [PullRequestReview](https://docs.github.com/en/graphql/reference/objects#pullrequestreview) | from schema, not live-verified | |
| `issue_comments.json` | REST "List issue comments" example ([issue comments](https://docs.github.com/en/rest/issues/comments?apiVersion=2022-11-28#list-issue-comments)); second entry a bot (`type: Bot`, `[bot]` login) | from docs, not live-verified | bot detection by `user.type` on REST |
| `rate_limit_403.json` | primary limit body and headers ([rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api#exceeding-the-rate-limit)) | from docs, not live-verified | |
| `rate_limit_403.headers.json` | `x-ratelimit-remaining: 0`, `x-ratelimit-reset` (`{reset}` replaced by the test) | from docs, not live-verified | |
| `secondary_rate_limit_403.json` | secondary limit body (same page) | from docs, not live-verified | exact message text: UNVERIFIED (nit does not match on it) |
| `secondary_rate_limit_403.headers.json` | `retry-after` (`{retry_after}` replaced by the test) | from docs, not live-verified | |
| `sso_403.json` | SAML enforcement body ([REST auth](https://docs.github.com/en/rest/authentication/authenticating-to-the-rest-api)) | from docs, not live-verified | message text: UNVERIFIED (nit keys on the header only) |
| `sso_403.headers.json` | `X-GitHub-SSO: required; url=...` (same page) | from docs, not live-verified | header format |
| `fine_grained_403.json` | fine-grained PAT refusal ([permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-fine-grained-personal-access-tokens)) | from docs, not live-verified | message text: UNVERIFIED |
| `fine_grained_403.headers.json` | `X-Accepted-GitHub-Permissions` (same page) | from docs, not live-verified | header format |
| `graphql_errors.json` | GraphQL `errors` array with `type: NOT_FOUND` ([forming calls](https://docs.github.com/en/graphql/guides/forming-calls-with-graphql)) | from docs, not live-verified | GraphQL error mapping per `type`: only `NOT_FOUND`, `FORBIDDEN`, `RATE_LIMITED` are mapped, the rest is `Decode` |
