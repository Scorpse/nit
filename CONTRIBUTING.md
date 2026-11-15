# Contributing to nit

Thanks for helping. nit is pre-1.0, Windows only and Bitbucket Cloud first, so please open an issue to discuss larger changes before writing them.

## Build and test

You need a stable Rust toolchain (rustup), Git for Windows 2.31 or newer on `PATH`, and Windows 10 or 11.

```powershell
cargo build --workspace
cargo run -p app --release           # nit.exe
cargo test --workspace
```

A few tests are timing-sensitive on a loaded machine (highlighting in chunks, hunk colouring order, the store writer's failure report). If one of them fails once, run it again before digging in; CI retries the test run once for the same reason.

## Trying it without an account

- `cargo run -p app --release -- --demo` starts with built-in data: no network, no credentials.
- `nit --demo --shots <dir>` renders every demo scene to `<dir>/<scene>.png` and exits; `--screens a,b` picks scenes. Add a row to `SCENES` in `crates/app/src/shots.rs` for a new scene. Use it to show UI changes in a pull request.
- `nit --check-token` tests a real token without changing anything on Bitbucket (see the README). Use it, rather than describing your token, when reporting sign-in or permission problems.

## Style

- `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` must be clean; CI checks both.
- Keep changes small and focused; one concern per pull request and per commit.
- Error text shown to users is a category, never URLs, response bodies or credentials.

## Tests first

New behaviour and bug fixes come with tests, written first: a failing test that shows the bug or the missing behaviour, then the change that makes it pass. Network code is tested against `wiremock`, git code against temporary repositories, never against a live Bitbucket.

## No secrets, ever

Never paste a token, password, `Authorization` header, cookie or private repository content into an issue, pull request, test or fixture. Redact `token-check.txt` and logs before attaching them (nit keeps the token out of them, but check). Report security problems privately, as described in [SECURITY.md](SECURITY.md).

## Licence

nit is licensed under MIT OR Apache-2.0. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work, as defined in the Apache-2.0 licence, shall be dual licensed as above, without any additional terms or conditions (see the Licence section of the README).

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
