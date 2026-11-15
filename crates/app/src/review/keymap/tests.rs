use super::*;

fn fired(action: Action, count: Option<u32>) -> Feed {
    Feed::Fired { action, count }
}

fn type_keys(st: &mut KeyState, keys: &str, now: Instant) -> Vec<Feed> {
    keys.chars()
        .map(|c| st.feed(KeyIn::Char(c), now, true))
        .collect()
}

#[test]
fn single_keys_fire_their_action() {
    let now = Instant::now();
    let mut st = KeyState::default();
    for (c, a) in [
        ('j', A::Down),
        ('k', A::Up),
        ('G', A::Bottom),
        (']', A::NextHunk),
        ('[', A::PrevHunk),
        ('}', A::NextFile),
        ('{', A::PrevFile),
        ('/', A::Search),
        ('n', A::SearchNext),
        ('N', A::SearchPrev),
        ('c', A::Comment),
        ('C', A::FileComment),
        ('v', A::Visual),
        ('V', A::Visual),
        ('r', A::ToggleViewed),
        ('?', A::Help),
        ('q', A::Back),
        ('t', A::FocusFilter),
        ('y', A::CopyPath),
        ('h', A::SideLeft),
        ('l', A::SideRight),
    ] {
        assert_eq!(st.feed(KeyIn::Char(c), now, true), fired(a, None), "{c}");
    }
    assert_eq!(
        st.feed(KeyIn::Ctrl('d'), now, true),
        fired(A::HalfDown, None)
    );
    assert_eq!(st.feed(KeyIn::Ctrl('u'), now, true), fired(A::HalfUp, None));
    assert_eq!(st.feed(KeyIn::Enter, now, true), fired(A::Open, None));
    assert_eq!(st.feed(KeyIn::Backspace, now, true), fired(A::Back, None));
    assert_eq!(st.feed(KeyIn::Escape, now, true), fired(A::Cancel, None));
    assert_eq!(
        st.feed(KeyIn::Alt('s'), now, true),
        fired(A::ToggleLayout, None)
    );
    // not ours: left to the rest of the UI
    assert_eq!(st.feed(KeyIn::Char('z'), now, true), Feed::Ignored);
    assert_eq!(
        st.feed(KeyIn::Ctrl('a'), now, true),
        Feed::Ignored,
        "select all"
    );
    assert_eq!(st.feed(KeyIn::Ctrl('c'), now, true), Feed::Ignored, "copy");
}

#[test]
fn counts_prefix_the_next_action() {
    let now = Instant::now();
    let mut st = KeyState::default();
    let out = type_keys(&mut st, "12j", now);
    assert_eq!(
        out,
        [Feed::Pending, Feed::Pending, fired(A::Down, Some(12))]
    );
    assert_eq!(
        st.feed(KeyIn::Char('j'), now, true),
        fired(A::Down, None),
        "used up"
    );
    // 0 alone is not a count; inside one it is a digit
    assert_eq!(st.feed(KeyIn::Char('0'), now, true), Feed::Ignored);
    assert_eq!(
        type_keys(&mut st, "105G", now).last(),
        Some(&fired(A::Bottom, Some(105)))
    );
    // an unbound key drops the count
    type_keys(&mut st, "7z", now);
    assert_eq!(st.feed(KeyIn::Char('k'), now, true), fired(A::Up, None));
    // capped, no overflow
    let big = "9".repeat(30) + "j";
    assert_eq!(
        type_keys(&mut st, &big, now).last(),
        Some(&fired(A::Down, Some(MAX_COUNT)))
    );
    assert_eq!(st.pending(), None);
    type_keys(&mut st, "42", now);
    assert_eq!(st.pending().as_deref(), Some("42"));
    st.reset();
    assert_eq!(st.pending(), None);
}

#[test]
fn gg_is_a_chord_with_a_timeout() {
    let t0 = Instant::now();
    let mut st = KeyState::default();
    assert_eq!(st.feed(KeyIn::Char('g'), t0, true), Feed::Pending);
    assert_eq!(st.pending().as_deref(), Some("g"));
    assert_eq!(st.deadline(), Some(t0 + CHORD_TIMEOUT));
    let t1 = t0 + CHORD_TIMEOUT / 2;
    assert_eq!(st.feed(KeyIn::Char('g'), t1, true), fired(A::Top, None));
    // with a count: 5gg
    assert_eq!(
        type_keys(&mut st, "5gg", t0).last(),
        Some(&fired(A::Top, Some(5)))
    );
    // too slow: the first g is dropped, the second starts a new chord
    st.feed(KeyIn::Char('g'), t0, true);
    let late = t0 + CHORD_TIMEOUT + Duration::from_millis(1);
    assert_eq!(st.feed(KeyIn::Char('g'), late, true), Feed::Pending);
    assert_eq!(
        st.feed(KeyIn::Char('g'), late + Duration::from_millis(10), true),
        fired(A::Top, None)
    );
    // a timed-out chord takes its count with it
    type_keys(&mut st, "3g", t0);
    st.expire(late);
    assert_eq!(st.pending(), None);
    assert_eq!(st.feed(KeyIn::Char('j'), late, true), fired(A::Down, None));
    // g then another key: the key acts on its own, without the count
    type_keys(&mut st, "4g", t0);
    assert_eq!(st.feed(KeyIn::Char('j'), t0, true), fired(A::Down, None));
}

