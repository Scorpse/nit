//! Settings screen: where repo mirrors go (`download_dir`) and which folder is scanned for
//! existing clones (`clone_root`). The logic (validation, save, review-button rules) is plain
//! Rust in `SettingsState` and the free functions; `show_settings` only draws it.
//!
//! Nothing here touches the filesystem on the UI thread: string checks (absolute path, quotes,
//! nesting) run per keystroke, while checks that look at the disk, folder pickers, git and the
//! clone scan run on worker threads and report back through a channel. Results carry a
//! generation number; a result whose generation is no longer current is dropped.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui::{self, Color32, RichText, Stroke};
use gitlocal::{discover_clones, Git, LocalClone, SkippedRepo, NO_HTTPS_REMOTE};
use prcore::model::{AccountId, Forge, PrState};
use providers::github::GitHubBases;
use providers::gitlab::GitLabBases;
use store::{Settings, StoreWriter};

use crate::review::split::DiffLayout;
use crate::theme;

/// How many levels below `clone_root` the clone scan looks.
const SCAN_DEPTH: usize = 4;
const GIT_MISSING: &str = "git not found / too old (need 2.31 or newer)";
/// An example download folder and clones folder in the platform's path syntax.
const EXAMPLE_DOWNLOAD: &str = if cfg!(windows) {
    r"D:\nit\mirrors"
} else if cfg!(target_os = "macos") {
    "/Users/you/nit/mirrors"
} else {
    "/home/you/nit/mirrors"
};
const EXAMPLE_CLONES: &str = if cfg!(windows) {
    r"D:\code (optional)"
} else if cfg!(target_os = "macos") {
    "/Users/you/code (optional)"
} else {
    "/home/you/code (optional)"
};
const FULL_PATH_HINT: &str = if cfg!(windows) {
    r"Use a full path, e.g. D:\nit\mirrors"
} else if cfg!(target_os = "macos") {
    "Use a full path, e.g. /Users/you/nit/mirrors"
} else {
    "Use a full path, e.g. /home/you/nit/mirrors"
};

pub const REVIEW_NEEDS_SIGN_IN: &str = "Sign in to review this pull request";
pub const REVIEW_NEEDS_GIT: &str = "Git 2.31 or newer is needed to review (install it and restart)";
pub const REVIEW_NEEDS_OPEN: &str = "Only open pull requests can be reviewed here.";

/// `mirrors` in the data folder (`brand::data_dir`; on Windows under the old `prview` folder
/// while it has not been moved, see `brand`); under the temp dir when there is no data folder.
pub fn default_download_dir() -> PathBuf {
    let base = crate::brand::data_base().unwrap_or_else(std::env::temp_dir);
    crate::brand::data_dir_at(&base).join("mirrors")
}

// ---- pure string checks (no filesystem) ------------------------------------------------

/// Trims whitespace and one pair of surrounding double or single quotes (as pasted from
/// Explorer's "Copy as path" or a terminal).
pub fn clean_path_text(text: &str) -> &str {
    let t = text.trim();
    let unquoted = ['"', '\'']
        .iter()
        .find_map(|q| t.strip_prefix(*q).and_then(|r| r.strip_suffix(*q)));
    unquoted.unwrap_or(t).trim()
}

/// Requires a full path: a drive or UNC prefix plus root. Does not look at the disk.
pub fn check_absolute(text: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(clean_path_text(text));
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(FULL_PATH_HINT.into())
    }
}

/// Lower-cased path segments with `.`/`..` resolved and separators unified.
fn segments(path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in path.replace('/', "\\").split('\\') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s.to_lowercase()),
        }
    }
    out
}

/// The mirror folder and the clone folder must be unrelated: neither equal, inside nor an
/// ancestor of the other (Windows paths: case-insensitive, `/` and `\` alike).
pub fn check_not_nested(download: &str, clone_root: &str) -> Result<(), String> {
    let (d, c) = (clean_path_text(download), clean_path_text(clone_root));
    if d.is_empty() || c.is_empty() {
        return Ok(());
    }
    let (ds, cs) = (segments(d), segments(c));
    let n = ds.len().min(cs.len());
    if ds[..n] == cs[..n] {
        return Err(format!(
            "Download folder {d} and clones folder {c} must not be the same or inside one another"
        ));
    }
    Ok(())
}

fn sync_check_download(text: &str) -> Result<PathBuf, String> {
    if clean_path_text(text).is_empty() {
        return Err("Choose a folder".into());
    }
    check_absolute(text)
}

fn sync_check_clone(text: &str) -> Result<Option<PathBuf>, String> {
    if clean_path_text(text).is_empty() {
        return Ok(None);
    }
    check_absolute(text).map(Some)
}

// ---- filesystem checks (call from a worker thread only) --------------------------------

fn probe_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".nit-write-test-{}-{nanos}", std::process::id())
}

/// Proves `dir` accepts new files: creates a uniquely named file (`create_new`) and removes it.
fn check_writable_with(dir: &Path, name: impl FnOnce() -> String) -> Result<(), String> {
    use std::io::Write;
    let probe = dir.join(name());
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|e| format!("Not writable: {e}"))?;
    let written = f.write_all(b"x");
    drop(f);
    let _ = std::fs::remove_file(&probe);
    written.map_err(|e| format!("Not writable: {e}"))
}

fn check_writable(dir: &Path) -> Result<(), String> {
    check_writable_with(dir, probe_name)
}

/// The download folder must be a writable directory, or creatable: its nearest existing
/// ancestor must be a writable directory (Save creates the missing levels).
pub fn fs_check_download(path: &Path) -> Result<(), String> {
    if path.exists() {
        if !path.is_dir() {
            return Err("This is a file, not a folder".into());
        }
        return check_writable(path);
    }
    let Some(existing) = path.ancestors().skip(1).find(|a| a.exists()) else {
        return Err("The drive or parent folder does not exist".into());
    };
    if !existing.is_dir() {
        return Err(format!(
            "Cannot create it: {} is a file, not a folder",
            existing.display()
        ));
    }
    check_writable(existing).map_err(|e| format!("Cannot create it here. {e}"))
}

/// The clone folder must be a readable directory.
pub fn fs_check_clone(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("This folder does not exist".into());
    }
    if !path.is_dir() {
        return Err("This is a file, not a folder".into());
    }
    std::fs::read_dir(path)
        .map(|_| ())
        .map_err(|e| format!("Cannot read this folder: {e}"))
}

/// Free bytes for this user on the drive holding `path` (or its nearest existing ancestor).
pub fn free_space(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|a| a.exists())?;
    free_space_at(existing)
}

