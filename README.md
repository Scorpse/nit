<p align="center"><img src="assets/logo.png" alt="nit" width="128"></p>

<h1 align="center">nit</h1>

<p align="center">Fast, local pull request review for Windows, Linux and macOS.<br>Bitbucket Cloud, GitHub and GitLab in one inbox.</p>

<p align="center">
  <a href="https://github.com/Scorpse/nit/actions/workflows/ci.yml"><img src="https://github.com/Scorpse/nit/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/Scorpse/nit/actions/workflows/coverage.yml"><img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2FScorpse%2Fnit%2Fbadges%2Fcoverage.json" alt="Line coverage"></a>
  <a href="https://github.com/Scorpse/nit/releases"><img src="https://img.shields.io/github/v/release/Scorpse/nit?include_prereleases&sort=semver" alt="Latest release"></a>
  <a href="#licence"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License MIT or Apache-2.0"></a>
</p>


**Review pull requests the way you read code: fast, local, from the keyboard.**

nit is a desktop app (Rust, egui) for Windows, Linux and macOS. One inbox shows the pull requests and merge requests that concern you from **Bitbucket Cloud**, **GitHub** (github.com and Enterprise) and **GitLab** (gitlab.com and self-managed). The diff is computed locally with git, so large PRs stay usable.

## Why

- **One inbox, three forges.** Several accounts at once; views for Mine, Needs me, All, Too big and Stacked.
- **Fast diffs.** Computed locally against the merge base, virtualized, syntax-highlighted, unified or split, driven with vim-style keys.
- **Drafts first.** Comments (inline, file, PR, replies) are local drafts you submit in one confirmed batch. Nothing is posted on a guessed line.
- **Knows the limits.** PRs too big for their forge's web page are flagged: Bitbucket over 200 files or 8,000 lines, GitHub over 300 files or 20,000 lines, GitLab over 1,000 files or 50,000 lines. nit still reviews them.
- **Stacked PRs.** Detected, with advice on review order, held approvals and merge order.
- **Private by design.** Tokens live only in your OS credential store; nothing leaves your machine except calls to your forges.
- **Copy as markdown.** The whole review, anchored `path:line`, ready for notes or an LLM.

## Install

Download a file from the [latest release](https://github.com/Scorpse/nit/releases/latest) (`SHA256SUMS` lists the checksums). Every platform needs git 2.31 or newer.

| Platform | Download | Install and run |
| --- | --- | --- |
| Windows 10 / 11, x86_64 | `nit-<version>-windows-x86_64-setup.exe` | Run the installer (no admin rights) and start nit from the Start Menu. Uninstall in Settings, Apps. |
| | `nit-<version>-windows-x86_64.exe` or `.zip` | Portable: run the exe from any folder. |
| macOS 11 or later | `nit-<version>-macos-arm64.dmg` (Apple silicon), `nit-<version>-macos-x86_64.dmg` (Intel) | Open the disk image and drag nit to Applications. |
| Debian 12+, Ubuntu 22.04+ | `nit-<version>-linux-x86_64.deb` (or `-aarch64`) | `sudo apt install ./nit-<version>-linux-x86_64.deb` |
| Fedora, openSUSE | `nit-<version>-linux-x86_64.rpm` (or `-aarch64`) | `sudo dnf install ./nit-<version>-linux-x86_64.rpm` (`zypper install` on openSUSE) |
| Other Linux | `nit-<version>-linux-x86_64.AppImage` or `.tar.gz` | `chmod +x nit-*.AppImage` and run it, or unpack the archive and run `./nit`. |

The downloads are not code-signed yet:

- Windows: SmartScreen may say "Windows protected your PC". Choose More info, then Run anyway.
- macOS: nit is not signed with an Apple Developer ID and not notarised, so Gatekeeper blocks the first start. Control-click (or right-click) nit in Applications, choose Open, then Open again. On macOS 15 or later, try to open nit once, then choose Open Anyway in System Settings, Privacy & Security. Or remove the quarantine flag in Terminal: `xattr -dr com.apple.quarantine /Applications/nit.app`

On Linux:

- Tokens are stored through the Secret Service: GNOME Keyring, KWallet or KeePassXC (with its Secret Service integration on) must be running. Without one, sign-in fails with "credential store unavailable".
- The packages need glibc 2.35 or newer, a desktop session (X11 or Wayland) and a Vulkan or OpenGL driver. The `.deb` and `.rpm` install git, libdbus, libxkbcommon and xdg-utils; for the AppImage and the archive, see the run-time list in [Building](docs/building.md).
- The AppImage needs FUSE; without it, start it with `--appimage-extract-and-run`.

## Quick start

1. Create a token ([Bitbucket](docs/accounts.md#bitbucket-token), [GitHub](docs/accounts.md#github-token), [GitLab](docs/accounts.md#gitlab-token)).
2. Start nit and sign in, or add an account in Settings, Accounts. Choose the repositories you review.
3. Select a PR and press Enter. Try it without an account: `nit --demo`.

## What works per forge

| | Bitbucket Cloud | GitHub | GitLab |
|---|---|---|---|
| Inbox, sizes, stacks, Needs me | yes | yes | yes |
| Review screen (local diff, Viewed marks) | yes | yes | yes |
| PR-level and file comments, replies | yes | yes | yes |
| Inline comments | disabled until verified | yes, single and multi-line | yes, single and multi-line |
| Resolve / unresolve threads | yes | yes | yes |
| Approve / request changes | yes | yes | yes |
| Fork PRs | not supported | yes | yes |

Details and exceptions: [Using nit](docs/usage.md).

## Documentation

- [Accounts, tokens and the token check](docs/accounts.md)
- [Using nit: settings, review, shortcuts, comments](docs/usage.md)
- [Data, security and troubleshooting](docs/troubleshooting.md)
- [Building from source and developer notes](docs/building.md), [ARCHITECTURE.md](ARCHITECTURE.md)
- [CHANGELOG.md](CHANGELOG.md), [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md)

Status: pre-1.0. The Linux and macOS builds are newer than the Windows one.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT) at your option. Bundled fonts, syntax definitions and dependency licences: [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work, as defined in the Apache-2.0 licence, shall be dual licensed as above, without any additional terms or conditions.
