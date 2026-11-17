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

/// The local time as RFC 3339 with its UTC offset, to the millisecond.
fn now_rfc3339() -> String {
    jiff::Zoned::now()
        .strftime("%Y-%m-%dT%H:%M:%S%.3f%:z")
        .to_string()
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
    let entry = format_note(&now_rfc3339(), what, message);
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
            &now_rfc3339(),
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
mod tests;