#[cfg(windows)]
fn free_space_at(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free: u64 = 0;
    // SAFETY: a NUL-terminated path and an out-parameter that live for the call; the two
    // other out-parameters are optional.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

/// macOS / Unix: `statvfs`, the blocks available to an unprivileged user (`f_bavail`, not
/// the root reserve) times the fragment size.
#[cfg(unix)]
fn free_space_at(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero statvfs is a valid value of this plain C struct
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and an out-parameter that live for the call
    if unsafe { libc::statvfs(path.as_ptr(), &mut s) } != 0 {
        return None;
    }
    // the field types differ per platform (u32 or u64): widen both
    #[allow(clippy::useless_conversion)]
    let free = u64::from(s.f_bavail).saturating_mul(u64::from(s.f_frsize));
    Some(free)
}

#[cfg(not(any(windows, unix)))]
fn free_space_at(_: &Path) -> Option<u64> {
    None
}

/// `fs_check_download` plus what the field shows under it: exists yet? free space?
pub fn fs_probe_download(path: &Path) -> Result<FolderInfo, String> {
    fs_check_download(path)?;
    Ok(FolderInfo {
        exists: path.is_dir(),
        free: free_space(path),
    })
}

/// Everything Save needs, blocking: string checks, disk checks, then creates the download
/// folder. Runs on a worker; the store is not involved.
pub fn run_save_checks(download: &str, clone_root: &str) -> Result<Settings, String> {
    let download_dir = sync_check_download(download)?;
    let clone_root = sync_check_clone(clone_root)?;
    check_not_nested(download, &clone_root_text(&clone_root))?;
    fs_check_download(&download_dir).map_err(|e| format!("Download folder: {e}"))?;
    if let Some(c) = &clone_root {
        fs_check_clone(c).map_err(|e| format!("Clones folder: {e}"))?;
    }
    std::fs::create_dir_all(&download_dir).map_err(|e| format!("Cannot create folder: {e}"))?;
    Ok(Settings {
        clone_root,
        download_dir,
    })
}

fn clone_root_text(c: &Option<PathBuf>) -> String {
    c.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Whether the folder fields differ from what is saved (same folder spelled differently,
/// e.g. other case, `/` or a trailing separator, counts as unchanged).
pub fn storage_changed(saved: (&str, &str), download: &str, clone: &str) -> bool {
    let same = |a: &str, b: &str| segments(clean_path_text(a)) == segments(clean_path_text(b));
    !same(saved.0, download) || !same(saved.1, clone)
}

// ---- pure summary strings (unit tested; the drawing code only shows them) -------------

/// How a status line or chip is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Bad,
    Busy,
    Muted,
}

/// The oldest git nit works with.
pub const GIT_MIN: (u32, u32) = (2, 31);

/// (major, minor, patch) of a `git --version` line such as `git version 2.47.1.windows.1`.
pub fn parse_git_version(line: &str) -> Option<(u32, u32, u32)> {
    let rest = line.trim().strip_prefix("git version ")?;
    let mut nums = rest.split('.').map(|p| p.parse::<u32>().ok());
    let major = nums.next()??;
    let minor = nums.next()??;
    let patch = nums.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// What the About card says about git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitInfo {
    Checking,
    /// Usable; the version as `2.47.1` (or the raw line when it has an unusual shape).
    Ok(String),
    /// Found but older than `GIT_MIN`.
    Old(String),
    Missing,
}

pub fn git_info(status: Option<&Result<String, String>>) -> GitInfo {
    match status {
        None => GitInfo::Checking,
        Some(Err(_)) => GitInfo::Missing,
        Some(Ok(line)) => match parse_git_version(line) {
            Some((a, b, c)) if (a, b) < GIT_MIN => GitInfo::Old(format!("{a}.{b}.{c}")),
            Some((a, b, c)) => GitInfo::Ok(format!("{a}.{b}.{c}")),
            None => GitInfo::Ok(line.trim().to_string()),
        },
    }
}

/// (value, chip text and tone, hint under it) for the git row.
pub fn git_row(info: &GitInfo) -> (String, Option<(&'static str, Tone)>, Option<&'static str>) {
    const INSTALL: &str = "Reviews need Git for Windows 2.31 or newer. Install or update it, \
                           then restart nit.";
    match info {
        GitInfo::Checking => ("checking...".into(), None, None),
        GitInfo::Ok(v) => (v.clone(), Some(("OK", Tone::Good)), None),
        GitInfo::Old(v) => (v.clone(), Some(("too old", Tone::Warn)), Some(INSTALL)),
        GitInfo::Missing => (
            "not found".into(),
            Some(("missing", Tone::Bad)),
            Some(INSTALL),
        ),
    }
}

/// (headline, explanation) of the Repositories card.
pub fn repos_summary(selected: usize, known: usize) -> (String, &'static str) {
    if known == 0 {
        (
            "No repositories listed yet".into(),
            "Refresh the list to see the repositories of your workspaces.",
        )
    } else if selected == 0 {
        (
            format!("None of {known} chosen"),
            "Nothing is synced until you choose repositories.",
        )
    } else {
        (
            format!("{selected} of {known} chosen"),
            "Their pull requests, sizes and comments are synced; the others are not read.",
        )
    }
}

/// Filled part of the progress chip, 0 to 1.
pub fn repos_fraction(selected: usize, known: usize) -> f32 {
    if known == 0 {
        0.0
    } else {
        (selected as f32 / known as f32).clamp(0.0, 1.0)
    }
}

/// wgpu's backend names as people know them.
fn backend_name(raw: &str) -> &str {
    match raw {
        "Dx12" => "DirectX 12",
        "Vulkan" => "Vulkan",
        "Gl" => "OpenGL",
        "Metal" => "Metal",
        "BrowserWebGpu" => "WebGPU",
        other => other,
    }
}

/// `"<adapter> (<Backend>)"` from `gpu::adapter_line` as (adapter, backend); `None` before
/// the window started.
pub fn adapter_parts(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    let open = line.rfind(" (")?;
    let backend = line[open + 2..].strip_suffix(')')?;
    let name = line[..open].trim();
    if name.is_empty() || backend.is_empty() {
        return None;
    }
    Some((name.to_string(), backend_name(backend).to_string()))
}

/// `1536` -> `2 KB`, `5.5 * 2^30` -> `5.5 GB`: steps of 1024, one decimal from GB up.
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    match i {
        0 => format!("{n} bytes"),
        1 | 2 => format!("{v:.0} {}", UNITS[i]),
        _ => format!("{v:.1} {}", UNITS[i]),
    }
}

/// Below this, the download folder's free space is shown as a warning.
pub const LOW_SPACE: u64 = 5 * 1024 * 1024 * 1024;

/// What the disk check found about a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FolderInfo {
    pub exists: bool,
    /// Free bytes on its drive, when known.
    pub free: Option<u64>,
}

/// The line under the download folder after a successful disk check.
pub fn download_line(info: FolderInfo) -> (String, Tone) {
    let mut text = if info.exists {
        "Exists and is writable".to_string()
    } else {
        "Does not exist yet; Save creates it".to_string()
    };
    let mut tone = Tone::Good;
    if let Some(free) = info.free {
        if free < LOW_SPACE {
            tone = Tone::Warn;
            text.push_str(&format!(
                " \u{00b7} only {} free; mirrors of big repositories need more",
                format_bytes(free)
            ));
        } else {
            text.push_str(&format!(" \u{00b7} {} free", format_bytes(free)));
        }
    }
    (text, tone)
}

/// "12 usable clones found · 2 skipped".
pub fn clones_line(clones: usize, skipped: usize) -> String {
    let found = match clones {
        0 => "No usable clones found".to_string(),
        1 => "1 usable clone found".to_string(),
        n => format!("{n} usable clones found"),
    };
    if skipped == 0 {
        found
    } else {
        format!("{found} \u{00b7} {skipped} skipped")
    }
}

/// Up to two capital initials of a display name for the account badge; `?` when empty.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let pick = match words.as_slice() {
        [] => return "?".into(),
        [one] => vec![*one],
        [first, .., last] => vec![*first, *last],
    };
    pick.iter()
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

/// The account's connection, as the Account card shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Conn {
    /// `--demo`: no network.
    Demo,
    Connecting,
    Syncing {
        done: usize,
        total: usize,
    },
    Connected,
    /// Signed in, but the last connect or sync failed with this message.
    Problem(String),
    /// No answer from Bitbucket yet this run (cached data only).
    #[default]
    Offline,
}

/// Picks the connection state from the inbox's sync status.
pub fn connection(
    demo: bool,
    connecting: bool,
    running: bool,
    progress: (usize, usize),
    note: Option<&str>,
    me_known: bool,
) -> Conn {
    if demo {
        Conn::Demo
    } else if connecting {
        Conn::Connecting
    } else if running {
        Conn::Syncing {
            done: progress.0,
            total: progress.1,
        }
    } else if let Some(n) = note {
        Conn::Problem(n.to_string())
    } else if me_known {
        Conn::Connected
    } else {
        Conn::Offline
    }
}

/// (chip text, tone, detail line) of a connection state.
pub fn conn_label(c: &Conn) -> (&'static str, Tone, String) {
    match c {
        Conn::Demo => ("Demo", Tone::Muted, "Demo data, no network".into()),
        Conn::Connecting => (
            "Connecting",
            Tone::Busy,
            "Signing in to Bitbucket...".into(),
        ),
        Conn::Syncing { done, total } => (
            "Syncing",
            Tone::Busy,
            format!("Connected \u{00b7} syncing {done} of {total} repositories"),
        ),
        Conn::Connected => (
            "Connected",
            Tone::Good,
            "Connected to Bitbucket Cloud".into(),
        ),
        Conn::Problem(e) => ("Problem", Tone::Bad, e.clone()),
        Conn::Offline => (
            "Offline",
            Tone::Warn,
            "Not connected: showing cached data".into(),
        ),
    }
}

/// The command the token check runs as.
pub const CHECK_TOKEN_CMD: &str = "nit --check-token";

// ---- accounts ---------------------------------------------------------------------------

/// An account besides the signed-in one, as the Accounts card lists it. Never holds a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherAccount {
    pub id: AccountId,
    pub forge: Forge,
    pub host: String,
    pub name: String,
    pub enabled: bool,
    /// When the account's token expires, where the forge says (GitLab).
    pub expires: Option<chrono::NaiveDate>,
    /// Until when the account's client waits out a rate limit (set by the caller).
    pub paused_until: Option<std::time::SystemTime>,
    /// Reached over plain http (a private address): the token travels unencrypted.
    pub cleartext: bool,
}

/// The chip of an account reached over plain http, None for an https one.
pub fn cleartext_chip(a: &OtherAccount) -> Option<(&'static str, Tone)> {
    a.cleartext.then_some(("http", Tone::Warn))
}

/// The accounts other than the signed-in default one, in store order.
#[cfg(test)]
pub fn other_accounts(all: &[store::Account]) -> Vec<OtherAccount> {
    other_accounts_with(all, |_| None)
}

/// `other_accounts` with each account's token expiry (`expiry`, from the store).
pub fn other_accounts_with(
    all: &[store::Account],
    expiry: impl Fn(&AccountId) -> Option<chrono::NaiveDate>,
) -> Vec<OtherAccount> {
    all.iter()
        .filter(|a| a.id != AccountId::default_bitbucket())
        .map(|a| OtherAccount {
            id: a.id.clone(),
            forge: a.forge,
            host: a.git_host.clone(),
            name: a.display_name.clone(),
            enabled: a.enabled,
            expires: expiry(&a.id),
            paused_until: None,
            cleartext: [&a.web_base, &a.api_base]
                .iter()
                .any(|u| u.to_ascii_lowercase().starts_with("http://")),
        })
        .collect()
}

/// The removal confirmation's summary of what `Store::removal_counts` found.
pub fn removal_summary(c: &store::RemovedCounts) -> String {
    let plural = |n: usize, one: &str| {
        if n == 1 {
            format!("{n} {one}")
        } else {
            format!("{n} {one}s")
        }
    };
    format!(
        "Removes {}, keeps {} marked",
        plural(c.prs, "cached PR"),
        plural(c.drafts_kept, "draft")
    )
}

