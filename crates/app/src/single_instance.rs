//! One nit per user session (audit 5.6): two processes share `cache.sqlite` and each has its
//! own in-flight post registry, so both could offer and post the same draft. A named mutex is
//! taken at start-up; a second start brings the first window to the front and exits quietly,
//! or, when that window cannot be found, shows a message box (a release build has no console).
//! An old `prview` (the name before the rename) counts as a running instance too: it holds the
//! same cache until the data folder is moved (see `brand`).
//!
//! The decision is plain code over the `Os` trait; the Win32 calls live in `win` below, the
//! macOS / Unix lock file and focus socket in `unix`.

/// Result of trying to become the one running instance.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Acquired {
    /// Nobody else holds the name; it stays held until the process exits.
    First,
    /// Another process holds it.
    Second,
    /// The guard could not be made (or this platform has none): start anyway, a broken guard
    /// must not lock the owner out.
    Unavailable,
}

pub trait Os {
    fn acquire(&mut self) -> Acquired;
    /// Raises the other instance's window; false when it could not be found or raised.
    fn focus_existing(&mut self) -> bool;
    fn message_box(&mut self, text: &str);
}

pub const ALREADY_RUNNING: &str = "nit is already running.";

/// The executables of a real instance: `nit.exe`, and `prview.exe` from before the rename.
const IMAGE_NAMES: [&str; 2] = ["nit.exe", "prview.exe"];

/// Whether `image` (a process's full image path) is a nit (or old prview) executable: a
/// window with its title and class in another program is never raised in its place.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn is_own_image(image: &std::path::Path) -> bool {
    image
        .file_name()
        .is_some_and(|n| IMAGE_NAMES.iter().any(|i| n.eq_ignore_ascii_case(i)))
}

/// True when this process should go on starting.
pub fn may_start(os: &mut impl Os) -> bool {
    match os.acquire() {
        Acquired::First | Acquired::Unavailable => true,
        Acquired::Second => {
            if !os.focus_existing() {
                os.message_box(ALREADY_RUNNING);
            }
            false
        }
    }
}

/// Modes that never touch the store or the network may run next to a real instance.
pub fn guarded(demo: bool) -> bool {
    // --shot, --shots, --bench and --bench-review all require --demo (see `validate_args`)
    !demo
}

/// Shows `text` in a message box (a release build has no console); on stderr elsewhere.
pub fn show_message(text: &str) {
    #[cfg(windows)]
    win::Win32.message_box(text);
    #[cfg(not(windows))]
    eprintln!("{text}");
}

/// The real guard: true when this process should go on starting.
pub fn check() -> bool {
    #[cfg(windows)]
    {
        may_start(&mut win::Win32)
    }
    #[cfg(unix)]
    {
        let Some(dir) = crate::brand::data_dir() else {
            return true; // no data folder: no guard (and no cache to share either)
        };
        let mut os = unix::Unix::new(dir);
        let start = may_start(&mut os);
        if start {
            // the lock and the focus listener live as long as the process
            std::mem::forget(os);
        }
        start
    }
    #[cfg(not(any(windows, unix)))]
    {
        true
    }
}

/// Called (on a background thread) when a second start asks this instance to come to the
/// front; set once the window exists. Windows raises the window itself (`win`).
pub fn on_focus_request(raise: impl Fn() + Send + Sync + 'static) {
    let _ = FOCUS.set(Box::new(raise));
}

static FOCUS: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

/// macOS and Unix: an exclusive `flock` on `nit.lock` in the data folder marks the running
/// instance (the kernel drops it when the process ends, also on a crash, so a stale file never
/// locks anyone out). The first instance also listens on `nit.sock` there; a second start
/// connects and writes `focus`, and the first raises its window (`on_focus_request`).
#[cfg(unix)]
mod unix {
    use super::{Acquired, Os};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::time::Duration;

    pub(super) const LOCK_FILE: &str = "nit.lock";
    pub(super) const SOCKET_FILE: &str = "nit.sock";
    const FOCUS_MESSAGE: &[u8] = b"focus\n";

    pub struct Unix {
        dir: PathBuf,
        /// Held while this is the running instance.
        lock: Option<File>,
        /// Called for each focus request (the real one calls `super::FOCUS`).
        raise: fn(),
    }

    fn raise_window() {
        if let Some(f) = super::FOCUS.get() {
            f();
        }
    }

    impl Unix {
        pub fn new(dir: PathBuf) -> Unix {
            Unix::with_raise(dir, raise_window)
        }

        pub(super) fn with_raise(dir: PathBuf, raise: fn()) -> Unix {
            Unix {
                dir,
                lock: None,
                raise,
            }
        }

