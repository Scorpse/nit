//! Opening, saving, converting and deleting drafts.

use super::*;

impl Composer {
    // ---- editing ----

    /// Opens an empty composer for `target`; a composer with unsaved text is kept as a draft.
    pub fn open_editor(&mut self, target: Target) {
        if self.read_only {
            self.error = Some(format!("Commenting is {READ_ONLY_TIP}"));
            return;
        }
        if self.batch {
            return;
        }
        if matches!(target, Target::Inline { start: Some(_), .. }) && !self.multi_line() {
            self.error = Some("Comments on a range of lines are not supported here".into());
            return;
        }
        if self
            .editor
            .as_ref()
            .is_some_and(|e| e.target == target && e.draft_id.is_none())
        {
            return;
        }
        self.stash_editor();
        self.editor = Some(Editor {
            target,
            text: String::new(),
            draft_id: None,
            comment_type: None,
            focus: true,
            suggestions: capabilities_for(self.forge).suggestions,
        });
        self.bump();
    }

    /// The forge takes comments on a range of lines.
    pub fn multi_line(&self) -> bool {
        capabilities_for(self.forge).multi_line_comments
    }

    /// "Suggest change": appends the composer's new-side lines (from `rows`, the file's diff
    /// rows) as a suggestion in the forge's syntax. Refused, with a message, for old-side
    /// lines and for lines not shown in `rows`.
    pub fn insert_suggestion(&mut self, rows: &[Row]) {
        let Some(e) = self.editor.as_mut() else {
            return;
        };
        let Some(syntax) = e.suggestions else {
            return;
        };
        let Target::Inline {
            side: Side::New,
            line,
            start,
            ..
        } = e.target
        else {
            self.error = Some("Suggestions apply to new-side lines only".into());
            return;
        };
        let first = match start {
            None => line,
            Some((Side::New, n)) => n,
            Some((Side::Old, _)) => {
                self.error = Some("Suggestions apply to new-side lines only".into());
                return;
            }
        };
        if syntax == SuggestionSyntax::GitLabRange && line - first > SUGGESTION_MAX_OFFSET {
            self.error = Some(format!(
                "GitLab takes suggestions of at most {SUGGESTION_MAX_OFFSET} lines above the \
                 commented line"
            ));
            return;
        }
        let lines: Vec<String> = rows
            .iter()
            .filter(|r| r.new_line.is_some_and(|n| (first..=line).contains(&n)))
            .map(|r| r.text.clone())
            .collect();
        if lines.len() as u64 != u64::from(line - first) + 1 {
            self.error = Some("The commented lines are not all shown in this diff".into());
            return;
        }
        if !e.text.is_empty() && !e.text.ends_with('\n') {
            e.text.push('\n');
        }
        e.text
            .push_str(&suggestion_block(syntax, &lines, line, (first, line)));
        e.focus = true;
        self.bump();
    }

    /// Opens the composer on an existing draft.
    pub fn edit_draft(&mut self, id: i64) {
        if self.batch || self.editor.as_ref().is_some_and(|e| e.draft_id == Some(id)) {
            return;
        }
        if self.refuse_in_flight(id) {
            return;
        }
        let Some(d) = self.draft(id) else { return };
        let Some(target) = Target::of(d) else { return };
        let (text, comment_type) = (d.text.clone(), d.comment_type);
        self.stash_editor();
        self.editor = Some(Editor {
            target,
            text,
            draft_id: Some(id),
            comment_type,
            focus: true,
            suggestions: capabilities_for(self.forge).suggestions,
        });
        self.bump();
    }

    pub(super) fn stash_editor(&mut self) {
        if let Some(e) = self.editor.take() {
            if !e.text.trim().is_empty() {
                self.save(e);
            }
        }
    }

    /// Ctrl+Enter / "Save draft": stores the composer's text. Empty text is never saved;
    /// text that would run a GitLab quick action is refused and stays in the composer (a
    /// draft stashed by opening another composer is refused when posting instead).
    pub fn save_editor(&mut self) -> bool {
        let Some(e) = self.editor.take() else {
            return false;
        };
        self.bump();
        if e.text.trim().is_empty() {
            return false;
        }
        if let Err(why) = quick_action_guard(self.forge, &e.text) {
            self.error = Some(why);
            self.editor = Some(e);
            return false;
        }
        self.save(e)
    }

    pub fn cancel_editor(&mut self) {
        if self.editor.take().is_some() {
            self.bump();
        }
    }

    pub(super) fn save(&mut self, e: Editor) -> bool {
        let existing = e.draft_id.and_then(|id| self.draft(id)).cloned();
        let mut d = existing.unwrap_or_else(|| Draft {
            id: 0,
            repo: self.repo.clone(),
            pr_id: self.pr_id,
            kind: DraftKind::Pr,
            path: None,
            line: None,
            side: None,
            start: None,
            reply_to: None,
            text: String::new(),
            source_commit: self.head.clone(),
            comment_type: None,
        });
        e.target.fill(&mut d);
        d.text = e.text.clone();
        d.comment_type = e.comment_type;
        let saved = self.lock().save_draft(&d);
        self.bump();
        match saved {
            Ok(id) => {
                d.id = id;
                self.drafts.retain(|x| x.id != id);
                self.drafts.push(d);
                self.drafts.sort_by_key(|x| x.id);
                // an edited draft is checked again on the next submit; one that may already
                // be posted stays unconfirmed (only "Post again anyway" re-arms it)
                if !matches!(self.states.get(&id), Some(DraftState::Unconfirmed { .. })) {
                    self.states.remove(&id);
                }
                true
            }
            Err(_) => {
                self.error = Some("Could not save the draft".into());
                self.editor = Some(e);
                false
            }
        }
    }