/// The warning under an account whose token expires within 14 days (spec 5.3), or None.
pub fn account_expiry_status(a: &OtherAccount, today: chrono::NaiveDate) -> Option<(String, Tone)> {
    crate::checktoken::gitlab::expiry_warning(a.expires, today).map(|w| {
        let mut w = w;
        if let Some(first) = w.get(..1) {
            w = format!("{}{}", first.to_uppercase(), &w[1..]);
        }
        (
            format!("{w}: create a new token and add the account again"),
            Tone::Warn,
        )
    })
}

/// (status text, tone) of another account's row.
pub fn other_account_status(a: &OtherAccount) -> (String, Tone) {
    let paused = a.paused_until.filter(|t| *t > std::time::SystemTime::now());
    match (a.enabled, paused) {
        (false, _) => (
            "Disabled: not synced, its repository choice is kept".into(),
            Tone::Muted,
        ),
        (true, Some(t)) => (
            format!(
                "Paused by the rate limit until {}",
                crate::backend::clock(t)
            ),
            Tone::Warn,
        ),
        (true, None) => ("Synced with the other accounts".into(), Tone::Good),
    }
}

/// Shown next to a forge that cannot be added yet.
pub const COMING_SOON: &str = "coming soon";

/// The forges "Add account" offers, and whether each can be picked yet (GitLab is
/// read-only in this version).
pub fn forge_choices() -> [(Forge, bool); 3] {
    [
        (Forge::BitbucketCloud, true),
        (Forge::GitHub, true),
        (Forge::GitLab, true),
    ]
}

/// What a GitLab token needs (spec 5.3), shown under the form.
pub const GITLAB_TOKEN_HELP: &str = "Use a personal access token with the api scope to \
    comment and approve (it also covers git), or read_api and read_repository to only read. \
    Enter gitlab.com, or your self-managed GitLab's web address, including its path if it \
    runs under one (e.g. https://git.example.com/gitlab).";

/// Shown under an http address in the form and on such an account in the list.
pub const CLEARTEXT_WARNING: &str = prcore::origin::CLEARTEXT_WARNING;

/// The web address prefilled for a GitHub account.
pub const GITHUB_DEFAULT_URL: &str = "https://github.com";

/// What a GitHub token needs (spec 5.3), shown under the form.
pub const GITHUB_TOKEN_HELP: &str = "Use a fine-grained personal access token with access to \
    the repositories you review and the permissions Pull requests: read and write, Contents: read (for \
    git) and Metadata: read. A fine-grained token belongs to one owner: add one account per \
    organization, or use a classic token with the repo scope. With SAML single sign-on, \
    authorize the token for the organization first.";

/// A checked "Add account" entry. `Debug` never shows the token.
#[derive(Clone, PartialEq, Eq)]
pub enum NewAccount {
    Bitbucket { email: String, token: String },
    GitHub { bases: GitHubBases, token: String },
    GitLab { bases: GitLabBases, token: String },
}

impl std::fmt::Debug for NewAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NewAccount::Bitbucket { email, .. } => f
                .debug_struct("Bitbucket")
                .field("email", email)
                .field("token", &"***")
                .finish(),
            NewAccount::GitHub { bases, .. } => f
                .debug_struct("GitHub")
                .field("bases", bases)
                .field("token", &"***")
                .finish(),
            NewAccount::GitLab { bases, .. } => f
                .debug_struct("GitLab")
                .field("bases", bases)
                .field("token", &"***")
                .finish(),
        }
    }
}

/// The "Add account" form. Not `Debug`: it holds the typed token until it is submitted.
#[derive(Default)]
pub struct AddAccountForm {
    pub open: bool,
    pub forge: Forge,
    pub email: String,
    /// GitHub's web address (github.com or a GitHub Enterprise host); empty is github.com.
    pub web_url: String,
    pub token: String,
    /// Checking the token right now (the caller clears it when the answer arrives).
    pub busy: bool,
    /// The last outcome to show (e.g. "Added Bo"), or why an add or removal failed.
    pub result: Option<Result<String, String>>,
    /// The answer of "Test connection" (who the token signs in as, or why not).
    pub tested: Option<Result<String, String>>,
    /// "I understand" ticked for a plain-http address.
    pub cleartext_ok: bool,
}

impl AddAccountForm {
    /// A GitHub or GitLab address typed as plain `http://` (shows the cleartext warning).
    pub fn cleartext(&self) -> bool {
        matches!(self.forge, Forge::GitHub | Forge::GitLab)
            && self
                .web_url
                .trim()
                .to_ascii_lowercase()
                .starts_with("http://")
    }

    /// Add and Test connection are offered: an http address needs "I understand" ticked.
    pub fn can_submit(&self) -> bool {
        !self.cleartext() || self.cleartext_ok
    }

    fn check_cleartext(&self) -> Result<(), String> {
        match self.can_submit() {
            true => Ok(()),
            false => Err("Tick \"I understand\" to use an unencrypted http connection".into()),
        }
    }

    /// The cleaned account to add (or test), or why not.
    pub fn submit(&self) -> Result<NewAccount, String> {
        let (enabled, label) = forge_choices()
            .into_iter()
            .find(|(f, _)| *f == self.forge)
            .map_or((false, ""), |(f, on)| (on, f.label()));
        if !enabled {
            return Err(format!("{label} accounts are {COMING_SOON}"));
        }
        let token = crate::welcome::clean_token(&self.token).to_string();
        match self.forge {
            Forge::GitHub => {
                let url = match self.web_url.trim() {
                    "" => GITHUB_DEFAULT_URL,
                    u => u,
                };
                let bases = providers::github::bases_for(url)?;
                self.check_cleartext()?;
                if token.is_empty() {
                    return Err("Paste a personal access token".into());
                }
                Ok(NewAccount::GitHub { bases, token })
            }
            Forge::GitLab => {
                let url = match self.web_url.trim() {
                    "" => providers::gitlab::GITLAB_COM,
                    u => u,
                };
                let bases = providers::gitlab::bases_for(url)?;
                self.check_cleartext()?;
                if token.is_empty() {
                    return Err("Paste a personal access token".into());
                }
                Ok(NewAccount::GitLab { bases, token })
            }
            Forge::BitbucketCloud => {
                let email = crate::welcome::clean_email(&self.email);
                if !email.contains('@') {
                    return Err("Enter the email of your Atlassian account".into());
                }
                if token.is_empty() {
                    return Err("Paste an API token".into());
                }
                Ok(NewAccount::Bitbucket {
                    email: email.to_string(),
                    token,
                })
            }
        }
    }
}

// ---- review button ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewButton {
    Enabled,
    /// Shown greyed out with this hint.
    Disabled(&'static str),
    Hidden,
}

/// Whether the PR detail pane's "Review" button is usable. Forks are not supported (hidden);
/// merged, declined and superseded pull requests cannot be reviewed.
pub fn review_button_state(
    is_fork: bool,
    state: PrState,
    has_creds: bool,
    git_ok: bool,
) -> ReviewButton {
    if is_fork {
        ReviewButton::Hidden
    } else if state != PrState::Open {
        ReviewButton::Disabled(REVIEW_NEEDS_OPEN)
    } else if !has_creds {
        ReviewButton::Disabled(REVIEW_NEEDS_SIGN_IN)
    } else if !git_ok {
        ReviewButton::Disabled(REVIEW_NEEDS_GIT)
    } else {
        ReviewButton::Enabled
    }
}

// ---- state ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    DownloadDir,
    CloneRoot,
}

/// Outcome of the disk checks for one folder field.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum Check {
    #[default]
    Unchecked,
    Checking,
    Ok(FolderInfo),
    Failed(String),
}

#[derive(Default)]
pub struct Folder {
    pub text: String,
    check: Check,
    generation: u64,
}

