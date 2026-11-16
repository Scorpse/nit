use super::*;
use chrono::{TimeZone, Utc};
use prcore::model::{PrState, ReviewState, Reviewer, UserRef};

impl Store {
    fn safe(&self) -> MutexGuard<'_, Connection> {
        lock(&self.safe)
    }

    /// Rows changed so far through the connection of the rebuildable tables.
    fn fast_changes(&self) -> u64 {
        lock(&self.fast).total_changes()
    }
}

fn repo(ws: &str, slug: &str) -> RepoRef {
    RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: ws.into(),
        slug: slug.into(),
    }
}

fn user(n: &str) -> UserRef {
    UserRef {
        id: format!("{{{n}}}"),
        display_name: n.into(),
        login: None,
    }
}

fn pr(r: &RepoRef, id: u64, state: PrState) -> PullRequest {
    PullRequest {
        repo: r.clone(),
        id,
        title: format!("PR {id}"),
        state,
        draft: true,
        author: user("alice"),
        source_branch: "feat".into(),
        dest_branch: "main".into(),
        source_commit: "abc123".into(),
        dest_commit: "def456".into(),
        source_repo: repo("fork", "api"),
        comment_count: 3,
        task_count: 2,
        reviewers: vec![
            Reviewer {
                user: user("bob"),
                state: ReviewState::Approved,
            },
            Reviewer {
                user: user("carol"),
                state: ReviewState::ChangesRequested,
            },
        ],
        created_on: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap(),
        updated_on: Utc.with_ymd_and_hms(2026, 2, 3, 4, 5, 6).unwrap(),
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

#[test]
fn repos_round_trip() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_repos(&[repo("b", "z"), repo("a", "y")]).unwrap();
    s.upsert_repos(&[repo("a", "y")]).unwrap();
    assert_eq!(
        s.list_repos().unwrap(),
        vec![repo("a", "y"), repo("b", "z")]
    );
}

#[test]
fn replace_prs_removes_prs_no_longer_returned() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let other = repo("acme", "web");
    s.replace_prs(&r, &[pr(&r, 1, PrState::Open), pr(&r, 2, PrState::Merged)])
        .unwrap();
    s.replace_prs(&other, &[pr(&other, 9, PrState::Open)])
        .unwrap();
    s.replace_prs(&r, &[pr(&r, 2, PrState::Merged), pr(&r, 3, PrState::Open)])
        .unwrap();
    let ids: Vec<(String, u64)> = s
        .list_prs()
        .unwrap()
        .iter()
        .map(|p| (p.repo.full_name(), p.id))
        .collect();
    assert_eq!(
        ids,
        vec![
            ("acme/api".to_string(), 2),
            ("acme/api".to_string(), 3),
            ("acme/web".to_string(), 9)
        ]
    );
}

#[test]
fn set_pr_reviewers_updates_only_that_pr() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    s.replace_prs(&r, &[pr(&r, 1, PrState::Open), pr(&r, 2, PrState::Open)])
        .unwrap();
    let mine = vec![Reviewer {
        user: user("me"),
        state: ReviewState::Approved,
    }];
    assert!(s.set_pr_reviewers(&r, 2, &mine).unwrap());
    assert!(!s.set_pr_reviewers(&r, 9, &mine).unwrap(), "not cached");
    let prs = s.list_prs().unwrap();
    assert_eq!(
        prs[0],
        pr(&r, 1, PrState::Open),
        "the other PR is untouched"
    );
    let want = PullRequest {
        reviewers: mine,
        ..pr(&r, 2, PrState::Open)
    };
    assert_eq!(prs[1], want);
}

#[test]
fn replace_prs_is_atomic_on_failure() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let old = vec![pr(&r, 1, PrState::Open), pr(&r, 2, PrState::Open)];
    s.replace_prs(&r, &old).unwrap();
    // Failure is induced by passing two PRs with the same id: the second insert violates the
    // (repo, id) primary key after the old rows were deleted and the first row inserted, so
    // the transaction must roll back and leave the old rows untouched.
    let bad = vec![pr(&r, 5, PrState::Open), pr(&r, 5, PrState::Merged)];
    assert!(s.replace_prs(&r, &bad).is_err());
    assert_eq!(s.list_prs().unwrap(), old);
}

#[test]
fn prs_round_trip_all_fields() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let prs = vec![pr(&r, 1, PrState::Open), pr(&r, 2, PrState::Superseded)];
    s.replace_prs(&r, &prs).unwrap();
    assert_eq!(s.list_prs().unwrap(), prs);
}

#[test]
fn diffstat_round_trip_with_commit() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let st = DiffStat {
        files: 3,
        lines_added: 10,
        lines_removed: 4,
        max_file_lines: 7,
    };
    s.put_diffstat(&r, 1, "c1", &st).unwrap();
    assert_eq!(
        s.get_diffstat(&r, 1).unwrap(),
        Some(("c1".to_string(), st.clone()))
    );
    let st2 = DiffStat { files: 1, ..st };
    s.put_diffstat(&r, 1, "c2", &st2).unwrap();
    assert_eq!(
        s.get_diffstat(&r, 1).unwrap(),
        Some(("c2".to_string(), st2))
    );
}

#[test]
fn diffstat_missing_is_none() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.get_diffstat(&repo("a", "b"), 1).unwrap(), None);
}

#[test]
fn repo_error_set_and_cleared() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    s.set_repo_error(&r, Some("boom")).unwrap();
    assert_eq!(
        s.repo_errors().unwrap(),
        vec![(r.clone(), "boom".to_string())]
    );
    s.set_repo_error(&r, Some("bang")).unwrap();
    assert_eq!(
        s.repo_errors().unwrap(),
        vec![(r.clone(), "bang".to_string())]
    );
    s.set_repo_error(&r, None).unwrap();
    assert!(s.repo_errors().unwrap().is_empty());
}

#[test]
fn meta_round_trip() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.get_meta("k").unwrap(), None);
    s.set_meta("k", "v1").unwrap();
    s.set_meta("k", "v2").unwrap();
    assert_eq!(s.get_meta("k").unwrap(), Some("v2".to_string()));
    s.delete_meta("k").unwrap();
    assert_eq!(s.get_meta("k").unwrap(), None);
    s.delete_meta("k").unwrap();
}

#[test]
fn reopening_file_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let r = repo("acme", "api");
    {
        let s = Store::open(&path).unwrap();
        s.upsert_repos(std::slice::from_ref(&r)).unwrap();
        s.replace_prs(&r, &[pr(&r, 1, PrState::Open)]).unwrap();
        s.set_meta("k", "v").unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.list_repos().unwrap(), vec![r.clone()]);
    assert_eq!(s.list_prs().unwrap(), vec![pr(&r, 1, PrState::Open)]);
    assert_eq!(s.get_meta("k").unwrap(), Some("v".to_string()));
}

#[test]
fn newer_schema_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    drop(Store::open(&path).unwrap());
    let c = rusqlite::Connection::open(&path).unwrap();
    c.pragma_update(None, "user_version", 999).unwrap();
    drop(c);
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::NewerSchema { found: 999, .. })
    ));
}

fn draft(r: &RepoRef, pr_id: u64, text: &str) -> Draft {
    Draft {
        id: 0,
        repo: r.clone(),
        pr_id,
        kind: DraftKind::Inline,
        path: Some("src/a.rs".into()),
        line: Some(12),
        side: Some(Side::Old),
        start: None,
        reply_to: None,
        text: text.into(),
        source_commit: "abc123".into(),
        comment_type: None,
    }
}

#[test]
fn viewed_roundtrip_and_toggle_off() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    assert!(s.viewed_map(&r, 1).unwrap().is_empty());
    s.set_viewed(&r, 1, "a.rs", "oid1", true).unwrap();
    s.set_viewed(&r, 1, "b.rs", "oid2", true).unwrap();
    s.set_viewed(&r, 1, "a.rs", "oid3", true).unwrap();
    let m = s.viewed_map(&r, 1).unwrap();
    assert_eq!(m.len(), 2);
    assert_eq!(m["a.rs"], "oid3");
    assert_eq!(m["b.rs"], "oid2");
    s.set_viewed(&r, 1, "a.rs", "oid3", false).unwrap();
    s.set_viewed(&r, 1, "a.rs", "oid3", false).unwrap();
    let m = s.viewed_map(&r, 1).unwrap();
    assert_eq!(m.len(), 1);
    assert!(!m.contains_key("a.rs"));
}

#[test]
fn viewed_many_sets_and_clears_in_one_go() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    s.set_viewed(&r, 1, "c.rs", "oc", true).unwrap();
    s.set_viewed_many(
        &r,
        1,
        &[
            ("a.rs", "oa", true),
            ("b.rs", "ob", true),
            ("c.rs", "oc", false),
        ],
    )
    .unwrap();
    let m = s.viewed_map(&r, 1).unwrap();
    assert_eq!(m.len(), 2);
    assert_eq!(m["a.rs"], "oa");
    assert_eq!(m["b.rs"], "ob");
    s.set_viewed_many(&r, 1, &[]).unwrap();
    assert_eq!(s.viewed_map(&r, 1).unwrap().len(), 2);
}

