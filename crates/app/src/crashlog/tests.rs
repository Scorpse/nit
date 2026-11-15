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
    assert!(
        e.starts_with("[2026-11-12T10:00:00+01:00] panic in thread \"worker-1\" at src/a.rs:3:5\n")
    );
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
        l.len() == 7 && b[0] == b't' && b[2] == b'-' && l[3..].bytes().all(|c| c.is_ascii_digit())
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
    assert!(s.contains("crashlog"), "{s}");
}
