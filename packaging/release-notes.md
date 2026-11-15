## Downloads

| Platform | File | Notes |
| --- | --- | --- |
| Windows 10 / 11, x86_64 | `nit-@VERSION@-windows-x86_64-setup.exe` | Installer: per user, no admin rights, uninstall from Settings, Apps |
| | `nit-@VERSION@-windows-x86_64.exe`, `.zip` | Portable: run the exe from anywhere |
| macOS 11 or later, Intel (Apple silicon runs it under Rosetta 2) | `nit-@VERSION@-macos-x86_64.dmg` | As above |
| Linux, x86_64 | `nit-@VERSION@-linux-x86_64.deb` | Debian 12+, Ubuntu 22.04+: `sudo apt install ./nit-…deb` |
| | `nit-@VERSION@-linux-x86_64.rpm` | Fedora, openSUSE: `sudo dnf install ./nit-…rpm` |
| | `nit-@VERSION@-linux-x86_64.AppImage` | `chmod +x` and run; needs FUSE 2 or 3 |
| | `nit-@VERSION@-linux-x86_64.tar.gz` | The binary with its desktop file and icon |

All builds need git 2.31 or newer. On Linux tokens are stored through the Secret Service (GNOME Keyring, KWallet or KeePassXC), and the builds need glibc 2.35 or newer. `SHA256SUMS` lists the checksums (`sha256sum -c SHA256SUMS --ignore-missing`).

### The downloads are not code-signed

- Windows: SmartScreen may say "Windows protected your PC". Choose More info, then Run anyway.
- macOS: nit is not signed with an Apple Developer ID and not notarised, so Gatekeeper blocks the first start. Control-click (or right-click) nit in Applications, choose Open, then Open again. On macOS 15 or later: try to open nit once, then open System Settings, Privacy & Security, and choose Open Anyway next to the message about nit. Or remove the quarantine flag in Terminal: `xattr -dr com.apple.quarantine /Applications/nit.app`