#[test]
fn viewed_scoped_per_pr() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let other = repo("acme", "web");
    s.set_viewed(&r, 1, "a.rs", "o", true).unwrap();
    assert!(s.viewed_map(&r, 2).unwrap().is_empty());
    assert!(s.viewed_map(&other, 1).unwrap().is_empty());
    assert_eq!(s.viewed_map(&r, 1).unwrap().len(), 1);
}

#[test]
fn draft_crud_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let r = repo("acme", "api");
    let (id1, id2);
    {
        let s = Store::open(&path).unwrap();
        id1 = s.save_draft(&draft(&r, 1, "first")).unwrap();
        let mut reply = draft(&r, 1, "second");
        reply.kind = DraftKind::Reply;
        reply.reply_to = Some(ThreadId::from(77));
        reply.path = None;
        reply.line = None;
        reply.side = None;
        id2 = s.save_draft(&reply).unwrap();
        assert_ne!(id1, id2);
        let mut edited = draft(&r, 1, "first edited");
        edited.id = id1;
        edited.side = Some(Side::New);
        assert_eq!(s.save_draft(&edited).unwrap(), id1);
    }
    let s = Store::open(&path).unwrap();
    let ds = s.list_drafts(&r, 1).unwrap();
    assert_eq!(ds.len(), 2);
    assert_eq!(ds[0].id, id1);
    assert_eq!(ds[0].text, "first edited");
    assert_eq!(ds[0].side, Some(Side::New));
    assert_eq!(ds[0].line, Some(12));
    assert_eq!(ds[1].id, id2);
    assert_eq!(ds[1].kind, DraftKind::Reply);
    assert_eq!(ds[1].reply_to, Some(ThreadId::from(77)));
    assert_eq!(ds[1].path, None);
    s.delete_draft(id1).unwrap();
    s.delete_draft(id1).unwrap();
    let ds = s.list_drafts(&r, 1).unwrap();
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].id, id2);
}

#[test]
fn drafts_scoped_per_pr() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let other = repo("acme", "web");
    s.save_draft(&draft(&r, 1, "x")).unwrap();
    s.save_draft(&draft(&r, 2, "y")).unwrap();
    s.save_draft(&draft(&other, 1, "z")).unwrap();
    let ds = s.list_drafts(&r, 1).unwrap();
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].text, "x");
    assert_eq!(s.list_drafts(&other, 1).unwrap()[0].text, "z");
    assert!(s.list_drafts(&other, 2).unwrap().is_empty());
}

#[test]
fn settings_defaults_then_roundtrip() {
    let s = Store::open_in_memory().unwrap();
    let default = Path::new("C:/data/mirrors");
    let st = s.settings(default).unwrap();
    assert_eq!(st.clone_root, None);
    assert_eq!(st.download_dir, default);
    let new = Settings {
        clone_root: Some(PathBuf::from("D:/code")),
        download_dir: PathBuf::from("E:/mirrors"),
    };
    s.save_settings(&new).unwrap();
    assert_eq!(s.settings(default).unwrap(), new);
    let cleared = Settings {
        clone_root: None,
        ..new
    };
    s.save_settings(&cleared).unwrap();
    assert_eq!(s.settings(default).unwrap(), cleared);
}

#[test]
fn migration_from_v1_keeps_existing_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let r = repo("acme", "api");
    let p = pr(&r, 1, PrState::Open);
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(SCHEMA_V1).unwrap();
        c.pragma_update(None, "user_version", 1).unwrap();
        c.execute(
            "INSERT INTO repos (full_name, workspace, slug) VALUES ('acme/api', 'acme', 'api')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO prs (repo, id, state, updated_on, json) VALUES (?1, 1, 'OPEN', ?2, ?3)",
            params![
                "acme/api",
                p.updated_on.to_rfc3339(),
                serde_json::to_string(&p).unwrap()
            ],
        )
        .unwrap();
        c.execute("INSERT INTO meta (key, value) VALUES ('k', 'v')", [])
            .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.list_repos().unwrap(), vec![r.clone()]);
    assert_eq!(s.list_prs().unwrap(), vec![p]);
    assert_eq!(s.get_meta("k").unwrap(), Some("v".to_string()));
    let v: i64 = s
        .safe()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    s.set_viewed(&r, 1, "a.rs", "o", true).unwrap();
    assert!(s.save_draft(&draft(&r, 1, "t")).unwrap() > 0);
}

#[test]
fn posted_tombstone_survives_reopen_and_blocks_edits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let r = repo("acme", "api");
    let (a, b);
    {
        let s = Store::open(&path).unwrap();
        a = s.save_draft(&draft(&r, 1, "a")).unwrap();
        b = s.save_draft(&draft(&r, 1, "b")).unwrap();
        s.mark_draft_posted(a).unwrap();
        assert!(matches!(
            s.mark_draft_posted(999),
            Err(StoreError::NotFound)
        ));
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.posted_draft_ids(&r, 1).unwrap(), vec![a]);
    assert!(s.posted_draft_ids(&r, 2).unwrap().is_empty());
    // a posted draft cannot be edited back into a postable one
    let mut edited = draft(&r, 1, "changed");
    edited.id = a;
    assert!(matches!(s.save_draft(&edited), Err(StoreError::NotFound)));
    edited.id = b;
    assert_eq!(s.save_draft(&edited).unwrap(), b);
    s.delete_draft(a).unwrap();
    assert!(s.posted_draft_ids(&r, 1).unwrap().is_empty());
}

#[test]
fn migration_from_v2_keeps_drafts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let r = repo("acme", "api");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(SCHEMA_V1).unwrap();
        c.execute_batch(SCHEMA_V2).unwrap();
        c.pragma_update(None, "user_version", 2).unwrap();
        c.execute(
            "INSERT INTO drafts (repo, pr_id, kind, text, source_commit)
             VALUES ('acme/api', 1, 'pr', 'kept', 'h')",
            [],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    let ds = s.list_drafts(&r, 1).unwrap();
    assert_eq!(ds.len(), 1);
    assert!(s.posted_draft_ids(&r, 1).unwrap().is_empty());
    s.mark_draft_posted(ds[0].id).unwrap();
    assert_eq!(s.posted_draft_ids(&r, 1).unwrap(), vec![ds[0].id]);
}

#[test]
fn migration_from_v3_keeps_drafts_and_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let r = repo("acme", "api");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(SCHEMA_V1).unwrap();
        c.execute_batch(SCHEMA_V2).unwrap();
        c.execute_batch(SCHEMA_V3).unwrap();
        c.pragma_update(None, "user_version", 3).unwrap();
        c.execute(
            "INSERT INTO drafts (repo, pr_id, kind, text, source_commit)
             VALUES ('acme/api', 1, 'pr', 'kept', 'h')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO drafts (repo, pr_id, kind, text, source_commit, posted_at)
             VALUES ('acme/api', 1, 'pr', 'posted', 'h', datetime('now'))",
            [],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    let ds = s.list_drafts(&r, 1).unwrap();
    assert_eq!(ds.len(), 2);
    assert_eq!(s.posted_draft_ids(&r, 1).unwrap(), vec![ds[1].id]);
    assert!(s.posting_draft_ids(&r, 1).unwrap().is_empty());
    let v: i64 = s
        .safe()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
}

/// A v4 cache file holding PRs of `with_prs` (and a repo row without PRs).
fn v4_cache(path: &Path, with_prs: &[RepoRef]) {
    let c = rusqlite::Connection::open(path).unwrap();
    for sql in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4] {
        c.execute_batch(sql).unwrap();
    }
    c.pragma_update(None, "user_version", 4).unwrap();
    c.execute(
        "INSERT INTO repos (full_name, workspace, slug) VALUES ('acme/idle', 'acme', 'idle')",
        [],
    )
    .unwrap();
    for (i, r) in with_prs.iter().enumerate() {
        let p = pr(r, i as u64 + 1, PrState::Open);
        for id in [p.id, p.id + 100] {
            c.execute(
                "INSERT INTO prs (repo, id, state, updated_on, json) VALUES (?1, ?2, 'OPEN', ?3, ?4)",
                params![
                    r.full_name(),
                    id as i64,
                    p.updated_on.to_rfc3339(),
                    serde_json::to_string(&p).unwrap()
                ],
            )
            .unwrap();
        }
    }
}

#[test]
fn upgrade_preselects_repos_with_cached_prs_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let (api, web) = (repo("acme", "api"), repo("beta", "web"));
    v4_cache(&path, &[api.clone(), web.clone()]);
    let s = Store::open(&path).unwrap();
    // nothing disappears: both repos with PRs are selected, the idle one is not
    assert_eq!(s.selected_repos().unwrap(), vec![api.clone(), web.clone()]);
    let snap = s.cache_snapshot().unwrap();
    assert_eq!(snap.prs.len(), 4);
    assert_eq!((snap.repo_count, snap.preselected), (2, Some(2)));
    assert!(!s.discovered_any().unwrap(), "no discovery has run yet");
    let choices = s.repo_choices().unwrap();
    assert!(choices
        .iter()
        .all(|c| c.selected && c.discovered_at.is_none()));
    // once: the user unselects one and dismisses the notice; a reopen changes nothing
    s.set_selection(std::slice::from_ref(&api)).unwrap();
    s.delete_meta(META_REPOS_PRESELECTED).unwrap();
    drop(s);
    let s = Store::open(&path).unwrap();
    assert_eq!(s.selected_repos().unwrap(), vec![api]);
    assert_eq!(s.cache_snapshot().unwrap().preselected, None);
}