#[derive(Debug)]
enum Worker {
    /// `None` when the dialog was cancelled.
    Picked(Field, Option<PathBuf>),
    Git(Result<String, String>),
    Checked {
        field: Field,
        generation: u64,
        result: Result<FolderInfo, String>,
    },
    Scanned {
        generation: u64,
        result: Result<(Vec<LocalClone>, Vec<SkippedRepo>), String>,
    },
    Saved {
        generation: u64,
        result: Result<Settings, String>,
    },
    /// The store write of a Save finished.
    Persisted(Result<(), String>),
    /// "Open folder" started Explorer, or why not.
    Opened(Result<(), (&'static str, String)>),
}

#[derive(Default, Debug, PartialEq, Eq)]
enum Scan {
    #[default]
    Idle,
    Running,
    Done {
        clones: usize,
        skipped: Vec<SkippedRepo>,
    },
    Failed(String),
}

pub enum SettingsAction {
    Back,
    /// Open "Choose repositories".
    ChooseRepos,
    /// List the workspaces and repositories again; the caller clears `discovering` when done.
    RefreshRepos,
    /// Confirmed: delete the stored credentials and return to the sign-in screen.
    SignOut,
    /// "Add account" (the credentials are checked by the caller).
    AddAccount(NewAccount),
    /// "Test connection": sign in with it and say as whom, without saving anything.
    TestConnection(NewAccount),
    /// Confirmed: remove this account (not the signed-in one).
    RemoveAccount(AccountId),
    /// Enable (true) or disable another account; its repository choice stays.
    SetEnabled(AccountId, bool),
    /// Give another account this label (empty: back to its login).
    RenameAccount(AccountId, String),
    /// Checked settings ready to be written; the caller owns the store and calls `persist`.
    Persist(Settings),
}

/// Who is signed in, set by the caller each frame. Never holds the token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub email: String,
    pub conn: Conn,
}

pub struct SettingsState {
    pub download: Folder,
    pub clone: Folder,
    default_download: PathBuf,
    /// The folders as last loaded or saved; Save is enabled only when the fields differ.
    saved: (String, String),
    /// Settings handed to the store and not confirmed yet.
    pending: Option<Settings>,
    /// Set when the stored settings could not be read; Save stays blocked.
    pub load_error: Option<String>,
    git: Option<Result<String, String>>,
    scan: Scan,
    scan_generation: u64,
    save_generation: u64,
    saving: bool,
    browsing: bool,
    /// Outcome of the last Save, shown next to the buttons.
    pub message: Option<Result<String, String>>,
    /// (selected, known) repositories, set by the caller each frame.
    pub repos: (usize, usize),
    /// "Refresh list" is running; the caller clears it when the listing ends.
    pub discovering: bool,
    /// Why the last "Refresh list" failed.
    pub repos_error: Option<String>,
    /// The review screen's vim-style keys are on (applied and saved by the caller at once).
    pub vim_keys: bool,
    /// The review screen's diff layout (applied and saved by the caller at once).
    pub layout: DiffLayout,
    /// Set by the caller each frame.
    pub account: Account,
    /// The other accounts (set by the caller when they change).
    pub others: Vec<OtherAccount>,
    pub add: AddAccountForm,
    /// The account whose Remove confirmation is open.
    confirm_remove: Option<OtherAccount>,
    /// What removing it deletes and keeps (`Store::removal_counts`), set by the caller
    /// while the confirmation is open.
    pub removal_counts: Option<Result<store::RemovedCounts, String>>,
    /// The account whose label is being edited, and the text so far.
    renaming: Option<(AccountId, String)>,
    /// The data folder (`brand::data_dir`), shown under About.
    data_dir: Option<PathBuf>,
    /// The Sign out confirmation is open.
    confirm_sign_out: bool,
    /// The copy button pressed last ("Copied" shows next to it).
    copied: Option<&'static str>,
    /// Why "Open" could not start Explorer, tagged with the card it was pressed in.
    open_error: Option<(&'static str, String)>,
    ctx: egui::Context,
    tx: Sender<Worker>,
    rx: Receiver<Worker>,
}

impl SettingsState {
    /// The account whose removal confirmation is open (the caller fills `removal_counts`).
    pub fn pending_removal(&self) -> Option<&AccountId> {
        self.confirm_remove.as_ref().map(|o| &o.id)
    }

    /// Does no I/O; call `start` to run the first checks in the background.
    pub fn new(
        ctx: egui::Context,
        current: Result<&Settings, String>,
        default_download: PathBuf,
    ) -> Self {
        let (tx, rx) = channel();
        let (download, clone, load_error) = match current {
            Ok(s) => (
                s.download_dir.display().to_string(),
                clone_root_text(&s.clone_root),
                None,
            ),
            Err(e) => (
                default_download.display().to_string(),
                String::new(),
                Some(e),
            ),
        };
        SettingsState {
            saved: (download.clone(), clone.clone()),
            download: Folder {
                text: download,
                ..Folder::default()
            },
            clone: Folder {
                text: clone,
                ..Folder::default()
            },
            default_download,
            pending: None,
            load_error,
            git: None,
            scan: Scan::Idle,
            scan_generation: 0,
            save_generation: 0,
            saving: false,
            browsing: false,
            message: None,
            repos: (0, 0),
            discovering: false,
            repos_error: None,
            vim_keys: true,
            layout: DiffLayout::Unified,
            account: Account::default(),
            others: Vec::new(),
            add: AddAccountForm::default(),
            confirm_remove: None,
            removal_counts: None,
            renaming: None,
            data_dir: crate::brand::data_dir(),
            confirm_sign_out: false,
            copied: None,
            open_error: None,
            ctx,
            tx,
            rx,
        }
    }

    /// Fixed content for `--demo` screenshots: no processes, no folders touched.
    /// Paths are in the host's syntax, so the demo's own checks accept them everywhere.
    pub fn demo(ctx: egui::Context) -> Self {
        let p =
            |windows: &str, unix: &str| PathBuf::from(if cfg!(windows) { windows } else { unix });
        let current = Settings {
            clone_root: Some(p(r"D:\code", "/home/you/code")),
            download_dir: p(
                r"C:\Users\you\AppData\Local\nit\mirrors",
                "/home/you/.local/share/nit/mirrors",
            ),
        };
        let mut s = SettingsState::new(ctx, Ok(&current), default_download_dir());
        s.download.check = Check::Ok(FolderInfo {
            exists: true,
            free: Some(214 * 1024 * 1024 * 1024 + 640 * 1024 * 1024),
        });
        s.clone.check = Check::Ok(FolderInfo {
            exists: true,
            free: None,
        });
        s.data_dir = Some(p(
            r"C:\Users\you\AppData\Local\nit",
            "/home/you/.local/share/nit",
        ));
        s.git = Some(Ok("git version 2.47.1.windows.1".into()));
        s.scan = Scan::Done {
            clones: 12,
            skipped: vec![
                SkippedRepo {
                    path: p(r"D:\code\legacy-tools", "/home/you/code/legacy-tools"),
                    reason: NO_HTTPS_REMOTE.into(),
                },
                SkippedRepo {
                    path: p(r"D:\code\infra\terraform", "/home/you/code/infra/terraform"),
                    reason: NO_HTTPS_REMOTE.into(),
                },
            ],
        };
        s
    }

    /// `demo` with unsaved edits that need attention: a download folder still to be created on
    /// a nearly full drive, a relative clones path (Save blocked) and a git that is too old.
    pub fn demo_problems(ctx: egui::Context) -> Self {
        let mut s = SettingsState::demo(ctx);
        s.download.text = if cfg!(windows) {
            r"E:\nit\mirrors"
        } else {
            "/mnt/data/nit/mirrors"
        }
        .into();
        s.download.check = Check::Ok(FolderInfo {
            exists: false,
            free: Some(3 * 1024 * 1024 * 1024 + 300 * 1024 * 1024),
        });
        s.clone.text = "code".into();
        s.clone.check = Check::Unchecked;
        s.scan = Scan::Idle;
        s.git = Some(Ok("git version 2.30.1.windows.1".into()));
        s
    }

    /// Starts the background git check, the disk checks and, when a clone folder is set, the scan.
    pub fn start(&mut self) {
        self.job(|| Worker::Git(git_status()));
        self.request_check(Field::DownloadDir);
        self.request_check(Field::CloneRoot);
        self.rescan();
    }

    pub fn git_ok(&self) -> bool {
        matches!(self.git, Some(Ok(_)))
    }

    fn job(&self, f: impl FnOnce() -> Worker + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
    }

    fn folder(&mut self, f: Field) -> &mut Folder {
        match f {
            Field::DownloadDir => &mut self.download,
            Field::CloneRoot => &mut self.clone,
        }
    }

    /// The string-only error for a field, cheap enough for every frame.
    fn sync_error(&self, f: Field) -> Option<String> {
        let nested = check_not_nested(&self.download.text, &self.clone.text).err();
        match f {
            Field::DownloadDir => sync_check_download(&self.download.text).err().or(nested),
            Field::CloneRoot => sync_check_clone(&self.clone.text).err().or(nested),
        }
    }

    /// Text of a field changed: its old disk result and (for the clone folder) scan are stale.
    fn edited(&mut self, f: Field) {
        let folder = self.folder(f);
        folder.generation += 1;
        folder.check = Check::Unchecked;
        if f == Field::CloneRoot {
            self.scan_generation += 1;
            self.scan = Scan::Idle;
        }
        self.message = None;
    }

    /// Cleans the text and checks the disk for one field on a worker thread.
    fn request_check(&mut self, f: Field) {
        if let Some(e) = self.sync_error(f) {
            self.folder(f).check = Check::Failed(e);
            return;
        }
        let folder = self.folder(f);
        folder.text = clean_path_text(&folder.text).to_string();
        folder.generation += 1;
        let generation = folder.generation;
        let path = match f {
            Field::DownloadDir => sync_check_download(&folder.text).map(Some),
            Field::CloneRoot => sync_check_clone(&folder.text),
        };
        let Ok(Some(path)) = path else {
            folder.check = Check::Unchecked;
            return;
        };
        folder.check = Check::Checking;
        self.job(move || Worker::Checked {
            field: f,
            generation,
            result: match f {
                Field::DownloadDir => fs_probe_download(&path),
                Field::CloneRoot => fs_check_clone(&path).map(|()| FolderInfo {
                    exists: true,
                    free: None,
                }),
            },
        });
    }

    /// The folder fields differ from what is saved.
    pub fn changed(&self) -> bool {
        storage_changed(
            (&self.saved.0, &self.saved.1),
            &self.download.text,
            &self.clone.text,
        )
    }

    /// Why Save cannot be pressed (string checks only; the disk is checked by Save itself).
    fn save_blocked(&self) -> Option<&'static str> {
        if self.load_error.is_some() {
            Some("The saved settings could not be read")
        } else if self.saving {
            Some("Saving...")
        } else if !self.changed() {
            Some("Nothing changed")
        } else if self.sync_error(Field::DownloadDir).is_some()
            || self.sync_error(Field::CloneRoot).is_some()
        {
            Some("Fix the folder above to save")
        } else {
            None
        }
    }

    fn can_save(&self) -> bool {
        self.save_blocked().is_none()
    }

    /// Runs the disk checks on a worker; the result comes back as `Persist` from `poll`.
    pub fn begin_save(&mut self) {
        if !self.can_save() {
            return;
        }
        self.save_generation += 1;
        self.scan_generation += 1;
        self.saving = true;
        self.message = None;
        let generation = self.save_generation;
        let (d, c) = (self.download.text.clone(), self.clone.text.clone());
        self.job(move || Worker::Saved {
            generation,
            result: run_save_checks(&d, &c),
        });
    }

