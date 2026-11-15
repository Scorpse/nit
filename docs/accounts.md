# Accounts, tokens and the token check

How to sign in to Bitbucket Cloud, GitHub and GitLab, which token scopes nit needs, and how to test a token. Back to the [README](../README.md).

## First run

1. Create a token for your forge (see the token sections below).
2. Start `nit.exe` from Explorer or a plain PowerShell window, not from an IDE terminal (see [Troubleshooting](troubleshooting.md)).
3. Sign in on the welcome screen (Bitbucket Cloud), or add a GitHub or GitLab account in Settings, Accounts. nit lists the repositories, then opens "Choose repositories": tick the ones you review (search, or All / None per workspace) and press Save. Only those are synced; nothing is synced until you choose. Change the choice later in Settings, Choose repositories.
4. Optional: open Settings to choose the download folder and a folder with your existing clones.
5. Select a PR and press Review (or Enter).

## Bitbucket token

In your Atlassian account settings, open Security, API tokens, "Create API token with scopes" and choose Bitbucket.

- The Bitbucket REST API is called with your Atlassian email and the token.
- The token needs read access to repositories and pull requests, and write access to pull requests for posting comments, resolving threads, approving and requesting changes. Pick Bitbucket: read repository, read pull request, write pull request (the names as shown by Atlassian when you create the token). Reading your user and workspaces needs the account / workspace read access the screen offers.
- Git operations (fetching the PR's branches) use the same token with the fixed username `x-bitbucket-api-token-auth`; the app does this, you do not configure git.

If the inbox says "Access denied on all repos: the token may be missing scopes", the token was accepted but lacks a read scope. If reading works but posting or approving says "access denied", the token lacks write access to pull requests. Sign out and sign in with a new token.

## Self-hosted forges and plain http

Settings > Accounts > Add account adds a GitHub (github.com or GitHub Enterprise) or GitLab (gitlab.com or self-managed, relative URL root included) account with a personal access token; the form says which scopes it needs (GitLab: `read_api` and `read_repository`, or `api`). Bitbucket is Bitbucket Cloud only.

Plain `http://` (no TLS) is accepted only for a self-hosted GitHub Enterprise or GitLab on a private or local address, written literally: `localhost`, `*.localhost`, `127.0.0.0/8`, `::1`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` and link-local `169.254.0.0/16` (for example `http://192.168.1.20:8080`). Host names are never resolved to decide this, and any other `http://` address is refused as before. The form then shows "Unencrypted connection: the token is sent in clear text on your network" and Add / Test connection stay disabled until "I understand" is ticked; the account list marks such an account with an `http` chip and the same warning. The scheme is kept in the account's stored address (no extra secret is stored). The token and git credentials go only to exactly that `http://host:port`, never to another host, port or to `https://` on the same host, and redirects are not followed with credentials. Use https wherever the server offers it.

## Accounts

nit can be signed in to several accounts at once: Bitbucket Cloud, GitHub (github.com, GitHub Enterprise Server, GHE.com) and GitLab (gitlab.com or self-managed), also several of the same forge and host (for example a work and a personal github.com account). There is one inbox for all of them.

- The first Bitbucket sign-in is the welcome screen's. Add the others in Settings, Accounts, "Add account": pick the forge, enter the web address (GitHub Enterprise or self-managed GitLab only; with its path if GitLab runs under one) and the token. "Test connection" (GitHub, GitLab) says who the token signs in as and what it can see, without saving anything.
- Each account's token is stored in Windows Credential Manager under its own entry (`account.<id>.token`, plus `account.<id>.email` for Bitbucket). The cache holds only the account's id, forge, addresses, user id, login and display name.
- Each repository syncs through exactly one account, chosen in "Choose repositories" (grouped by account). A repository both accounts can see is synced once.
- Each account row in Settings has Rename (the name it is shown by; empty goes back to its login), Disable / Enable and Remove. A disabled account is not synced; its repository choice and cached pull requests stay, and it syncs them again once enabled (from the next Refresh).
- When a forge's rate limit pauses an account, its row says "Paused by the rate limit until 14:05" and the inbox status line names it ("GitHub (mona): paused until 14:05"). The other accounts keep syncing.
- Remove asks first and says what it removes, for example "Removes 34 cached PRs, keeps 2 drafts marked": the account's saved token, its repository choices and its cached pull requests go; drafts written on its pull requests stay in the cache, still marked with the removed account's id. The other accounts are not touched. The signed-in Bitbucket account is removed by signing out.
- The inbox shows a forge tag on GitHub and GitLab rows, and filter chips (All, one per forge, and one per account when a forge has several) once pull requests come from more than one account.
- GitHub only: after a sync nit asks GitHub's search once (never more than once a minute) for open pull requests that request your review. When some are in repositories no account syncs, a hint names them ("3 open pull requests request your review in repositories you have not selected: ..."), with "Choose repositories" and Dismiss. It never selects anything.

## GitHub token

Create a personal access token on GitHub (Settings, Developer settings, Personal access tokens):

- Fine-grained (preferred): resource owner = the user or organization whose repositories you review (a fine-grained token covers one owner: add one account per organization), repository access to those repositories, and the permissions Pull requests: Read and write, Contents: Read (git fetches the PR's branches with it) and Metadata: Read (GitHub adds it). Pull requests: Read only is enough to read; posting comments, replies, resolving threads and reviews need Read and write.
- Classic: the `repo` scope (private repositories; it also covers writing). Without `repo`, "Test connection" says so and private repositories stay hidden.
- With SAML single sign-on, authorize the token for the organization first (GitHub's token page, "Configure SSO"); nit says when a request was refused for that reason.
- Git uses the token with the user name `x-access-token`, sent only to the account's own git host.

## GitLab token

Create a personal access token in GitLab (Preferences, Access tokens) with the scope `api` to comment, resolve, approve and request changes (it also covers git), or `read_api` and `read_repository` to only read (`read_repository` lets git fetch the merge request's branches). nit records the token's expiry date and warns in Settings, Accounts from 14 days before it. For a self-managed GitLab with a company certificate, the API trusts the Windows certificate store; git uses its own settings (see [Troubleshooting](troubleshooting.md)). Git uses the token with the user name `oauth2`, sent only to the account's own host.

## Testing your token

`nit --check-token` checks a token without opening the window and without changing anything on the forge:

```powershell
.\nit.exe --check-token | Out-Host                         # pipe it so PowerShell waits and shows the output
.\nit.exe --check-token --repo acme/api --pr 42 | Out-Host # a specific repository and pull request
$LASTEXITCODE                                                  # 0: no problem, 1: at least one
```

It uses the credentials saved by nit (Windows Credential Manager). When none are saved it reads the environment variables `NIT_EMAIL` and `NIT_TOKEN` (`$env:NIT_EMAIL="you@example.com"; $env:NIT_TOKEN="..."`; the old names `PRVIEW_EMAIL` and `PRVIEW_TOKEN` still work but are deprecated); the token is never taken as a command-line argument. Each check prints PASS, FAIL or SKIP with the reason and, on a failure, what to change:

1. Credentials present.
2. REST sign-in: `GET /user` (shows your display name only).
3. The token can list workspaces and repositories.
4. Open pull requests of `--repo`, or of the first repository that has some (it says which).
5. Reading a pull request: diffstat, diff, comments (bot and app comments included) and tasks.
6. Reviewers and approvals of that PR, and your permission on the repository when the token may read it.
7. Git sign-in: `git ls-remote` of the repository with the token, the way nit runs git (user name `x-bitbucket-api-token-auth`). When git refuses it, the account email, the Bitbucket username and the account UUID are tried as user name, only to tell which one the token works with; the report names it and says what to change.
8. System git: present, version 2.31 or newer.
9. HTTPS to `api.bitbucket.org`: tells a certificate problem (a TLS-inspecting company proxy) from a network or proxy problem.

For a GitLab or GitHub account (`--account <id or host>`, or `NIT_GITLAB_TOKEN` / `NIT_GITLAB_URL` and `NIT_GITHUB_TOKEN` / `NIT_GITHUB_URL` without a stored one) the same checks run against that forge. An account on a private `http://` address prints `warning: Unencrypted connection: the token is sent in clear text on your network` first, check 7 runs git over that http origin and check 9 says the API answered over plain HTTP; a public `http://` address is refused and nothing is sent.

The report ends with `ALL OK` or the number of problems, and is always saved to `token-check.txt` in the data folder, on Windows `%LOCALAPPDATA%\nit\token-check.txt` (also when started from a shortcut without a console; it is then opened in Notepad; on macOS and Linux the report always goes to stdout). The token never appears in it. From `cmd.exe`, use `start /wait nit.exe --check-token` so the exit code is set.

Write test (off by default): `--check-token --write-test --repo <workspace>/<repo> --pr <id> --i-understand-this-posts` also posts one PR-level comment on that pull request, reads it back and deletes it, reporting each step. It refuses to run unless all four options are given. If the delete fails, the report says which comment to delete by hand.

With several accounts, `--account <id or host>` picks the one to test (the account id, or the host when only one account is on it); without it, and without `--repo`, `--pr` or `--write-test`, every enabled account is tested in turn (Bitbucket, GitHub and GitLab alike): one report section per account, then `Accounts checked: N, with problems: M` (disabled accounts are counted as skipped), and the exit code is 1 when any account has a problem. `--repo`, `--pr` and `--write-test` belong to one forge, so with them and no `--account` only the signed-in Bitbucket account is tested, as before. A cache without accounts still tests the default Bitbucket sign-in. For a GitHub or GitLab account the checks are that forge's own and read-only (identity, token kind or scopes, for GitLab the expiry, repositories, `--repo` / `--pr` reads, git sign-in). For an account not added yet, `NIT_GITHUB_TOKEN` (plus `NIT_GITHUB_URL` for GitHub Enterprise) with `--account github`, or `NIT_GITLAB_TOKEN` (plus `NIT_GITLAB_URL` for a self-managed GitLab) with `--account gitlab`, are read from the environment.
