# Building and developing

Build from source, run-time requirements on Linux, and developer notes. Back to the [README](../README.md).

## Build and run

```powershell
cargo run -p app --release           # the binary is nit.exe (nit on macOS and Linux)
cargo run -p app --release -- --demo # built-in data, no network, no credentials
cargo test --workspace
```

Windows is the main platform. macOS (Apple silicon and Intel) and Linux (x86_64, aarch64) build from the same sources and run the same tests in CI. On Linux (Debian / Ubuntu names):

- To build: `sudo apt install build-essential pkg-config libdbus-1-dev` (a C compiler for the bundled SQLite and Oniguruma; libdbus for the Secret Service). Nothing else: X11, Wayland, xkbcommon, Vulkan / OpenGL and the portal's libdbus are loaded at run time.
- To run: a desktop session (X11 or Wayland) with `libxkbcommon0` (plus `libxkbcommon-x11-0`, `libx11-xcb1`, `libxcursor1`, `libxrandr2`, `libxi6` on X11, `libwayland-client0` on Wayland), a Vulkan or OpenGL driver (`mesa-vulkan-drivers` or `libgl1-mesa-dri` + `libegl1`), `libdbus-1-3`, `git` 2.31 or newer, `xdg-utils` (Open folder / Open in browser), `xdg-desktop-portal` with a backend for the folder dialog (or `zenity`), and a Secret Service (GNOME Keyring, KWallet or KeePassXC) to store tokens. Without a Secret Service sign-in fails with "credential store unavailable"; tokens are never written to a file instead.

On macOS the tokens go to the login Keychain; Cmd takes the place of Ctrl in the shortcuts (except `Ctrl+d` / `Ctrl+u`).

The data folder (cache, mirrors, `crash.log`, `token-check.txt`) is `%LOCALAPPDATA%\nit` on Windows, `~/Library/Application Support/nit` on macOS and `$XDG_DATA_HOME/nit` (default `~/.local/share/nit`) on Linux.

`--demo` also accepts `--shot out.png [--screen login|inbox|stacked|parent-changed|rejected|denied|settings|repos|review|review-comments|review-drafts|review-selection|review-actions]` to save a screenshot and exit. Unknown options exit with a usage line.

## Developer notes

`nit --demo --shots <dir> [--screens a,b,c] [--width N --height N]` renders every demo
scene into `<dir>/<scene>.png` in one process and one (hidden, unfocused) window, then exits
(code 1 if a scene did not settle or could not be saved). `--screens` picks scenes by name; an
unknown name prints the valid ones. The scene table, `SCENES` in `crates/app/src/shots.rs`, is
the single list behind `--screen` and `--shots`: add a row there for a new scene. Prefer
`--shots` over many `--shot out.png --screen <name>` runs.
