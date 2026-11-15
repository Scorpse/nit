//! Placing drafts under diff rows and threads.

use super::*;

impl Composer {
    // ---- placement ----

    /// The draft open in the composer (its card is replaced by the composer).
    pub(super) fn editing(&self) -> Option<i64> {
        self.editor.as_ref().and_then(|e| e.draft_id)
    }

    /// Reply drafts under thread `t`, oldest first (not the one being edited).
    pub fn replies_to<'a>(&'a self, t: &'a Thread) -> impl Iterator<Item = &'a Draft> + 'a {
        let editing = self.editing();
        self.drafts
            .iter()
            .filter(move |d| d.replies_to(t) && Some(d.id) != editing)
    }

    pub fn editor_is(&self, target: &Target) -> bool {
        self.editor.as_ref().is_some_and(|e| &e.target == target)
    }

    pub fn of_kind<'a>(
        &'a self,
        kind: DraftKind,
        path: Option<&'a str>,
    ) -> impl Iterator<Item = &'a Draft> + 'a {
        let editing = self.editing();
        self.drafts.iter().filter(move |d| {
            d.kind == kind && (path.is_none() || d.path.as_deref() == path) && Some(d.id) != editing
        })
    }

    /// Lays drafts and the composer onto `path`'s rows next to the placed threads.
    pub fn plan(
        &self,
        path: &str,
        rows: &[Row],
        placement: &Placement,
        threads: &[Thread],
    ) -> RowPlan {
        let mut plan = RowPlan {
            path: path.to_string(),
            ..Default::default()
        };
        let reply_target = match self.editor.as_ref().map(|e| &e.target) {
            Some(Target::Reply { thread }) => Some(thread),
            _ => None,
        };
        for (row, ids) in &placement.inline {
            let stack = plan.rows.entry(*row).or_default();
            for &t in ids {
                stack.push(Slot::Thread(t));
                let th = &threads[t];
                stack.extend(self.replies_to(th).map(|d| Slot::Draft(d.id)));
                if th.id().is_some() && reply_target == th.id() {
                    stack.push(Slot::Editor);
                }
            }
        }
        let inline: Vec<&Draft> = self.of_kind(DraftKind::Inline, Some(path)).collect();
        let editor_at = match self.editor.as_ref().map(|e| &e.target) {
            Some(Target::Inline {
                path: p,
                side,
                line,
                ..
            }) if p == path => Some((*side == Side::New, *line)),
            _ => None,
        };
        let mut wanted: HashSet<(bool, u32)> = inline
            .iter()
            .filter_map(|d| Some((d.side? == Side::New, d.line?)))
            .collect();
        wanted.extend(editor_at);
        let at = row_index(rows, &wanted);
        for d in inline {
            match d
                .side
                .zip(d.line)
                .and_then(|(s, n)| at.get(&(s == Side::New, n)))
            {
                Some(&r) => plan.rows.entry(r).or_default().push(Slot::Draft(d.id)),
                None => plan.unplaced.push(d.id),
            }
        }
        if let Some(k) = editor_at {
            match at.get(&k) {
                Some(&r) => plan.rows.entry(r).or_default().push(Slot::Editor),
                None => plan.editor_unplaced = true,
            }
        }
        plan
    }

    /// Extra heights (beyond the thread cards) per row for `Comments::layout_with`; keeps
    /// the plan for painting.
    pub fn row_extras(
        &self,
        path: &str,
        rows: &[Row],
        placement: &Placement,
        threads: &[Thread],
        width: f32,
        measure: &mut dyn FnMut(&str, f32) -> f32,
    ) -> Vec<(usize, f32)> {
        let plan = self.plan(path, rows, placement, threads);
        let out = plan
            .rows
            .iter()
            .filter_map(|(&row, slots)| {
                let h: f32 = slots
                    .iter()
                    .map(|s| match *s {
                        Slot::Thread(_) => 0.0,
                        Slot::Draft(id) => self.draft(id).map_or(0.0, |d| {
                            draft_height(d, &self.state(id), width, measure) + CARD_MARGIN
                        }),
                        Slot::Editor => editor_height() + CARD_MARGIN,
                    })
                    .sum();
                (h > 0.0).then_some((row, h))
            })
            .collect();
        *self.plan.borrow_mut() = plan;
        out
    }

    /// The plan of the last layout of `path`.
    pub fn plan_for(&self, path: &str) -> Option<std::cell::Ref<'_, RowPlan>> {
        let p = self.plan.borrow();
        (p.path == path).then_some(p)
    }
}

/// First row showing each wanted (new side?, line); one pass over the rows.
pub(super) fn row_index(
    rows: &[Row],
    wanted: &HashSet<(bool, u32)>,
) -> HashMap<(bool, u32), usize> {
    let mut at = HashMap::new();
    if wanted.is_empty() {
        return at;
    }
    for (i, r) in rows.iter().enumerate() {
        if matches!(r.kind, RowKind::Context | RowKind::Add) {
            if let Some(n) = r.new_line.filter(|n| wanted.contains(&(true, *n))) {
                at.entry((true, n)).or_insert(i);
            }
        }
        if matches!(r.kind, RowKind::Context | RowKind::Del) {
            if let Some(n) = r.old_line.filter(|n| wanted.contains(&(false, *n))) {
                at.entry((false, n)).or_insert(i);
            }
        }
    }
    at
}