    /// Deletes a draft locally (it was never posted).
    pub fn delete_draft(&mut self, id: i64) {
        if self.batch || self.refuse_in_flight(id) {
            return;
        }
        if self.lock().delete_draft(id).is_err() {
            self.error = Some("Could not delete the draft".into());
            return;
        }
        self.drafts.retain(|d| d.id != id);
        self.states.remove(&id);
        if self.editor.as_ref().is_some_and(|e| e.draft_id == Some(id)) {
            self.editor = None;
        }
        self.bump();
    }

    /// "Post as file comment instead" on an inline draft: an explicit click that replaces it
    /// by a new file draft whose text starts with `(line N)`. Nothing is posted here.
    pub fn as_file_comment(&mut self, id: i64) {
        self.convert_inline(id, DraftKind::File);
    }

    /// "Post as PR comment instead" on an inline draft: an explicit click that replaces it
    /// by a new PR draft whose text starts with `(path line N)`. Nothing is posted here.
    pub fn as_pr_comment(&mut self, id: i64) {
        self.convert_inline(id, DraftKind::Pr);
    }

    pub(super) fn convert_inline(&mut self, id: i64, to: DraftKind) {
        if self.batch || self.refuse_in_flight(id) {
            return;
        }
        let Some(d) = self.draft(id).cloned() else {
            return;
        };
        let (DraftKind::Inline, Some(path)) = (d.kind, d.path.clone()) else {
            return;
        };
        // a range keeps its first line in the text: "(lines 9-12)"
        let lines = |n: u32| match d.start {
            Some((_, s)) => format!("lines {s}-{n}"),
            None => format!("line {n}"),
        };
        let (kept_path, text) = match (to, d.line) {
            (DraftKind::File, Some(n)) => (Some(path), format!("({}) {}", lines(n), d.text)),
            (DraftKind::File, None) => (Some(path), d.text.clone()),
            (_, Some(n)) => (None, format!("({path} {}) {}", lines(n), d.text)),
            (_, None) => (None, format!("({path}) {}", d.text)),
        };
        let file = Draft {
            id: 0,
            kind: to,
            path: kept_path,
            line: None,
            side: None,
            start: None,
            reply_to: None,
            text,
            ..d
        };
        let saved = self.lock().save_draft(&file);
        match saved {
            Ok(new_id) => {
                self.drafts.push(Draft { id: new_id, ..file });
                self.drafts.sort_by_key(|x| x.id);
                self.delete_draft(id);
            }
            Err(_) => self.error = Some("Could not save the file draft".into()),
        }
        self.bump();
    }

    /// After a (re)fetch: inline drafts written on another head than `head` are flagged as
    /// outdated at once (a submit checks every inline draft again anyway).
    pub fn revalidate_heads(&mut self) {
        for d in &self.drafts {
            if d.kind == DraftKind::Inline && !same_commit_strict(&d.source_commit, &self.head) {
                let st = self.states.entry(d.id).or_default();
                if *st == DraftState::Idle {
                    *st = DraftState::Outdated;
                }
            }
        }
        self.bump();
    }

    /// Scene setup and tests: marks a draft as blocked without running a batch.
    pub fn mark_blocked(&mut self, id: i64, block: InlineBlock) {
        self.states.insert(id, DraftState::Blocked(block));
        self.bump();
    }

    // ---- unconfirmed posts ----

    /// "Check comments" on an unconfirmed draft: the next comments that arrive are compared
    /// with it. True when the caller should refresh the comments now.
    pub fn check_comments(&mut self, id: i64) -> bool {
        if !matches!(self.state(id), DraftState::Unconfirmed { .. }) {
            return false;
        }
        self.checking.insert(id);
        true
    }

    /// Freshly fetched comments: every draft waiting for a check is marked checked, with a
    /// hint when a comment of mine with exactly its text sits in its place. Nothing is
    /// deleted or posted here.
    pub fn comments_checked(&mut self, comments: &Comments, me: Option<&UserRef>) {
        for id in std::mem::take(&mut self.checking) {
            let Some(d) = self.draft(id) else { continue };
            if !matches!(self.state(id), DraftState::Unconfirmed { .. }) {
                continue;
            }
            // what was sent, and what the draft would send now (edited after the unknown
            // outcome); both with the type's prefix, exactly as posted
            let now = d.posted_text();
            let texts: Vec<String> = self
                .sent_text
                .get(&id)
                .into_iter()
                .chain(std::iter::once(&now))
                .map(|t| normalize_for_match(self.forge, t))
                .collect();
            let looks_posted = me.is_some_and(|me| {
                !me.id.is_empty()
                    && comments
                        .threads
                        .iter()
                        .flat_map(|t| std::iter::once(&t.root).chain(&t.replies))
                        .any(|c| {
                            !c.deleted
                                && c.author.id == me.id
                                && texts.contains(&normalize_for_match(self.forge, &c.body))
                                && same_place(c, d)
                        })
            });
            self.states.insert(
                id,
                DraftState::Unconfirmed {
                    checked: true,
                    looks_posted,
                },
            );
            self.bump();
        }
    }

    /// "Post again anyway": an explicit, per-draft choice, offered only after the comments
    /// were checked. The draft becomes postable again. Returns whether it was re-armed.
    pub fn rearm(&mut self, id: i64) -> bool {
        if self.batch
            || !matches!(
                self.state(id),
                DraftState::Unconfirmed { checked: true, .. }
            )
        {
            return false;
        }
        if self.lock().clear_draft_posting(id).is_err() {
            self.error = Some("Could not update the draft".into());
            return false;
        }
        self.states.remove(&id);
        self.sent_text.remove(&id);
        self.bump();
        true
    }
}
