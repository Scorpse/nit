use super::*;
use chrono::{TimeZone, Utc};
use prcore::model::Reviewer;

fn repo(n: &str) -> RepoRef {
    RepoRef::parse_bitbucket(n).unwrap()
}

fn user(uuid: &str, name: &str) -> UserRef {
    UserRef {
        id: uuid.into(),
        display_name: name.into(),
        login: None,
    }
}

fn me() -> UserRef {
    user("{me}", "Me Myself")
}

fn other() -> UserRef {
    user("{o}", "Olivia Other")
}

/// Open PR in `repo_name`, branch `b{id}` -> `main`, updated at `1_700_000_000 + id`.
fn pr(repo_name: &str, id: u64) -> PullRequest {
    let t = Utc.timestamp_opt(1_700_000_000 + id as i64, 0).unwrap();
    PullRequest {
        repo: repo(repo_name),
        id,
        title: format!("pr {id}"),
        state: PrState::Open,
        draft: false,
        author: other(),
        source_branch: format!("b{id}"),
        dest_branch: "main".into(),
        source_commit: format!("s{id}"),
        dest_commit: "head".into(),
        source_repo: repo(repo_name),
        comment_count: 0,
        task_count: 0,
        reviewers: vec![],
        created_on: t,
        updated_on: t,
        forge: Default::default(),
        account: Default::default(),
        web_url: String::new(),
        node_id: None,
        review_requested_from_me: false,
        locked: false,
        checks: Default::default(),
        has_conflicts: false,
    }
}

/// A GitHub PR of account `acc-gh`, repository `octo/<name>` on github.com.
fn gh(name: &str, id: u64) -> PullRequest {
    let mut p = pr("x/y", id);
    p.repo = RepoRef::parse("github.com", &format!("octo/{name}")).unwrap();
    p.source_repo = p.repo.clone();
    p.forge = Forge::GitHub;
    p.account = AccountId("acc-gh".into());
    p
}

/// GitLab's "Needs me" is the provider's call too (reviewer state and approvals), and
/// GitLab fork MRs are listed.
#[test]
fn needs_me_uses_provider_flag_for_gitlab() {
    let gl = |id: u64| {
        let mut p = gh("api", id);
        p.repo = RepoRef::parse("git.corp.example/gitlab", "grp/sub/api").unwrap();
        p.source_repo = p.repo.clone();
        p.forge = Forge::GitLab;
        p
    };
    let mut requested = gl(1);
    requested.review_requested_from_me = true;
    // listed as a pending reviewer, but the provider said no (e.g. I approved already)
    let mut approved = gl(2);
    approved.reviewers = vec![Reviewer {
        user: me(),
        state: ReviewState::Pending,
    }];
    let mut fork = gl(3);
    fork.source_repo = RepoRef::parse("git.corp.example/gitlab", "unknown/project-9").unwrap();
    fork.review_requested_from_me = true;
    let prs = [requested, approved, fork];
    let inbox = build_inbox(&prs, &no_stats(), &me(), View::NeedsMe, "");
    let mut got = ids(&inbox);
    got.sort_unstable();
    assert_eq!(got, vec![1, 3]);
    let all = build_inbox(&prs, &no_stats(), &me(), View::All, "");
    assert!(all.rows.iter().all(|r| !r.fork_unsupported));
}

#[test]
fn needs_me_uses_provider_flag_for_github() {
    let mut requested = gh("api", 1);
    requested.review_requested_from_me = true;
    // a pending reviewer entry alone does not count on GitHub (teams, re-reviews and
    // reviewers who already reviewed are the provider's call)
    let mut listed = gh("api", 2);
    listed.reviewers = vec![Reviewer {
        user: me(),
        state: ReviewState::Pending,
    }];
    let mut draft = gh("api", 3);
    draft.review_requested_from_me = true;
    draft.draft = true;
    let inbox = build_inbox(
        &[requested, listed, draft],
        &no_stats(),
        &me(),
        View::NeedsMe,
        "",
    );
    assert_eq!(ids(&inbox), vec![1]);
}