#[test]
fn with_vim_off_only_the_layout_toggle_works() {
    let now = Instant::now();
    let mut st = KeyState::default();
    for c in "jkgG12/?q".chars() {
        assert_eq!(st.feed(KeyIn::Char(c), now, false), Feed::Ignored, "{c}");
    }
    assert_eq!(st.feed(KeyIn::Backspace, now, false), Feed::Ignored);
    assert_eq!(st.feed(KeyIn::Ctrl('d'), now, false), Feed::Ignored);
    assert_eq!(
        st.feed(KeyIn::Alt('s'), now, false),
        fired(A::ToggleLayout, None)
    );
    assert!(vim_keys_on(None) && vim_keys_on(Some("on")) && !vim_keys_on(Some("off")));
    assert_eq!(vim_keys_value(false), "off");
    assert!(vim_keys_on(Some(vim_keys_value(true))));
}

#[test]
fn the_keymap_has_no_conflicts() {
    for (i, a) in KEYMAP.iter().enumerate() {
        if !matches!(a.bind, Bind::Doc(_)) {
            assert!(a.action.is_some(), "{}", label(a.bind));
        }
        for b in &KEYMAP[..i] {
            assert_ne!(a.bind, b.bind, "{} is bound twice", label(a.bind));
            assert!(
                !(matches!(a.bind, Bind::Doc(_)) && label(a.bind) == label(b.bind)),
                "{} is listed twice",
                label(a.bind)
            );
        }
        match a.bind {
            // a chord's first key cannot also be a key of its own, nor a digit
            Bind::Chord(c, _) => {
                assert!(lookup(Bind::Key(KeyIn::Char(c)), true).is_none());
                assert!(!c.is_ascii_digit());
            }
            Bind::Key(KeyIn::Char(c)) => assert!(!c.is_ascii_digit(), "digits are counts"),
            _ => {}
        }
    }
    // every action is reachable
    for act in [
        A::Down,
        A::Up,
        A::HalfDown,
        A::HalfUp,
        A::Top,
        A::Bottom,
        A::NextHunk,
        A::PrevHunk,
        A::NextFile,
        A::PrevFile,
        A::SideLeft,
        A::SideRight,
        A::Search,
        A::SearchNext,
        A::SearchPrev,
        A::Comment,
        A::FileComment,
        A::Visual,
        A::ToggleViewed,
        A::Open,
        A::Help,
        A::Back,
        A::FocusFilter,
        A::CopyPath,
        A::Cancel,
        A::ToggleLayout,
    ] {
        assert!(KEYMAP.iter().any(|b| b.action == Some(act)), "{act:?}");
    }
}

#[test]
fn help_lists_every_entry_once_and_merges_same_action_keys() {
    let rows: Vec<(String, &str)> = Group::ALL
        .iter()
        .flat_map(|&g| help_rows(g, true))
        .collect();
    assert!(rows.contains(&("q / Backspace".to_string(), "Back to the inbox")));
    assert!(rows.iter().any(|(k, _)| k == "gg"));
    assert!(rows.iter().any(|(k, _)| k == "Ctrl+d"));
    assert!(rows.iter().any(|(k, _)| k == "Alt+S"));
    assert!(
        rows.iter().any(|(k, _)| k == CMD_C),
        "existing shortcuts too"
    );
    // v and V differ in their text: two rows
    assert!(rows.iter().any(|(k, _)| k == "v") && rows.iter().any(|(k, _)| k == "V"));
    let keys: usize = rows.iter().map(|(k, _)| k.split(" / ").count()).sum();
    let listed: usize = KEYMAP
        .iter()
        .map(|b| label(b.bind).split(" / ").count())
        .sum();
    assert_eq!(keys, listed, "every key once");
    // vim off: only what still works
    let off: Vec<(String, &str)> = Group::ALL
        .iter()
        .flat_map(|&g| help_rows(g, false))
        .collect();
    assert!(off.iter().all(|(k, _)| k != "j"));
    assert!(off.iter().any(|(k, _)| k == "Alt+S"));
}
