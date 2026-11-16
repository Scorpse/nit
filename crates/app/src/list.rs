//! The virtualized PR list (rows painted by hand) and the detail pane.

use std::collections::HashSet;
use std::sync::Arc;

use eframe::egui::{
    self, text::LayoutJob, vec2, Align2, Color32, FontId, Galley, Painter, Rect, Sense, TextFormat,
    Ui,
};
use prcore::model::{
    CheckState, DiffStat, Forge, PrState, PullRequest, RepoRef, ReviewState, BB_MAX_FILES,
};

use crate::inbox::{Inbox, InboxRow};
use crate::theme::{AMBER, GREEN, GREY, RED};

pub const ROW_H: f32 = 26.0;
const INDENT: f32 = 22.0;

pub enum Item {
    Header { name: String, count: usize },
    Row { idx: usize, depth: u8 },
}

/// Flattens the inbox into header and row items; `depth` is the number of ancestors in the
/// same stack that are themselves rows (merged parents are not shown, so they do not indent).
pub fn build_items(inbox: &Inbox) -> Vec<Item> {
    let shown: HashSet<(&RepoRef, u64)> =
        inbox.rows.iter().map(|r| (&r.pr.repo, r.pr.id)).collect();
    let mut items = Vec::with_capacity(inbox.rows.len() + 32);
    let mut last: Option<&RepoRef> = None;
    for (idx, row) in inbox.rows.iter().enumerate() {
        if last != Some(&row.pr.repo) {
            let count = inbox.rows[idx..]
                .iter()
                .take_while(|r| r.pr.repo == row.pr.repo)
                .count();
            items.push(Item::Header {
                name: row.pr.repo.full_name(),
                count,
            });
            last = Some(&row.pr.repo);
        }
        let mut depth = 0u8;
        if let Some(si) = row.stack {
            let stack = &inbox.stacks[si];
            let mut cur = row.pr.id;
            for _ in 0..stack.members.len() {
                let Some(&(parent, _)) = stack.edges.iter().find(|&&(_, c)| c == cur) else {
                    break;
                };
                if shown.contains(&(&row.pr.repo, parent)) {
                    depth += 1;
                }
                cur = parent;
            }
        }
        items.push(Item::Row { idx, depth });
    }
    items
}

/// A list navigation key (arrows, paging, Home / End).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

impl Nav {
    /// The navigation key pressed this frame, if any (Up and Down first).
    pub fn pressed(i: &egui::InputState) -> Option<Nav> {
        use egui::Key;
        [
            (Key::ArrowUp, Nav::Up),
            (Key::ArrowDown, Nav::Down),
            (Key::PageUp, Nav::PageUp),
            (Key::PageDown, Nav::PageDown),
            (Key::Home, Nav::Home),
            (Key::End, Nav::End),
        ]
        .into_iter()
        .find_map(|(k, n)| i.key_pressed(k).then_some(n))
    }
}

/// Where `key` moves a selection at row `cur` of `n` rows, only `selectable` rows being
/// eligible (headers are not). Clamps at both ends; a page is `page` rows. With no current
/// row, End goes to the last selectable row and every other key to the first. None when no
/// row is selectable.
pub fn nav_step(
    n: usize,
    selectable: impl Fn(usize) -> bool,
    cur: Option<usize>,
    key: Nav,
    page: usize,
) -> Option<usize> {
    let first = (0..n).find(|&k| selectable(k))?;
    let last = (0..n).rev().find(|&k| selectable(k))?;
    let page = page.max(1);
    let Some(c) = cur.filter(|&c| c < n) else {
        return Some(if key == Nav::End { last } else { first });
    };
    // the first selectable row at or after `from` (<= last), or at or before it (>= first)
    let down = |from: usize| (from..=last).find(|&k| selectable(k)).unwrap_or(last);
    let up = |from: usize| {
        (first..=from)
            .rev()
            .find(|&k| selectable(k))
            .unwrap_or(first)
    };
    Some(match key {
        Nav::Down => down((c + 1).min(last)),
        Nav::Up => up(c.saturating_sub(1).max(first)),
        Nav::PageDown => down((c + page).min(last)),
        Nav::PageUp => up(c.saturating_sub(page).max(first)),
        Nav::Home => first,
        Nav::End => last,
    })
}

