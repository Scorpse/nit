//! Storage page: download and clone folders, their checks and the save row.

use super::*;

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
pub(super) fn segments(path: &str) -> Vec<String> {
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

pub(super) fn sync_check_download(text: &str) -> Result<PathBuf, String> {
    if clean_path_text(text).is_empty() {
        return Err("Choose a folder".into());
    }
    check_absolute(text)
}

pub(super) fn sync_check_clone(text: &str) -> Result<Option<PathBuf>, String> {
    if clean_path_text(text).is_empty() {
        return Ok(None);
    }
    check_absolute(text).map(Some)
}

// ---- filesystem checks (call from a worker thread only) --------------------------------

pub(super) fn probe_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".nit-write-test-{}-{nanos}", std::process::id())
}

/// Proves `dir` accepts new files: creates a uniquely named file (`create_new`) and removes it.
pub(super) fn check_writable_with(dir: &Path, name: impl FnOnce() -> String) -> Result<(), String> {
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

pub(super) fn check_writable(dir: &Path) -> Result<(), String> {
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
pub(super) fn free_space_at(dir: &Path) -> Option<u64> {
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
pub(super) fn free_space_at(dir: &Path) -> Option<u64> {
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
pub(super) fn free_space_at(_: &Path) -> Option<u64> {
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

pub(super) fn clone_root_text(c: &Option<PathBuf>) -> String {
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

/// What the user did with one folder row this frame.
#[derive(Default)]
pub(super) struct RowInput {
    pub(super) changed: bool,
    pub(super) blurred: bool,
    pub(super) browse: bool,
    pub(super) open: bool,
}

/// Label, description, path field with Browse... and Open, and the check result under it.
pub(super) fn folder_row(
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

pub(super) fn storage_card(ui: &mut egui::Ui, state: &mut SettingsState) {
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
pub(super) fn clones_status(ui: &mut egui::Ui, state: &mut SettingsState) {
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
pub(super) fn save_row(ui: &mut egui::Ui, state: &mut SettingsState) {
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
