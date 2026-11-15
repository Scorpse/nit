# Data, security and troubleshooting

Where nit keeps its data, how it treats credentials, and what to do when something fails. Back to the [README](../README.md).

## Where data lives

- Email and token: only in Windows Credential Manager, service `nit`, one entry per account (`account.<id>.token`, and `account.<id>.email` for Bitbucket; the first Bitbucket sign-in is `acc-bitbucket-default`). Never in the cache, settings, logs, error text, git config, remote URLs or command lines. A test (`no_token_in_data_dir`) scans the data folder for every token in plain and encoded form.
- Cache: `cache.sqlite` in the data folder (Windows `%LOCALAPPDATA%\nit\cache.sqlite`). It holds PR titles, authors, reviewer names, branch and repo names, drafts, viewed marks and settings in plain text, plus each account's user id and display name. Delete the file to clear it (unsent drafts go with it); the app rebuilds it on the next sync.
- Mirrors: the download folder (default `mirrors` in the data folder). Delete a mirror to re-download it on the next review.

## Security notes

- The token is stored only in Windows Credential Manager and is never written to config files or the cache.
- Git gets the credentials through environment variables of the git process only, for that one command.
- Requests with credentials go only to the account's own origin; redirects to other origins are not followed.
- Error messages show a category (or the forge's own short message for a refused write), never URLs, response bodies or credentials.

## Resetting credentials

- In the app: Sign out (top bar of the inbox, or the login screen once credentials are saved). It deletes the stored email, token and user, and returns to the login screen.
- Other accounts: Settings, Accounts, Remove (see [Accounts](accounts.md#accounts)).
- By hand: in Windows Credential Manager, Windows Credentials, remove the generic entries for service `nit`, keys `account.<id>.email` and `account.<id>.token` (versions before multi-account used `bitbucket.email` and `bitbucket.token`, which the keyring crate normally names `bitbucket.email.nit` and `bitbucket.token.nit`).

A rejected email or token (HTTP 401) does not delete anything. The app returns to the login screen with the email filled in: re-enter the token, or sign out.

## Troubleshooting

- "git not found / too old (need 2.31 or newer)" or "Git is unavailable": install Git for Windows 2.31 or newer and make sure `git` is on `PATH`, then restart nit. Without git the inbox works but reviews cannot open.
- Authentication failures: "authentication failed" means the forge refused the token (or the email, on Bitbucket) (expired, revoked, or the wrong email). "access denied" means the token works but lacks a scope (see the token sections above). Sign out and sign in with a new token.
- The first review of a repository is slow: nit creates a bare mirror of the repository first. Later reviews fetch only what changed. A clone in the clones folder avoids the download.
- Corporate network / proxy / certificates: the API client trusts the Windows certificate store (plus the bundled public roots), so a company root certificate installed in Windows is accepted. "the server's certificate was not trusted" means a TLS-inspecting proxy whose root is not installed: ask IT to install it in Windows. The Windows system proxy and the `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` environment variables are honoured; proxies that need NTLM or Kerberos login are not supported. Git uses its own settings: configure `http.proxy` and `http.sslCAInfo` (or schannel) for it.
- "nit could not open its window": the graphics driver refused every GPU backend. Start nit with `WGPU_BACKEND=gl` (OpenGL instead of DirectX/Vulkan) or `WGPU_POWER_PREF=low` (prefer the integrated GPU), for example in PowerShell `$env:WGPU_BACKEND="gl"; .\nit.exe`. The error is also written to `crash.log` in the data folder (the message names its path).
- "The cache folder does not support WAL mode (network drive?)": the data folder is on a file system without SQLite WAL support. nit keeps working with a slower rollback journal; move the profile folder to a local disk to get the normal speed back.
- Signing in with a different Bitbucket account empties the cached inbox (PRs, diffstats, viewed marks, the repository list and choice) before the first sync, so nothing of the previous account is shown. Drafts stay in the cache file: they are stored per repository and PR, not per account.
- Started from an IDE terminal: IDE terminals can set git variables (askpass helpers, credential helpers, config) that git inherits, which can make fetches prompt or fail. Start nit from Explorer or a plain PowerShell window.
