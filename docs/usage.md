# Using nit

Settings, the review screen, shortcuts, comments and approvals. Back to the [README](../README.md).

## Settings

Settings (top bar of the inbox; Esc or Back returns) is one column of cards:

- Account: the signed-in name and email (never the token), whether nit is connected, syncing or offline, and Sign out (asks first; deletes the saved email and token, keeps the cache and settings).
- Repositories: how many repositories are chosen out of how many are listed, "Choose repositories..." and "Refresh list".
- Storage: the download folder and the local clones folder (below), each with Browse... and Open. Each field is checked as you leave it: whether the folder exists and is writable, the free space on its drive (a warning under 5 GB), and for the clones folder how many usable clones were found (Rescan looks again). Pasted paths lose surrounding spaces and quotes; relative paths are refused. Save is enabled once a folder changed and every field is valid, and says "Saved" when done; "Reset to defaults" only fills in the defaults, Save keeps them.
- Review: "Vim keys" and the default diff layout (Unified or Split). These apply at once; no Save needed.
- About: the nit version, the git version (with a warning when it is missing or older than 2.31), the graphics adapter and backend, the data folder (Open, Copy path), how to run the token check (`nit --check-token`, Copy command) and the licence.

Choose repositories: the list of your workspaces and repositories, grouped by workspace, with a search box, All / None per workspace and the count chosen. "Refresh list" lists the workspaces and repositories again (no pull requests are read). A repository a refresh no longer lists stays in the list as "no longer accessible" while it is chosen, and is not synced. Save syncs just the chosen repositories. Unticking a repository stops syncing it and asks whether to keep its cached pull requests (the default: kept on disk, hidden from the inbox, shown again if you choose it again) or delete them.

After an upgrade from a version that synced every repository, the repositories you already had pull requests from are chosen once, so nothing disappears; a one-line notice in the inbox says so until you dismiss it.

The folders (Storage):

- Download folder: where bare mirrors of the repositories are created (default `mirrors` in the data folder, on Windows `%LOCALAPPDATA%\nit\mirrors`).
- Local clones folder (optional): existing clones under it are scanned and reused instead of mirrored. nit only fetches into `refs/nit/pr-<id>/...`; it never touches your branches, working tree or index. Clones with only SSH remotes are listed as ignored.

## Size flags

The inbox flags a PR whose diff its forge's web page cannot show in full (view "Too big"). The numbers are each forge's own limits:

| Forge | Flagged when |
| --- | --- |
| Bitbucket Cloud | more than 200 files, 8,000 changed lines, or 2,000 lines in one file |
| GitHub | more than 300 files or 20,000 changed lines |
| GitLab | more than 1,000 files or 50,000 changed lines (the instance defaults) |

nit itself is not limited by these: it computes the diff locally with git.

## Reviewing a PR

Select a PR in the inbox and press Review (or Enter, or double-click its row). The review screen shows the changed files on the left and the selected file's diff in the middle; the Conversation tab holds the PR-level comments and an overview of the file comments.

- Files: a collapsible directory tree of the changed files (or a flat list: Tree / List, remembered), with +/- counts, a Viewed box per file and a tri-state one per directory (marks every file below). Generated and lock files are collapsed in their own group; mark files Viewed as you go (kept per file version). Filter text and the A/M/D/R and Commented chips narrow the tree and expand it.
- Comment markers: a speech bubble per file shows its threads (amber: the unresolved count, grey: all resolved) and a blue dot its unsent drafts; directories add up everything below them, also when collapsed. Hover for the counts. In the diff, a strip right of the code has one tick per thread or draft row (click to jump there), and the line-number gutter shows a dot on rows with threads.
- Mouse: double-click a PR in the inbox to open its review (only when the Review button is enabled; otherwise the reason shows in the status line); double-click a file to show its diff, a directory to expand or collapse it.
- Diff: computed locally from git against the merge base, like Bitbucket. Syntax highlighting arrives shortly after the rows; files with more than 20,000 changed lines are shown without highlighting.
- Selection: drag over the diff text to select rows; `Ctrl+A` selects every row, `Ctrl+C` copies the selected code.
- Unified or split: the Unified / Split toggle above the diff (or `Alt+S`) shows the old file left and the new file right; a change's deletions and additions line up row by row, the shorter side padded. Remembered for the next review. Word-level marks and syntax colours stay on both sides; a selection stays on the side it started on and copies only that side's text; comment cards span the full width under their line; each side's line-number gutter comments on its own side (old or new line). Windows narrower than 900 px (or a diff pane under 600 px) show the unified view with a "Too narrow for split" note until there is room.

