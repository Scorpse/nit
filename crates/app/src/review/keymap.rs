//! Keyboard map of the review screen: the one table (`KEYMAP`) behind both the key handler
//! and the `?` help overlay, and the pure key state machine (counts, the `gg` chord and its
//! timeout). Key events go in, actions come out; `review::ui` applies them.
//!
//! The vim-style keys are on by default and can be turned off in Settings ("Vim keys"); the
//! entries marked `always` (the diff layout toggle) work either way. Entries with a `Doc`
//! binding are handled elsewhere (text selection, file list, composer) and are listed here so
//! the help overlay shows every shortcut from one place.

use std::time::{Duration, Instant};

/// How long `g` waits for the second `g`.
pub const CHORD_TIMEOUT: Duration = Duration::from_millis(1000);
/// Counts stop growing here (`99999999j` is not an overflow).
const MAX_COUNT: u32 = 999_999;

/// Store meta key that remembers whether the vim keys are on.
pub const VIM_META_KEY: &str = "review_vim_keys";

/// The shortcuts that use egui's `Modifiers::COMMAND` (and egui's own copy), named as the
/// platform names them: Cmd on macOS, Ctrl elsewhere. `Ctrl+d` / `Ctrl+u` are the Control
/// key everywhere (as in vim) and stay `KeyIn::Ctrl`.
const MAC: bool = cfg!(target_os = "macos");
pub const CMD_A: &str = if MAC { "Cmd+A" } else { "Ctrl+A" };
pub const CMD_C: &str = if MAC { "Cmd+C" } else { "Ctrl+C" };
pub const CMD_ENTER: &str = if MAC { "Cmd+Enter" } else { "Ctrl+Enter" };
pub const CMD_SHIFT_M: &str = if MAC { "Cmd+Shift+M" } else { "Ctrl+Shift+M" };

/// "off" turns them off; anything else (unset included) is on.
pub fn vim_keys_on(stored: Option<&str>) -> bool {
    stored != Some("off")
}

pub fn vim_keys_value(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// One key press as the handler sees it. Printable characters arrive as typed (so `?`, `{`
/// and `G` work on every keyboard layout); Ctrl and Alt combinations by their letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyIn {
    Char(char),
    Ctrl(char),
    Alt(char),
    Enter,
    Backspace,
    Escape,
}

/// What a key asks the review screen to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Down,
    Up,
    HalfDown,
    HalfUp,
    /// With a count: the new file's line N.
    Top,
    /// With a count: the new file's line N.
    Bottom,
    NextHunk,
    PrevHunk,
    NextFile,
    PrevFile,
    SideLeft,
    SideRight,
    Search,
    SearchNext,
    SearchPrev,
    Comment,
    FileComment,
    Visual,
    ToggleViewed,
    Open,
    Help,
    Back,
    FocusFilter,
    CopyPath,
    /// Esc: closes the help or the search, leaves visual mode, drops a pending count.
    Cancel,
    ToggleLayout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    Key(KeyIn),
    /// Two characters typed within `CHORD_TIMEOUT`.
    Chord(char, char),
    /// Listed in the help only; handled by other code.
    Doc(&'static str),
}

/// Sections of the help overlay, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Move,
    Search,
    Comment,
    Files,
    General,
}

impl Group {
    pub const ALL: [Group; 5] = [
        Group::Move,
        Group::Search,
        Group::Comment,
        Group::Files,
        Group::General,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::Move => "Moving in the diff",
            Group::Search => "Search",
            Group::Comment => "Comments and selection",
            Group::Files => "Files",
            Group::General => "General",
        }
    }
}

pub struct Binding {
    pub bind: Bind,
    /// None for `Doc` entries.
    pub action: Option<Action>,
    pub help: &'static str,
    pub group: Group,
    /// Works with the vim keys turned off too.
    pub always: bool,
}

const fn vim(bind: Bind, action: Action, group: Group, help: &'static str) -> Binding {
    Binding {
        bind,
        action: Some(action),
        help,
        group,
        always: false,
    }
}

const fn ch(c: char) -> Bind {
    Bind::Key(KeyIn::Char(c))
}

