//! Opening a folder or a URL with the desktop's own handler: Explorer on Windows, `open` on
//! macOS, `xdg-open` on Linux and the BSDs. The target is handed to that program as one
//! argument: never through a shell, never formatted into a command line. Callers pass only
//! absolute paths and validated `http(s)://` URLs, so a target never starts with `-` and
//! cannot be read as an option.

use std::ffi::OsStr;
use std::process::Command;

/// The program that opens folders and URLs on this platform.
pub const OPENER: &str = if cfg!(windows) {
    "explorer.exe"
} else if cfg!(target_os = "macos") {
    "open"
} else {
    "xdg-open"
};

/// What the platform's file manager is called in the UI ("Show it in Explorer").
pub const FILE_MANAGER: &str = if cfg!(windows) {
    "Explorer"
} else if cfg!(target_os = "macos") {
    "Finder"
} else {
    "the file manager"
};

/// The command that opens `target` (not started).
pub fn command(target: &OsStr) -> Command {
    let mut cmd = Command::new(OPENER);
    cmd.arg(target);
    #[cfg(not(windows))]
    {
        // a GUI app started from a terminal: the handler must not read from or write to it
        use std::process::Stdio;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    cmd
}

/// Opens `target` (an absolute folder path or a validated URL) without waiting for it.
pub fn open(target: impl AsRef<OsStr>) -> std::io::Result<()> {
    let child = command(target.as_ref()).spawn()?;
    #[cfg(unix)]
    {
        // `open` / `xdg-open` exit as soon as they handed the target over; reap them so no
        // zombie is left behind for the life of the app
        let mut child = child;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    #[cfg(not(unix))]
    drop(child);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_one_argument_of_the_platform_opener() {
        let url = "https://bitbucket.org/acme/api/pull-requests/7?a=1&b=2;x|y";
        let cmd = command(OsStr::new(url));
        assert_eq!(cmd.get_program(), OsStr::new(OPENER));
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(args, [OsStr::new(url)], "no shell, no splitting");
        let folder = std::env::temp_dir().join("a folder; with & spaces");
        let cmd = command(folder.as_os_str());
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), [folder.as_os_str()]);
    }

    #[test]
    fn each_platform_has_its_opener() {
        let want = if cfg!(windows) {
            ("explorer.exe", "Explorer")
        } else if cfg!(target_os = "macos") {
            ("open", "Finder")
        } else {
            ("xdg-open", "the file manager")
        };
        assert_eq!((OPENER, FILE_MANAGER), want);
    }
}