#[test]
fn a_new_cache_selects_nothing_and_has_no_notice() {
    let s = Store::open_in_memory().unwrap();
    assert!(s.selected_repos().unwrap().is_empty());
    let snap = s.cache_snapshot().unwrap();
    assert_eq!((snap.repo_count, snap.preselected), (0, None));
    // an upgrade of a cache without PRs selects nothing either
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    v4_cache(&path, &[]);
    let s = Store::open(&path).unwrap();
    assert!(s.repo_choices().unwrap().is_empty());
    assert_eq!(s.get_meta(META_REPOS_PRESELECTED).unwrap(), None);
}

fn info(ws: &str, slug: &str, private: bool) -> RepoInfo {
    RepoInfo {
        repo: repo(ws, slug),
        name: Some(slug.to_uppercase()),
        private: Some(private),
    }
}

#[test]
fn discovery_is_cached_and_never_selects() {
    let s = Store::open_in_memory().unwrap();
    let ws = vec!["acme".to_string()];
    s.record_discovery(
        &[info("acme", "web", false), info("acme", "api", true)],
        &ws,
        Some(&ws),
    )
    .unwrap();
    assert!(s.discovered_any().unwrap());
    let c = s.repo_choices().unwrap();
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].repo, repo("acme", "api"));
    assert_eq!(
        (c[0].name.as_deref(), c[0].private, c[0].selected, c[0].gone),
        (Some("API"), Some(true), false, false)
    );
    assert!(c[0].discovered_at.is_some());
    assert!(s.selected_repos().unwrap().is_empty());
    let snap = s.cache_snapshot().unwrap();
    assert_eq!((snap.repo_count, snap.known_repos), (0, 2));
    // a selection survives the next discovery
    s.set_selection(&[repo("acme", "api")]).unwrap();
    s.record_discovery(
        &[info("acme", "web", false), info("acme", "api", true)],
        &ws,
        Some(&ws),
    )
    .unwrap();
    assert_eq!(s.selected_repos().unwrap(), vec![repo("acme", "api")]);
}

#[test]
fn vanished_repos_are_marked_gone_when_selected_and_dropped_otherwise() {
    let s = Store::open_in_memory().unwrap();
    let ws = vec!["acme".to_string(), "beta".to_string()];
    s.record_discovery(
        &[
            info("acme", "api", false),
            info("acme", "web", false),
            info("acme", "old", false),
            info("beta", "x", false),
            info("gamma", "y", false),
        ],
        &ws,
        None,
    )
    .unwrap();
    s.set_selection(&[repo("acme", "api"), repo("beta", "x"), repo("gamma", "y")])
        .unwrap();
    // acme no longer lists api (selected) nor old (not selected); beta could not be
    // listed; gamma is no longer one of my workspaces
    let all = vec!["acme".to_string(), "beta".to_string()];
    s.record_discovery(
        &[info("acme", "web", false)],
        &["acme".to_string()],
        Some(&all),
    )
    .unwrap();
    let c = s.repo_choices().unwrap();
    let names: Vec<(String, bool, bool)> = c
        .iter()
        .map(|c| (c.repo.full_name(), c.selected, c.gone))
        .collect();
    assert_eq!(
        names,
        vec![
            ("acme/api".to_string(), true, true),
            ("acme/web".to_string(), false, false),
            ("beta/x".to_string(), true, false),
            ("gamma/y".to_string(), true, true),
        ]
    );
    // a gone repo is not synced, and forgotten once unselected
    assert_eq!(s.selected_repos().unwrap(), vec![repo("beta", "x")]);
    s.set_selection(&[repo("beta", "x"), repo("gamma", "y")])
        .unwrap();
    assert!(!s
        .repo_choices()
        .unwrap()
        .iter()
        .any(|c| c.repo == repo("acme", "api")));
    // listed again: accessible again, still selected
    s.record_discovery(&[info("gamma", "y", false)], &["gamma".to_string()], None)
        .unwrap();
    assert_eq!(
        s.selected_repos().unwrap(),
        vec![repo("beta", "x"), repo("gamma", "y")]
    );
}

#[test]
fn the_inbox_snapshot_shows_only_selected_repos() {
    let s = Store::open_in_memory().unwrap();
    let (a, b) = (repo("acme", "api"), repo("acme", "web"));
    s.apply_repo_sync(&a, Some(&[pr(&a, 1, PrState::Open)]), &[], Some("http 500"))
        .unwrap();
    s.apply_repo_sync(
        &b,
        Some(&[pr(&b, 2, PrState::Open)]),
        &[],
        Some("rate limited"),
    )
    .unwrap();
    s.set_repo_error(&repo("acme", "*"), Some("network error"))
        .unwrap();
    s.set_selection(std::slice::from_ref(&a)).unwrap();
    let snap = s.cache_snapshot().unwrap();
    let shown: Vec<&RepoRef> = snap.prs.iter().map(|(p, _)| &p.repo).collect();
    assert_eq!(shown, vec![&a]);
    assert_eq!(
        snap.errors,
        vec![
            (repo("acme", "*"), "network error".to_string()),
            (a.clone(), "http 500".to_string())
        ]
    );
    // unselected: hidden, still cached
    assert_eq!(s.list_prs().unwrap().len(), 2);
    // purged: gone from the cache, drafts and viewed marks kept
    s.save_draft(&draft(&b, 2, "keep")).unwrap();
    s.set_viewed(&b, 2, "a.rs", "o", true).unwrap();
    s.put_diffstat(&b, 2, "c", &st(1)).unwrap();
    s.purge_repos(std::slice::from_ref(&b)).unwrap();
    assert_eq!(s.list_prs().unwrap().len(), 1);
    assert_eq!(s.get_diffstat(&b, 2).unwrap(), None);
    assert_eq!(s.repo_errors().unwrap().len(), 2);
    assert_eq!(s.list_drafts(&b, 2).unwrap().len(), 1);
    assert_eq!(s.viewed_map(&b, 2).unwrap().len(), 1);
}

#[test]
fn choices_follow_the_account() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    s.set_selection(std::slice::from_ref(&r)).unwrap();
    s.set_meta(META_REPOS_PRESELECTED, "1").unwrap();
    // no account recorded yet (an upgraded cache): the choices are kept
    assert!(s
        .adopt_user(&AccountId::default_bitbucket(), "{a}", "")
        .unwrap());
    assert_eq!(s.selected_repos().unwrap(), vec![r.clone()]);
    assert!(!s
        .adopt_user(&AccountId::default_bitbucket(), "{a}", "")
        .unwrap());
    assert_eq!(s.selected_repos().unwrap(), vec![r]);
    // another account: none of the first one's choices
    assert!(s
        .adopt_user(&AccountId::default_bitbucket(), "{b}", "")
        .unwrap());
    assert!(s.repo_choices().unwrap().is_empty());
    assert_eq!(s.get_meta(META_REPOS_PRESELECTED).unwrap(), None);
}

#[test]
fn migration_from_v4_keeps_drafts_untyped_and_markers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let r = repo("acme", "api");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        for sql in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4] {
            c.execute_batch(sql).unwrap();
        }
        c.pragma_update(None, "user_version", 4).unwrap();
        c.execute(
            "INSERT INTO drafts (repo, pr_id, kind, text, source_commit, posting_started_at)
             VALUES ('acme/api', 1, 'pr', 'old one', 'h', datetime('now'))",
            [],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    let ds = s.list_drafts(&r, 1).unwrap();
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].text, "old one");
    assert_eq!(ds[0].comment_type, None, "old drafts have no type");
    assert_eq!(ds[0].posted_text(), "old one");
    assert_eq!(s.posting_draft_ids(&r, 1).unwrap(), vec![ds[0].id]);
    let v: i64 = s
        .safe()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
}

#[test]
fn draft_type_round_trips_and_can_be_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let r = repo("acme", "api");
    let id;
    {
        let s = Store::open(&path).unwrap();
        let mut d = draft(&r, 1, "rename");
        d.comment_type = Some(CommentType::Nit);
        id = s.save_draft(&d).unwrap();
        let mut q = draft(&r, 1, "why?");
        q.comment_type = Some(CommentType::Question);
        s.save_draft(&q).unwrap();
    }
    let s = Store::open(&path).unwrap();
    let ds = s.list_drafts(&r, 1).unwrap();
    assert_eq!(ds[0].comment_type, Some(CommentType::Nit));
    assert_eq!(ds[1].comment_type, Some(CommentType::Question));
    // the text is stored without the prefix; it is added at post time only
    assert_eq!(ds[0].text, "rename");
    assert_eq!(ds[0].posted_text(), "**nit:** rename");
    let mut edited = ds[0].clone();
    edited.comment_type = None;
    assert_eq!(s.save_draft(&edited).unwrap(), id);
    assert_eq!(s.list_drafts(&r, 1).unwrap()[0].comment_type, None);
    // an unknown stored type (a newer build) reads as no type, the draft is kept
    s.safe()
        .execute(
            "UPDATE drafts SET comment_type = 'blocker' WHERE id = ?1",
            params![id],
        )
        .unwrap();
    let ds = s.list_drafts(&r, 1).unwrap();
    assert_eq!(ds.len(), 2);
    assert_eq!(ds[0].comment_type, None);
}

