//! Diff geometry and mouse selection.

use super::*;

/// Horizontal layout of the diff: number gutters, text column, comment cards.
pub(super) struct DiffGeometry {
    pub(super) char_w: f32,
    pub(super) num_w: f32,
}

impl DiffGeometry {
    pub(super) fn new(ui: &Ui) -> DiffGeometry {
        let char_w = ui
            .painter()
            .layout_no_wrap("M".into(), FontId::monospace(MONO), Color32::WHITE)
            .size()
            .x
            .max(1.0);
        DiffGeometry {
            char_w,
            num_w: 6.0 * char_w,
        }
    }

    /// Where the text column starts, from the left edge of the pane.
    pub(super) fn text_x(&self) -> f32 {
        2.0 * self.num_w + 2.0 * self.char_w
    }

    pub(super) fn gutter_w(&self) -> f32 {
        2.0 * self.num_w + 4.0
    }

    /// Cards start right of the gutters and stop short of the scroll bar.
    pub(super) fn card_x(&self) -> f32 {
        self.gutter_w() + 10.0
    }

    pub(super) fn card_w(&self, pane_w: f32) -> f32 {
        (pane_w - self.card_x() - 24.0).min(1000.0)
    }
}

/// How the selection maps the pointer to text in the shown layout.
pub(super) struct SelGeo<'a> {
    pub(super) offsets: &'a Offsets,
    /// Split view: the lines, and where the right side starts (from the pane's left edge).
    pub(super) split: Option<(&'a SplitMap, f32)>,
    /// Where the text starts, from the left edge of its side (of the pane, when unified).
    pub(super) text_x: f32,
}

impl SelGeo<'_> {
    /// The side under pane x `x`; None in the unified view.
    pub(super) fn side_at(&self, x: f32) -> Option<SplitSide> {
        self.split.map(|(_, half)| {
            if x < half {
                SplitSide::Left
            } else {
                SplitSide::Right
            }
        })
    }

    /// Where `side` starts, from the pane's left edge.
    pub(super) fn x0(&self, side: Option<SplitSide>) -> f32 {
        match (self.split, side) {
            (Some((_, half)), Some(SplitSide::Right)) => half,
            _ => 0.0,
        }
    }

    /// Screen lines: rows, or split lines.
    pub(super) fn len(&self, rows: &[prcore::diff::Row]) -> usize {
        self.split.map_or(rows.len(), |(m, _)| m.lines.len())
    }

    /// The text of screen line `line` on `side` (None: padding).
    pub(super) fn text<'r>(
        &self,
        rows: &'r [prcore::diff::Row],
        line: usize,
        side: Option<SplitSide>,
    ) -> Option<&'r str> {
        let row = match (self.split, side) {
            (Some((m, _)), Some(sd)) => m.lines.get(line)?.side(sd)?,
            _ => line,
        };
        rows.get(row).map(|r| r.text.as_str())
    }

    /// What pane x `x`, content y `y` is over.
    pub(super) fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        hit_test(self.offsets, x - self.x0(self.side_at(x)), y, self.text_x)
    }
}

/// Pointer and keyboard input for the text selection of the current diff: press-drag,
/// shift-click, double click (word), triple click (row), Esc, Ctrl+A, Ctrl+C, and autoscroll
/// at the edges. Presses in the gutter or on comment cards never start a selection (the
/// gutter is left to the comment composer). In the split view a selection stays on the side
/// it started on. Returns the screen line and side of a press on text (it moves the cursor).
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_selection(
    ui: &mut Ui,
    rows: &[prcore::diff::Row],
    g: &SelGeo<'_>,
    st: &mut SelectionState,
    vis: Rect,
    scroll_x: f32,
    origin_y: f32,
    char_w: f32,
) -> Option<(usize, Option<SplitSide>)> {
    let hit = |p: Pos2| g.hit(p.x - vis.left(), p.y - origin_y);
    let side_at = |p: Pos2| g.side_at(p.x - vis.left());
    let pos_at = |p: Pos2, side: Option<SplitSide>| -> Option<Pos> {
        let row = g.offsets.row_at(p.y - origin_y)?;
        let x = p.x - vis.left() - g.x0(side) - g.text_x + scroll_x;
        Some(Pos {
            row,
            col: x_to_col(g.text(rows, row, side).unwrap_or(""), x, char_w),
        })
    };
    let mut pressed = None;
    let resp = ui.interact(
        vis,
        ui.scope_id().with("diff-select"),
        Sense::click_and_drag(),
    );
    let (shift, press, latest, down, dt) = ui.input(|i| {
        (
            i.modifiers.shift,
            i.pointer.press_origin(),
            i.pointer.latest_pos(),
            i.pointer.primary_down(),
            i.stable_dt.min(0.1),
        )
    });
    if resp.hovered()
        && resp
            .hover_pos()
            .is_some_and(|p| matches!(hit(p), Some(Hit::Text(_))))
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
    }
    if resp.drag_started() {
        // a gutter press (Press::GutterClick) is left to the comment composer
        if let Some(o) = press.filter(|&o| matches!(press_action(hit(o)), Press::Begin(_))) {
            let side = side_at(o);
            if let Some(at) = pos_at(o, side) {
                st.begin_on(at, shift, side);
                pressed = Some((at.row, side));
            }
        }
    }
    if st.dragging {
        if !down || resp.drag_stopped() {
            st.end_drag();
        } else if let Some(p) = latest {
            // the pointer may wander to the other side: the selection stays on its own
            if let Some(at) = pos_at(p, st.side) {
                st.drag_to(at);
            }
            let dy = autoscroll_delta(p.y, vis.top(), vis.bottom(), dt);
            if dy != 0.0 {
                ui.scroll_with_delta_animation(
                    vec2(0.0, -dy),
                    egui::style::ScrollAnimation::none(),
                );
            }
            ui.ctx().request_repaint();
        }
    }
    if let Some(p) = resp.interact_pointer_pos() {
        let side = side_at(p);
        if let (Press::Begin(row), Some(at)) = (press_action(hit(p)), pos_at(p, side)) {
            let text = g.text(rows, row, side).unwrap_or("");
            if resp.triple_clicked() {
                st.side = side;
                st.select_row_in(text, row);
            } else if resp.double_clicked() {
                st.side = side;
                st.select_word_in(text, at);
            } else if resp.clicked() {
                st.click_on(at, shift, side);
                pressed = Some((row, side));
            }
        }
    }

    let ctx = ui.ctx().clone();
    if ctx.egui_wants_keyboard_input() {
        return pressed;
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::A)) {
        st.sel = Selection::all(g.len(rows));
        st.side = g.split.map(|_| st.side.unwrap_or_default());
    }
    if ctx.input(|i| i.key_pressed(Key::Escape)) {
        st.clear();
    }
    if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
        // built here, on demand: never per frame
        if let Some(sel) = st.sel {
            let text = match g.split {
                Some((m, _)) => selected_text_split(rows, m, st.side.unwrap_or_default(), &sel),
                None => selected_text(rows, &sel),
            };
            if !text.is_empty() {
                ctx.copy_text(text);
            }
        }
    }
    pressed
}

pub(super) fn placeholder(ui: &mut Ui, text: &str) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| ui.label(RichText::new(text).color(GREY)));
}