/// The PR `key` moves the inbox selection to, in display order (stack indentation order),
/// skipping repository headers.
pub fn nav_items(
    inbox: &Inbox,
    items: &[Item],
    selected: Option<&(RepoRef, u64)>,
    key: Nav,
    page: usize,
) -> Option<(RepoRef, u64)> {
    let pr_at = |k: usize| match &items[k] {
        Item::Row { idx, .. } => Some(&inbox.rows[*idx].pr),
        Item::Header { .. } => None,
    };
    let cur = selected.and_then(|(r, id)| {
        (0..items.len()).find(|&k| pr_at(k).is_some_and(|p| p.repo == *r && p.id == *id))
    });
    let to = nav_step(items.len(), |k| pr_at(k).is_some(), cur, key, page)?;
    pr_at(to).map(|p| (p.repo.clone(), p.id))
}

/// Item position of the selected PR (to scroll it into view).
pub fn item_pos(inbox: &Inbox, items: &[Item], selected: &(RepoRef, u64)) -> Option<usize> {
    items.iter().position(|it| match it {
        Item::Row { idx, .. } => {
            let p = &inbox.rows[*idx].pr;
            p.repo == selected.0 && p.id == selected.1
        }
        Item::Header { .. } => false,
    })
}

/// What a click on a PR row asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowClick {
    /// Select the row (a single click, or the first click of a double click).
    Select,
    /// Select it and open its review (a double click with the Review button enabled).
    Open,
    /// Select it and say why it cannot be reviewed (a double click, button not enabled).
    ShowReason(&'static str),
}

/// Shown when a fork's row is double-clicked (its Review button is hidden).
pub const FORK_REASON: &str = "Pull requests from forks are not supported";

/// A click on a PR row: a double click opens the review exactly when the Review button
/// (of that PR, now selected) is enabled; otherwise it only selects and shows the reason.
pub fn row_click(double: bool, button: Option<crate::settings_ui::ReviewButton>) -> RowClick {
    use crate::settings_ui::ReviewButton;
    if !double {
        return RowClick::Select;
    }
    match button {
        Some(ReviewButton::Enabled) => RowClick::Open,
        Some(ReviewButton::Disabled(why)) => RowClick::ShowReason(why),
        Some(ReviewButton::Hidden) => RowClick::ShowReason(FORK_REASON),
        None => RowClick::Select,
    }
}

/// What `show_list` saw this frame.
pub struct ListOut {
    /// The PR row clicked, and whether it was a double click.
    pub clicked: Option<((RepoRef, u64), bool)>,
    /// (scroll offset, viewport height), for keeping the keyboard selection in view.
    pub view: (f32, f32),
}

/// Draws the visible slice of `items`; `scroll` forces the scroll offset once.
pub fn show_list(
    ui: &mut Ui,
    inbox: &Inbox,
    items: &[Item],
    selected: Option<&(RepoRef, u64)>,
    scroll: Option<f32>,
) -> ListOut {
    let mut clicked = None;
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut area = egui::ScrollArea::vertical().auto_shrink(false);
    if let Some(y) = scroll {
        area = area.vertical_scroll_offset(y);
    }
    let out = area.show_rows(ui, ROW_H, items.len(), |ui, range| {
        for item in &items[range] {
            match item {
                Item::Header { name, count } => header_row(ui, name, *count),
                Item::Row { idx, depth } => {
                    let row = &inbox.rows[*idx];
                    let is_sel =
                        selected.is_some_and(|(r, id)| *r == row.pr.repo && *id == row.pr.id);
                    let r = pr_row(ui, row, *depth, is_sel);
                    if r.double_clicked() || r.clicked() {
                        let key = (row.pr.repo.clone(), row.pr.id);
                        clicked = Some((key, r.double_clicked()));
                    }
                }
            }
        }
    });
    ListOut {
        clicked,
        view: (out.state.clamped_offset().y, out.inner_rect.height()),
    }
}

