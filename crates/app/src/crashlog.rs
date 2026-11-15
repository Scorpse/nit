//! Panic hook that appends to `crash.log` in the data folder (`brand::data_dir`, audit 5.1).
//! Release builds have no console, so a panic on the UI thread used to close the window with
//! no trace. The hook
//! writes the time, thread, location and message, then calls the previous hook; it changes
//! nothing about unwinding, so the `catch_unwind` around worker jobs keeps working.
//!
//! Panic messages come from our own code, but a long token-like run (a base64 or hex string of
//! more than `TOKEN_LEN` characters) is replaced by `***` anyway, in case one was formatted into
//! a message. A message is cut to `MAX_MESSAGE_CHARS` characters and an entry to
//! `MAX_ENTRY_BYTES`, so a panic formatting a large value never writes it whole.

use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::lock::lock_ok;

/// The log keeps about this much (the newest part).
const MAX_LOG_BYTES: usize = 200 * 1024;
/// A message is cut here before it is written (a panic can format a huge value).
const MAX_MESSAGE_BYTES: usize = 4 * 1024;
/// A message longer than this many characters is cut to it, marked "(cut)".
const MAX_MESSAGE_CHARS: usize = 300;
/// No entry (header, thread, location and message) is longer than this.
const MAX_ENTRY_BYTES: usize = 4 * 1024;
/// Runs of token characters longer than this are hidden.
const TOKEN_LEN: usize = 30;

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '_' | '-')
}