const fn doc(keys: &'static str, group: Group, help: &'static str) -> Binding {
    Binding {
        bind: Bind::Doc(keys),
        action: None,
        help,
        group,
        always: true,
    }
}

use Action as A;
use Group as G;

/// Every shortcut of the review screen.
pub const KEYMAP: &[Binding] = &[
    vim(ch('j'), A::Down, G::Move, "Next row (with a count: N rows)"),
    vim(ch('k'), A::Up, G::Move, "Previous row"),
    vim(
        Bind::Key(KeyIn::Ctrl('d')),
        A::HalfDown,
        G::Move,
        "Half a page down",
    ),
    vim(
        Bind::Key(KeyIn::Ctrl('u')),
        A::HalfUp,
        G::Move,
        "Half a page up",
    ),
    vim(Bind::Chord('g', 'g'), A::Top, G::Move, "First row"),
    vim(ch('G'), A::Bottom, G::Move, "Last row"),
    doc("{N}G", G::Move, "Line N of the new file (a count, then G)"),
    vim(ch(']'), A::NextHunk, G::Move, "Next hunk"),
    vim(ch('['), A::PrevHunk, G::Move, "Previous hunk"),
    vim(ch('h'), A::SideLeft, G::Move, "Split view: old side"),
    vim(ch('l'), A::SideRight, G::Move, "Split view: new side"),
    vim(ch('/'), A::Search, G::Search, "Search in this diff"),
    vim(ch('n'), A::SearchNext, G::Search, "Next match"),
    vim(ch('N'), A::SearchPrev, G::Search, "Previous match"),
    vim(ch('c'), A::Comment, G::Comment, "Comment on the cursor row"),
    vim(ch('C'), A::FileComment, G::Comment, "Comment on the file"),
    vim(
        ch('v'),
        A::Visual,
        G::Comment,
        "Select rows (j / k extend it)",
    ),
    vim(ch('V'), A::Visual, G::Comment, "Select rows"),
    doc(CMD_A, G::Comment, "Select every row of the diff"),
    doc(
        CMD_C,
        G::Comment,
        "Copy the selected text (one side in the split view)",
    ),
    doc(CMD_ENTER, G::Comment, "In the composer: save the draft"),
    vim(ch('}'), A::NextFile, G::Files, "Next file"),
    vim(ch('{'), A::PrevFile, G::Files, "Previous file"),
    vim(
        ch('r'),
        A::ToggleViewed,
        G::Files,
        "Toggle Viewed on the file",
    ),
    vim(ch('t'), A::FocusFilter, G::Files, "Filter the file list"),
    vim(ch('y'), A::CopyPath, G::Files, "Copy the file's path"),
    vim(
        Bind::Key(KeyIn::Enter),
        A::Open,
        G::Files,
        "Show the file at the list cursor",
    ),
    doc("Up / Down", G::Files, "File list: previous / next row"),
    doc("PageUp / PageDown", G::Files, "File list: a page up / down"),
    doc("Home / End", G::Files, "File list: first / last row"),
    doc("Left / Right", G::Files, "File tree: collapse / expand"),
    doc("Space", G::Files, "File list: toggle Viewed"),
    Binding {
        bind: Bind::Key(KeyIn::Alt('s')),
        action: Some(A::ToggleLayout),
        help: "Unified / split diff",
        group: G::General,
        always: true,
    },
    vim(ch('?'), A::Help, G::General, "Show or hide this help"),
    vim(ch('q'), A::Back, G::General, "Back to the inbox"),
    vim(
        Bind::Key(KeyIn::Backspace),
        A::Back,
        G::General,
        "Back to the inbox",
    ),
    vim(
        Bind::Key(KeyIn::Escape),
        A::Cancel,
        G::General,
        "Close the help or search, clear the selection",
    ),
];