/// The scroll offset that brings row `pos` (of height `row_h`) into the viewport
/// `(offset, height)`; None when it is already fully visible.
pub fn reveal_offset(pos: usize, row_h: f32, (off, h): (f32, f32)) -> Option<f32> {
    let y = pos as f32 * row_h;
    if y < off {
        Some(y)
    } else if y + row_h > off + h {
        Some(y + row_h - h)
    } else {
        None
    }
}

fn header_row(ui: &mut Ui, name: &str, count: usize) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
    let r = p.text(
        rect.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        name,
        FontId::proportional(14.0),
        ui.visuals().strong_text_color(),
    );
    p.text(
        egui::pos2(r.right() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        format!("{count} open"),
        FontId::proportional(12.0),
        GREY,
    );
}

/// Draws a rounded label starting at (`right_aligned` false) or ending at `x`; returns the
/// far edge: the right edge when drawing left-to-right, the left edge otherwise.
fn pill(
    p: &Painter,
    x: f32,
    cy: f32,
    text: &str,
    fg: Color32,
    bg: Color32,
    right_aligned: bool,
) -> f32 {
    let g: Arc<Galley> = p.layout_no_wrap(text.into(), FontId::proportional(11.5), fg);
    let size = g.size() + vec2(12.0, 4.0);
    let left = if right_aligned { x - size.x } else { x };
    let rect = Rect::from_min_size(egui::pos2(left, cy - size.y / 2.0), size);
    p.rect_filled(rect, 4.0, bg);
    p.galley(rect.min + vec2(6.0, 2.0), g, fg);
    if right_aligned {
        rect.left()
    } else {
        rect.right()
    }
}

/// "3 files", "1 file"; GitLab's capped count (`"1000+"`) as "1000+ files".
pub fn files_text(forge: Forge, files: u32) -> String {
    if forge == Forge::GitLab && files >= providers::gitlab::CAPPED_FILES {
        return "1000+ files".into();
    }
    format!("{files} {}", if files == 1 { "file" } else { "files" })
}

/// Whether the forge's size carries line counts: GitLab's is files only (decision D6).
pub fn has_line_counts(forge: Forge) -> bool {
    forge != Forge::GitLab
}

fn size_job(d: &DiffStat, forge: Forge) -> LayoutJob {
    let font = FontId::proportional(12.5);
    let mut job = LayoutJob::default();
    let fmt = |c| TextFormat {
        font_id: font.clone(),
        color: c,
        ..Default::default()
    };
    if !has_line_counts(forge) {
        job.append(&files_text(forge, d.files), 0.0, fmt(GREY));
        return job;
    }
    job.append(&format!("{}  ", files_text(forge, d.files)), 0.0, fmt(GREY));
    job.append(&format!("+{}", d.lines_added), 0.0, fmt(GREEN));
    job.append(" / ", 0.0, fmt(GREY));
    job.append(&format!("-{}", d.lines_removed), 0.0, fmt(RED));
    job
}

/// The status pills of an open PR, in display order: what a reviewer or author must act on
/// first (changes requested, failed checks, conflicts), then the good news.
pub fn status_pills(pr: &PullRequest) -> Vec<(String, Color32)> {
    let count = |s: ReviewState| pr.reviewers.iter().filter(|r| r.state == s).count();
    let (approved, changes) = (
        count(ReviewState::Approved),
        count(ReviewState::ChangesRequested),
    );
    let mut out = Vec::new();
    if changes > 0 {
        out.push(("changes requested".into(), RED));
    }
    if pr.has_conflicts {
        out.push(("conflicts".into(), RED));
    }
    match pr.checks {
        CheckState::Failing => out.push(("checks failed".into(), RED)),
        CheckState::Running => out.push(("checks running".into(), AMBER)),
        CheckState::Passing => out.push(("checks passed".into(), GREEN)),
        CheckState::Unknown => {}
    }
    if pr.task_count > 0 {
        out.push((format!("{} open tasks", pr.task_count), AMBER));
    }
    // only the yes is shown; no pill already means not approved
    if changes == 0 && approved > 0 {
        out.push(("approved".into(), GREEN));
    }
    out
}