    /// Writes checked settings on the store's writer thread (the UI thread never waits for
    /// the database); the outcome arrives through `poll`.
    pub fn persist(&mut self, settings: &Settings, writer: &StoreWriter) {
        self.pending = Some(settings.clone());
        let (tx, ctx, settings) = (self.tx.clone(), self.ctx.clone(), settings.clone());
        writer.submit(move |store| {
            let result = store.save_settings(&settings).map_err(|e| e.to_string());
            let _ = tx.send(Worker::Persisted(result));
            ctx.request_repaint();
        });
    }

    /// Puts the default folders into the fields; nothing is saved until Save.
    pub fn reset_to_default(&mut self) {
        self.download.text = self.default_download.display().to_string();
        self.clone.text.clear();
        self.download.generation += 1;
        self.clone.generation += 1;
        self.save_generation += 1;
        self.scan_generation += 1;
        self.saving = false;
        self.download.check = Check::Unchecked;
        self.clone.check = Check::Unchecked;
        self.scan = Scan::Idle;
        self.message = None;
        self.request_check(Field::DownloadDir);
    }

    /// The fields already hold the defaults (Reset would change nothing).
    fn at_default(&self) -> bool {
        !storage_changed(
            (&self.default_download.display().to_string(), ""),
            &self.download.text,
            &self.clone.text,
        )
    }

    /// Scans `clone_root` on a worker thread. A scan still running is superseded: its result
    /// arrives with an older generation and is ignored.
    pub fn rescan(&mut self) {
        self.scan_generation += 1;
        let generation = self.scan_generation;
        let Ok(Some(root)) = sync_check_clone(&self.clone.text) else {
            self.scan = Scan::Idle;
            return;
        };
        if self.sync_error(Field::CloneRoot).is_some() {
            self.scan = Scan::Idle;
            return;
        }
        self.scan = Scan::Running;
        self.job(move || Worker::Scanned {
            generation,
            result: fs_check_clone(&root)
                .and_then(|_| Git::shared().map_err(|e| e.to_string()))
                .map(|git| {
                    let found = discover_clones(&git, &root, SCAN_DEPTH);
                    // an explicit rescan replaces what reviews use
                    crate::review::remember_clones(&root, &found.0);
                    found
                }),
        });
    }

    /// Opens the native folder dialog on a worker thread; ignored while one is open.
    fn browse(&mut self, f: Field) {
        if self.browsing {
            return;
        }
        self.browsing = true;
        let start = match f {
            Field::DownloadDir => self.download.text.clone(),
            Field::CloneRoot => self.clone.text.clone(),
        };
        let start = clean_path_text(&start).to_string();
        self.job(move || {
            let mut dialog = rfd::FileDialog::new();
            if Path::new(&start).is_absolute() && Path::new(&start).is_dir() {
                dialog = dialog.set_directory(&start);
            }
            Worker::Picked(f, dialog.pick_folder())
        });
    }

    /// Shows `path` in Explorer (from a worker: Explorer is started there); `card` says where a
    /// failure is shown.
    fn open_folder(&mut self, card: &'static str, path: PathBuf) {
        self.open_error = None;
        self.job(move || Worker::Opened(open_in_explorer(&path).map_err(|e| (card, e))));
    }

    /// Applies one worker message. Returns settings to persist when a Save finished its checks.
    fn apply(&mut self, m: Worker) -> Option<Settings> {
        match m {
            Worker::Picked(f, path) => {
                self.browsing = false;
                if let Some(p) = path {
                    self.folder(f).text = p.display().to_string();
                    self.edited(f);
                    self.request_check(f);
                    if f == Field::CloneRoot {
                        self.rescan();
                    }
                }
            }
            Worker::Git(r) => self.git = Some(r),
            Worker::Checked {
                field,
                generation,
                result,
            } => {
                let folder = self.folder(field);
                if folder.generation == generation {
                    folder.check = match result {
                        Ok(info) => Check::Ok(info),
                        Err(e) => Check::Failed(e),
                    };
                }
            }
            Worker::Scanned { generation, result } => {
                if generation == self.scan_generation {
                    self.scan = match result {
                        Ok((clones, skipped)) => Scan::Done {
                            clones: clones.len(),
                            skipped,
                        },
                        Err(e) => Scan::Failed(e),
                    };
                }
            }
            Worker::Persisted(result) => match result {
                Ok(()) => {
                    if let Some(s) = self.pending.take() {
                        self.saved = (
                            s.download_dir.display().to_string(),
                            clone_root_text(&s.clone_root),
                        );
                    }
                    self.message = Some(Ok("Saved".into()));
                    self.rescan();
                }
                Err(e) => {
                    self.pending = None;
                    self.message = Some(Err(format!("Could not save the settings ({e})")));
                }
            },
            Worker::Saved { generation, result } => {
                if generation == self.save_generation {
                    self.saving = false;
                    match result {
                        Ok(s) => return Some(s),
                        Err(e) => self.message = Some(Err(e)),
                    }
                }
            }
            Worker::Opened(r) => self.open_error = r.err(),
        }
        None
    }

    /// Applies results from the worker threads; call once per frame.
    pub fn poll(&mut self) -> Option<Settings> {
        let mut persist = None;
        while let Ok(m) = self.rx.try_recv() {
            persist = self.apply(m).or(persist);
        }
        persist
    }
}

/// Version line of `git --version`, or the reason it cannot be used.
fn git_status() -> Result<String, String> {
    let git = Git::shared().map_err(|_| GIT_MISSING.to_string())?;
    let out = git
        .run(Path::new("."), &["--version"], None)
        .map_err(|_| GIT_MISSING.to_string())?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// Shows an existing folder in the file manager (Explorer, Finder, `xdg-open`); the path goes
/// to the opener as one argument (no shell involved, see `open`).
fn open_in_explorer(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || !path.is_dir() {
        return Err(format!("{} does not exist yet", path.display()));
    }
    crate::open::open(path).map_err(|_| "Could not open the folder".to_string())
}

// ---- drawing ----------------------------------------------------------------------------

/// The centred column all cards sit in.
const COLUMN_W: f32 = 760.0;
const CARD_PAD: i8 = 18;
/// Label column of the About rows.
const LABEL_W: f32 = 104.0;
/// Fixed widths so the path field can take exactly the rest of the row.
const BROWSE_W: f32 = 84.0;
const OPEN_W: f32 = 60.0;
const ROW_H: f32 = 28.0;

fn tone_color(t: Tone) -> Color32 {
    match t {
        Tone::Good => theme::GREEN,
        Tone::Warn => theme::AMBER,
        Tone::Bad => theme::RED,
        Tone::Busy => theme::ACCENT,
        Tone::Muted => theme::MUTED,
    }
}

/// Lays `add` out in a column at most `COLUMN_W` wide, centred in the available width.
fn column<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let full = ui.available_rect_before_wrap();
    let w = COLUMN_W.min(full.width() - 32.0).max(240.0);
    let rect = egui::Rect::from_min_size(
        egui::pos2((full.center().x - w / 2.0).round(), full.top()),
        egui::vec2(w, full.height()),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.set_width(w);
            add(ui)
        },
    )
    .inner
}

/// A small rounded label with a tinted fill.
fn chip(ui: &mut egui::Ui, text: &str, tone: Tone) {
    let c = tone_color(tone);
    egui::Frame::new()
        .fill(c.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, c.gamma_multiply(0.55)))
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(8, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).color(c));
        });
}

/// One status line: a coloured dot (a spinner while busy) and wrapped text.
fn status_line(ui: &mut egui::Ui, tone: Tone, text: &str) {
    ui.horizontal(|ui| {
        if tone == Tone::Busy {
            ui.add(egui::Spinner::new().size(12.0));
        } else {
            let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(r.center(), 3.5, tone_color(tone));
        }
        let color = match tone {
            Tone::Good | Tone::Busy => theme::TEXT,
            Tone::Muted => theme::MUTED,
            t => tone_color(t),
        };
        ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
    });
}

fn muted(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(theme::MUTED)).wrap());
}

/// The accent-filled button of a card (greyed when disabled).
fn primary_button(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    let fill = if enabled {
        theme::ACCENT_FILL
    } else {
        ui.visuals().widgets.inactive.weak_bg_fill
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).strong().color(Color32::WHITE))
            .fill(fill)
            .min_size(egui::vec2(0.0, ROW_H)),
    )
}

fn button(ui: &mut egui::Ui, enabled: bool, text: &str, width: f32) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(text).min_size(egui::vec2(width, ROW_H)),
    )
}

/// A card: rounded frame, title (with an optional chip on the right), one-line description,
/// then `body`.
fn card<R>(
    ui: &mut egui::Ui,
    title: &str,
    desc: &str,
    badge: Option<(&str, Tone)>,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let r = egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(10)
        .inner_margin(egui::Margin::same(CARD_PAD))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(title)
                        .size(16.0)
                        .strong()
                        .color(Color32::WHITE),
                );
                if let Some((text, tone)) = badge {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        chip(ui, text, tone)
                    });
                }
            });
            muted(ui, desc);
            ui.add_space(12.0);
            body(ui)
        })
        .inner;
    ui.add_space(14.0);
    r
}

