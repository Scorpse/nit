# Architecture

nit is a Cargo workspace. The dependency direction is one way: `app` → `sync`, `providers`, `gitlocal`, `store` → `prcore`.

| Crate | Role |
|---|---|
| `prcore` | Forge-neutral model: pull requests, repositories, diff parsing, comment threads, drafts, markdown export, and the rules for which origins may be reached (`origin`). No I/O. |
| `providers` | The `Provider` trait and its three implementations: Bitbucket Cloud, GitHub (REST and GraphQL, including Enterprise) and GitLab (REST and GraphQL, including self-managed). One shared HTTP client (`http`) that scopes credentials to one origin, follows no cross-origin redirect and never retries a write. |
| `store` | The local SQLite database (cache, drafts, settings), its single writer thread, and the OS credential store (`secrets`). Tokens never go into the database. |
| `sync` | Metadata-first sync: lists repositories, then stores PR metadata and diffstats of the selected ones. Diffs are never downloaded through the API. |
| `gitlocal` | Wraps the `git` executable: shallow fetch of the PR refs, per-file patches, blame, numstat. Cancelling a review kills git and its children. |
| `app` (binary `nit`) | The egui/eframe UI, the review service (worker threads that build and colour patches), accounts, settings, token check, single-instance lock, crash log. |

## Data flow

1. **Sync.** `sync` asks each enabled account's provider for open PRs of the selected repositories and writes them to `store`. A conditional request (ETag) keeps an unchanged list free of rate limit.
2. **Open a review.** `app::review::service` fetches the PR's head and base into a local clone (`gitlocal`), reads the changed files, then serves one parsed patch per file on demand. Neighbouring files are prefetched; syntax colours are computed in chunks, starting with the hunk on screen.
3. **Comment.** Drafts are kept in `store` until posted. A post goes through the account's provider; inline positions are built from the commit the reviewer read, and a position the forge cannot anchor is refused before anything is sent.

## Rules the code keeps

- A token is read from the credential store (or an environment variable) at the moment of use, and appears in no log, error, `Debug` output or file in the data folder (`backend/leak_check.rs` tests this).
- Plain `http://` works only for loopback, private and link-local hosts, per account, after an explicit acknowledgement (`prcore::origin`).
- Every forge call and every git process runs off the UI thread; the UI only reads events from the service.
- A panic while building one file's diff fails that file, not the app.

## Platform differences

`app::brand` holds the per-OS data folder, `store::secrets` the keyring backend (Windows Credential Manager, macOS Keychain, Secret Service), `app::single_instance` the one-instance lock, and `app::open` the platform opener. Packaging (installer, `.deb`, `.rpm`, AppImage, `.dmg`) lives in `packaging/`.