fn pr_row(ui: &mut Ui, row: &InboxRow, depth: u8, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let p = ui.painter_at(rect);
    if selected {
        p.rect_filled(rect, 0.0, ui.visuals().selection.bg_fill);
    } else if resp.hovered() {
        p.rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let cy = rect.center().y;

    // Right side, laid out right to left: size badge, then the limits marker.
    let mut right = rect.right() - 10.0;
    match &row.stat {
        Some(d) => {
            let g = p.layout_job(size_job(d, row.pr.forge));
            let pos = egui::pos2(right - g.size().x, cy - g.size().y / 2.0);
            right = pos.x;
            p.galley(pos, g, GREY);
            if d.exceeds_web_ui(row.pr.forge) {
                right = pill(
                    &p,
                    right - 10.0,
                    cy,
                    over_limit_label(row.pr.forge),
                    Color32::WHITE,
                    Color32::from_rgb(170, 40, 40),
                    true,
                );
            }
        }
        None => {
            let r = p.text(
                egui::pos2(right, cy),
                Align2::RIGHT_CENTER,
                "size pending",
                FontId::proportional(12.0),
                GREY,
            );
            right = r.left();
        }
    }

    // status pills left of the size badge (they are listed right to left)
    for (text, color) in status_pills(&row.pr).iter().rev() {
        right = pill(&p, right - 8.0, cy, text, Color32::BLACK, *color, true);
    }

    // Left side, clipped so it can never run into the right side.
    let lp = p.with_clip_rect(Rect::from_min_max(
        rect.min,
        egui::pos2(right - 12.0, rect.max.y),
    ));
    let mut x = rect.left() + 10.0 + f32::from(depth) * INDENT;
    if depth > 0 {
        let s = egui::Stroke::new(1.0, GREY);
        let lx = x - INDENT / 2.0 - 2.0;
        lp.line_segment([egui::pos2(lx, rect.top()), egui::pos2(lx, cy)], s);
        lp.line_segment([egui::pos2(lx, cy), egui::pos2(lx + 9.0, cy)], s);
    }
    let id = lp.text(
        egui::pos2(x, cy),
        Align2::LEFT_CENTER,
        row.pr.forge.change_ref(row.pr.id),
        FontId::proportional(13.0),
        GREY,
    );
    x = id.right() + 8.0;
    // the forge of PRs from accounts other than Bitbucket Cloud (text tag, no logo)
    if let Some(tag) = forge_tag(&row.pr) {
        x = pill(
            &lp,
            x,
            cy,
            tag,
            Color32::WHITE,
            Color32::from_rgb(45, 51, 59),
            false,
        ) + 6.0;
    }
    if row.pr.is_fork() && !row.fork_unsupported {
        x = pill(
            &lp,
            x,
            cy,
            "fork",
            Color32::BLACK,
            Color32::from_rgb(190, 195, 205),
            false,
        ) + 6.0;
    }
    if row.fork_unsupported {
        x = pill(
            &lp,
            x,
            cy,
            "fork, not supported",
            Color32::BLACK,
            AMBER,
            false,
        ) + 6.0;
    }
    if row.pr.draft {
        x = pill(
            &lp,
            x,
            cy,
            "draft",
            Color32::WHITE,
            Color32::from_rgb(90, 90, 105),
            false,
        ) + 6.0;
    }
    x = pill(
        &lp,
        x,
        cy,
        &format!("-> {}", row.pr.dest_branch),
        Color32::WHITE,
        Color32::from_rgb(52, 90, 140),
        false,
    ) + 6.0;
    let title_color = if row.pr.draft || row.fork_unsupported {
        GREY
    } else {
        ui.visuals().text_color()
    };
    lp.text(
        egui::pos2(x, cy),
        Align2::LEFT_CENTER,
        &row.pr.title,
        FontId::proportional(14.0),
        title_color,
    );
    resp
}

/// The marker of a PR over its forge's web UI limits.
pub fn over_limit_label(forge: Forge) -> &'static str {
    match forge {
        Forge::BitbucketCloud => "exceeds Bitbucket limits",
        Forge::GitHub => "exceeds GitHub's web diff limits (300 files, 20,000 lines)",
        Forge::GitLab => "exceeds GitLab's default diff limits (1000 files, 50,000 lines)",
    }
}