#[test]
fn posting_marker_survives_reopen_and_is_guarded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let r = repo("acme", "api");
    let (a, b);
    {
        let s = Store::open(&path).unwrap();
        a = s.save_draft(&draft(&r, 1, "a")).unwrap();
        b = s.save_draft(&draft(&r, 1, "b")).unwrap();
        s.mark_draft_posting(a).unwrap();
        s.mark_draft_posting(b).unwrap();
        // a posted (tombstoned) draft gets no new marker
        s.mark_draft_posted(b).unwrap();
        assert!(matches!(s.mark_draft_posting(b), Err(StoreError::NotFound)));
        assert!(matches!(
            s.mark_draft_posting(999),
            Err(StoreError::NotFound)
        ));
    }
    let s = Store::open(&path).unwrap();
    // posted ones are not "posting": only a, which has no outcome yet
    assert_eq!(s.posting_draft_ids(&r, 1).unwrap(), vec![a]);
    assert!(s.posting_draft_ids(&r, 2).unwrap().is_empty());
    s.clear_draft_posting(a).unwrap();
    assert!(s.posting_draft_ids(&r, 1).unwrap().is_empty());
    // editing keeps the marker: an edit is not a re-arm
    s.mark_draft_posting(a).unwrap();
    let mut edited = draft(&r, 1, "edited");
    edited.id = a;
    s.save_draft(&edited).unwrap();
    assert_eq!(s.posting_draft_ids(&r, 1).unwrap(), vec![a]);
}

#[test]
fn save_after_delete_returns_not_found() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let mut d = draft(&r, 1, "x");
    d.id = s.save_draft(&d).unwrap();
    s.delete_draft(d.id).unwrap();
    assert!(matches!(s.save_draft(&d), Err(StoreError::NotFound)));
    assert!(s.list_drafts(&r, 1).unwrap().is_empty());
}

#[test]
fn save_cannot_move_draft_to_other_pr() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let mut d = draft(&r, 1, "x");
    d.id = s.save_draft(&d).unwrap();
    let mut moved = d.clone();
    moved.pr_id = 2;
    assert!(matches!(s.save_draft(&moved), Err(StoreError::NotFound)));
    moved.pr_id = 1;
    moved.repo = repo("acme", "web");
    assert!(matches!(s.save_draft(&moved), Err(StoreError::NotFound)));
    assert_eq!(s.list_drafts(&r, 1).unwrap(), vec![d]);
    assert!(s.list_drafts(&r, 2).unwrap().is_empty());
}

#[test]
fn list_skips_corrupt_row() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let good = s.save_draft(&draft(&r, 1, "good")).unwrap();
    for (kind, side) in [("bogus", None), ("inline", Some("sideways"))] {
        s.safe()
            .execute(
                "INSERT INTO drafts (repo_key, pr_id, kind, side, text, source_commit)
                 VALUES ('bitbucket.org/acme/api', 1, ?1, ?2, 't', 'c')",
                params![kind, side],
            )
            .unwrap();
    }
    let ds = s.list_drafts(&r, 1).unwrap();
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].id, good);
}

#[test]
fn no_secret_columns_exist() {
    let s = Store::open_in_memory().unwrap();
    let tables: Vec<String> = s
        .safe()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!tables.is_empty());
    for t in tables {
        let cols: Vec<String> = s
            .safe()
            .prepare("SELECT name FROM pragma_table_info(?1)")
            .unwrap()
            .query_map([&t], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for c in cols {
            let c = c.to_lowercase();
            for bad in ["token", "password", "secret"] {
                assert!(!c.contains(bad), "column {t}.{c} looks secret");
            }
        }
    }
}

fn st(n: u32) -> DiffStat {
    DiffStat {
        files: n,
        lines_added: n,
        lines_removed: 0,
        max_file_lines: n,
    }
}

#[test]
fn wal_mode_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("cache.db")).unwrap();
    let pragma = |c: &Conn, name: &str| -> String {
        lock(c)
            .query_row(&format!("PRAGMA {name}"), [], |r| {
                r.get::<_, rusqlite::types::Value>(0)
            })
            .map(|v| match v {
                rusqlite::types::Value::Text(t) => t.to_lowercase(),
                rusqlite::types::Value::Integer(i) => i.to_string(),
                other => format!("{other:?}"),
            })
            .unwrap()
    };
    for c in [&s.safe, &s.fast, &s.read] {
        assert_eq!(pragma(c, "journal_mode"), "wal");
        assert_eq!(pragma(c, "busy_timeout"), "5000");
    }
    // R-store: drafts and markers are written with FULL (2), the cache with NORMAL (1)
    assert_eq!(pragma(&s.safe, "synchronous"), "2");
    assert_eq!(pragma(&s.fast, "synchronous"), "1");
    assert!(!Arc::ptr_eq(&s.safe, &s.fast) && !Arc::ptr_eq(&s.fast, &s.read));
    // the reader cannot write
    assert!(lock(&s.read).execute("DELETE FROM meta", []).is_err());
}

#[test]
fn without_wal_the_store_falls_back_to_a_full_sync_rollback_journal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let no_wal = |_: &Connection| -> StoreResult<()> { Err(StoreError::WalUnavailable) };
    let msg = StoreError::WalUnavailable.to_string();
    assert!(
        msg.contains("WAL") && msg.contains("network drive"),
        "{msg}"
    );
    let s = Store::open_with(&path, no_wal).unwrap();
    assert!(s.wal_unavailable());
    let pragma = |c: &Conn, name: &str| -> String {
        lock(c)
            .query_row(&format!("PRAGMA {name}"), [], |r| {
                r.get::<_, rusqlite::types::Value>(0)
            })
            .map(|v| match v {
                rusqlite::types::Value::Text(t) => t.to_lowercase(),
                rusqlite::types::Value::Integer(i) => i.to_string(),
                other => format!("{other:?}"),
            })
            .unwrap()
    };
    for c in [&s.safe, &s.fast, &s.read] {
        assert_eq!(pragma(c, "journal_mode"), "delete");
    }
    // both writing connections are FULL: without WAL, NORMAL can corrupt on power loss
    assert_eq!(pragma(&s.safe, "synchronous"), "2");
    assert_eq!(pragma(&s.fast, "synchronous"), "2");
    let r = repo("acme", "api");
    let id = s.save_draft(&draft(&r, 1, "kept")).unwrap();
    s.replace_prs(&r, &[pr(&r, 1, PrState::Open)]).unwrap();
    s.set_selection(std::slice::from_ref(&r)).unwrap();
    assert_eq!(s.cache_snapshot().unwrap().prs.len(), 1);
    drop(s);
    let again = Store::open(&path).unwrap();
    assert!(!again.wal_unavailable());
    assert_eq!(again.list_drafts(&r, 1).unwrap()[0].id, id);
}

#[test]
fn a_different_account_clears_the_rebuildable_tables_only() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let fill = |s: &Store| {
        s.upsert_repos(std::slice::from_ref(&r)).unwrap();
        s.apply_repo_sync(
            &r,
            Some(&[pr(&r, 1, PrState::Open)]),
            &[(1, "c".into(), st(1))],
            Some("e"),
        )
        .unwrap();
        s.set_viewed(&r, 1, "a.rs", "o", true).unwrap();
    };
    let draft_id = s.save_draft(&draft(&r, 1, "mine")).unwrap();
    // the first sign-in of a cache without an owner: whatever is there is not trusted
    fill(&s);
    assert!(s
        .adopt_user(&AccountId::default_bitbucket(), "{account-a}", "")
        .unwrap());
    assert!(s.list_prs().unwrap().is_empty());
    fill(&s);
    // the same account again keeps everything
    assert!(!s
        .adopt_user(&AccountId::default_bitbucket(), "{account-a}", "")
        .unwrap());
    assert_eq!(s.list_prs().unwrap().len(), 1);
    assert_eq!(
        s.cache_snapshot().unwrap().account_id.as_deref(),
        Some("{account-a}")
    );
    // another account: nothing of the first one stays readable, drafts are kept
    assert!(s
        .adopt_user(&AccountId::default_bitbucket(), "{account-b}", "")
        .unwrap());
    let snap = s.cache_snapshot().unwrap();
    assert!(snap.prs.is_empty() && snap.errors.is_empty() && snap.repo_count == 0);
    assert_eq!(snap.account_id.as_deref(), Some("{account-b}"));
    assert!(s.list_repos().unwrap().is_empty());
    assert_eq!(s.get_diffstat(&r, 1).unwrap(), None);
    assert!(s.viewed_map(&r, 1).unwrap().is_empty());
    assert_eq!(s.list_drafts(&r, 1).unwrap()[0].id, draft_id);
    assert_eq!(
        s.get_meta("account_id").unwrap().as_deref(),
        Some("{account-b}")
    );
}