/// Text for a binding in the help.
pub fn label(b: Bind) -> String {
    match b {
        Bind::Key(KeyIn::Char(c)) => c.to_string(),
        Bind::Key(KeyIn::Ctrl(c)) => format!("Ctrl+{c}"),
        Bind::Key(KeyIn::Alt(c)) => format!("Alt+{}", c.to_ascii_uppercase()),
        Bind::Key(KeyIn::Enter) => "Enter".into(),
        Bind::Key(KeyIn::Backspace) => "Backspace".into(),
        Bind::Key(KeyIn::Escape) => "Esc".into(),
        Bind::Chord(a, b) => format!("{a}{b}"),
        Bind::Doc(s) => s.into(),
    }
}

/// The help overlay's rows of `group`: keys with the same action and text share a row
/// ("q / Backspace"). Only the entries that are active (`vim` off leaves the `always` ones).
pub fn help_rows(group: Group, vim: bool) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    let mut last: Option<(Option<Action>, &str)> = None;
    for b in KEYMAP
        .iter()
        .filter(|b| b.group == group && (vim || b.always))
    {
        let key = (b.action, b.help);
        match out.last_mut() {
            Some((keys, _)) if b.action.is_some() && last == Some(key) => {
                keys.push_str(" / ");
                keys.push_str(&label(b.bind));
            }
            _ => out.push((label(b.bind), b.help)),
        }
        last = Some(key);
    }
    out
}

fn lookup(bind: Bind, vim: bool) -> Option<Action> {
    KEYMAP
        .iter()
        .find(|b| b.bind == bind && (vim || b.always))
        .and_then(|b| b.action)
}

fn starts_chord(c: char) -> bool {
    KEYMAP
        .iter()
        .any(|b| matches!(b.bind, Bind::Chord(a, _) if a == c))
}

/// What a key did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    Fired {
        action: Action,
        count: Option<u32>,
    },
    /// Part of a count or a chord: consumed, nothing to do yet.
    Pending,
    /// Not ours: leave the event to the rest of the UI.
    Ignored,
}

/// Count prefix and chord state between key presses.
#[derive(Debug, Default, Clone)]
pub struct KeyState {
    count: Option<u32>,
    pending: Option<(char, Instant)>,
}

impl KeyState {
    /// Feeds one key pressed at `now`; `vim` is the Settings toggle.
    pub fn feed(&mut self, key: KeyIn, now: Instant, vim: bool) -> Feed {
        self.expire(now);
        if let Some((first, _)) = self.pending.take() {
            if let KeyIn::Char(c) = key {
                if let Some(action) = lookup(Bind::Chord(first, c), vim) {
                    return self.fire(action);
                }
            }
            // not the chord: it is dropped with its count, the key counts on its own
            self.count = None;
        }
        if vim {
            if let KeyIn::Char(d @ '0'..='9') = key {
                if d != '0' || self.count.is_some() {
                    let n = self.count.unwrap_or(0);
                    self.count = Some((n * 10 + d as u32 - '0' as u32).min(MAX_COUNT));
                    return Feed::Pending;
                }
            }
            if let KeyIn::Char(c) = key {
                if starts_chord(c) {
                    self.pending = Some((c, now));
                    return Feed::Pending;
                }
            }
        }
        match lookup(Bind::Key(key), vim) {
            Some(action) => self.fire(action),
            None => {
                self.count = None;
                Feed::Ignored
            }
        }
    }

    fn fire(&mut self, action: Action) -> Feed {
        Feed::Fired {
            action,
            count: self.count.take(),
        }
    }

    /// Drops a chord that waited too long (its count goes with it).
    pub fn expire(&mut self, now: Instant) {
        if let Some((_, at)) = self.pending {
            if now.saturating_duration_since(at) > CHORD_TIMEOUT {
                self.pending = None;
                self.count = None;
            }
        }
    }

    /// Forgets a count and chord in progress (a text field took the keyboard).
    pub fn reset(&mut self) {
        *self = KeyState::default();
    }

    /// What has been typed so far ("12", "g", "3g"), for the status line.
    pub fn pending(&self) -> Option<String> {
        let count = self.count.map(|n| n.to_string()).unwrap_or_default();
        let chord = self.pending.map(|(c, _)| c.to_string()).unwrap_or_default();
        let s = count + &chord;
        (!s.is_empty()).then_some(s)
    }

    /// When the pending chord times out (to repaint then and clear the status).
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, at)| at + CHORD_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
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
}