### Shortcuts

Vim-style keys are on by default; Settings, "Vim keys" turns them off (then `j` / `k` move between files, `v` toggles Viewed and `c` comments on the hovered row, as before). `?` shows every key in the app; the list below is the same.

| Key | Action |
|---|---|
| `j` / `k` | Next / previous diff row (the first press shows the cursor at the top of the view); a count first moves that many rows (`5j`) |
| `Ctrl+d` / `Ctrl+u` | Half a page down / up |
| `gg` / `G` | First / last row (`gg` within one second) |
| `{N}G` | Line N of the new file (type the number, then `G`) |
| `]` / `[` | Next / previous hunk |
| `h` / `l` | Split view: cursor on the old / new side |
| `/` | Search in this diff (matches are highlighted; Enter jumps to the first one, `Esc` closes the bar) |
| `n` / `N` | Next / previous match |
| `c` | Comment on the cursor row (in the split view, on the cursor's side) |
| `C` | Comment on the file |
| `v` / `V` | Select rows from the cursor; `j` / `k` extend the selection, `c` comments on the cursor row, `Esc` ends it |
| `}` / `{` | Next / previous file |
| `r` | Toggle the file's Viewed mark |
| `t` | Focus the file filter |
| `y` | Copy the file's path |
| `Enter` | Show the file at the file list cursor; in the inbox, open the selected PR's review |
| `q` / `Backspace` | Back to the inbox (like Back: waits for a post in flight) |
| `?` | Show or hide the keyboard help |
| `Alt+S` | Unified / split diff (also with the vim keys off) |
| `Ctrl+A` | Select every row of the diff |
| `Ctrl+C` | Copy the selected diff text |
| `Esc` | Close the help or search, end visual mode, clear the selection; in the composer, cancel |
| `Ctrl+Enter` | In the composer, save the draft |
| `Tab` / `Shift+Tab` | In the composer, next / previous comment type (none, issue, nit, question, suggestion, praise) |
| `Ctrl+Shift+M` | Copy the review as markdown, with the scope last chosen in "Copy as markdown" (default: all unresolved) |
| `Up` / `Down` | Inbox: previous / next PR; file list: previous / next row (directories included) |
| `PageUp` / `PageDown` | Move the selection by a page |
| `Home` / `End` | First / last row |
| `Left` / `Right` | File tree: collapse / expand the directory, or move to its parent / first child |
| `Space` | File list: toggle Viewed of the file, or of every file below the directory |

The keys do nothing while a text field (filter, search, composer) has focus or any confirmation dialog is open; the letter keys also need `Ctrl`, `Alt` and `Cmd` released. A click on the diff text moves the cursor there. With the pointer over the diff (or the Conversation tab), the arrow and page keys scroll it instead of moving in the file list.

### Comments

- Every comment starts as a local draft: on a diff line (gutter click, or `c`), on a file ("Comment on file"), on the PR ("Comment on PR" in the Conversation tab or the top bar's More menu) or as a reply. Drafts are saved in the cache and survive restarts.
- "Submit drafts" lists every draft and posts nothing until you confirm. Drafts are posted one at a time and the batch stops at the first failure. A draft is deleted only after the forge accepted it, so a failure or a restart never loses one.
- Inline comments: posted on GitHub and GitLab (single and multi-line, on the added, context or deleted line). On Bitbucket inline posting is disabled until the anchoring of inline comments is verified there: Submit does not post inline drafts, they stay saved, and each can be posted as a file comment or a PR comment instead (an explicit click, never automatic). Starting nit with `NIT_INLINE_EXPERIMENT=1` enables Bitbucket inline posting for that experiment only and shows an "experimental" chip.
- Blocked drafts: an inline draft is re-checked right before posting against the PR's current source commit and the file's diff. When the PR moved, the line is not in the diff, or the file is binary, it is not posted and shows "blocked" with the reason; you can post it as a file comment instead or delete it. It is never posted on a guessed line.
- Unconfirmed posts: when the connection fails after a comment was sent (a timeout, a reset, a server error), the forge may or may not have it. The draft shows "Not confirmed: the connection failed after sending. Refresh comments to check before posting again." and Submit leaves it out, also after a restart (the draft is marked in the cache before it is sent). Click "Check comments": when one of your comments with exactly that text is in its place, the draft says "It looks like this comment was posted" and you can delete it. Only "Post again anyway", available after the check, makes it postable again. A comment that could not be sent at all (no connection) is a plain "Not posted" and can be submitted again.
- Resolve / Unresolve on a thread is shown at once and undone if the forge refuses; the toast offers Undo for a few seconds.
- Comment types: the composer's chips (or `Tab` / `Shift+Tab` in its text field) give a draft an optional type: issue, nit, question, suggestion or praise. The draft keeps its text as written; when it is posted, the type is put in front of it as `**nit:** ` (once, also when a post is retried). "Check comments" compares with that exact posted text. Comments whose text starts with `**<type>:** ` (yours or anyone's) show the type as a coloured badge and their text without the prefix.

### Copy as markdown

"Copy as markdown" on the top bar puts the review on the clipboard as markdown, in one of three scopes: "My drafts only", "All comments (unresolved)" (unresolved threads and your drafts) or "Everything" (resolved threads too). `Ctrl+Shift+M` copies again with the scope last chosen. The text starts with the PR title, id, repository, `source -> destination` branches and the short head commit, then numbered items, each anchored `path:line` (new side), `path:old N` (old side), `path (file)` or `PR-level`, with the type tag, author, unresolved / resolved / draft / outdated / file-not-in-diff marks, the text as a quote, replies indented and, when that file's diff is loaded, the anchored line with up to two lines before it in a `diff` block. Nothing is fetched for it: diffs not opened yet have no excerpt.

### Approve and request changes

- The top bar has Approve and Request changes. When you already approved, Approve becomes "Remove approval"; when you already requested changes, Request changes becomes "Withdraw request". If your user is not known yet, both plain buttons are shown with a note.
- Each action asks for confirmation first. Approve lists warnings before its button: a stack parent that is not approved or merged yet, unresolved comment threads, unsent drafts. They never block.
- Before sending, the app reads the PR again. When its source commit is not the one on screen, nothing is sent and the screen turns stale ("The PR changed since you opened it. Reload before approving."). When the PR cannot be read (network), nothing is sent either ("Could not confirm the PR is unchanged"). Otherwise exactly one request is sent; the buttons are disabled while it runs. Afterwards the reviewers are read back from the forge; on an error the previous state is shown again with the reason (for a refusal, the forge's own short message).
- "Open in Bitbucket" (or GitHub / GitLab) opens the PR's page in your default browser.

### When the PR changes

The PR's source commit is checked when the review opens, when the window gets focus again, and once a minute while the screen is open. When it moved, a "PR changed, reload" banner appears and Approve / Request changes are disabled until you Reload; PR-level and file comments can still be posted. Reload fetches again and keeps your drafts; inline drafts written on the older version are marked blocked.

### Limits

- PRs from Bitbucket forks are not supported (the Review button is hidden); GitHub and GitLab fork PRs are reviewed.
- Only open PRs can be reviewed.
- SSH remotes are ignored: mirrors and clones are fetched over HTTPS only.
- No merges, rebases, retargets or pushes; no Jira.