/// One row of the About card: a muted label on the left, `value` on the right.
fn about_row(ui: &mut egui::Ui, label: &str, value: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_W, 18.0),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| {
                ui.set_min_width(LABEL_W);
                ui.label(RichText::new(label).color(theme::MUTED));
            },
        );
        ui.vertical(|ui| value(ui));
    });
    ui.add_space(10.0);
}

/// What the user did with one folder row this frame.
#[derive(Default)]
struct RowInput {
    changed: bool,
    blurred: bool,
    browse: bool,
    open: bool,
}

/// Label, description, path field with Browse... and Open, and the check result under it.
fn folder_row(
    ui: &mut egui::Ui,
    label: &str,
    desc: &str,
    hint: &str,
    folder: &mut Folder,
    sync_error: Option<String>,
    browse_enabled: bool,
) -> RowInput {
    let mut input = RowInput::default();
    ui.label(RichText::new(label).strong());
    muted(ui, desc);
    ui.add_space(2.0);
    let can_open = matches!(folder.check, Check::Ok(FolderInfo { exists: true, .. }));
    ui.horizontal(|ui| {
        let spacing = ui.spacing().item_spacing.x;
        let w = (ui.available_width() - BROWSE_W - OPEN_W - 2.0 * spacing).max(120.0);
        let r = ui.add(
            egui::TextEdit::singleline(&mut folder.text)
                .desired_width(w)
                .margin(egui::vec2(8.0, 5.0))
                .hint_text(hint),
        );
        input.changed = r.changed();
        input.blurred = r.lost_focus();
        input.browse = button(ui, browse_enabled, "Browse...", BROWSE_W)
            .on_hover_text("Pick the folder in a dialog")
            .clicked();
        input.open = button(ui, can_open, "Open", OPEN_W)
            .on_hover_text(format!("Show the folder in {}", crate::open::FILE_MANAGER))
            .on_disabled_hover_text("The folder does not exist yet")
            .clicked();
    });
    ui.add_space(2.0);
    match (&sync_error, &folder.check) {
        (Some(e), _) | (None, Check::Failed(e)) => status_line(ui, Tone::Bad, e),
        (None, Check::Checking) => status_line(ui, Tone::Busy, "Checking the folder..."),
        (None, Check::Unchecked | Check::Ok(_)) => {}
    }
    input
}

/// Draws the screen. Returns what the caller must act on this frame.
pub fn show_settings(root: &mut egui::Ui, state: &mut SettingsState) -> Option<SettingsAction> {
    let ctx = root.ctx().clone();
    // read before any widget runs: Esc in a text field only ends the edit
    let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape))
        && ctx.memory(|m| m.focused().is_none())
        && !state.confirm_sign_out
        && state.confirm_remove.is_none();
    let mut action = state.poll().map(SettingsAction::Persist);
    if esc {
        action = Some(SettingsAction::Back);
    }
    egui::Panel::top("settings-bar").show(root, |ui| {
        ui.add_space(6.0);
        column(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button("< Back")
                    .on_hover_text("Back to the inbox (Esc)")
                    .clicked()
                {
                    action = Some(SettingsAction::Back);
                }
                ui.add_space(4.0);
                ui.heading("Settings");
            });
        });
        ui.add_space(6.0);
    });
    egui::CentralPanel::default().show(root, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                column(ui, |ui| {
                    ui.add_space(14.0);
                    if let Some(e) = &state.load_error {
                        status_line(
                            ui,
                            Tone::Bad,
                            &format!(
                                "Could not read the saved settings ({e}). Saving is disabled."
                            ),
                        );
                        ui.add_space(8.0);
                    }
                    if let Some(a) = account_card(ui, state) {
                        action = Some(a);
                    }
                    repos_card(ui, state, &mut action);
                    storage_card(ui, state);
                    review_card(ui, state);
                    about_card(ui, state);
                    ui.add_space(10.0);
                });
            });
    });
    if let Some(other) = state.confirm_remove.clone() {
        match remove_confirm(&ctx, &other, state.removal_counts.as_ref()) {
            Some(true) => {
                state.confirm_remove = None;
                state.removal_counts = None;
                action = Some(SettingsAction::RemoveAccount(other.id));
            }
            Some(false) => {
                state.confirm_remove = None;
                state.removal_counts = None;
            }
            None => {}
        }
    }
    if state.confirm_sign_out {
        match sign_out_confirm(&ctx) {
            Some(true) => {
                state.confirm_sign_out = false;
                action = Some(SettingsAction::SignOut);
            }
            Some(false) => state.confirm_sign_out = false,
            None => {}
        }
    }
    action
}

/// The round initials badge of an account row.
fn account_badge(ui: &mut egui::Ui, name: &str, size: f32) {
    let (badge, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    ui.painter()
        .circle_filled(badge.center(), size / 2.0, theme::ACCENT_FILL);
    ui.painter().text(
        badge.center(),
        egui::Align2::CENTER_CENTER,
        initials(name),
        egui::FontId::proportional(size * 0.36),
        Color32::WHITE,
    );
}

/// The forge tag and host of an account, on one line.
fn forge_line(ui: &mut egui::Ui, forge: Forge, host: &str) {
    ui.horizontal(|ui| {
        chip(ui, forge.label(), Tone::Muted);
        ui.label(RichText::new(host).color(theme::MUTED));
    });
}

/// The Accounts card: the signed-in account, the others, and "Add account". Returns an
/// action for the caller (an account to add).
fn account_card(ui: &mut egui::Ui, state: &mut SettingsState) -> Option<SettingsAction> {
    let (chip_text, tone, detail) = conn_label(&state.account.conn);
    let mut action = None;
    card(
        ui,
        "Accounts",
        "The accounts nit reads and comments as. Each repository syncs through one of them.",
        Some((chip_text, tone)),
        |ui| {
            ui.horizontal(|ui| {
                let name = state.account.name.trim();
                account_badge(ui, name, 44.0);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    let shown = if name.is_empty() {
                        "Signed-in user not known yet"
                    } else {
                        name
                    };
                    ui.label(
                        RichText::new(shown)
                            .size(15.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                    let email = state.account.email.trim();
                    if !email.is_empty() {
                        ui.label(RichText::new(email).color(theme::MUTED));
                    }
                    forge_line(ui, Forge::BitbucketCloud, prcore::model::BITBUCKET_HOST);
                    status_line(ui, tone, &detail);
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button(ui, true, "Sign out...", 0.0)
                        .on_hover_text(format!(
                            "Delete the saved email and token from {}",
                            store::CREDENTIAL_STORE
                        ))
                        .clicked()
                    {
                        state.confirm_sign_out = true;
                    }
                });
            });
            for other in state.others.clone() {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                let name = if other.name.trim().is_empty() {
                    other.id.as_str()
                } else {
                    other.name.trim()
                };
                ui.horizontal(|ui| {
                    account_badge(ui, &other.name, 36.0);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        match state.renaming.as_mut().filter(|(id, _)| *id == other.id) {
                            Some((_, text)) => {
                                let mut done = None;
                                ui.horizontal(|ui| {
                                    let edit = ui.add(
                                        egui::TextEdit::singleline(text)
                                            .desired_width(200.0)
                                            .margin(egui::vec2(8.0, 4.0))
                                            .hint_text("Label (empty: the login)"),
                                    );
                                    let enter = edit.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    if button(ui, true, "Save", 0.0).clicked() || enter {
                                        done = Some(true);
                                    }
                                    if button(ui, true, "Cancel", 0.0).clicked() {
                                        done = Some(false);
                                    }
                                });
                                match done {
                                    Some(true) => {
                                        action = Some(SettingsAction::RenameAccount(
                                            other.id.clone(),
                                            text.trim().to_string(),
                                        ));
                                        state.renaming = None;
                                    }
                                    Some(false) => state.renaming = None,
                                    None => {}
                                }
                            }
                            None => {
                                ui.label(RichText::new(name).strong().color(Color32::WHITE));
                            }
                        }
                        ui.horizontal(|ui| {
                            forge_line(ui, other.forge, &other.host);
                            if let Some((text, tone)) = cleartext_chip(&other) {
                                chip(ui, text, tone);
                            }
                        });
                        if other.cleartext {
                            status_line(ui, Tone::Warn, CLEARTEXT_WARNING);
                        }
                        let (text, tone) = other_account_status(&other);
                        status_line(ui, tone, &text);
                        let today = chrono::Local::now().date_naive();
                        if let Some((warn, tone)) = account_expiry_status(&other, today) {
                            status_line(ui, tone, &warn);
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, true, "Remove...", 0.0)
                            .on_hover_text(
                                "Delete this account's saved credentials and its cached data",
                            )
                            .clicked()
                        {
                            state.confirm_remove = Some(other.clone());
                            state.removal_counts = None;
                        }
                        let (toggle, hint) = if other.enabled {
                            ("Disable", "Stop syncing it; its repository choice is kept")
                        } else {
                            ("Enable", "Sync it again with its kept repository choice")
                        };
                        if button(ui, true, toggle, 0.0).on_hover_text(hint).clicked() {
                            action =
                                Some(SettingsAction::SetEnabled(other.id.clone(), !other.enabled));
                        }
                        if button(ui, state.renaming.is_none(), "Rename", 0.0)
                            .on_hover_text("The name this account is shown by")
                            .clicked()
                        {
                            state.renaming = Some((other.id.clone(), name.to_string()));
                        }
                    });
                });
            }
            ui.add_space(10.0);
            if state.add.open {
                action = add_account_form(ui, &mut state.add);
            } else {
                ui.horizontal(|ui| {
                    if button(ui, true, "Add account", 0.0)
                        .on_hover_text("Sign in with another account")
                        .clicked()
                    {
                        state.add.open = true;
                        state.add.result = None;
                    }
                    match &state.add.result {
                        Some(Ok(note)) => status_line(ui, Tone::Good, note),
                        Some(Err(e)) => status_line(ui, Tone::Bad, e),
                        None => {}
                    }
                });
            }
        },
    );
    action
}