/// The forge tag of an inbox row: none for Bitbucket Cloud (the app's first forge, and
/// every row before multi-forge), the product name otherwise.
pub fn forge_tag(pr: &PullRequest) -> Option<&'static str> {
    (pr.forge != Forge::BitbucketCloud).then(|| pr.forge.label())
}

/// Everything the detail pane shows, computed when the selection or the data changes.
pub struct Detail {
    pub pr: PullRequest,
    pub stat: Option<DiffStat>,
    /// (position, total) within the stack, 1-based.
    pub stack: Option<(usize, usize)>,
    pub advice: Vec<String>,
    /// Raw commit hashes for stack members (first-run checks against real data).
    pub debug: Option<String>,
}

pub fn show_detail(ui: &mut Ui, d: &Detail) {
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            let pr = &d.pr;
            ui.add_space(6.0);
            ui.add(egui::Label::new(egui::RichText::new(&pr.title).heading()).wrap());
            ui.label(
                egui::RichText::new(format!(
                    "{} in {}",
                    pr.forge.change_ref(pr.id),
                    pr.repo.full_name()
                ))
                .color(GREY),
            );
            ui.horizontal_wrapped(|ui| {
                let state = match pr.state {
                    PrState::Open => "open",
                    PrState::Merged => "merged",
                    PrState::Declined => "declined",
                    PrState::Superseded => "superseded",
                };
                ui.label(egui::RichText::new(state).strong());
                if pr.draft {
                    ui.label(egui::RichText::new("draft").color(GREY));
                }
                if pr.is_fork() && pr.forge != Forge::BitbucketCloud {
                    ui.label(egui::RichText::new("fork").color(GREY));
                } else if pr.is_fork() {
                    ui.label(egui::RichText::new("fork, not supported").color(AMBER));
                }
            });
            ui.add_space(6.0);

            if let Some((pos, total)) = d.stack {
                egui::Frame::group(ui.style())
                    .fill(Color32::from_rgb(52, 46, 24))
                    .stroke(egui::Stroke::new(1.0, AMBER))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            egui::RichText::new(format!("{pos} of {total} in stack"))
                                .strong()
                                .color(AMBER),
                        );
                        for line in &d.advice {
                            ui.add(egui::Label::new(format!("\u{2022} {line}")).wrap());
                        }
                        if let Some(debug) = &d.debug {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(debug).monospace().small().color(GREY),
                                )
                                .wrap(),
                            );
                        }
                    });
                ui.add_space(6.0);
            }

            egui::Grid::new("meta")
                .num_columns(2)
                .spacing([14.0, 6.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Author").color(GREY));
                    ui.label(&pr.author.display_name);
                    ui.end_row();
                    ui.label(egui::RichText::new("Branches").color(GREY));
                    ui.add(
                        egui::Label::new(format!("{} -> {}", pr.source_branch, pr.dest_branch))
                            .wrap(),
                    );
                    ui.end_row();
                    ui.label(egui::RichText::new("Reviewers").color(GREY));
                    ui.vertical(|ui| {
                        if pr.reviewers.is_empty() {
                            ui.label("none");
                        }
                        for r in &pr.reviewers {
                            let (word, color) = match r.state {
                                ReviewState::Approved => ("approved", GREEN),
                                ReviewState::ChangesRequested => ("changes requested", RED),
                                ReviewState::Commented => ("commented", GREY),
                                ReviewState::Pending => ("pending", GREY),
                            };
                            ui.horizontal(|ui| {
                                ui.label(&r.user.display_name);
                                ui.label(egui::RichText::new(word).color(color));
                            });
                        }
                    });
                    ui.end_row();
                    let who = |s: ReviewState| {
                        pr.reviewers
                            .iter()
                            .filter(|r| r.state == s)
                            .map(|r| r.user.display_name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    let changes = who(ReviewState::ChangesRequested);
                    let approved = who(ReviewState::Approved);
                    if !changes.is_empty() || !approved.is_empty() {
                        ui.label(egui::RichText::new("Approval").color(GREY));
                        ui.vertical(|ui| {
                            if !changes.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!(
                                            "changes requested by {changes}"
                                        ))
                                        .color(RED),
                                    )
                                    .wrap(),
                                );
                            }
                            if !approved.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("approved by {approved}"))
                                            .color(GREEN),
                                    )
                                    .wrap(),
                                );
                            }
                        });
                        ui.end_row();
                    }
                    ui.label(egui::RichText::new("Checks").color(GREY));
                    match pr.checks {
                        CheckState::Failing => ui.colored_label(RED, "failed"),
                        CheckState::Running => ui.colored_label(AMBER, "running"),
                        CheckState::Passing => ui.colored_label(GREEN, "passed"),
                        CheckState::Unknown => ui.label("none reported"),
                    };
                    ui.end_row();
                    if pr.has_conflicts {
                        ui.label(egui::RichText::new("Merge").color(GREY));
                        ui.colored_label(RED, "conflicts with the target branch");
                        ui.end_row();
                    }
                    ui.label(egui::RichText::new("Comments").color(GREY));
                    if pr.forge == Forge::BitbucketCloud {
                        ui.label(format!(
                            "{} comments, {} open tasks",
                            pr.comment_count, pr.task_count
                        ));
                    } else {
                        // tasks are Bitbucket's only
                        ui.label(format!("{} comments", pr.comment_count));
                    }
                    ui.end_row();
                    ui.label(egui::RichText::new("Updated").color(GREY));
                    ui.label(pr.updated_on.format("%Y-%m-%d %H:%M UTC").to_string());
                    ui.end_row();
                    ui.label(egui::RichText::new("Size").color(GREY));
                    match &d.stat {
                        Some(s) if !has_line_counts(pr.forge) => ui.label(format!(
                            "{} (line counts are known once it is opened)",
                            files_text(pr.forge, s.files)
                        )),
                        Some(s) => ui.label(format!(
                            "{} files, +{} / -{} lines, largest file {} lines",
                            s.files, s.lines_added, s.lines_removed, s.max_file_lines
                        )),
                        None => ui.label("not fetched yet"),
                    };
                    ui.end_row();
                });

            let over = d.stat.as_ref().filter(|s| s.exceeds_web_ui(d.pr.forge));
            if over.is_some() && d.pr.forge != Forge::BitbucketCloud {
                ui.add_space(8.0);
                ui.colored_label(RED, over_limit_label(d.pr.forge));
            }
            if let Some(s) = over.filter(|_| d.pr.forge == Forge::BitbucketCloud) {
                ui.add_space(8.0);
                ui.colored_label(
                    RED,
                    "Exceeds Bitbucket limits, its web diff will not render fully:",
                );
                if s.files > BB_MAX_FILES {
                    ui.colored_label(RED, format!("\u{2022} {} files (limit 200)", s.files));
                }
                if s.total_lines() > prcore::model::BB_MAX_TOTAL_LINES {
                    ui.colored_label(
                        RED,
                        format!("\u{2022} {} changed lines (limit 8000)", s.total_lines()),
                    );
                }
                if s.max_file_lines > prcore::model::BB_MAX_FILE_LINES {
                    ui.colored_label(
                        RED,
                        format!(
                            "\u{2022} one file with {} changed lines (limit 2000)",
                            s.max_file_lines
                        ),
                    );
                }
            }
        });
}

#[cfg(test)]
mod tests;
