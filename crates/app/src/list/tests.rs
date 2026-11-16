use super::*;
use crate::demo::demo_data;
use crate::inbox::{build_inbox, View};

#[test]
fn stack_children_indent_by_open_ancestors_and_headers_precede_rows() {
    let (me, prs, stats) = demo_data();
    let inbox = build_inbox(&prs, &stats, &me, View::Stacked, "");
    let items = build_items(&inbox);
    let depth_of = |id: u64| {
        items.iter().find_map(|i| match i {
            Item::Row { idx, depth } if inbox.rows[*idx].pr.id == id => Some(*depth),
            _ => None,
        })
    };
    assert_eq!(depth_of(101), Some(0));
    assert_eq!(depth_of(102), Some(1));
    assert_eq!(depth_of(103), Some(2));
    // merged parent #301 is not a row, so its child #302 is not indented
    assert_eq!(depth_of(302), Some(0));
    assert!(matches!(items[0], Item::Header { .. }));
}

#[test]
fn nav_step_skips_headers_and_clamps_at_both_ends() {
    // rows 0 and 3 are headers
    let sel = |k: usize| k != 0 && k != 3;
    let n = 7;
    assert_eq!(nav_step(n, sel, None, Nav::Down, 5), Some(1), "first row");
    assert_eq!(nav_step(n, sel, None, Nav::Up, 5), Some(1));
    assert_eq!(nav_step(n, sel, None, Nav::End, 5), Some(6));
    assert_eq!(nav_step(n, sel, Some(1), Nav::Down, 5), Some(2));
    assert_eq!(
        nav_step(n, sel, Some(2), Nav::Down, 5),
        Some(4),
        "header skipped"
    );
    assert_eq!(
        nav_step(n, sel, Some(4), Nav::Up, 5),
        Some(2),
        "header skipped"
    );
    assert_eq!(
        nav_step(n, sel, Some(6), Nav::Down, 5),
        Some(6),
        "clamped at the end"
    );
    assert_eq!(
        nav_step(n, sel, Some(1), Nav::Up, 5),
        Some(1),
        "clamped at the start"
    );
    assert_eq!(nav_step(n, sel, Some(1), Nav::Home, 5), Some(1));
    assert_eq!(nav_step(n, sel, Some(1), Nav::End, 5), Some(6));
}

#[test]
fn nav_step_pages_and_empty_lists() {
    let all = |_| true;
    assert_eq!(nav_step(100, all, Some(10), Nav::PageDown, 20), Some(30));
    assert_eq!(nav_step(100, all, Some(90), Nav::PageDown, 20), Some(99));
    assert_eq!(nav_step(100, all, Some(30), Nav::PageUp, 20), Some(10));
    assert_eq!(nav_step(100, all, Some(5), Nav::PageUp, 20), Some(0));
    assert_eq!(
        nav_step(100, all, Some(5), Nav::PageDown, 0),
        Some(6),
        "page >= 1"
    );
    // a page landing on a header moves on to the next row in that direction
    let sel = |k: usize| !k.is_multiple_of(10);
    assert_eq!(nav_step(100, sel, Some(1), Nav::PageDown, 9), Some(11));
    assert_eq!(nav_step(100, sel, Some(19), Nav::PageUp, 9), Some(9));
    assert_eq!(nav_step(0, all, None, Nav::Down, 5), None);
    assert_eq!(
        nav_step(3, |_| false, Some(1), Nav::Down, 5),
        None,
        "only headers"
    );
    assert_eq!(
        nav_step(3, all, Some(9), Nav::Down, 5),
        Some(0),
        "stale position"
    );
}

#[test]
fn inbox_navigation_walks_rows_in_display_order() {
    let (me, prs, stats) = demo_data();
    let inbox = build_inbox(&prs, &stats, &me, View::Stacked, "");
    let items = build_items(&inbox);
    let order: Vec<(RepoRef, u64)> = items
        .iter()
        .filter_map(|i| match i {
            Item::Row { idx, .. } => {
                Some((inbox.rows[*idx].pr.repo.clone(), inbox.rows[*idx].pr.id))
            }
            Item::Header { .. } => None,
        })
        .collect();
    assert!(order.len() >= 3);
    let next = |s: Option<&(RepoRef, u64)>, k| nav_items(&inbox, &items, s, k, 10);
    assert_eq!(next(None, Nav::Down).as_ref(), Some(&order[0]));
    let mut cur = order[0].clone();
    for want in &order[1..] {
        cur = next(Some(&cur), Nav::Down).unwrap();
        assert_eq!(&cur, want, "the header between repos is skipped");
    }
    assert_eq!(
        next(Some(&cur), Nav::Down).as_ref(),
        Some(order.last().unwrap())
    );
    assert_eq!(next(Some(&cur), Nav::Home).as_ref(), Some(&order[0]));
    assert_eq!(next(Some(&order[1]), Nav::Up).as_ref(), Some(&order[0]));
    let pos = item_pos(&inbox, &items, &order[0]).unwrap();
    assert!(matches!(items[pos], Item::Row { .. }));
    let empty = build_inbox(&[], &stats, &me, View::All, "");
    assert_eq!(
        nav_items(&empty, &build_items(&empty), None, Nav::Down, 10),
        None
    );
}