#[test]
fn scope_chips_per_forge_and_account() {
    let gh_work = gh("api", 1);
    let mut gh_home = gh("web", 2);
    gh_home.account = AccountId("acc-gh2".into());
    let bb = pr("acme/api", 3);
    let labels = vec![
        (AccountId::default_bitbucket(), "Ana".to_string()),
        (AccountId("acc-gh".into()), "mona".to_string()),
        (AccountId("acc-gh2".into()), "octocat".to_string()),
    ];
    // one account: no chips at all
    assert!(scope_chips(std::slice::from_ref(&bb), &labels).is_empty());
    let prs = [gh_work.clone(), gh_home.clone(), bb.clone()];
    let chips = scope_chips(&prs, &labels);
    let names: Vec<&str> = chips.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["All", "Bitbucket Cloud", "GitHub", "Ana", "mona", "octocat"]
    );
    let pick = |name: &str| chips.iter().find(|(_, n)| n == name).unwrap().0.clone();
    let only =
        |s: &Scope| -> Vec<u64> { prs.iter().filter(|p| s.matches(p)).map(|p| p.id).collect() };
    assert_eq!(only(&pick("All")), vec![1, 2, 3]);
    assert_eq!(only(&pick("GitHub")), vec![1, 2]);
    assert_eq!(only(&pick("octocat")), vec![2]);
    assert_eq!(only(&pick("Ana")), vec![3]);
    // one account per forge: the forge chips say it all
    let chips = scope_chips(&[gh_work, bb], &labels);
    let names: Vec<&str> = chips.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(names, vec!["All", "Bitbucket Cloud", "GitHub"]);
}

#[test]
fn mine_and_needs_me_use_each_accounts_user() {
    let mut mine_on_gh = gh("api", 1);
    mine_on_gh.author = user("42", "Mona");
    let mut bb_requested = pr("acme/api", 2);
    bb_requested.reviewers = vec![Reviewer {
        user: me(),
        state: ReviewState::Pending,
    }];
    let users: HashMap<AccountId, String> = [(AccountId("acc-gh".into()), "42".to_string())].into();
    let prs = [mine_on_gh, bb_requested];
    let mine = build_inbox_for(&prs, &no_stats(), &me(), &users, View::Mine, "");
    assert_eq!(ids(&mine), vec![1]);
    let needs = build_inbox_for(&prs, &no_stats(), &me(), &users, View::NeedsMe, "");
    assert_eq!(ids(&needs), vec![2]);
    // without the account's user, "Mine" falls back to the default user
    let mine = build_inbox(&prs, &no_stats(), &me(), View::Mine, "");
    assert!(ids(&mine).is_empty());
}

#[test]
fn github_fork_is_listed_but_never_stacked() {
    let parent = gh("api", 1);
    let mut fork = child_of(gh("api", 2), &parent);
    fork.source_repo = RepoRef::parse("github.com", "forker/api").unwrap();
    fork.review_requested_from_me = true;
    let prs = [parent, fork];
    let all = build_inbox(&prs, &no_stats(), &me(), View::All, "");
    let row = all.rows.iter().find(|r| r.pr.id == 2).unwrap();
    assert!(!row.fork_unsupported && row.stack.is_none());
    let needs = build_inbox(&prs, &no_stats(), &me(), View::NeedsMe, "");
    assert_eq!(ids(&needs), vec![2]);
}

fn at(mut p: PullRequest, secs: i64) -> PullRequest {
    p.updated_on = Utc.timestamp_opt(secs, 0).unwrap();
    p
}

fn child_of(mut p: PullRequest, parent: &PullRequest) -> PullRequest {
    p.dest_branch = parent.source_branch.clone();
    p.dest_commit = parent.source_commit.clone();
    p
}

fn big() -> DiffStat {
    DiffStat {
        files: 300,
        lines_added: 10,
        lines_removed: 10,
        max_file_lines: 10,
    }
}