#[test]
fn rows_written_for_an_account_are_owned_by_it_and_purged_with_it() {
    let s = Store::open_in_memory().unwrap();
    let def = AccountId::default_bitbucket();
    let other = AccountId("acc-second".into());
    let (a, b) = (repo("acme", "api"), repo("beta", "web"));
    s.set_selection_for(&def, std::slice::from_ref(&a)).unwrap();
    s.set_selection_for(&other, std::slice::from_ref(&b))
        .unwrap();
    s.apply_repo_sync_for(&def, &a, Some(&[pr(&a, 1, PrState::Open)]), &[], None)
        .unwrap();
    s.apply_repo_sync_for(
        &other,
        &b,
        Some(&[pr(&b, 2, PrState::Open)]),
        &[(2, "c".into(), st(2))],
        Some("rate limited"),
    )
    .unwrap();
    s.set_repo_error_for(&other, &repo("beta", "*"), Some("access denied"))
        .unwrap();
    s.set_repo_error_for(&def, &repo("acme", "*"), Some("network error"))
        .unwrap();
    let snap = s.cache_snapshot().unwrap();
    assert_eq!(snap.prs.len(), 2, "both accounts' PRs are in the inbox");
    assert_eq!(snap.errors.len(), 3);
    let owner = |table: &str, key: &str| -> String {
        lock(&s.fast)
            .query_row(
                &format!("SELECT account_id FROM {table} WHERE repo_key = ?1"),
                params![key],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(owner("prs", &b.key()), "acc-second");
    assert_eq!(owner("diffstats", &b.key()), "acc-second");
    assert_eq!(owner("repo_errors", "acc-second:beta/*"), "acc-second");
    assert_eq!(owner("prs", &a.key()), "acc-bitbucket-default");
    // the repository moves to the other account (D8): its rows follow on the next sync
    s.set_selection_for(&other, &[a.clone(), b.clone()])
        .unwrap();
    s.apply_repo_sync_for(&other, &a, Some(&[pr(&a, 1, PrState::Open)]), &[], None)
        .unwrap();
    assert_eq!(owner("prs", &a.key()), "acc-second");
    let removed = s.remove_account(&other).unwrap();
    assert_eq!(
        (removed.prs, removed.diffstats, removed.repo_errors),
        (2, 1, 2)
    );
    let snap = s.cache_snapshot().unwrap();
    assert!(snap.prs.is_empty());
    assert_eq!(
        snap.errors,
        vec![(repo("acme", "*"), "network error".to_string())],
        "the other account's rows only"
    );
}

#[test]
fn selection_of_every_account_in_one_save() {
    let s = Store::open_in_memory().unwrap();
    let (one, two) = (AccountId("acc-1".into()), AccountId("acc-2".into()));
    let (a, b, c) = (repo("acme", "api"), repo("acme", "web"), repo("beta", "x"));
    let listed = |r: &RepoRef| [RepoInfo::from(r.clone())];
    s.record_discovery_for(&one, &listed(&a), &[], None)
        .unwrap();
    s.record_discovery_for(&two, &listed(&a), &[], None)
        .unwrap();
    s.set_selection_for(&one, &[a.clone(), b.clone()]).unwrap();
    // a moves to account two, b is dropped, c is new under one; a listed twice keeps
    // its first account
    s.set_selection_all(&[
        (two.clone(), a.clone()),
        (one.clone(), c.clone()),
        (one.clone(), a.clone()),
    ])
    .unwrap();
    let mut got = s.selected_repos_by_account().unwrap();
    got.sort_by_key(|(acc, r)| (acc.clone(), r.key()));
    assert_eq!(got, vec![(one.clone(), c), (two.clone(), a.clone())]);
    let a_rows: Vec<(AccountId, bool)> = s
        .repo_choices()
        .unwrap()
        .into_iter()
        .filter(|ch| ch.repo == a)
        .map(|ch| (ch.account, ch.selected))
        .collect();
    assert_eq!(a_rows, vec![(one, false), (two, true)]);
}

#[test]
fn repo_account_prefers_the_selecting_account() {
    let s = Store::open_in_memory().unwrap();
    let (one, two) = (AccountId("acc-1".into()), AccountId("acc-2".into()));
    let r = repo("acme", "api");
    assert_eq!(s.repo_account(&r).unwrap(), None);
    let listed = [RepoInfo::from(r.clone())];
    s.record_discovery_for(&one, &listed, &["acme".into()], None)
        .unwrap();
    s.record_discovery_for(&two, &listed, &["acme".into()], None)
        .unwrap();
    assert_eq!(s.repo_account(&r).unwrap(), Some(one.clone()));
    s.set_selection_for(&two, std::slice::from_ref(&r)).unwrap();
    assert_eq!(s.repo_account(&r).unwrap(), Some(two));
}

#[test]
fn adopting_a_user_touches_only_that_accounts_rows() {
    let s = Store::open_in_memory().unwrap();
    let def = AccountId::default_bitbucket();
    let other = AccountId("acc-second".into());
    let (a, b) = (repo("acme", "api"), repo("beta", "web"));
    s.set_selection_for(&def, std::slice::from_ref(&a)).unwrap();
    s.set_selection_for(&other, std::slice::from_ref(&b))
        .unwrap();
    assert!(s.adopt_user(&def, "{ana}", "Ana").unwrap());
    assert!(s.adopt_user(&other, "{bo}", "Bo").unwrap());
    s.apply_repo_sync_for(&def, &a, Some(&[pr(&a, 1, PrState::Open)]), &[], None)
        .unwrap();
    s.apply_repo_sync_for(&other, &b, Some(&[pr(&b, 2, PrState::Open)]), &[], None)
        .unwrap();
    // the other account signs in as someone else: only its rows go
    assert!(s.adopt_user(&other, "{cy}", "Cy").unwrap());
    let snap = s.cache_snapshot().unwrap();
    assert_eq!(snap.prs.len(), 1);
    assert_eq!(snap.prs[0].0.repo, a);
    assert_eq!(s.selected_repos().unwrap(), vec![a.clone()]);
    assert_eq!(snap.account_id.as_deref(), Some("{ana}"));
    let accounts = s.accounts().unwrap();
    let o = accounts.iter().find(|x| x.id == other).unwrap();
    assert_eq!(
        (o.user_id.as_str(), o.display_name.as_str()),
        ("{cy}", "Cy")
    );
    // the default account keeps its user: nothing changes
    assert!(!s.adopt_user(&def, "{ana}", "Ana").unwrap());
    assert_eq!(s.cache_snapshot().unwrap().prs.len(), 1);
}

#[test]
fn diffstats_alone_are_written_in_one_transaction() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    s.apply_repo_sync(&r, Some(&[pr(&r, 1, PrState::Open)]), &[], Some("e"))
        .unwrap();
    s.put_diffstats(&r, &[(1, "c1".into(), st(1)), (2, "c2".into(), st(2))])
        .unwrap();
    assert_eq!(s.get_diffstat(&r, 2).unwrap(), Some(("c2".into(), st(2))));
    // the PR list and the error row are not touched
    assert_eq!(s.list_prs().unwrap().len(), 1);
    assert_eq!(s.repo_errors().unwrap().len(), 1);
}

#[test]
fn draft_marker_durable_with_two_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let r = repo("acme", "api");
    let s = Store::open(&path).unwrap();
    let id = s.save_draft(&draft(&r, 1, "send me")).unwrap();
    // cache writes on the other connection, before and after the marker
    s.replace_prs(&r, &[pr(&r, 1, PrState::Open)]).unwrap();
    s.mark_draft_posting(id).unwrap();
    s.put_diffstat(&r, 1, "c", &st(1)).unwrap();
    s.set_viewed(&r, 1, "a.rs", "o", true).unwrap();
    // a crash: no close, no checkpoint. The marker lives in the WAL only; copy the files
    // as they are on disk and open the copy.
    let wal = dir.path().join("cache.db-wal");
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 0,
        "WAL holds the commits"
    );
    let copy = tempfile::tempdir().unwrap();
    std::fs::copy(&path, copy.path().join("cache.db")).unwrap();
    std::fs::copy(&wal, copy.path().join("cache.db-wal")).unwrap();
    std::mem::forget(s);
    let again = Store::open(&copy.path().join("cache.db")).unwrap();
    assert_eq!(again.posting_draft_ids(&r, 1).unwrap(), vec![id]);
    assert_eq!(again.list_drafts(&r, 1).unwrap()[0].text, "send me");
    assert_eq!(again.list_prs().unwrap().len(), 1);
}

#[test]
fn replace_unchanged_list_does_not_write() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let prs = vec![pr(&r, 2, PrState::Open), pr(&r, 1, PrState::Merged)];
    let stats = vec![(2, "abc123".to_string(), st(4))];
    s.upsert_repos(std::slice::from_ref(&r)).unwrap();
    s.apply_repo_sync(&r, Some(&prs), &stats, Some("rate limited"))
        .unwrap();
    let before = s.fast_changes();
    // the same result again (in another order): nothing is rewritten
    let reordered = vec![prs[1].clone(), prs[0].clone()];
    s.apply_repo_sync(&r, Some(&reordered), &stats, Some("rate limited"))
        .unwrap();
    s.replace_prs(&r, &prs).unwrap();
    s.upsert_repos(std::slice::from_ref(&r)).unwrap();
    assert_eq!(s.fast_changes(), before, "a no-change sync writes no rows");
    // a changed PR is written
    let mut moved = prs.clone();
    moved[0].source_commit = "new".into();
    s.replace_prs(&r, &moved).unwrap();
    assert!(s.fast_changes() > before);
    let stored = s.list_prs().unwrap();
    assert!(stored.iter().any(|p| p.source_commit == "new"));
}

