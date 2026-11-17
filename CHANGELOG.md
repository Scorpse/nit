# Changelog

All notable changes to nit are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0, minor versions may contain breaking changes.

## [Unreleased]

## [0.1.3] - 2026-11-17

- Every open pull request in the inbox now shows an "approved" pill once approved (nothing when not), and the detail pane has an Approval row naming who approved or asked for changes.
- Bitbucket: people who approved or requested changes are shown even when they were never added as reviewers; before, their approval was hidden.
- Review screen: a Commits tab lists the pull request's commits, newest first, marking those after your last review as new. Choose a commit, or "All N new, together", to see its files and diff (read only). Your last review is the exact reviewed commit on GitHub; on Bitbucket and GitLab it is the time of your last activity.
- Inbox: a "new commits" pill on pull requests you opened in nit whose head moved since you were last caught up.
- Inbox pills: white text on red for changes requested and conflicts, a darker readable green for approved and checks passed, and the size-limit badge in warning amber.
- Bitbucket: a Tasks tab in the review screen lists the pull request's tasks (open first, with who made them) and adds a new task.

## [0.1.2] - 2026-11-16

- Inbox rows show review and build status: changes requested, conflicts, checks failed, running or passed, open tasks and approvals. The detail pane adds Checks and Merge rows. Bitbucket reads build statuses with one extra request per open pull request; Bitbucket has no conflict flag.

## [0.1.1] - 2026-11-16

- Inline comments on Bitbucket can be posted: the anchoring was checked against a real pull request (added and context lines anchor to the new-file line, deleted lines to the old-file line). `NIT_INLINE_EXPERIMENT` is gone. Bitbucket has no multi-line comments.
- macOS builds are Intel only; Apple silicon runs them under Rosetta 2. Linux and macOS ARM builds are no longer produced.
- `nit --check-token` without `--account` now tests every enabled account (Bitbucket, GitHub, GitLab) and ends with a one-line summary; it used to test only the signed-in Bitbucket account.

## [0.1.0] - 2026-11-15

First public version. Windows, Linux and macOS. Bitbucket Cloud, GitHub and GitLab.

### Added

- Sign-in with an Atlassian email and API token (Bitbucket), a personal access token (GitHub, GitLab); tokens are kept only in the OS credential store.
- Repository selection: nit lists your workspaces and repositories and syncs only the ones you choose.
- Inbox of open pull requests with the views Mine, Needs me, All, Too big and Stacked, size badges from the diffstat, and a flag for PRs over Bitbucket's web-diff limits.
- Stacked PRs: detection of PRs that target another PR's branch, with advice on review order, parent changes, held approvals and merge order.
- Review screen: file tree with Viewed marks, generated and lock files grouped, filters, and a virtualized, syntax-highlighted diff computed locally with git against the merge base.
- Unified and split diff (`Alt+S`), remembered between reviews.
- Vim-style keys (on by default, `?` lists them): row and hunk movement, counts, search, visual selection, commenting.
- Comments as local drafts (inline, file-level, PR-level, replies) that survive restarts and are posted only after confirmation; resolve and unresolve with undo; drafts blocked rather than posted on a guessed line when the PR moved.
- Outdated comments: threads anchored on an older version of the PR are marked outdated.
- Optional comment types (issue, nit, question, suggestion, praise), shown as coloured badges.
- Copy as markdown: the review with every comment anchored `path:line` and a code excerpt, in three scopes.
- Approve and request changes (and their clear forms), re-checked against the PR's current source commit before sending.
- Token self-test: `nit --check-token` checks credentials, scopes, git access, system git and HTTPS without changing anything on Bitbucket.
- `--demo` mode with built-in data, and `--shot` / `--shots` to render screenshots.
- Offline-first cache: the inbox opens from the local cache; Refresh re-syncs on demand.
- GitHub (github.com and Enterprise): inbox, review requests, size badges, comments, replies, resolving threads, approving and requesting changes, inline comments (single and multi-line), fork PRs.
- GitLab (gitlab.com and self-managed): inbox, merge request discussions and review, and writing (comments, inline comments, replies, resolve, approve, request changes).
- Several accounts at once, also of one forge; plain `http://` allowed on private addresses only, with a warning.
- macOS and Linux builds: the data folder follows each platform (`~/Library/Application Support/nit`, `$XDG_DATA_HOME/nit`), tokens go to the Keychain or the Secret Service, one instance per user (a second start raises the first window), folders and links open with `open` / `xdg-open`, the folder dialog uses the XDG portal on Linux, cancelling a review stops git's helper processes, Settings shows the free space, and the shortcut labels say Cmd on macOS. CI builds and tests on Windows, Linux and macOS.
- Release packages: a Windows installer (per user, no admin rights) and a portable exe and zip; macOS disk images for Apple silicon and Intel (not signed or notarised yet); Linux `.deb`, `.rpm`, AppImage and `.tar.gz` for x86_64 and aarch64. See "Install" in the README.
- An app icon (window, taskbar, installers, desktop file) and `nit --version`.
- Settings, Accounts: Rename (a label per account), Disable / Enable (a disabled account is not synced; its repository choice and cache are kept), Remove with a confirmation that says what it removes ("Removes 34 cached PRs, keeps 2 drafts marked"), and a per-account "Paused by the rate limit until ..." status.
- Inbox: filter chips per forge and per account, and the rate-limit pause of an account on the status line.
- GitHub: a dismissible hint when open pull requests request your review in repositories no account syncs (one search per sync, at most one a minute; it never selects anything).
- README: sections Accounts, GitHub token, GitLab token and What works per forge.
- GitLab writes: MR comments, file and inline comments (single and multi-line, on added, context and deleted lines, renames included), replies, resolve and unresolve, approve and unapprove, request changes. Inline positions use the diff version of the commit you reviewed and GitLab's own diff of it; a line outside that diff is refused before anything is sent. Text with a line starting with `/` and a letter is refused (it would run a quick action).

### Known limitations

- Inline comments on Bitbucket cannot be posted yet: until the anchoring is confirmed against Bitbucket, Submit refuses inline drafts (they stay saved and can be posted as a file or PR comment instead). `NIT_INLINE_EXPERIMENT=1` enables them for the experiment only (see `docs/findings/comment-anchoring.md`).
- PRs from Bitbucket forks are not supported; only open PRs can be reviewed; SSH remotes are ignored.
- macOS and Linux builds are newer and have had less testing than Windows; the macOS disk images are not signed or notarised.

[Unreleased]: https://github.com/Scorpse/nit/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/Scorpse/nit/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Scorpse/nit/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Scorpse/nit/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Scorpse/nit/releases/tag/v0.1.0