fn small() -> DiffStat {
    DiffStat {
        files: 2,
        lines_added: 10,
        lines_removed: 10,
        max_file_lines: 10,
    }
}

fn no_stats() -> HashMap<(RepoRef, u64), DiffStat> {
    HashMap::new()
}

fn ids(inbox: &Inbox) -> Vec<u64> {
    inbox.rows.iter().map(|r| r.pr.id).collect()
}

#[test]
fn mine_shows_only_my_open_prs() {
    let mut a = pr("acme/api", 1);
    a.author = me();
    let b = pr("acme/api", 2);
    let mut c = pr("acme/api", 3);
    c.author = me();
    c.state = PrState::Merged;
    let mut d = pr("acme/api", 4);
    d.author = me();
    d.state = PrState::Declined;
    let inbox = build_inbox(&[a, b, c, d], &no_stats(), &me(), View::Mine, "");
    assert_eq!(ids(&inbox), vec![1]);
}

#[test]
fn needs_me_excludes_drafts_and_already_reviewed() {
    let rev = |s| {
        vec![Reviewer {
            user: me(),
            state: s,
        }]
    };
    let mut pending = pr("acme/api", 1);
    pending.reviewers = rev(ReviewState::Pending);
    let mut draft = pr("acme/api", 2);
    draft.reviewers = rev(ReviewState::Pending);
    draft.draft = true;
    let mut approved = pr("acme/api", 3);
    approved.reviewers = rev(ReviewState::Approved);
    let mut changes = pr("acme/api", 4);
    changes.reviewers = rev(ReviewState::ChangesRequested);
    let mut someone_else = pr("acme/api", 5);
    someone_else.reviewers = vec![Reviewer {
        user: other(),
        state: ReviewState::Pending,
    }];
    let no_reviewers = pr("acme/api", 6);
    let mut merged = pr("acme/api", 7);
    merged.reviewers = rev(ReviewState::Pending);
    merged.state = PrState::Merged;
    let prs = [
        pending,
        draft,
        approved,
        changes,
        someone_else,
        no_reviewers,
        merged,
    ];
    let inbox = build_inbox(&prs, &no_stats(), &me(), View::NeedsMe, "");
    assert_eq!(ids(&inbox), vec![1]);
}

#[test]
fn too_big_uses_diffstat_limits() {
    let prs = [
        pr("acme/api", 1),
        pr("acme/api", 2),
        pr("acme/api", 3),
        pr("acme/api", 4),
    ];
    let r = repo("acme/api");
    let mut stats = HashMap::new();
    stats.insert((r.clone(), 1), big());
    stats.insert((r.clone(), 2), small());
    stats.insert(
        (r.clone(), 3),
        DiffStat {
            files: 5,
            lines_added: 5000,
            lines_removed: 3001,
            max_file_lines: 10,
        },
    );
    stats.insert(
        (r.clone(), 4),
        DiffStat {
            files: 5,
            lines_added: 5000,
            lines_removed: 3000,
            max_file_lines: 2000,
        },
    );
    let inbox = build_inbox(&prs, &stats, &me(), View::TooBig, "");
    // Same repo, no stacks: updated_on descending, so 3 (newer) before 1.
    assert_eq!(ids(&inbox), vec![3, 1]);
    assert_eq!(inbox.rows[1].stat, Some(big()));
}

#[test]
fn too_big_hides_prs_without_stats() {
    let prs = [pr("acme/api", 1), pr("acme/api", 2)];
    let mut stats = HashMap::new();
    stats.insert((repo("acme/api"), 2), big());
    let inbox = build_inbox(&prs, &stats, &me(), View::TooBig, "");
    assert_eq!(ids(&inbox), vec![2]);
    let all = build_inbox(&prs, &stats, &me(), View::All, "");
    assert_eq!(all.rows.iter().find(|r| r.pr.id == 1).unwrap().stat, None);
}

