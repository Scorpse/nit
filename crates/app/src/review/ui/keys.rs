//! Keyboard handling: which keys are allowed, event mapping, the help overlay.

use super::*;

/// j/k/v act only when no text field has keyboard focus and no command modifier is held
/// (Ctrl+V, Alt+J and the like belong to someone else).
pub fn nav_keys_allowed(wants_keyboard: bool, m: egui::Modifiers) -> bool {
    !(wants_keyboard || m.ctrl || m.alt || m.command || m.mac_cmd)
}

/// Review shortcuts (j/k/v/c): as `nav_keys_allowed`, and never while a modal is open.
pub fn review_keys_allowed(wants_keyboard: bool, m: egui::Modifiers, modal_open: bool) -> bool {
    !modal_open && nav_keys_allowed(wants_keyboard, m)
}

/// File list keys (arrows, paging, Left / Right, Space, Enter): as `review_keys_allowed`,
/// and not while the pointer is over the central pane, whose scroll area keeps egui's own
/// arrow and page scrolling.
pub fn list_keys_allowed(
    wants_keyboard: bool,
    m: egui::Modifiers,
    modal_open: bool,
    pointer_over_center: bool,
) -> bool {
    !pointer_over_center && review_keys_allowed(wants_keyboard, m, modal_open)
}

/// A file list key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKey {
    Nav(Nav),
    Left,
    Right,
    /// Toggle viewed (a file) or the tri-state box (a directory).
    Space,
    /// On a file: show its diff.
    Enter,
}

impl ListKey {
    pub(super) fn pressed(i: &egui::InputState) -> Option<ListKey> {
        if let Some(n) = Nav::pressed(i) {
            return Some(ListKey::Nav(n));
        }
        [
            (Key::ArrowLeft, ListKey::Left),
            (Key::ArrowRight, ListKey::Right),
            (Key::Space, ListKey::Space),
            (Key::Enter, ListKey::Enter),
        ]
        .into_iter()
        .find_map(|(k, v)| i.key_pressed(k).then_some(v))
    }
}

/// Id of the file list's filter field (`t` focuses it).
pub(super) const FILTER_ID: &str = "review-file-filter";
/// Id of the diff's search field.
pub(super) const SEARCH_ID: &str = "review-diff-search";

/// A key event as the keymap sees it: typed characters as typed (any layout), Ctrl / Alt
/// with a letter, and Enter / Backspace / Esc without modifiers. Alt+letter text is not a
/// typed character.
pub(super) fn key_in(e: &egui::Event, alt: bool) -> Option<KeyIn> {
    match e {
        egui::Event::Text(t) if !alt => {
            let mut cs = t.chars();
            let c = cs.next()?;
            (cs.next().is_none() && !c.is_whitespace() && !c.is_control()).then_some(KeyIn::Char(c))
        }
        egui::Event::Key {
            key,
            pressed: true,
            modifiers: m,
            ..
        } => {
            let letter = || {
                let mut n = key.name().chars();
                match (n.next(), n.next()) {
                    (Some(c), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_lowercase()),
                    _ => None,
                }
            };
            if m.ctrl && !m.alt && !m.shift {
                letter().map(KeyIn::Ctrl)
            } else if m.alt && !m.ctrl && !m.command && !m.shift {
                letter().map(KeyIn::Alt)
            } else if m.is_none() {
                match key {
                    Key::Enter => Some(KeyIn::Enter),
                    Key::Backspace => Some(KeyIn::Backspace),
                    Key::Escape => Some(KeyIn::Escape),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Feeds this frame's key events to the keymap and carries out what fired. The events it
/// used are removed, so a text field focused by a key (`t`, `/`) never types that key.
/// Esc and Enter stay for the selection and the file list.
pub(super) fn handle_keys(ctx: &egui::Context, s: &mut ReviewScreen) -> KeyEffects {
    let now = Instant::now();
    let (events, alt) = ctx.input(|i| (i.events.clone(), i.modifiers.alt));
    let mut fx = KeyEffects::default();
    let mut used = Vec::new();
    for (k, e) in events.iter().enumerate() {
        let Some(key) = key_in(e, alt) else {
            continue;
        };
        match s.keys.feed(key, now, s.vim) {
            Feed::Ignored => continue,
            Feed::Pending => {}
            Feed::Fired { action, count } => {
                let f = s.key_action(action, count);
                fx.acts.extend(f.acts);
                fx.back |= f.back;
                fx.copy = f.copy.or(fx.copy);
                fx.focus_filter |= f.focus_filter;
            }
        }
        if !matches!(key, KeyIn::Escape | KeyIn::Enter) {
            used.push(k);
        }
    }
    if !used.is_empty() {
        ctx.input_mut(|i| {
            let mut k = 0;
            i.events.retain(|_| {
                k += 1;
                !used.contains(&(k - 1))
            });
        });
        ctx.request_repaint();
    }
    s.keys.expire(now);
    if let Some(d) = s.keys.deadline() {
        // the pending `g` is shown until it times out
        ctx.request_repaint_after(d.saturating_duration_since(now));
    }
    fx
}

/// The `?` overlay: every shortcut from the keymap table, by section.
pub(super) fn help_overlay(ctx: &egui::Context, open: &mut bool, vim: bool) {
    let mut close = false;
    let resp = egui::Modal::new(egui::Id::unique("review-keyboard-help")).show(ctx, |ui| {
        ui.label(RichText::new("Keyboard shortcuts").strong().size(16.0));
        if !vim {
            ui.label(RichText::new("Vim keys are off (Settings): only these work.").color(AMBER));
        }
        ui.add_space(6.0);
        // one grid per column, so the descriptions of all its sections line up
        ui.horizontal_top(|ui| {
            for (c, groups) in [&Group::ALL[..2], &Group::ALL[2..]].into_iter().enumerate() {
                ui.vertical(|ui| {
                    egui::Grid::new(("help", c))
                        .num_columns(2)
                        .spacing(vec2(14.0, 3.0))
                        .show(ui, |ui| {
                            for g in groups {
                                let rows = help_rows(*g, vim);
                                if rows.is_empty() {
                                    continue;
                                }
                                ui.label(RichText::new(g.title()).strong().color(AMBER));
                                ui.end_row();
                                for (keys, text) in rows {
                                    ui.label(RichText::new(keys).monospace().color(Color32::WHITE));
                                    ui.label(text);
                                    ui.end_row();
                                }
                                ui.label("");
                                ui.end_row();
                            }
                        });
                });
                ui.add_space(18.0);
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("? or Esc closes this help.").color(GREY));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.button("Close").clicked();
            });
        });
    });
    if resp.should_close() || close {
        *open = false;
    }
}
