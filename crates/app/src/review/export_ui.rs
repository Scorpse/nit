//! "Copy as markdown" on the review top bar (and Ctrl+Shift+M): the review rendered by
//! `prcore::export` for the chosen scope, put on the clipboard.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use eframe::egui::{self, Id, Key, Modifiers, RichText, Ui};
use prcore::diff::Row;
use prcore::export::{export_markdown, ExportInput, ExportScope, EXPORT_SCOPES};

use super::ui::ReviewScreen;
use crate::theme::GREEN;

pub const COPY_LABEL: &str = "Copy as markdown";
/// How long "Copied" shows after a copy.
const COPIED_FOR: Duration = Duration::from_secs(2);

/// The review as markdown for `scope`: threads and drafts as loaded now, code excerpts from
/// the patches already on screen (nothing is fetched for an export).
pub fn markdown(s: &ReviewScreen, scope: ExportScope) -> String {
    let diff_paths: Option<HashSet<String>> = s.files.as_ref().map(|l| {
        l.files()
            .iter()
            .flat_map(|f| std::iter::once(f.path.clone()).chain(f.old_path.clone()))
            .collect()
    });
    let anchored = s
        .comments
        .threads
        .iter()
        .filter_map(|t| t.root.anchor.as_ref().map(|a| a.path.clone()))
        .chain(s.composer.drafts.iter().filter_map(|d| d.path.clone()));
    let mut patches: HashMap<String, &[Row]> = HashMap::new();
    for p in anchored {
        if let Some(v) = s.diff.view(&p) {
            patches.insert(p, v.rows.as_slice());
        }
    }
    export_markdown(
        &ExportInput {
            pr: &s.pr,
            threads: &s.comments.threads,
            drafts: &s.composer.drafts,
            me: s.actions.ctx.me.as_ref(),
            diff_paths: diff_paths.as_ref(),
            patches: &patches,
        },
        scope,
    )
}

/// What this window remembers between frames: the last scope and when it was copied.
#[derive(Clone, Copy, Default)]
struct Memo {
    scope: ExportScope,
    at: Option<Instant>,
}

fn memo_id() -> Id {
    Id::unique("review-export-markdown")
}

fn memo(ctx: &egui::Context) -> Memo {
    ctx.data(|d| d.get_temp::<Memo>(memo_id()))
        .unwrap_or_default()
}

/// Copies the review for `scope` and remembers the scope for the shortcut.
pub fn copy(ctx: &egui::Context, s: &ReviewScreen, scope: ExportScope) {
    ctx.copy_text(markdown(s, scope));
    ctx.data_mut(|d| {
        d.insert_temp(
            memo_id(),
            Memo {
                scope,
                at: Some(Instant::now()),
            },
        )
    });
    ctx.request_repaint_after(COPIED_FOR);
}

/// Ctrl+Shift+M (Cmd+Shift+M): the scope to copy (the last one chosen), unless a dialog is
/// open. Ctrl+Shift+C is not used: the platform turns it into a plain copy.
pub fn shortcut(ctx: &egui::Context, modal_open: bool) -> Option<ExportScope> {
    if modal_open {
        return None;
    }
    ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::M))
        .then(|| memo(ctx).scope)
}

/// The top bar's "Copy as markdown" menu: one entry per scope; a click copies at once.
pub fn menu(ui: &mut Ui, s: &ReviewScreen) {
    let m = memo(ui.ctx());
    if m.at.is_some_and(|t| t.elapsed() < COPIED_FOR) {
        ui.label(RichText::new("Copied").color(GREEN));
    }
    ui.menu_button(COPY_LABEL, |ui| {
        for (scope, label) in EXPORT_SCOPES {
            let text = if scope == m.scope {
                RichText::new(label).strong()
            } else {
                RichText::new(label)
            };
            if ui.button(text).clicked() {
                copy(ui.ctx(), s, scope);
                ui.close();
            }
        }
        ui.separator();
        ui.label(
            RichText::new(format!(
                "{} copies the bold one again",
                super::keymap::CMD_SHIFT_M
            ))
            .color(crate::theme::GREY)
            .size(12.0),
        );
    })
    .response
    .on_hover_text("Copy the review as markdown (for notes or an LLM)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use chrono::{TimeZone, Utc};
    use prcore::comment_type::CommentType;
    use prcore::comments::{build_threads, Anchor, Comment};
    use prcore::diff::RowKind;
    use prcore::draft::{Draft, DraftKind};
    use prcore::model::UserRef;
    use store::Store;

    fn screen() -> ReviewScreen {
        let (_, prs, _) = crate::demo::demo_data();
        let pr = prs[0].clone();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        ReviewScreen::new(pr.repo.clone(), pr, store)
    }

    #[test]
    fn export_reads_threads_drafts_me_and_loaded_patches() {
        let mut s = screen();
        let t = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let root = Comment {
            id: 1.into(),
            parent_id: None,
            author: UserRef {
                id: "{a}".into(),
                display_name: "Ana".into(),
                login: None,
            },
            body: "**nit:** Name it.".into(),
            created_on: t,
            updated_on: t,
            deleted: false,
            resolved: false,
            anchor: Some(Anchor {
                path: "a.rs".into(),
                from: None,
                to: Some(2),
                start: None,
                outdated: false,
                old_path: None,
                commit: None,
            }),
            author_is_bot: false,
            thread: Some(prcore::comments::ThreadId::from(1)),
            can_resolve: true,
        };
        s.comments.set(build_threads(vec![root]), 0);
        s.composer.drafts.push(Draft {
            id: 5,
            repo: s.repo.clone(),
            pr_id: s.pr.id,
            kind: DraftKind::Pr,
            path: None,
            line: None,
            side: None,
            start: None,
            reply_to: None,
            text: "Overall fine.".into(),
            source_commit: s.pr.source_commit.clone(),
            comment_type: Some(CommentType::Praise),
        });
        s.actions.ctx.me = Some(UserRef {
            id: "{me}".into(),
            display_name: "Me".into(),
            login: None,
        });
        let row = |kind, n: u32, text: &str| Row {
            kind,
            old_line: Some(n),
            new_line: Some(n),
            text: text.into(),
        };
        let without = markdown(&s, ExportScope::Unresolved);
        assert!(!without.contains("```diff"), "no patch loaded: no excerpt");
        s.diff.set_patch_computed(
            "a.rs",
            Arc::new(vec![
                row(RowKind::Context, 1, "a"),
                row(RowKind::Add, 2, "b"),
            ]),
            false,
        );
        let md = markdown(&s, ExportScope::Unresolved);
        assert!(md.starts_with(&format!("# PR #{}: ", s.pr.id)), "{md}");
        assert!(
            md.contains("## 1. PR-level [praise]\n\n**Me** · draft"),
            "{md}"
        );
        assert!(
            md.contains("## 2. `a.rs:2` [nit]\n\n**Ana** · unresolved"),
            "{md}"
        );
        assert!(md.contains("```diff\n a\n+b\n```"), "{md}");
        let mine = markdown(&s, ExportScope::MyDrafts);
        assert!(
            mine.contains("- Items: 1\n") && !mine.contains("Ana"),
            "{mine}"
        );
    }
}