#[test]
fn apply_repo_sync_atomic() {
    let s = Store::open_in_memory().unwrap();
    let r = repo("acme", "api");
    let old = vec![pr(&r, 1, PrState::Open), pr(&r, 2, PrState::Open)];
    s.apply_repo_sync(
        &r,
        Some(&old),
        &[(1, "c1".into(), st(1))],
        Some("old error"),
    )
    .unwrap();
    assert_eq!(s.list_prs().unwrap(), old);
    // the diffstats and the error row are written first, then the PRs fail (two rows
    // with one id): everything rolls back
    let bad = vec![pr(&r, 5, PrState::Open), pr(&r, 5, PrState::Merged)];
    assert!(s
        .apply_repo_sync(&r, Some(&bad), &[(1, "c2".into(), st(9))], None)
        .is_err());
    assert_eq!(s.list_prs().unwrap(), old);
    assert_eq!(s.get_diffstat(&r, 1).unwrap(), Some(("c1".into(), st(1))));
    assert_eq!(
        s.repo_errors().unwrap(),
        vec![(r.clone(), "old error".to_string())]
    );
    // without a PR list the stored PRs stay, the rest is written
    s.apply_repo_sync(&r, None, &[(2, "c3".into(), st(3))], None)
        .unwrap();
    assert_eq!(s.list_prs().unwrap(), old);
    assert_eq!(s.get_diffstat(&r, 2).unwrap(), Some(("c3".into(), st(3))));
    assert!(s.repo_errors().unwrap().is_empty());
    assert_eq!(
        s.diffstat_commits(&r).unwrap(),
        HashMap::from([(1, "c1".to_string()), (2, "c3".to_string())])
    );
}

#[test]
fn cache_snapshot_equals_n_plus_one_reads() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("cache.db")).unwrap();
    let (a, b) = (repo("acme", "api"), repo("acme", "web"));
    s.upsert_repos(&[a.clone(), b.clone(), repo("acme", "empty")])
        .unwrap();
    s.replace_prs(&b, &[pr(&b, 3, PrState::Open)]).unwrap();
    s.replace_prs(&a, &[pr(&a, 2, PrState::Open), pr(&a, 1, PrState::Merged)])
        .unwrap();
    s.put_diffstat(&a, 2, "x", &st(2)).unwrap();
    s.put_diffstat(&b, 3, "y", &st(3)).unwrap();
    // a diffstat of a PR that is gone is not part of the inbox
    s.put_diffstat(&b, 99, "z", &st(9)).unwrap();
    s.set_repo_error(&b, Some("http 500")).unwrap();
    s.set_selection(&[a.clone(), b.clone(), repo("acme", "empty")])
        .unwrap();
    let snap = s.cache_snapshot().unwrap();
    let old: Vec<(PullRequest, Option<(String, DiffStat)>)> = s
        .list_prs()
        .unwrap()
        .into_iter()
        .map(|p| {
            let d = s.get_diffstat(&p.repo, p.id).unwrap();
            (p, d)
        })
        .collect();
    assert_eq!(snap.prs, old);
    assert_eq!(snap.errors, s.repo_errors().unwrap());
    assert_eq!(snap.repo_count, s.list_repos().unwrap().len());
    assert_eq!(snap.prs.len(), 3);
}

#[test]
fn cache_snapshot_reports_an_undecodable_pr() {
    let s = Store::open_in_memory().unwrap();
    s.safe()
        .execute(
            "INSERT INTO prs (repo_key, id, state, updated_on, json)
             VALUES ('bitbucket.org/a/b', 1, 'OPEN', '', '{')",
            [],
        )
        .unwrap();
    s.set_selection(&[repo("a", "b")]).unwrap();
    assert!(s.cache_snapshot().is_err());
}

#[test]
fn in_memory_store_still_works() {
    let s = Store::open_in_memory().unwrap();
    assert!(Arc::ptr_eq(&s.safe, &s.fast) && Arc::ptr_eq(&s.fast, &s.read));
    let r = repo("acme", "api");
    s.apply_repo_sync(
        &r,
        Some(&[pr(&r, 1, PrState::Open)]),
        &[(1, "c".into(), st(1))],
        None,
    )
    .unwrap();
    let id = s.save_draft(&draft(&r, 1, "x")).unwrap();
    s.mark_draft_posting(id).unwrap();
    s.set_viewed_many(&r, 1, &[("a.rs", "o", true)]).unwrap();
    s.save_settings(&Settings {
        clone_root: None,
        download_dir: PathBuf::from("d"),
    })
    .unwrap();
    s.set_selection(std::slice::from_ref(&r)).unwrap();
    assert_eq!(s.cache_snapshot().unwrap().prs.len(), 1);
    assert_eq!(s.posting_draft_ids(&r, 1).unwrap(), vec![id]);
    assert_eq!(s.viewed_map(&r, 1).unwrap().len(), 1);
    s.checkpoint().unwrap();
}

#[test]
fn concurrent_readers_and_writers_do_not_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(Store::open(&dir.path().join("cache.db")).unwrap());
    let all: Vec<RepoRef> = (0..6).map(|t| repo("w", &format!("r{t}"))).collect();
    s.set_selection(&all).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let mut threads = Vec::new();
    for t in 0..6u64 {
        let (s, done_tx) = (s.clone(), done_tx.clone());
        threads.push(std::thread::spawn(move || {
            let r = repo("w", &format!("r{t}"));
            for i in 0..40u64 {
                match t % 3 {
                    0 => s
                        .apply_repo_sync(
                            &r,
                            Some(&[pr(&r, i % 5, PrState::Open)]),
                            &[(i, format!("c{i}"), st(i as u32))],
                            (i % 2 == 0).then_some("e"),
                        )
                        .unwrap(),
                    1 => {
                        let id = s.save_draft(&draft(&r, i, "d")).unwrap();
                        s.mark_draft_posting(id).unwrap();
                        s.set_viewed(&r, i, "a.rs", "o", true).unwrap();
                    }
                    _ => {
                        s.cache_snapshot().unwrap();
                        s.list_drafts(&r, i).unwrap();
                    }
                }
            }
            let _ = done_tx.send(());
        }));
    }
    drop(done_tx);
    for _ in 0..6 {
        done_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("a store thread is stuck");
    }
    for t in threads {
        t.join().unwrap();
    }
    // two writer threads, each repo's list replaced by its last single-PR list
    assert_eq!(s.cache_snapshot().unwrap().prs.len(), 2);
}

// ---- v7: accounts and host-qualified keys ----

/// A PR exactly as the v6 build serialized it (no host, `workspace`, `uuid`).
fn v6_pr_json(ws: &str, slug: &str, id: u64) -> String {
    format!(
        r#"{{"repo":{{"workspace":"{ws}","slug":"{slug}"}},"id":{id},"title":"PR {id}",
        "state":"Open","draft":false,"author":{{"uuid":"{{a}}","display_name":"Al"}},
        "source_branch":"f{id}","dest_branch":"main","source_commit":"s{id}",
        "dest_commit":"d{id}","source_repo":{{"workspace":"{ws}","slug":"{slug}"}},
        "comment_count":0,"task_count":0,"reviewers":[],
        "created_on":"2026-01-01T00:00:00Z","updated_on":"2026-01-02T00:00:00Z"}}"#
    )
}