#[test]
fn stacked_view_groups_members_base_first() {
    let p1 = at(pr("acme/api", 1), 100);
    let p2 = child_of(at(pr("acme/api", 2), 300), &p1);
    let p3 = child_of(at(pr("acme/api", 3), 200), &p2);
    let loner = at(pr("acme/api", 9), 900);
    let inbox = build_inbox(&[loner, p3, p2, p1], &no_stats(), &me(), View::Stacked, "");
    assert_eq!(ids(&inbox), vec![1, 2, 3]);
    assert_eq!(inbox.stacks.len(), 1);
    assert_eq!(inbox.stacks[0].members, vec![1, 2, 3]);
    assert!(inbox.rows.iter().all(|r| r.stack == Some(0)));
}

#[test]
fn merged_parent_not_listed_but_anchors_stack() {
    let mut parent = pr("acme/api", 1);
    parent.state = PrState::Merged;
    let child = child_of(pr("acme/api", 2), &parent);
    let inbox = build_inbox(
        &[parent, child.clone()],
        &no_stats(),
        &me(),
        View::Stacked,
        "",
    );
    assert_eq!(ids(&inbox), vec![2]);
    assert_eq!(inbox.stacks.len(), 1);
    assert_eq!(inbox.stacks[0].members, vec![1, 2]);
    assert_eq!(inbox.rows[0].stack, Some(0));
    let alone = build_inbox(&[child], &no_stats(), &me(), View::All, "");
    assert_eq!(
        alone.rows[0].stack, None,
        "without the merged parent there is no stack"
    );
}

#[test]
fn fork_pr_only_in_all_and_flagged() {
    let mut fork = pr("acme/api", 1);
    fork.source_repo = repo("someone/api");
    fork.author = me();
    fork.reviewers = vec![Reviewer {
        user: me(),
        state: ReviewState::Pending,
    }];
    let normal = pr("acme/api", 2);
    let child_of_fork = child_of(pr("acme/api", 3), &fork);
    let prs = [fork, normal, child_of_fork];
    let mut stats = HashMap::new();
    stats.insert((repo("acme/api"), 1), big());

    for view in [View::Mine, View::NeedsMe, View::TooBig, View::Stacked] {
        let inbox = build_inbox(&prs, &stats, &me(), view, "");
        assert!(inbox.rows.iter().all(|r| r.pr.id != 1), "{view:?}");
        assert!(inbox.rows.iter().all(|r| !r.fork_unsupported), "{view:?}");
    }
    let all = build_inbox(&prs, &stats, &me(), View::All, "");
    // No stacks: updated_on descending (3, 2, 1).
    assert_eq!(ids(&all), vec![3, 2, 1]);
    let flags: Vec<bool> = all.rows.iter().map(|r| r.fork_unsupported).collect();
    assert_eq!(flags, vec![false, false, true]);
    assert_eq!(all.rows[2].stack, None);
    assert!(all.stacks.is_empty());
}

#[test]
fn query_matches_title_branch_author_case_insensitively() {
    let mut title = pr("acme/api", 1);
    title.title = "Fix Login Flow".into();
    let mut branch = pr("acme/api", 2);
    branch.source_branch = "feature/PAYMENTS".into();
    let mut author = pr("acme/api", 3);
    author.author = user("{x}", "Zoltan Quux");
    let in_repo = pr("zeta/Widgets", 4);
    let none = pr("acme/api", 5);
    let prs = [title, branch, author, in_repo, none];
    let q = |s: &str| ids(&build_inbox(&prs, &no_stats(), &me(), View::All, s));
    assert_eq!(q("login FLOW"), vec![1]);
    assert_eq!(q("payments"), vec![2]);
    assert_eq!(q("zoltan"), vec![3]);
    assert_eq!(q("ZETA/widgets"), vec![4]);
    assert_eq!(q("nomatch"), Vec::<u64>::new());
}