/// The inline "Add account" form: forge, email, API token (masked), Add / Cancel.
fn add_account_form(ui: &mut egui::Ui, form: &mut AddAccountForm) -> Option<SettingsAction> {
    let mut action = None;
    ui.label(RichText::new("Add account").strong());
    ui.horizontal(|ui| {
        for (forge, enabled) in forge_choices() {
            let text = if enabled {
                forge.label().to_string()
            } else {
                format!("{} ({COMING_SOON})", forge.label())
            };
            ui.add_enabled_ui(enabled && !form.busy, |ui| {
                ui.radio_value(&mut form.forge, forge, text)
                    .on_disabled_hover_text(format!(
                        "{} accounts arrive in a later version",
                        forge.label()
                    ));
            });
        }
    });
    ui.add_space(4.0);
    let github = form.forge == Forge::GitHub;
    let gitlab = form.forge == Forge::GitLab;
    let pat = github || gitlab;
    ui.add_enabled_ui(!form.busy, |ui| {
        if github {
            ui.add(
                egui::TextEdit::singleline(&mut form.web_url)
                    .desired_width(320.0)
                    .margin(egui::vec2(8.0, 5.0))
                    .hint_text(GITHUB_DEFAULT_URL),
            )
            .on_hover_text(
                "github.com, or the https:// address of your GitHub Enterprise server \
                 (http:// only on a private or local address)",
            );
        } else if gitlab {
            ui.add(
                egui::TextEdit::singleline(&mut form.web_url)
                    .desired_width(320.0)
                    .margin(egui::vec2(8.0, 5.0))
                    .hint_text(providers::gitlab::GITLAB_COM),
            )
            .on_hover_text(
                "gitlab.com, or the https:// address of your self-managed GitLab (with its \
                 path, e.g. https://git.example.com/gitlab; http:// only on a private or \
                 local address)",
            );
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut form.email)
                    .desired_width(320.0)
                    .margin(egui::vec2(8.0, 5.0))
                    .hint_text("Atlassian account email"),
            );
        }
        ui.add(
            egui::TextEdit::singleline(&mut form.token)
                .password(true)
                .desired_width(320.0)
                .margin(egui::vec2(8.0, 5.0))
                .hint_text(if pat {
                    "Personal access token"
                } else {
                    "API token"
                }),
        );
    });
    if form.cleartext() {
        ui.add_space(2.0);
        ui.add(
            egui::Label::new(
                RichText::new(CLEARTEXT_WARNING)
                    .strong()
                    .color(theme::AMBER),
            )
            .wrap(),
        );
        ui.add_enabled_ui(!form.busy, |ui| {
            ui.checkbox(
                &mut form.cleartext_ok,
                "I understand: use this unencrypted connection anyway",
            );
        });
    }
    if github {
        ui.add(egui::Label::new(RichText::new(GITHUB_TOKEN_HELP).small().weak()).wrap());
    } else if gitlab {
        ui.add(egui::Label::new(RichText::new(GITLAB_TOKEN_HELP).small().weak()).wrap());
    }
    ui.add_space(4.0);
    let can = !form.busy && form.can_submit();
    ui.horizontal(|ui| {
        if primary_button(ui, can, "Add").clicked() {
            match form.submit() {
                Ok(new) => {
                    form.busy = true;
                    form.result = None;
                    form.tested = None;
                    form.token.clear();
                    action = Some(SettingsAction::AddAccount(new));
                }
                Err(e) => form.result = Some(Err(e)),
            }
        }
        if pat && button(ui, can, "Test connection", 0.0).clicked() {
            match form.submit() {
                Ok(new) => {
                    form.busy = true;
                    form.result = None;
                    form.tested = None;
                    action = Some(SettingsAction::TestConnection(new));
                }
                Err(e) => form.result = Some(Err(e)),
            }
        }
        if button(ui, !form.busy, "Cancel", 0.0).clicked() {
            form.open = false;
            form.cleartext_ok = false;
            form.token.clear();
            form.result = None;
            form.tested = None;
        }
        if form.busy {
            status_line(ui, Tone::Busy, "Checking the token...");
        }
    });
    match &form.tested {
        Some(Ok(m)) => status_line(ui, Tone::Good, m),
        Some(Err(e)) => status_line(ui, Tone::Bad, e),
        None => {}
    }
    if let Some(Err(e)) = &form.result {
        status_line(ui, Tone::Bad, e);
    }
    action
}

/// The Remove question for another account; `Some(true)` confirmed.
fn remove_confirm(
    ctx: &egui::Context,
    other: &OtherAccount,
    counts: Option<&Result<store::RemovedCounts, String>>,
) -> Option<bool> {
    let mut answer = None;
    let resp = egui::Modal::new(egui::Id::unique("settings-remove-account")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading(format!("Remove {}?", other.name));
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(format!(
                "nit deletes this {} account's saved credentials from Windows Credential \
                 Manager, its repository choices and its cached pull requests. Drafts stay on \
                 this computer. The other accounts are not touched.",
                other.forge.label()
            ))
            .wrap(),
        );
        ui.add_space(6.0);
        match counts {
            Some(Ok(c)) => status_line(ui, Tone::Warn, &removal_summary(c)),
            Some(Err(e)) => status_line(ui, Tone::Bad, e),
            None => status_line(ui, Tone::Busy, "Counting what it removes..."),
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let remove = ui.add(
                egui::Button::new(RichText::new("Remove").strong().color(Color32::WHITE))
                    .fill(theme::RED.gamma_multiply(0.75))
                    .min_size(egui::vec2(0.0, ROW_H)),
            );
            if remove.clicked() {
                answer = Some(true);
            }
            if button(ui, true, "Cancel", 0.0).clicked() {
                answer = Some(false);
            }
        });
    });
    if answer.is_none() && resp.should_close() {
        answer = Some(false);
    }
    answer
}

/// The Sign out question; `Some(true)` confirmed, `Some(false)` cancelled.
fn sign_out_confirm(ctx: &egui::Context) -> Option<bool> {
    let mut answer = None;
    let resp = egui::Modal::new(egui::Id::unique("settings-sign-out")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading("Sign out?");
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(format!(
                "nit deletes the saved email and API token from {} and returns to the \
                 sign-in screen. Cached pull requests, drafts and these settings stay on this \
                 computer.",
                store::CREDENTIAL_STORE
            ))
            .wrap(),
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let sign_out = ui.add(
                egui::Button::new(RichText::new("Sign out").strong().color(Color32::WHITE))
                    .fill(theme::RED.gamma_multiply(0.75))
                    .min_size(egui::vec2(0.0, ROW_H)),
            );
            if sign_out.clicked() {
                answer = Some(true);
            }
            if button(ui, true, "Cancel", 0.0).clicked() {
                answer = Some(false);
            }
        });
    });
    if answer.is_none() && resp.should_close() {
        answer = Some(false);
    }
    answer
}

/// The progress-style chip: the chosen share as a filled bar with "3 / 12" on it.
fn repos_meter(ui: &mut egui::Ui, selected: usize, known: usize) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(132.0, 24.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 12.0, theme::BG_DEEP);
    let frac = repos_fraction(selected, known);
    if frac > 0.0 {
        let w = (rect.width() * frac).max(24.0);
        let fill = egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height()));
        p.rect_filled(fill, 12.0, theme::ACCENT_FILL);
    }
    p.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Inside,
    );
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{selected} / {known}"),
        egui::FontId::proportional(12.5),
        Color32::WHITE,
    );
}

fn repos_card(ui: &mut egui::Ui, state: &mut SettingsState, action: &mut Option<SettingsAction>) {
    let (selected, known) = state.repos;
    let (headline, detail) = repos_summary(selected, known);
    card(
        ui,
        "Repositories",
        "Which of your workspaces' repositories nit keeps in sync.",
        None,
        |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&headline)
                            .size(22.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                    let tone = if known > 0 && selected == 0 {
                        Tone::Warn
                    } else {
                        Tone::Muted
                    };
                    ui.label(RichText::new(detail).color(tone_color(tone)));
                });
                if known > 0 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        repos_meter(ui, selected, known)
                    });
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if primary_button(ui, true, "Choose repositories...")
                    .on_hover_text("Tick the repositories to sync")
                    .clicked()
                {
                    *action = Some(SettingsAction::ChooseRepos);
                }
                if button(ui, !state.discovering, "Refresh list", 0.0)
                    .on_hover_text(
                        "List your workspaces and repositories again (no pull requests are read)",
                    )
                    .clicked()
                {
                    state.discovering = true;
                    state.repos_error = None;
                    *action = Some(SettingsAction::RefreshRepos);
                }
                if state.discovering {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(RichText::new("Listing repositories...").color(theme::MUTED));
                }
            });
            if let Some(e) = &state.repos_error {
                ui.add_space(4.0);
                status_line(ui, Tone::Bad, e);
            }
        },
    );
}