/// A v6 cache populated in every table, written with the v1..v6 schema exactly as the
/// previous build left it.
fn v6_cache(path: &Path) {
    let c = rusqlite::Connection::open(path).unwrap();
    for sql in [
        SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6,
    ] {
        c.execute_batch(sql).unwrap();
    }
    c.pragma_update(None, "user_version", 6).unwrap();
    c.execute_batch(
        "INSERT INTO meta (key, value) VALUES ('account_id', '{me}');
         INSERT INTO meta (key, value) VALUES ('me', '{me}\nAnn Example');
         INSERT INTO meta (key, value) VALUES ('clone_root', 'C:\\src');
         INSERT INTO repos (full_name, workspace, slug) VALUES ('acme/api', 'acme', 'api');
         INSERT INTO repos (full_name, workspace, slug) VALUES ('acme/web', 'acme', 'web');
         INSERT INTO repos (full_name, workspace, slug) VALUES ('Other/Mixed', 'Other', 'Mixed');
         INSERT INTO diffstats (repo, id, source_commit, json)
             VALUES ('acme/api', 1, 's1',
                     '{\"files\":3,\"lines_added\":10,\"lines_removed\":2,\"max_file_lines\":7}');
         INSERT INTO repo_errors (full_name, workspace, slug, message)
             VALUES ('*/*', '*', '*', 'network error');
         INSERT INTO repo_errors (full_name, workspace, slug, message)
             VALUES ('bad/*', 'bad', '*', 'access denied');
         INSERT INTO repo_errors (full_name, workspace, slug, message)
             VALUES ('acme/web', 'acme', 'web', 'not found');
         INSERT INTO viewed (repo, pr_id, path, oid) VALUES ('acme/api', 1, 'a.rs', 'o1');
         INSERT INTO viewed (repo, pr_id, path, oid) VALUES ('acme/api', 1, 'b.rs', 'o2');
         INSERT INTO viewed (repo, pr_id, path, oid) VALUES ('acme/web', 2, 'a.rs', 'o3');
         INSERT INTO drafts (repo, pr_id, kind, text, source_commit)
             VALUES ('acme/api', 1, 'pr', 'top level', 's1');
         INSERT INTO drafts (repo, pr_id, kind, parent_id, text, source_commit, comment_type)
             VALUES ('acme/api', 1, 'reply', 77, 'a reply', 's1', 'nit');
         INSERT INTO drafts (repo, pr_id, kind, path, line, side, text, source_commit,
                             posting_started_at)
             VALUES ('acme/web', 2, 'inline', 'src/x.rs', 9, 'new', 'sent, unknown', 's2',
                     datetime('now'));
         INSERT INTO drafts (repo, pr_id, kind, text, source_commit, posted_at)
             VALUES ('acme/web', 2, 'pr', 'posted already', 's2', datetime('now'));
         DELETE FROM repo_choices;
         INSERT INTO repo_choices (full_name, workspace, slug, name, is_private, selected,
                                   discovered_at, gone)
             VALUES ('acme/api', 'acme', 'api', 'API', 1, 1, '2026-01-01 00:00:00', 0);
         INSERT INTO repo_choices (full_name, workspace, slug, name, is_private, selected,
                                   discovered_at, gone)
             VALUES ('acme/web', 'acme', 'web', 'Web', 0, 1, '2026-01-01 00:00:00', 0);
         INSERT INTO repo_choices (full_name, workspace, slug, name, is_private, selected,
                                   discovered_at, gone)
             VALUES ('acme/idle', 'acme', 'idle', NULL, NULL, 0, '2026-01-01 00:00:00', 0);
         INSERT INTO repo_choices (full_name, workspace, slug, selected, gone)
             VALUES ('acme/gone', 'acme', 'gone', 1, 1);",
    )
    .unwrap();
    for (ws, slug, id) in [("acme", "api", 1), ("acme", "web", 2), ("acme", "idle", 3)] {
        c.execute(
            "INSERT INTO prs (repo, id, state, updated_on, json)
             VALUES (?1, ?2, 'OPEN', '2026-01-02T00:00:00+00:00', ?3)",
            params![format!("{ws}/{slug}"), id as i64, v6_pr_json(ws, slug, id)],
        )
        .unwrap();
    }
}

#[test]
fn v6_to_v7_rewrites_keys_and_keeps_drafts_viewed_selection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    v6_cache(&path);
    let s = Store::open(&path).unwrap();
    let v: i64 = s
        .safe()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    let (api, web) = (repo("acme", "api"), repo("acme", "web"));
    let acc = AccountId::default_bitbucket();

    // the single sign-in became the default Bitbucket account (no secret in it)
    let accounts = s.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, acc);
    assert_eq!(accounts[0].forge, Forge::BitbucketCloud);
    assert_eq!(accounts[0].git_host, "bitbucket.org");
    assert_eq!(accounts[0].user_id, "{me}");
    assert_eq!(accounts[0].display_name, "Ann Example");
    assert_eq!(
        s.get_meta("clone_root").unwrap().as_deref(),
        Some("C:\\src")
    );

    // drafts: every one kept with its text, kind, reply target, type and markers
    let ds = s.list_drafts(&api, 1).unwrap();
    assert_eq!(ds.len(), 2);
    assert_eq!(
        (ds[0].kind, ds[0].text.as_str()),
        (DraftKind::Pr, "top level")
    );
    assert_eq!(ds[1].kind, DraftKind::Reply);
    assert_eq!(ds[1].reply_to, Some(ThreadId::from(77)));
    assert_eq!(ds[1].comment_type, Some(CommentType::Nit));
    assert_eq!(ds[1].posted_text(), "**nit:** a reply");
    assert!(s.posted_draft_ids(&api, 1).unwrap().is_empty());
    let wd = s.list_drafts(&web, 2).unwrap();
    assert_eq!(wd.len(), 2);
    assert_eq!(
        (wd[0].path.as_deref(), wd[0].line, wd[0].side),
        (Some("src/x.rs"), Some(9), Some(Side::New))
    );
    assert_eq!(s.posting_draft_ids(&web, 2).unwrap(), vec![wd[0].id]);
    assert_eq!(s.posted_draft_ids(&web, 2).unwrap(), vec![wd[1].id]);

    // viewed marks per repo and PR, nothing mixed between them
    let va = s.viewed_map(&api, 1).unwrap();
    assert_eq!(va.len(), 2);
    assert_eq!(va["a.rs"], "o1");
    assert_eq!(s.viewed_map(&web, 2).unwrap()["a.rs"], "o3");
    assert!(s.viewed_map(&api, 2).unwrap().is_empty());
    assert!(s.viewed_map(&web, 1).unwrap().is_empty());

    // selection and discovery cache, owned by the default account
    let choices = s.repo_choices().unwrap();
    let summary: Vec<_> = choices
        .iter()
        .map(|c| {
            (
                c.repo.full_name(),
                c.name.clone(),
                c.private,
                c.selected,
                c.gone,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                "acme/api".into(),
                Some("API".into()),
                Some(true),
                true,
                false
            ),
            ("acme/gone".into(), None, None, true, true),
            ("acme/idle".into(), None, None, false, false),
            (
                "acme/web".into(),
                Some("Web".into()),
                Some(false),
                true,
                false
            ),
        ]
    );
    assert!(choices.iter().all(|c| c.account == acc));
    assert!(choices.iter().all(|c| c.repo.host == "bitbucket.org"));
    assert_eq!(s.selected_repos().unwrap(), vec![api.clone(), web.clone()]);
    assert!(s.discovered_any().unwrap());

    // cached PRs decode (old JSON, serde defaults) and stay in their repos
    let prs = s.list_prs().unwrap();
    assert_eq!(prs.len(), 3);
    assert!(prs
        .iter()
        .all(|p| p.forge == Forge::BitbucketCloud && p.account == acc));
    assert_eq!(prs[0].repo, api);
    assert_eq!(prs[0].author.id, "{a}");
    let snap = s.cache_snapshot().unwrap();
    let ids: Vec<(String, u64)> = snap
        .prs
        .iter()
        .map(|(p, _)| (p.repo.full_name(), p.id))
        .collect();
    // acme/idle is not selected: cached, but not in the inbox
    assert_eq!(ids, vec![("acme/api".into(), 1), ("acme/web".into(), 2)]);
    assert_eq!(
        snap.prs[0].1.as_ref().map(|(c, d)| (c.as_str(), d.files)),
        Some(("s1", 3))
    );
    assert!(snap.prs[1].1.is_none(), "no diffstat moved to another PR");
    assert_eq!(snap.account_id.as_deref(), Some("{me}"));
    assert_eq!(snap.repo_count, 2);
    assert_eq!(snap.known_repos, 3);

    // errors: listing pseudo rows read back as before, the repo error kept
    assert_eq!(
        snap.errors,
        vec![
            (repo("*", "*"), "network error".to_string()),
            (repo("acme", "web"), "not found".to_string()),
            (repo("bad", "*"), "access denied".to_string()),
        ]
    );

    // the stored keys are the new shape, nothing left in the old one
    let keys = |sql: &str| -> Vec<String> {
        let c = s.safe();
        let mut st = c.prepare(sql).unwrap();
        let rows = st.query_map([], |r| r.get(0)).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(
        keys("SELECT repo_key FROM repo_errors ORDER BY repo_key"),
        [
            "acc-bitbucket-default:*",
            "acc-bitbucket-default:bad/*",
            "bitbucket.org/acme/web"
        ]
    );
    assert_eq!(
        keys("SELECT DISTINCT repo_key FROM drafts ORDER BY repo_key"),
        ["bitbucket.org/acme/api", "bitbucket.org/acme/web"]
    );
    assert_eq!(
        keys("SELECT repo_key FROM repos ORDER BY repo_key"),
        [
            "bitbucket.org/acme/api",
            "bitbucket.org/acme/web",
            "bitbucket.org/other/mixed"
        ]
    );
    for table in [
        "prs",
        "diffstats",
        "viewed",
        "drafts",
        "repo_choices",
        "repos",
    ] {
        let odd = keys(&format!(
            "SELECT account_id FROM {table} WHERE account_id <> 'acc-bitbucket-default'"
        ));
        assert!(odd.is_empty(), "{table}: {odd:?}");
    }
    // the mixed-case repository keeps its display case
    assert!(s.list_repos().unwrap().contains(&repo("Other", "Mixed")));

    // and the upgraded cache keeps working
    s.set_viewed(&api, 1, "c.rs", "o9", true).unwrap();
    assert_eq!(s.viewed_map(&api, 1).unwrap().len(), 3);
    let mut d = ds[1].clone();
    d.text = "edited".into();
    assert_eq!(s.save_draft(&d).unwrap(), d.id);
    assert_eq!(
        s.list_drafts(&api, 1).unwrap()[1].reply_to,
        Some(ThreadId::from(77))
    );
    drop(s);
    // reopening does not migrate again
    let s = Store::open(&path).unwrap();
    assert_eq!(s.list_drafts(&api, 1).unwrap().len(), 2);
    assert_eq!(s.accounts().unwrap().len(), 1);
}

