# Packaging

Inputs for the release packages. `.github/workflows/release.yml` builds all of them on a
`v<version>` tag and publishes a GitHub Release; a manual run of the workflow builds the same
files as workflow artifacts without publishing anything.

| Folder | What | Used by |
| --- | --- | --- |
| `icons/` | `nit.ico`, `nit.icns` and `nit-<size>.png` (16 to 512), rendered once from `assets/logo.svg` and committed | `crates/app/build.rs` (exe icon), the window icon, all packages |
| `windows/` | `nit.iss`, the Inno Setup 6 installer script | Windows job |
| `linux/` | `nit.desktop`, the AppStream metainfo, `package.sh` (`.deb`, `.rpm`, `.AppImage`, `.tar.gz`) | Linux jobs; `.deb` and `.rpm` metadata is in `crates/app/Cargo.toml` |
| `macos/` | `Info.plist` template, `bundle.sh` (`nit.app`, `.dmg`, `.tar.gz`) | macOS jobs |
| `release-notes.md` | Download table and signing notes appended to the CHANGELOG section | Release job |

## Making a release

1. Set `version` in `[workspace.package]` of the root `Cargo.toml` (every crate inherits it)
   and add a `## [x.y.z]` section to `CHANGELOG.md`.
2. Push the tag `vx.y.z`. The workflow fails early when the tag and the version differ or the
   CHANGELOG section is missing. A version with a suffix (`1.2.0-rc.1`) makes a pre-release.

The release gets every package plus `SHA256SUMS`, and build provenance attestations once the
repository is public.

## Building locally

All packages start from `cargo build --release --locked -p app`.

- Windows: `ISCC /DAppVersion=x.y.z packaging\windows\nit.iss` writes the installer to
  `dist-out\`.
- Linux: with `cargo-deb`, `cargo-generate-rpm` and `appimagetool` installed,
  `packaging/linux/package.sh x.y.z dist`. The release builds in an Ubuntu 22.04 container,
  so the packages need glibc 2.35 or newer.
- macOS: `MACOSX_DEPLOYMENT_TARGET=11.0` for the build, then
  `packaging/macos/bundle.sh app x.y.z dist` and `packaging/macos/bundle.sh dmg x.y.z x86_64 dist`.

## What the packages contain

- Windows installer: per user under `%LOCALAPPDATA%\Programs\nit`, no admin rights, Start Menu
  shortcut, optional desktop shortcut and PATH entry. Upgrades install over the old version and
  close a running nit first. The uninstaller asks whether to delete the data folder and the
  saved sign-ins (default: keep). Keep the `AppId` unchanged.
- Linux: the binary, desktop file, icons and metainfo. Nothing else is bundled, the AppImage
  included: glibc, libdbus, libxkbcommon, the X11 / Wayland client libraries, the Vulkan or
  OpenGL driver, git and the Secret Service come from the system. The `.deb` and `.rpm` declare
  git 2.31+, libdbus, libxkbcommon and xdg-utils as dependencies.
- macOS: `nit.app` with the bundle id `io.github.scorpse.nit`, for macOS 11 or later.

## Code signing

Nothing is signed with a publisher certificate yet.

- Windows: SmartScreen warns about an unknown publisher (More info, Run anyway).
- macOS: the app is ad-hoc signed only and not notarised, so Gatekeeper blocks the first start.
  Users Control-click nit, choose Open and confirm (macOS 15 and later: System Settings,
  Privacy & Security, Open Anyway), or run `xattr -dr com.apple.quarantine /Applications/nit.app`.

The macOS jobs already contain the signing and notarisation steps; they run only when these
repository secrets exist, and are skipped otherwise:

| Secret | Content |
| --- | --- |
| `MACOS_CERTIFICATE_P12` | Developer ID Application certificate and key, `.p12`, base64 |
| `MACOS_CERTIFICATE_PASSWORD` | Password of the `.p12` |
| `MACOS_SIGNING_IDENTITY` | For example `Developer ID Application: Name (TEAMID)` |
| `MACOS_NOTARY_APPLE_ID`, `MACOS_NOTARY_TEAM_ID`, `MACOS_NOTARY_PASSWORD` | Apple ID, team id and app-specific password for `notarytool` |

Once they are set, update the signing notes in `release-notes.md` and the README.