fn storage_card(ui: &mut egui::Ui, state: &mut SettingsState) {
    card(
        ui,
        "Storage",
        "Where nit keeps repository mirrors, and where your own clones are.",
        None,
        |ui| {
            let browse_ok = !state.browsing;
            for field in [Field::DownloadDir, Field::CloneRoot] {
                let (label, desc, hint) = match field {
                    Field::DownloadDir => (
                        "Download folder",
                        "Bare mirrors of the repositories you review are created here.",
                        EXAMPLE_DOWNLOAD,
                    ),
                    Field::CloneRoot => (
                        "Local clones folder (optional)",
                        "Clones found under it are reused instead of mirrored; nit only \
                         fetches into refs/nit/... and never touches your branches.",
                        EXAMPLE_CLONES,
                    ),
                };
                let sync_error = state.sync_error(field);
                let ok = sync_error.is_none();
                let input = folder_row(
                    ui,
                    label,
                    desc,
                    hint,
                    state.folder(field),
                    sync_error,
                    browse_ok,
                );
                if ok {
                    match (field, &state.folder(field).check) {
                        (Field::DownloadDir, Check::Ok(info)) => {
                            let (text, tone) = download_line(*info);
                            status_line(ui, tone, &text);
                        }
                        (Field::CloneRoot, _) => clones_status(ui, state),
                        _ => {}
                    }
                }
                if input.changed {
                    state.edited(field);
                }
                if input.blurred {
                    state.request_check(field);
                    if field == Field::CloneRoot {
                        state.rescan();
                    }
                }
                if input.browse {
                    state.browse(field);
                }
                if input.open {
                    let path = PathBuf::from(clean_path_text(&state.folder(field).text));
                    state.open_folder("storage", path);
                }
                ui.add_space(14.0);
            }
            if let Some(("storage", e)) = &state.open_error {
                status_line(ui, Tone::Bad, e);
                ui.add_space(6.0);
            }
            ui.separator();
            ui.add_space(6.0);
            save_row(ui, state);
        },
    );
}

/// The clone scan under the clones folder: count, Rescan, skipped repositories.
fn clones_status(ui: &mut egui::Ui, state: &mut SettingsState) {
    let empty = clean_path_text(&state.clone.text).is_empty();
    if empty {
        status_line(
            ui,
            Tone::Muted,
            "Not set: every repository is mirrored into the download folder.",
        );
        return;
    }
    if let Check::Failed(_) | Check::Checking = state.clone.check {
        return;
    }
    let busy = state.scan == Scan::Running;
    let mut rescan = false;
    ui.horizontal(|ui| {
        match &state.scan {
            Scan::Idle => status_line(
                ui,
                Tone::Muted,
                "Not scanned yet: Rescan or leave the field to look for clones.",
            ),
            Scan::Running => status_line(ui, Tone::Busy, "Scanning for clones..."),
            Scan::Failed(e) => status_line(ui, Tone::Bad, e),
            Scan::Done { clones, skipped } => {
                let tone = if *clones == 0 { Tone::Warn } else { Tone::Good };
                status_line(ui, tone, &clones_line(*clones, skipped.len()));
            }
        }
        rescan = ui
            .add_enabled(!busy, egui::Button::new("Rescan").small())
            .on_hover_text("Look for clones under this folder again")
            .clicked();
    });
    if let Scan::Done { skipped, .. } = &state.scan {
        if !skipped.is_empty() {
            egui::CollapsingHeader::new(
                RichText::new(format!("Skipped repositories ({})", skipped.len()))
                    .color(theme::MUTED),
            )
            .id_salt("settings-skipped")
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .show(ui, |ui| {
                        for s in skipped {
                            let why = if s.reason == NO_HTTPS_REMOTE {
                                "SSH-only (no HTTPS remote), ignored"
                            } else {
                                s.reason.as_str()
                            };
                            muted(ui, &format!("{}: {why}", s.path.display()));
                        }
                    });
            });
        }
    }
    if rescan && state.sync_error(Field::CloneRoot).is_none() {
        state.rescan();
    }
}

/// Save / Reset of the Storage card and what the last Save did.
fn save_row(ui: &mut egui::Ui, state: &mut SettingsState) {
    ui.horizontal(|ui| {
        let blocked = state.save_blocked();
        let save = primary_button(ui, blocked.is_none(), "Save");
        let save = match blocked {
            Some(why) => save.on_disabled_hover_text(why),
            None => save.on_hover_text("Check the folders and save them"),
        };
        if save.clicked() {
            state.begin_save();
        }
        if button(ui, !state.at_default(), "Reset to defaults", 0.0)
            .on_hover_text("Put the default folders in the fields; Save keeps them")
            .clicked()
        {
            state.reset_to_default();
        }
        ui.add_space(6.0);
        if state.saving {
            status_line(ui, Tone::Busy, "Checking folders...");
        } else if let Some(m) = &state.message {
            match m {
                Ok(m) => status_line(ui, Tone::Good, m),
                Err(e) => status_line(ui, Tone::Bad, e),
            }
        } else if state.changed() {
            match state.save_blocked() {
                Some(why) => status_line(ui, Tone::Bad, why),
                None => status_line(ui, Tone::Warn, "Unsaved changes"),
            }
        } else {
            status_line(ui, Tone::Muted, "No unsaved changes");
        }
    });
}

fn review_card(ui: &mut egui::Ui, state: &mut SettingsState) {
    card(
        ui,
        "Review",
        "How the review screen behaves.",
        Some(("Applies at once", Tone::Muted)),
        |ui| {
            ui.checkbox(
                &mut state.vim_keys,
                RichText::new("Vim-style keys in the diff").strong(),
            );
            ui.indent("vim-help", |ui| {
                muted(
                    ui,
                    "j / k move by row, gg / G go to the top / bottom, ] / [ jump between \
                     hunks, / searches and c comments. Off: j / k move between files and v \
                     marks a file Viewed.",
                );
                ui.label(
                    RichText::new("See ? in review for the key list.")
                        .color(ui.visuals().hyperlink_color),
                );
            });
            ui.add_space(12.0);
            ui.label(RichText::new("Default diff layout").strong());
            ui.add_space(2.0);
            // a segmented control: both choices in one recessed frame
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .stroke(Stroke::new(1.0, theme::BORDER))
                .corner_radius(6)
                .inner_margin(egui::Margin::same(2))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.horizontal(|ui| {
                        for (l, name) in [
                            (DiffLayout::Unified, "Unified"),
                            (DiffLayout::Split, "Split"),
                        ] {
                            if ui
                                .add(
                                    egui::Button::selectable(state.layout == l, name)
                                        .min_size(egui::vec2(84.0, ROW_H - 4.0)),
                                )
                                .clicked()
                            {
                                state.layout = l;
                            }
                        }
                    });
                });
            ui.add_space(2.0);
            muted(
                ui,
                "Split shows the old file left and the new one right, in windows 900 px or \
                 wider. Alt+S switches while reviewing.",
            );
        },
    );
}

fn about_card(ui: &mut egui::Ui, state: &mut SettingsState) {
    card(
        ui,
        "About",
        "Version and what nit runs on; useful when reporting a problem.",
        None,
        |ui| {
            about_row(ui, "Version", |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} {}",
                        crate::brand::NAME,
                        env!("CARGO_PKG_VERSION")
                    ))
                    .strong(),
                );
            });
            let (value, badge, hint) = git_row(&git_info(state.git.as_ref()));
            about_row(ui, "Git", |ui| {
                ui.horizontal(|ui| {
                    ui.label(value);
                    if let Some((text, tone)) = badge {
                        chip(ui, text, tone);
                    }
                });
                if let Some(h) = hint {
                    muted(ui, h);
                }
            });
            about_row(ui, "Graphics", |ui| {
                match adapter_parts(crate::gpu::adapter_line()) {
                    Some((name, backend)) => {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(name);
                            ui.label(RichText::new(format!("via {backend}")).color(theme::MUTED));
                        });
                    }
                    None => muted(ui, "not started"),
                }
            });
            about_row(ui, "Data folder", |ui| match state.data_dir.clone() {
                Some(dir) => {
                    ui.add(egui::Label::new(dir.display().to_string()).wrap());
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("Open")
                            .on_hover_text(format!("Show it in {}", crate::open::FILE_MANAGER))
                            .clicked()
                        {
                            state.open_folder("data", dir.clone());
                        }
                        if ui.small_button("Copy path").clicked() {
                            ui.ctx().copy_text(dir.display().to_string());
                            state.copied = Some("data");
                        }
                        if state.copied == Some("data") {
                            ui.label(RichText::new("Copied").color(theme::GREEN));
                        }
                    });
                    if let Some(("data", e)) = &state.open_error {
                        status_line(ui, Tone::Bad, e);
                    }
                }
                None => muted(ui, crate::brand::DATA_DIR_UNSET),
            });
            about_row(ui, "Token check", |ui| {
                muted(
                    ui,
                    "Checks the saved token against Bitbucket and git without changing \
                     anything, and saves a report (token-check.txt) in the data folder. Run it \
                     in a terminal:",
                );
                ui.horizontal(|ui| {
                    egui::Frame::new()
                        .fill(theme::BG_DEEP)
                        .stroke(Stroke::new(1.0, theme::BORDER))
                        .corner_radius(4)
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .show(ui, |ui| {
                            ui.label(RichText::new(CHECK_TOKEN_CMD).monospace());
                        });
                    if ui.small_button("Copy command").clicked() {
                        ui.ctx().copy_text(CHECK_TOKEN_CMD.to_string());
                        state.copied = Some("cmd");
                    }
                    if state.copied == Some("cmd") {
                        ui.label(RichText::new("Copied").color(theme::GREEN));
                    }
                });
            });
            about_row(ui, "Licence", |ui| {
                ui.label("MIT OR Apache-2.0");
            });
        },
    );
}

#[cfg(test)]
mod tests;