#[test]
fn v6_cache_without_sign_in_gets_no_account() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        for sql in [
            SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6,
        ] {
            c.execute_batch(sql).unwrap();
        }
        c.pragma_update(None, "user_version", 6).unwrap();
        c.execute(
            "INSERT INTO drafts (repo, pr_id, kind, text, source_commit)
             VALUES ('acme/api', 1, 'pr', 'kept', 'h')",
            [],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert!(s.accounts().unwrap().is_empty());
    assert_eq!(s.list_drafts(&repo("acme", "api"), 1).unwrap().len(), 1);
}

#[test]
fn v7_refuses_newer_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", 9).unwrap();
    }
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::NewerSchema {
            found: 9,
            supported: 8
        })
    ));
}

/// v8: drafts gain the first line (and its side) of a multi-line comment; existing
/// drafts are single-line.
#[test]
fn v7_drafts_gain_start_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    {
        let mut c = rusqlite::Connection::open(&path).unwrap();
        for sql in [
            SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6,
        ] {
            c.execute_batch(sql).unwrap();
        }
        let tx = c.transaction().unwrap();
        migrate_v7(&tx).unwrap();
        tx.pragma_update(None, "user_version", 7).unwrap();
        tx.commit().unwrap();
        c.execute(
            "INSERT INTO drafts (repo_key, pr_id, kind, path, line, side, text, source_commit)
             VALUES ('bitbucket.org/acme/api', 1, 'inline', 'a.rs', 3, 'new', 'kept', 'h')",
            [],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    let r = repo("acme", "api");
    let ds = s.list_drafts(&r, 1).unwrap();
    assert_eq!((ds.len(), ds[0].start), (1, None));
    let mut d = ds[0].clone();
    d.start = Some((Side::Old, 2));
    s.save_draft(&d).unwrap();
    let mut n = draft(&r, 1, "range");
    n.start = Some((Side::New, 10));
    s.save_draft(&n).unwrap();
    let starts: Vec<_> = s
        .list_drafts(&r, 1)
        .unwrap()
        .iter()
        .map(|d| d.start)
        .collect();
    assert_eq!(starts, [Some((Side::Old, 2)), Some((Side::New, 10))]);
    // a start with an unknown side is skipped like any corrupt row
    s.safe()
        .execute(
            "INSERT INTO drafts (repo_key, pr_id, kind, line, side, start_side, start_line,
                                 text, source_commit)
             VALUES ('bitbucket.org/acme/api', 1, 'inline', 4, 'new', 'up', 1, 't', 'c')",
            [],
        )
        .unwrap();
    assert_eq!(s.list_drafts(&r, 1).unwrap().len(), 2);
}

fn account(id: &str, forge: Forge, host: &str) -> Account {
    Account {
        id: AccountId(id.into()),
        forge,
        web_base: format!("https://{host}"),
        api_base: format!("https://api.{host}"),
        graphql_base: None,
        git_host: host.into(),
        user_id: "1".into(),
        login: "me".into(),
        display_name: "Me".into(),
        auth_kind: "pat".into(),
        enabled: true,
    }
}

#[test]
fn accounts_round_trip_and_enable() {
    let s = Store::open_in_memory().unwrap();
    let a = account("acc-gh", Forge::GitHub, "github.com");
    s.upsert_account(&a).unwrap();
    assert_eq!(s.accounts().unwrap(), vec![a.clone()]);
    s.set_account_enabled(&a.id, false).unwrap();
    assert!(!s.accounts().unwrap()[0].enabled);
    assert!(matches!(
        s.set_account_enabled(&AccountId("nope".into()), true),
        Err(StoreError::NotFound)
    ));
}

#[test]
fn same_repo_two_accounts_select_one_only() {
    let s = Store::open_in_memory().unwrap();
    let (a, b) = (AccountId("acc-a".into()), AccountId("acc-b".into()));
    let r = RepoRef::parse("github.com", "org/tool").unwrap();
    let info = [RepoInfo::from(r.clone())];
    s.record_discovery_for(&a, &info, &["org".into()], None)
        .unwrap();
    s.record_discovery_for(&b, &info, &["org".into()], None)
        .unwrap();
    assert_eq!(s.repo_choices().unwrap().len(), 2, "listed under both");
    s.set_selection_for(&a, std::slice::from_ref(&r)).unwrap();
    assert_eq!(
        s.selected_repos_by_account().unwrap(),
        vec![(a.clone(), r.clone())]
    );
    s.set_selection_for(&b, std::slice::from_ref(&r)).unwrap();
    assert_eq!(
        s.selected_repos_by_account().unwrap(),
        vec![(b.clone(), r.clone())]
    );
    // a's own selection call never touches b's other choices
    s.set_selection_for(&a, &[]).unwrap();
    assert_eq!(s.selected_repos_by_account().unwrap(), vec![(b, r)]);
}

#[test]
fn remove_account_purges_its_rows_keeps_drafts_marked() {
    let s = Store::open_in_memory().unwrap();
    let gone = account("acc-gl", Forge::GitLab, "gitlab.com");
    s.upsert_account(&gone).unwrap();
    let keep = Account::default_bitbucket("{me}".into(), "Me".into());
    s.upsert_account(&keep).unwrap();
    let mine = RepoRef::parse("gitlab.com", "grp/sub/proj").unwrap();
    let other = repo("acme", "api");
    s.record_discovery_for(&gone.id, &[RepoInfo::from(mine.clone())], &[], None)
        .unwrap();
    s.set_selection_for(&gone.id, std::slice::from_ref(&mine))
        .unwrap();
    s.set_selection(std::slice::from_ref(&other)).unwrap();
    let mut p = pr(&mine, 4, PrState::Open);
    p.account = gone.id.clone();
    s.replace_prs(&mine, &[p]).unwrap();
    s.replace_prs(&other, &[pr(&other, 1, PrState::Open)])
        .unwrap();
    // rows written for the removed account carry its id
    s.safe()
        .execute(
            "UPDATE prs SET account_id = 'acc-gl' WHERE repo_key = ?1",
            params![mine.key()],
        )
        .unwrap();
    s.set_sync_cursor(&gone.id, &mine, "open", &SyncCursorRow::default())
        .unwrap();
    let mut d = draft(&mine, 4, "keep me");
    d.kind = DraftKind::Pr;
    s.save_draft(&d).unwrap();
    s.safe()
        .execute("UPDATE drafts SET account_id = 'acc-gl'", [])
        .unwrap();

    let n = s.remove_account(&gone.id).unwrap();
    assert_eq!(
        n,
        RemovedCounts {
            prs: 1,
            diffstats: 0,
            repo_choices: 1,
            repo_errors: 0,
            cursors: 1,
            drafts_kept: 1,
        }
    );
    assert_eq!(s.accounts().unwrap(), vec![keep]);
    assert_eq!(s.selected_repos().unwrap(), vec![other.clone()]);
    assert_eq!(
        s.list_prs().unwrap().len(),
        1,
        "the other account's PR stays"
    );
    assert_eq!(s.list_drafts(&mine, 4).unwrap().len(), 1, "drafts are kept");
    assert!(s.sync_cursor(&gone.id, &mine, "open").unwrap().is_none());
}

#[test]
fn nested_namespace_round_trips() {
    let s = Store::open_in_memory().unwrap();
    let acc = AccountId("acc-gl".into());
    let r = RepoRef::parse("git.corp.example/gitlab", "Group/Sub/Proj").unwrap();
    s.record_discovery_for(&acc, &[RepoInfo::from(r.clone())], &[], None)
        .unwrap();
    s.set_selection_for(&acc, std::slice::from_ref(&r)).unwrap();
    assert_eq!(s.selected_repos().unwrap(), vec![r.clone()]);
    assert_eq!(s.repo_choices().unwrap()[0].repo, r);
    s.upsert_repos(std::slice::from_ref(&r)).unwrap();
    assert_eq!(s.list_repos().unwrap(), vec![r.clone()]);
    s.set_viewed(&r, 3, "x", "o", true).unwrap();
    assert_eq!(s.viewed_map(&r, 3).unwrap().len(), 1);
    // the same path on another host is another repository
    let bb = repo("Group/Sub", "Proj");
    assert!(s.viewed_map(&bb, 3).unwrap().is_empty());
}

#[test]
fn cursor_set_get() {
    let s = Store::open_in_memory().unwrap();
    let acc = AccountId("acc-gh".into());
    let r = RepoRef::parse("github.com", "o/r").unwrap();
    assert!(s.sync_cursor(&acc, &r, "open").unwrap().is_none());
    let c = SyncCursorRow {
        etag: Some("W/\"abc\"".into()),
        last_updated: Some("2026-11-13T10:00:00Z".into()),
    };
    s.set_sync_cursor(&acc, &r, "open", &c).unwrap();
    assert_eq!(s.sync_cursor(&acc, &r, "open").unwrap(), Some(c.clone()));
    assert!(s.sync_cursor(&acc, &r, "closed").unwrap().is_none());
    let other = AccountId("acc-other".into());
    assert!(s.sync_cursor(&other, &r, "open").unwrap().is_none());
    let c2 = SyncCursorRow {
        etag: None,
        last_updated: Some("2026-11-14T00:00:00Z".into()),
    };
    s.set_sync_cursor(&acc, &r, "open", &c2).unwrap();
    assert_eq!(s.sync_cursor(&acc, &r, "open").unwrap(), Some(c2));
}