#[test]
fn query_is_trimmed_and_empty_matches_all() {
    let mut t = pr("acme/api", 1);
    t.title = "Fix login".into();
    let prs = [t, pr("acme/api", 2)];
    let q = |s: &str| ids(&build_inbox(&prs, &no_stats(), &me(), View::All, s));
    assert_eq!(q(""), vec![2, 1]);
    assert_eq!(q("   \t "), vec![2, 1]);
    assert_eq!(q("  fix login  "), vec![1]);
}

#[test]
fn sort_is_repo_then_stack_then_recent() {
    // acme/api: stack 10 -> 11 (old), loners 20 (newest), 21, 22 (tie with 21).
    let s10 = at(pr("acme/api", 10), 50);
    let s11 = child_of(at(pr("acme/api", 11), 40), &s10);
    let l20 = at(pr("acme/api", 20), 900);
    let l21 = at(pr("acme/api", 21), 500);
    let l22 = at(pr("acme/api", 22), 500);
    // acme/web: loner 30, plus stack 5 -> 6.
    let w30 = at(pr("acme/web", 30), 999);
    let w5 = at(pr("acme/web", 5), 1);
    let w6 = child_of(at(pr("acme/web", 6), 2), &w5);
    // aaa/zzz sorts before acme/*.
    let z1 = at(pr("aaa/zzz", 1), 1);
    let prs = [l22, w6, l21, w30, s11, l20, z1, w5, s10];
    let inbox = build_inbox(&prs, &no_stats(), &me(), View::All, "");
    assert_eq!(ids(&inbox), vec![1, 10, 11, 20, 21, 22, 5, 6, 30]);
    let repos: Vec<String> = inbox.rows.iter().map(|r| r.pr.repo.full_name()).collect();
    assert_eq!(repos[0], "aaa/zzz");
    assert_eq!(repos[1], "acme/api");
    assert_eq!(repos[6], "acme/web");
}

#[test]
fn same_branch_names_in_different_repos_do_not_cross_link() {
    let a1 = pr("acme/api", 1);
    let mut w2 = pr("acme/web", 2);
    w2.dest_branch = a1.source_branch.clone();
    let inbox = build_inbox(&[a1, w2], &no_stats(), &me(), View::Stacked, "");
    assert!(inbox.stacks.is_empty());
    assert!(inbox.rows.is_empty());
}

#[test]
fn stack_advice_rekeys_stats_per_repo() {
    // Same ids in two repos; only acme/api's stack is individually reviewable
    // but too large combined, so ReviewablePerPr must fire for it.
    let p1 = pr("acme/api", 1);
    let p2 = child_of(pr("acme/api", 2), &p1);
    let q1 = pr("acme/web", 1);
    let q2 = child_of(pr("acme/web", 2), &q1);
    let prs = [p1, p2, q1, q2];
    let chunk = DiffStat {
        files: 10,
        lines_added: 3000,
        lines_removed: 1500,
        max_file_lines: 100,
    };
    let mut stats = HashMap::new();
    stats.insert((repo("acme/api"), 1), chunk.clone());
    stats.insert((repo("acme/api"), 2), chunk);
    stats.insert((repo("acme/web"), 1), small());
    stats.insert((repo("acme/web"), 2), small());

    let inbox = build_inbox(&prs, &stats, &me(), View::Stacked, "");
    assert_eq!(inbox.stacks.len(), 2);
    assert_eq!(inbox.stacks[0].repo, repo("acme/api"));
    let api = stack_advice(&inbox, &prs, &stats, 0);
    assert_eq!(api, vec![Advice::ReviewBottomUp, Advice::ReviewablePerPr]);
    let web = stack_advice(&inbox, &prs, &stats, 1);
    assert_eq!(web, vec![Advice::ReviewBottomUp]);
}

#[test]
fn empty_input_gives_empty_inbox() {
    for view in [
        View::Mine,
        View::NeedsMe,
        View::All,
        View::TooBig,
        View::Stacked,
    ] {
        let inbox = build_inbox(&[], &no_stats(), &me(), view, "");
        assert!(inbox.rows.is_empty());
        assert!(inbox.stacks.is_empty());
    }
}
