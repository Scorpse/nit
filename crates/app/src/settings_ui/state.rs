//! Settings state: the form, its workers and their results.

use super::*;

// ---- state ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    DownloadDir,
    CloneRoot,
}

/// Outcome of the disk checks for one folder field.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) enum Check {
    #[default]
    Unchecked,
    Checking,
    Ok(FolderInfo),
    Failed(String),
}

#[derive(Default)]
pub struct Folder {
    pub text: String,
    pub(super) check: Check,
    pub(super) generation: u64,
}

#[derive(Debug)]
pub(super) enum Worker {
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
pub(super) enum Scan {
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
    pub(super) default_download: PathBuf,
    /// The folders as last loaded or saved; Save is enabled only when the fields differ.
    pub(super) saved: (String, String),
    /// Settings handed to the store and not confirmed yet.
    pub(super) pending: Option<Settings>,
    /// Set when the stored settings could not be read; Save stays blocked.
    pub load_error: Option<String>,
    pub(super) git: Option<Result<String, String>>,
    pub(super) scan: Scan,
    pub(super) scan_generation: u64,
    pub(super) save_generation: u64,
    pub(super) saving: bool,
    pub(super) browsing: bool,
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
    pub(super) confirm_remove: Option<OtherAccount>,
    /// What removing it deletes and keeps (`Store::removal_counts`), set by the caller
    /// while the confirmation is open.
    pub removal_counts: Option<Result<store::RemovedCounts, String>>,
    /// The account whose label is being edited, and the text so far.
    pub(super) renaming: Option<(AccountId, String)>,
    /// The data folder (`brand::data_dir`), shown under About.
    pub(super) data_dir: Option<PathBuf>,
    /// The Sign out confirmation is open.
    pub(super) confirm_sign_out: bool,
    /// The copy button pressed last ("Copied" shows next to it).
    pub(super) copied: Option<&'static str>,
    /// Why "Open" could not start Explorer, tagged with the card it was pressed in.
    pub(super) open_error: Option<(&'static str, String)>,
    pub(super) ctx: egui::Context,
    pub(super) tx: Sender<Worker>,
    pub(super) rx: Receiver<Worker>,
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

    pub(super) fn job(&self, f: impl FnOnce() -> Worker + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
    }

    pub(super) fn folder(&mut self, f: Field) -> &mut Folder {
        match f {
            Field::DownloadDir => &mut self.download,
            Field::CloneRoot => &mut self.clone,
        }
    }

    /// The string-only error for a field, cheap enough for every frame.
    pub(super) fn sync_error(&self, f: Field) -> Option<String> {
        let nested = check_not_nested(&self.download.text, &self.clone.text).err();
        match f {
            Field::DownloadDir => sync_check_download(&self.download.text).err().or(nested),
            Field::CloneRoot => sync_check_clone(&self.clone.text).err().or(nested),
        }
    }

    /// Text of a field changed: its old disk result and (for the clone folder) scan are stale.
    pub(super) fn edited(&mut self, f: Field) {
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
    pub(super) fn request_check(&mut self, f: Field) {
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
    pub(super) fn save_blocked(&self) -> Option<&'static str> {
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

    pub(super) fn can_save(&self) -> bool {
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
    pub(super) fn at_default(&self) -> bool {
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
    pub(super) fn browse(&mut self, f: Field) {
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
    pub(super) fn open_folder(&mut self, card: &'static str, path: PathBuf) {
        self.open_error = None;
        self.job(move || Worker::Opened(open_in_explorer(&path).map_err(|e| (card, e))));
    }

    /// Applies one worker message. Returns settings to persist when a Save finished its checks.
    pub(super) fn apply(&mut self, m: Worker) -> Option<Settings> {
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
pub(super) fn git_status() -> Result<String, String> {
    let git = Git::shared().map_err(|_| GIT_MISSING.to_string())?;
    let out = git
        .run(Path::new("."), &["--version"], None)
        .map_err(|_| GIT_MISSING.to_string())?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// Shows an existing folder in the file manager (Explorer, Finder, `xdg-open`); the path goes
/// to the opener as one argument (no shell involved, see `open`).
pub(super) fn open_in_explorer(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || !path.is_dir() {
        return Err(format!("{} does not exist yet", path.display()));
    }
    crate::open::open(path).map_err(|_| "Could not open the folder".to_string())
}