#[test]
fn forge_tag_only_for_other_forges() {
    let (_, prs, _) = crate::demo::demo_data();
    let bb = prs
        .iter()
        .find(|p| p.forge == Forge::BitbucketCloud)
        .unwrap();
    let gh = prs.iter().find(|p| p.forge == Forge::GitHub).unwrap();
    assert_eq!(forge_tag(bb), None);
    assert_eq!(forge_tag(gh), Some("GitHub"));
    assert_eq!(
        over_limit_label(Forge::BitbucketCloud),
        "exceeds Bitbucket limits"
    );
    assert!(over_limit_label(Forge::GitHub).contains("300 files"));
}

/// GitLab sizes are file counts only (D6); a capped "1000+" count says so.
#[test]
fn gitlab_size_is_files_only_and_capped_shows_1000_plus() {
    assert!(!has_line_counts(Forge::GitLab));
    assert!(has_line_counts(Forge::BitbucketCloud) && has_line_counts(Forge::GitHub));
    assert_eq!(files_text(Forge::GitLab, 12), "12 files");
    assert_eq!(files_text(Forge::GitLab, 1), "1 file");
    assert_eq!(
        files_text(Forge::GitLab, providers::gitlab::CAPPED_FILES),
        "1000+ files"
    );
    assert_eq!(files_text(Forge::GitHub, 3400), "3400 files");
    let job = size_job(
        &DiffStat {
            files: 7,
            lines_added: 0,
            lines_removed: 0,
            max_file_lines: 0,
        },
        Forge::GitLab,
    );
    assert_eq!(job.text, "7 files");
}

#[test]
fn reveal_offset_only_when_outside_the_viewport() {
    assert_eq!(reveal_offset(5, 10.0, (0.0, 100.0)), None);
    assert_eq!(reveal_offset(9, 10.0, (0.0, 100.0)), None);
    assert_eq!(reveal_offset(10, 10.0, (0.0, 100.0)), Some(10.0));
    assert_eq!(reveal_offset(2, 10.0, (50.0, 100.0)), Some(20.0));
}

#[test]
fn double_click_opens_only_when_the_review_button_is_enabled() {
    use crate::settings_ui::ReviewButton::*;
    assert_eq!(row_click(false, Some(Enabled)), RowClick::Select);
    assert_eq!(row_click(true, Some(Enabled)), RowClick::Open);
    assert_eq!(
        row_click(true, Some(Disabled("Sign in"))),
        RowClick::ShowReason("Sign in")
    );
    assert_eq!(
        row_click(true, Some(Hidden)),
        RowClick::ShowReason(FORK_REASON)
    );
    assert_eq!(row_click(false, Some(Hidden)), RowClick::Select);
    assert_eq!(row_click(true, None), RowClick::Select);
}

#[test]
fn status_pills_put_what_needs_action_first_and_say_nothing_when_unknown() {
    use prcore::model::{Reviewer, UserRef};
    let (_, mut prs, _) = demo_data();
    let mut pr = prs.remove(0);
    pr.reviewers.clear();
    pr.task_count = 0;
    pr.has_conflicts = false;
    pr.checks = CheckState::Unknown;
    let words = |pr: &PullRequest| -> Vec<String> {
        status_pills(pr).into_iter().map(|(t, _)| t).collect()
    };
    assert_eq!(words(&pr), ["not approved"]);
    pr.draft = true;
    assert!(words(&pr).is_empty(), "a draft is not up for approval");
    pr.draft = false;

    let who = |n: &str, state| Reviewer {
        user: UserRef {
            id: n.into(),
            display_name: n.into(),
            login: None,
        },
        state,
    };
    pr.reviewers = vec![
        who("a", ReviewState::Approved),
        who("b", ReviewState::Approved),
        who("c", ReviewState::ChangesRequested),
        who("d", ReviewState::Pending),
    ];
    pr.checks = CheckState::Failing;
    pr.has_conflicts = true;
    pr.task_count = 2;
    assert_eq!(
        words(&pr),
        [
            "changes requested",
            "conflicts",
            "checks failed",
            "2 open tasks"
        ]
    );
    pr.reviewers
        .retain(|r| r.state != ReviewState::ChangesRequested);
    assert_eq!(words(&pr).last().unwrap(), "approved");
}
