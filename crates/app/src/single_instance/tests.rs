use super::*;
use std::path::Path;

#[derive(Default)]
struct Fake {
    acquired: Option<Acquired>,
    can_focus: bool,
    focused: u32,
    boxes: Vec<String>,
}

impl Os for Fake {
    fn acquire(&mut self) -> Acquired {
        self.acquired.unwrap()
    }
    fn focus_existing(&mut self) -> bool {
        self.focused += 1;
        self.can_focus
    }
    fn message_box(&mut self, text: &str) {
        self.boxes.push(text.into());
    }
}

fn fake(a: Acquired, can_focus: bool) -> Fake {
    Fake {
        acquired: Some(a),
        can_focus,
        ..Default::default()
    }
}

#[test]
fn the_first_instance_starts_untouched() {
    let mut f = fake(Acquired::First, true);
    assert!(may_start(&mut f));
    assert_eq!((f.focused, f.boxes.len()), (0, 0));
}

#[test]
fn a_broken_guard_never_blocks_the_start() {
    let mut f = fake(Acquired::Unavailable, false);
    assert!(may_start(&mut f));
    assert_eq!((f.focused, f.boxes.len()), (0, 0));
}

#[test]
fn a_second_instance_raises_the_first_and_exits_silently() {
    let mut f = fake(Acquired::Second, true);
    assert!(!may_start(&mut f));
    assert_eq!((f.focused, f.boxes.len()), (1, 0));
}

#[test]
fn a_second_instance_that_cannot_find_the_window_says_so() {
    let mut f = fake(Acquired::Second, false);
    assert!(!may_start(&mut f));
    assert_eq!(f.boxes, vec![ALREADY_RUNNING.to_string()]);
}

#[test]
fn only_a_nit_or_old_prview_process_counts_as_the_running_instance() {
    // built with the host's separator: the check looks at the file name only
    let at = |name: &str| Path::new("Program Files").join("nit").join(name);
    assert!(is_own_image(&at("nit.exe")));
    assert!(is_own_image(&at("NIT.EXE")));
    // an old install still running before the rename
    assert!(is_own_image(&at("prview.exe")));
    assert!(!is_own_image(&at("notepad.exe")));
    assert!(!is_own_image(&at("nit.exe.bak")));
    assert!(!is_own_image(&at("not-nit.exe")));
}

#[cfg(windows)]
#[test]
fn real_windows_image_paths_are_recognised() {
    assert!(is_own_image(Path::new(r"C:\Program Files\nit\nit.exe")));
    assert!(is_own_image(Path::new(r"D:\tools\NIT.EXE")));
    assert!(!is_own_image(Path::new(r"C:\Windows\notepad.exe")));
}

#[cfg(unix)]
mod unix_guard {
    use super::super::unix::{Unix, LOCK_FILE, SOCKET_FILE};
    use super::super::{may_start, Acquired, Os};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    static RAISED: AtomicUsize = AtomicUsize::new(0);
    fn count_raise() {
        RAISED.fetch_add(1, Ordering::SeqCst);
    }
    fn no_raise() {}

    #[test]
    fn the_lock_is_taken_once_and_released_with_its_holder() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("nit");
        let mut first = Unix::with_raise(data.clone(), no_raise);
        assert_eq!(first.acquire(), Acquired::First);
        assert!(data.join(LOCK_FILE).is_file(), "the data folder is created");
        // another open of the same file (another process, in effect) sees it taken
        let mut second = Unix::with_raise(data.clone(), no_raise);
        assert_eq!(second.acquire(), Acquired::Second);
        drop(first);
        // the holder is gone (exit or crash): the next start is the first again, the
        // stale lock file and socket do not get in the way
        assert!(data.join(SOCKET_FILE).exists());
        let mut third = Unix::with_raise(data.clone(), no_raise);
        assert_eq!(third.acquire(), Acquired::First);
    }

    #[test]
    fn a_second_start_asks_the_first_to_raise_its_window_and_exits() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = Unix::with_raise(dir.path().to_path_buf(), count_raise);
        assert!(may_start(&mut first));
        let before = RAISED.load(Ordering::SeqCst);
        let mut second = Unix::with_raise(dir.path().to_path_buf(), no_raise);
        assert!(!may_start(&mut second), "the second start stops");
        let until = Instant::now() + Duration::from_secs(5);
        while RAISED.load(Ordering::SeqCst) == before && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(RAISED.load(Ordering::SeqCst), before + 1, "focus request");
    }

    #[test]
    fn a_folder_that_cannot_be_made_never_blocks_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let mut os = Unix::with_raise(file.join("nit"), no_raise);
        assert_eq!(os.acquire(), Acquired::Unavailable);
        assert!(may_start(&mut Unix::with_raise(file.join("nit"), no_raise)));
    }

    #[test]
    fn without_a_running_instance_focus_fails_and_nothing_is_raised() {
        let dir = tempfile::tempdir().unwrap();
        let mut os = Unix::with_raise(dir.path().to_path_buf(), no_raise);
        assert!(!os.focus_existing());
    }
}

#[test]
fn demo_modes_skip_the_guard() {
    assert!(!guarded(true));
    assert!(guarded(false));
}

#[cfg(windows)]
#[test]
fn the_real_mutex_is_taken_once_and_seen_by_a_second_handle() {
    // the test process is the first holder; a second acquire in the same process sees the
    // name as taken, which is what a second nit would see
    let mut os = win::Win32;
    // (a real nit or prview running on this machine makes the first one Second too)
    assert_ne!(os.acquire(), Acquired::Unavailable);
    assert_eq!(os.acquire(), Acquired::Second);
}

#[cfg(windows)]
#[test]
fn a_running_old_prview_counts_as_the_running_instance() {
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = win::LEGACY_MUTEX_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `name` is NUL-terminated; the handle lives until the test process exits,
    // like an old prview holding it.
    let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    assert!(!h.is_null());
    assert_eq!(win::Win32.acquire(), Acquired::Second);
}