/// `text` with every run of more than `TOKEN_LEN` base64/hex-like characters replaced by `***`.
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() > TOKEN_LEN {
            out.push_str("***");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if is_token_char(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Cuts `s` to at most `max` bytes on a char boundary.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// At most `MAX_MESSAGE_CHARS` characters of `message`, marked when cut.
fn short_message(message: &str) -> String {
    let message = truncate(message, MAX_MESSAGE_BYTES);
    match message.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((end, _)) => format!("{} (cut)", &message[..end]),
        None => message.to_string(),
    }
}

/// One log entry (ends with a newline), at most `MAX_ENTRY_BYTES` long.
pub fn format_entry(time: &str, thread: &str, location: &str, message: &str) -> String {
    let message = scrub(&short_message(message)).replace('\n', "\n    ");
    let entry = format!(
        "[{time}] panic in thread {:?} at {location}\n    {message}\n",
        scrub(thread)
    );
    if entry.len() <= MAX_ENTRY_BYTES {
        return entry;
    }
    format!("{}\n", truncate(&entry, MAX_ENTRY_BYTES - 1))
}

/// A log entry for a failure that is not a panic (e.g. the window could not be created).
pub fn format_note(time: &str, what: &str, message: &str) -> String {
    let message = scrub(&short_message(message)).replace('\n', "\n    ");
    let entry = format!("[{time}] {}\n    {message}\n", scrub(what));
    if entry.len() <= MAX_ENTRY_BYTES {
        return entry;
    }
    format!("{}\n", truncate(&entry, MAX_ENTRY_BYTES - 1))
}

/// Appends a `format_note` entry to the crash log (nothing when there is no data folder;
/// I/O errors are ignored).
pub fn record(what: &str, message: &str) {
    let Some(path) = log_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let entry = format_note(&chrono::Local::now().to_rfc3339(), what, message);
    let _ = append_rotated(&path, &entry, MAX_LOG_BYTES);
}

/// Appends `entry` to the file at `path`; when the result would exceed `max` bytes the oldest
/// part is dropped from the start (at an entry's first line boundary after the cut). Calls
/// from several threads (panics at once) run one at a time, so none is lost or interleaved.
pub fn append_rotated(path: &Path, entry: &str, max: usize) -> std::io::Result<()> {
    static LOG: Mutex<()> = Mutex::new(());
    let _one = lock_ok(&LOG);
    let len = std::fs::metadata(path).map_or(0, |m| m.len() as usize);
    if len + entry.len() <= max {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        return f.write_all(entry.as_bytes());
    }
    let old = std::fs::read(path).unwrap_or_default();
    let mut all = old;
    all.extend_from_slice(entry.as_bytes());
    let mut start = all.len().saturating_sub(max);
    // begin at a line start so a cut entry does not leave half a line
    if start > 0 {
        match all[start..].iter().position(|&b| b == b'\n') {
            Some(i) => start += i + 1,
            None => start = all.len(),
        }
    }
    std::fs::write(path, &all[start..])
}

fn message_of(info: &PanicHookInfo<'_>) -> String {
    let p = info.payload();
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(non-text panic payload)".into())
}

pub fn log_path() -> Option<PathBuf> {
    // the old prview folder while it has not been moved yet (see `brand`)
    Some(crate::brand::data_dir()?.join("crash.log"))
}

/// Installs the hook (the previous hook still runs afterwards). Writes nothing when
/// there is no data folder; every I/O error inside the hook is ignored.
pub fn install() {
    let Some(path) = log_path() else {
        return;
    };
    install_at(path);
}

fn install_at(path: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let location = info
            .location()
            .map_or_else(|| "unknown location".into(), ToString::to_string);
        let entry = format_entry(
            &chrono::Local::now().to_rfc3339(),
            thread.name().unwrap_or("<unnamed>"),
            &location,
            &message_of(info),
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = append_rotated(&path, &entry, MAX_LOG_BYTES);
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_hides_long_tokens_only() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let b64 = "ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789+/==";
        assert_eq!(scrub(&format!("token {hex} bad")), "token *** bad");
        assert_eq!(scrub(&format!("a={b64};")), "a=***==;", "padding stays");
        let exactly = "x".repeat(TOKEN_LEN);
        assert_eq!(scrub(&exactly), exactly, "30 characters stay");
        assert_eq!(scrub(&"x".repeat(TOKEN_LEN + 1)), "***");
        assert_eq!(
            scrub("index out of bounds: the len is 3 but the index is 7"),
            "index out of bounds: the len is 3 but the index is 7"
        );
        assert_eq!(scrub("é".repeat(40).as_str()), "é".repeat(40));
    }

    #[test]
    fn entry_has_all_fields_and_no_token() {
        let e = format_entry(
            "2026-11-12T10:00:00+01:00",
            "worker-1",
            "src/a.rs:3:5",
            &format!("boom {}\nsecond line", "a".repeat(64)),
        );
        assert!(e.starts_with(
            "[2026-11-12T10:00:00+01:00] panic in thread \"worker-1\" at src/a.rs:3:5\n"
        ));
        assert!(e.contains("boom ***\n    second line\n"), "{e}");
        assert!(e.ends_with('\n'));
    }

    #[test]
    fn long_messages_are_cut() {
        let e = format_entry("t", "m", "l", &"é ".repeat(MAX_MESSAGE_BYTES));
        assert!(e.len() < MAX_MESSAGE_BYTES * 2 + 200);
    }

    #[test]
    fn an_entry_is_capped_and_a_long_message_cut_to_300_chars() {
        let e = format_entry(
            "t",
            &"n".repeat(5000),
            &"l".repeat(5000),
            &"m ".repeat(5000),
        );
        assert!(e.len() <= MAX_ENTRY_BYTES, "{}", e.len());
        assert!(e.ends_with('\n'));
        let msg = format_entry("t", "main", "src/a.rs:1:1", &"é".repeat(400));
        assert!(msg.contains(&"é".repeat(MAX_MESSAGE_CHARS)), "{msg}");
        assert!(!msg.contains(&"é".repeat(MAX_MESSAGE_CHARS + 1)), "{msg}");
        assert!(msg.contains("(cut)"), "{msg}");
    }

    #[test]
    fn a_note_is_scrubbed_and_capped_like_a_panic() {
        let n = format_note(
            "t",
            "start-up failed",
            &format!("no adapter {}", "k".repeat(40)),
        );
        assert_eq!(n, "[t] start-up failed\n    no adapter ***\n");
        assert!(format_note("t", "x", &"y ".repeat(9000)).len() <= MAX_ENTRY_BYTES);
    }

    #[test]
    fn concurrent_appends_keep_whole_lines_within_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash.log");
        let max = 2000;
        std::thread::scope(|s| {
            for t in 0..8 {
                let p = &p;
                s.spawn(move || {
                    for i in 0..200 {
                        append_rotated(p, &format!("t{t}-{i:04}\n"), max).unwrap();
                    }
                });
            }
        });
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.len() <= max, "{}", text.len());
        let whole = |l: &str| {
            let b = l.as_bytes();
            l.len() == 7
                && b[0] == b't'
                && b[2] == b'-'
                && l[3..].bytes().all(|c| c.is_ascii_digit())
        };
        assert!(text.lines().all(whole), "{text}");
        assert!(
            text.len() > max / 2,
            "rotation lost most of the log: {}",
            text.len()
        );
    }

    #[test]
    fn log_appends_then_keeps_the_newest_part() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash.log");
        append_rotated(&p, "one\n", 100).unwrap();
        append_rotated(&p, "two\n", 100).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\ntwo\n");
        // 25 entries of 10 bytes against a limit of 100: only the newest remain
        for i in 0..25 {
            append_rotated(&p, &format!("entry-{i:03}\n"), 100).unwrap();
        }
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.len() <= 100, "{}", s.len());
        assert!(s.ends_with("entry-024\n"), "{s}");
        assert!(!s.contains("one"), "{s}");
        assert!(
            s.lines().all(|l| l.starts_with("entry-")),
            "no half line: {s}"
        );
    }

    #[test]
    fn an_entry_larger_than_the_limit_leaves_an_empty_or_whole_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash.log");
        append_rotated(&p, "old\n", 8).unwrap();
        append_rotated(&p, "this entry is far longer than eight bytes\n", 8).unwrap();
        assert!(std::fs::metadata(&p).unwrap().len() <= 8);
    }

    #[test]
    fn the_hook_writes_a_file_and_unwinding_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("crash.log");
        let before = std::panic::take_hook();
        install_at(p.clone());
        let r = std::panic::catch_unwind(|| {
            panic!("kaboom {}", "z".repeat(50));
        });
        // restore the test harness hook for the other tests
        let _ = std::panic::take_hook();
        std::panic::set_hook(before);
        assert!(r.is_err(), "the panic still unwinds");
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("kaboom ***"), "{s}");
        assert!(s.contains("crashlog.rs"), "{s}");
    }
}