        /// Serves focus requests on `nit.sock` for the life of the process. Only the lock
        /// holder gets here, so a socket file left by a crashed instance is stale.
        fn listen(&self) {
            let path = self.dir.join(SOCKET_FILE);
            let _ = std::fs::remove_file(&path);
            let Ok(listener) = UnixListener::bind(&path) else {
                return; // no focus requests then; the guard itself still works
            };
            let raise = self.raise;
            std::thread::Builder::new()
                .name("single-instance".into())
                .spawn(move || {
                    for mut stream in listener.incoming().flatten() {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                        let mut buf = [0u8; 16];
                        let mut got = 0;
                        while got < FOCUS_MESSAGE.len() {
                            match stream.read(&mut buf[got..]) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => got += n,
                            }
                        }
                        if &buf[..got] == FOCUS_MESSAGE {
                            raise();
                        }
                    }
                })
                .ok();
        }
    }

    impl Os for Unix {
        fn acquire(&mut self) -> Acquired {
            if std::fs::create_dir_all(&self.dir).is_err() {
                return Acquired::Unavailable;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(self.dir.join(LOCK_FILE));
            let Ok(file) = file else {
                return Acquired::Unavailable;
            };
            match file.try_lock() {
                Ok(()) => {
                    self.lock = Some(file);
                    self.listen();
                    Acquired::First
                }
                Err(std::fs::TryLockError::WouldBlock) => Acquired::Second,
                // a file system without locks: start anyway
                Err(std::fs::TryLockError::Error(_)) => Acquired::Unavailable,
            }
        }

        /// Asks the running instance to raise its window; false when it does not answer.
        fn focus_existing(&mut self) -> bool {
            UnixStream::connect(self.dir.join(SOCKET_FILE))
                .and_then(|mut s| s.write_all(FOCUS_MESSAGE))
                .is_ok()
        }

        /// No message box toolkit here: stderr (seen when started from a terminal).
        fn message_box(&mut self, text: &str) {
            eprintln!("{text}");
        }
    }
}

#[cfg(windows)]
mod win {
    use super::{is_own_image, Acquired, Os};
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HWND};
    use windows_sys::Win32::System::Threading::{
        CreateMutexW, OpenMutexW, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZATION_SYNCHRONIZE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, GetWindowThreadProcessId, IsIconic, MessageBoxW, SetForegroundWindow,
        ShowWindow, MB_ICONINFORMATION, MB_OK, SW_RESTORE,
    };

    const MUTEX_NAME: &str = "Local\\nit-single-instance";
    /// The mutex an old prview (before the rename) holds; only looked for, never created.
    pub(super) const LEGACY_MUTEX_NAME: &str = "Local\\prview-single-instance";
    /// The window titles (the second of each when the software renderer is in use, see `gpu`),
    /// then the old prview's.
    const TITLES: [&str; 4] = [
        "nit",
        "nit (software rendering)",
        "prview",
        "prview (software rendering)",
    ];
    /// The window class winit registers on Windows (eframe sets no other).
    const WINDOW_CLASS: &str = "Window Class";

    /// Full image path of the process that owns `hwnd`.
    fn image_of(hwnd: HWND) -> Option<PathBuf> {
        let mut pid = 0u32;
        // SAFETY: `hwnd` came from FindWindowExW; `pid` outlives the call. The process handle
        // is closed before returning; `buf` and `len` outlive the query.
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == 0 {
                return None;
            }
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
            CloseHandle(h);
            (ok != 0).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize])))
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Whether an old prview holds its mutex (opening an existing one creates nothing).
    fn legacy_running() -> bool {
        let name = wide(LEGACY_MUTEX_NAME);
        // SAFETY: `name` is NUL-terminated and outlives the call; a handle it returns is closed.
        unsafe {
            let h = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
            if h.is_null() {
                return false;
            }
            CloseHandle(h);
            true
        }
    }

    pub struct Win32;

    impl Os for Win32 {
        fn acquire(&mut self) -> Acquired {
            let name = wide(MUTEX_NAME);
            // SAFETY: `name` is NUL-terminated and outlives the call. The handle is never
            // closed on purpose: the mutex must live as long as the process.
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if handle.is_null() {
                return Acquired::Unavailable;
            }
            // SAFETY: reads the calling thread's last error, set by the call above.
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS || legacy_running() {
                Acquired::Second
            } else {
                Acquired::First
            }
        }

        /// Raises a top-level window with nit's class and title whose process is nit.exe (or
        /// an old prview.exe); any other program's window of that name is skipped.
        fn focus_existing(&mut self) -> bool {
            let class = wide(WINDOW_CLASS);
            for title in TITLES {
                let t = wide(title);
                let mut after: HWND = std::ptr::null_mut();
                loop {
                    // SAFETY: both strings are NUL-terminated; `after` is null or a window
                    // returned by the previous call.
                    let hwnd = unsafe {
                        FindWindowExW(std::ptr::null_mut(), after, class.as_ptr(), t.as_ptr())
                    };
                    if hwnd.is_null() {
                        break;
                    }
                    after = hwnd;
                    if !image_of(hwnd).is_some_and(|p| is_own_image(&p)) {
                        continue;
                    }
                    // SAFETY: `hwnd` came from FindWindowExW just now; the calls fail
                    // harmlessly if the window is gone.
                    unsafe {
                        if IsIconic(hwnd) != 0 {
                            ShowWindow(hwnd, SW_RESTORE);
                        }
                        if SetForegroundWindow(hwnd) != 0 {
                            return true;
                        }
                    }
                }
            }
            false
        }

        fn message_box(&mut self, text: &str) {
            let (text, title) = (wide(text), wide(crate::brand::NAME));
            // SAFETY: both strings are NUL-terminated; no owner window.
            unsafe {
                MessageBoxW(
                    std::ptr::null_mut(),
                    text.as_ptr(),
                    title.as_ptr(),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
