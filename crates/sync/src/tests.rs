use super::*;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use prcore::comments::{Comment, NewComment, PrTask, ReviewAction, ThreadId};
use prcore::model::RepoInfo;
use prcore::model::{DiffStat, PrState, PullRequest, UserRef};
use providers::provider::{CommentList, Discovered, PostRevision, ServerRevision};
use providers::ProviderError as PE;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

fn dup(e: &PE) -> PE {
    match e {
        PE::Auth => PE::Auth,
        PE::Forbidden => PE::Forbidden,
        PE::NotFound => PE::NotFound,
        PE::RateLimited => PE::RateLimited,
        PE::Http(c) => PE::Http(*c),
        PE::Network(s) => PE::Network(s.clone()),
        PE::Decode(s) => PE::Decode(s.clone()),
        PE::Invalid(s) => PE::Invalid(s.clone()),
        PE::PostedUnreadable => PE::PostedUnreadable,
        PE::Unconfirmed => PE::Unconfirmed,
        PE::Rejected { status, message } => PE::Rejected {
            status: *status,
            message: message.clone(),
        },
    }
}

fn repo(ws: &str, slug: &str) -> RepoRef {
    RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: ws.into(),
        slug: slug.into(),
    }
}

fn pr(r: &RepoRef, id: u64, state: PrState, commit: &str) -> PullRequest {
    let u = UserRef {
        id: "u".into(),
        display_name: "U".into(),
        login: None,
    };
    let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    PullRequest {
        repo: r.clone(),
        id,
        title: format!("pr {id}"),
        state,
        draft: false,
        author: u,
        source_branch: "feat".into(),
        dest_branch: "main".into(),
        source_commit: commit.into(),
        dest_commit: "d".into(),
        source_repo: r.clone(),
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

fn on(mut p: PullRequest, src: &str, dst: &str) -> PullRequest {
    p.source_branch = src.into();
    p.dest_branch = dst.into();
    p
}

fn stat(n: u32) -> DiffStat {
    DiffStat {
        files: n,
        lines_added: n,
        lines_removed: 0,
        max_file_lines: n,
    }
}

fn list(prs: Vec<PullRequest>) -> PrList {
    PrList {
        prs,
        skipped: vec![],
    }
}

fn skip(id: Option<u64>, field: &str) -> SkippedPr {
    SkippedPr {
        id,
        field: field.into(),
    }
}

#[derive(Default)]
struct Fake {
    workspaces: Mutex<Option<Result<Vec<String>, PE>>>,
    ws_repos: Mutex<HashMap<String, Result<Vec<RepoRef>, PE>>>,
    open: Mutex<HashMap<String, Result<PrList, PE>>>,
    merged_by_branch: Mutex<HashMap<(String, String), Result<PrList, PE>>>,
    merged_calls: Mutex<Vec<(String, String)>>,
    diffstat_err: Mutex<HashMap<(String, u64), PE>>,
    delay: Duration,
    /// When set, every diffstat call waits for a permit of this semaphore.
    diffstat_gate: Mutex<Option<Arc<Semaphore>>>,
    diffstat_calls: AtomicUsize,
    in_flight: AtomicUsize,
    high_water: AtomicUsize,
    /// Every provider call, as `kind:target` (`prs:w/a`, `diffstat:w/a#1`, `repos:w`).
    log: Mutex<Vec<String>>,
}

impl Fake {
    fn with_repos(ws: &str, slugs: &[&str]) -> Fake {
        let f = Fake::default();
        *f.workspaces.lock().unwrap() = Some(Ok(vec![ws.to_string()]));
        f.ws_repos.lock().unwrap().insert(
            ws.to_string(),
            Ok(slugs.iter().map(|s| repo(ws, s)).collect()),
        );
        f
    }
    fn set_open(&self, r: &RepoRef, v: Result<Vec<PullRequest>, PE>) {
        self.set_open_list(r, v.map(list));
    }
    fn set_merged(&self, r: &RepoRef, branch: &str, v: Result<Vec<PullRequest>, PE>) {
        self.set_merged_list(r, branch, v.map(list));
    }
    fn set_open_list(&self, r: &RepoRef, v: Result<PrList, PE>) {
        self.open.lock().unwrap().insert(r.full_name(), v);
    }
    fn set_merged_list(&self, r: &RepoRef, branch: &str, v: Result<PrList, PE>) {
        self.merged_by_branch
            .lock()
            .unwrap()
            .insert((r.full_name(), branch.to_string()), v);
    }
    fn merged_calls(&self) -> Vec<(String, String)> {
        let mut v = self.merged_calls.lock().unwrap().clone();
        v.sort();
        v
    }
    fn note(&self, call: String) {
        self.log.lock().unwrap().push(call);
    }
    fn calls(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    /// Every repository the fake would list.
    fn listable(&self) -> Vec<RepoRef> {
        let mut v: Vec<RepoRef> = self
            .ws_repos
            .lock()
            .unwrap()
            .values()
            .filter_map(|r| r.as_ref().ok())
            .flatten()
            .cloned()
            .collect();
        v.sort_by_key(|r| r.full_name());
        v
    }
}

#[async_trait]
impl Provider for Fake {
    fn forge(&self) -> prcore::model::Forge {
        prcore::model::Forge::BitbucketCloud
    }
    fn capabilities(&self) -> providers::provider::Capabilities {
        providers::provider::Capabilities::BITBUCKET
    }
    async fn me(&self) -> Result<UserRef, PE> {
        unimplemented!()
    }
    /// Bitbucket's two-level walk over the fake's workspaces.
    async fn discover(&self) -> Result<Discovered, PE> {
        self.note("workspaces".into());
        let namespaces = match self.workspaces.lock().unwrap().as_ref() {
            Some(Ok(v)) => v.clone(),
            Some(Err(e)) => return Err(dup(e)),
            None => vec![],
        };
        let mut out = Discovered::default();
        for ws in &namespaces {
            self.note(format!("repos:{ws}"));
            match self.ws_repos.lock().unwrap().get(ws) {
                Some(Ok(v)) => out.repos.extend(v.iter().cloned().map(RepoInfo::from)),
                Some(Err(PE::Auth)) => return Err(PE::Auth),
                Some(Err(e)) => out.failed.push((ws.clone(), dup(e))),
                None => {}
            }
        }
        out.namespaces = namespaces;
        Ok(out)
    }
    async fn pr(&self, _: &RepoRef, _: u64) -> Result<PullRequest, PE> {
        unimplemented!()
    }
    async fn revision(&self, _: &RepoRef, _: u64) -> Result<ServerRevision, PE> {
        unimplemented!()
    }
    async fn comments(&self, _: &RepoRef, _: u64) -> Result<CommentList, PE> {
        unimplemented!()
    }
    async fn tasks(&self, _: &RepoRef, _: u64) -> Result<Vec<PrTask>, PE> {
        unimplemented!()
    }
    async fn post_comment(
        &self,
        _: &RepoRef,
        _: u64,
        _: &PostRevision,
        _: &NewComment,
    ) -> Result<Comment, PE> {
        unimplemented!()
    }
    async fn set_resolved(&self, _: &RepoRef, _: u64, _: &ThreadId, _: bool) -> Result<(), PE> {
        unimplemented!()
    }
    async fn set_review(
        &self,
        _: &RepoRef,
        _: u64,
        _: ReviewAction,
        _: Option<&str>,
    ) -> Result<(), PE> {
        unimplemented!()
    }
    async fn prs(&self, r: &RepoRef, state: PrState, _max: usize) -> Result<PrList, PE> {
        self.note(format!("prs:{}", r.full_name()));
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.high_water.fetch_max(now, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(state, PrState::Open, "sync must only list Open PRs");
        match self.open.lock().unwrap().get(&r.full_name()) {
            Some(Ok(v)) => Ok(v.clone()),
            Some(Err(e)) => Err(dup(e)),
            None => Ok(PrList::default()),
        }
    }
    async fn merged_by_source_branch(&self, r: &RepoRef, branch: &str) -> Result<PrList, PE> {
        self.note(format!("merged:{}", r.full_name()));
        self.merged_calls
            .lock()
            .unwrap()
            .push((r.full_name(), branch.to_string()));
        match self
            .merged_by_branch
            .lock()
            .unwrap()
            .get(&(r.full_name(), branch.to_string()))
        {
            Some(Ok(v)) => Ok(v.clone()),
            Some(Err(e)) => Err(dup(e)),
            None => Ok(PrList::default()),
        }
    }
    async fn diffstat(&self, r: &RepoRef, id: u64) -> Result<DiffStat, PE> {
        self.note(format!("diffstat:{}#{id}", r.full_name()));
        self.diffstat_calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.diffstat_gate.lock().unwrap().clone();
        if let Some(g) = gate {
            g.acquire().await.unwrap().forget();
        }
        if let Some(e) = self.diffstat_err.lock().unwrap().get(&(r.full_name(), id)) {
            return Err(dup(e));
        }
        Ok(stat(3))
    }
}

fn new_store() -> Arc<Mutex<Store>> {
    Arc::new(Mutex::new(Store::open_in_memory().unwrap()))
}

async fn run(
    p: &Arc<Fake>,
    s: &Arc<Mutex<Store>>,
    opts: SyncOptions,
) -> (Result<(), PE>, Vec<SyncEvent>) {
    // the user selected every repository the fake can list
    s.lock().unwrap().set_selection(&p.listable()).unwrap();
    run_selected(p, s, opts).await
}

/// A sync of whatever is selected in `s`.
async fn run_selected(
    p: &Arc<Fake>,
    s: &Arc<Mutex<Store>>,
    opts: SyncOptions,
) -> (Result<(), PE>, Vec<SyncEvent>) {
    let (tx, rx) = unbounded_channel();
    let res = run_sync(p.clone(), s.clone(), opts, tx).await;
    (res, drain(rx))
}

fn drain(mut rx: UnboundedReceiver<SyncEvent>) -> Vec<SyncEvent> {
    let mut v = vec![];
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}

#[tokio::test]
async fn syncs_every_selected_repo() {
    let f = Fake::default();
    *f.workspaces.lock().unwrap() = Some(Ok(vec!["w1".into(), "w2".into()]));
    f.ws_repos
        .lock()
        .unwrap()
        .insert("w1".into(), Ok(vec![repo("w1", "a"), repo("w1", "b")]));
    f.ws_repos
        .lock()
        .unwrap()
        .insert("w2".into(), Ok(vec![repo("w2", "c")]));
    f.set_open(
        &repo("w1", "a"),
        Ok(vec![pr(&repo("w1", "a"), 1, PrState::Open, "c1")]),
    );
    f.set_open(
        &repo("w2", "c"),
        Ok(vec![pr(&repo("w2", "c"), 2, PrState::Open, "c2")]),
    );
    let p = Arc::new(f);
    let s = new_store();
    let (res, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    let st = s.lock().unwrap();
    assert_eq!(st.selected_repos().unwrap().len(), 3);
    assert_eq!(st.list_prs().unwrap().len(), 2);
    assert!(st.repo_errors().unwrap().is_empty());
    assert!(ev.contains(&SyncEvent::Discovered { repos: 3 }));
    assert!(ev.contains(&SyncEvent::RepoDone {
        repo: repo("w1", "a"),
        open: 1
    }));
    assert!(ev.contains(&SyncEvent::DiffstatDone {
        repo: repo("w1", "a"),
        id: 1
    }));
    assert!(st.get_diffstat(&repo("w1", "a"), 1).unwrap().is_some());
}

#[tokio::test]
async fn repo_403_is_recorded_and_others_continue() {
    let f = Fake::with_repos("w", &["a", "b"]);
    f.set_open(&repo("w", "a"), Err(PE::Forbidden));
    f.set_open(
        &repo("w", "b"),
        Ok(vec![pr(&repo("w", "b"), 1, PrState::Open, "c")]),
    );
    let p = Arc::new(f);
    let s = new_store();
    let (res, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    let st = s.lock().unwrap();
    assert_eq!(
        st.repo_errors().unwrap(),
        vec![(repo("w", "a"), "access denied".to_string())]
    );
    assert_eq!(st.list_prs().unwrap().len(), 1);
    assert!(ev.contains(&SyncEvent::RepoFailed {
        repo: repo("w", "a"),
        error: "access denied".into()
    }));
}

#[tokio::test]
async fn workspace_403_is_recorded_and_others_are_discovered() {
    let f = Fake::default();
    *f.workspaces.lock().unwrap() = Some(Ok(vec!["bad".into(), "good".into()]));
    f.ws_repos
        .lock()
        .unwrap()
        .insert("bad".into(), Err(PE::Forbidden));
    f.ws_repos
        .lock()
        .unwrap()
        .insert("good".into(), Ok(vec![repo("good", "a")]));
    let s = new_store();
    let found = run_discovery(&f, &s).await.unwrap();
    assert_eq!(
        found,
        Discovery {
            repos: 1,
            failed_workspaces: vec!["bad".into()]
        }
    );
    let st = s.lock().unwrap();
    assert_eq!(
        st.repo_errors().unwrap(),
        vec![(repo("bad", "*"), "access denied".to_string())]
    );
    let known: Vec<RepoRef> = st
        .repo_choices()
        .unwrap()
        .into_iter()
        .map(|c| c.repo)
        .collect();
    assert_eq!(known, vec![repo("good", "a")]);
}

#[tokio::test]
async fn discovery_lists_repos_and_makes_no_pr_calls() {
    let f = Fake::default();
    *f.workspaces.lock().unwrap() = Some(Ok(vec!["w1".into(), "w2".into()]));
    f.ws_repos
        .lock()
        .unwrap()
        .insert("w1".into(), Ok(vec![repo("w1", "a"), repo("w1", "b")]));
    f.ws_repos
        .lock()
        .unwrap()
        .insert("w2".into(), Ok(vec![repo("w2", "c")]));
    f.set_open(
        &repo("w1", "a"),
        Ok(vec![pr(&repo("w1", "a"), 1, PrState::Open, "c1")]),
    );
    let s = new_store();
    let found = run_discovery(&f, &s).await.unwrap();
    assert_eq!(found.repos, 3);
    assert_eq!(f.calls(), vec!["workspaces", "repos:w1", "repos:w2"]);
    let st = s.lock().unwrap();
    let choices = st.repo_choices().unwrap();
    assert_eq!(choices.len(), 3);
    // discovery never selects anything, so nothing is synced until the user picks
    assert!(choices.iter().all(|c| !c.selected));
    assert!(st.selected_repos().unwrap().is_empty());
    assert!(st.list_prs().unwrap().is_empty());
}

#[tokio::test]
async fn discovery_keeps_the_selection_and_marks_vanished_repos() {
    let f = Fake::with_repos("w", &["a", "b"]);
    let s = new_store();
    run_discovery(&f, &s).await.unwrap();
    s.lock().unwrap().set_selection(&[repo("w", "a")]).unwrap();
    f.ws_repos
        .lock()
        .unwrap()
        .insert("w".into(), Ok(vec![repo("w", "b")]));
    run_discovery(&f, &s).await.unwrap();
    let st = s.lock().unwrap();
    let c = st.repo_choices().unwrap();
    assert_eq!(c.len(), 2);
    assert!(c[0].selected && c[0].gone, "{c:?}");
    assert!(!c[1].selected && !c[1].gone);
    assert!(st.selected_repos().unwrap().is_empty());
}

#[tokio::test]
async fn a_run_covers_its_accounts_repos_and_owns_what_it_writes() {
    let (mine, theirs) = (repo("w", "a"), repo("v", "c"));
    let f = Fake::default();
    f.set_open(&mine, Ok(vec![pr(&mine, 1, PrState::Open, "c1")]));
    f.set_open(&theirs, Ok(vec![pr(&theirs, 2, PrState::Open, "c2")]));
    *f.workspaces.lock().unwrap() = Some(Ok(vec!["w".into()]));
    f.ws_repos
        .lock()
        .unwrap()
        .insert("w".into(), Err(PE::Forbidden));
    let p = Arc::new(f);
    let s = new_store();
    let second = AccountId("acc-second".into());
    {
        let st = s.lock().unwrap();
        st.set_selection(std::slice::from_ref(&theirs)).unwrap();
        st.set_selection_for(&second, std::slice::from_ref(&mine))
            .unwrap();
    }
    let opts = SyncOptions {
        account: second.clone(),
        ..Default::default()
    };
    let (res, _) = run_selected(&p, &s, opts).await;
    assert_eq!(res, Ok(()));
    assert!(
        !p.calls().iter().any(|c| c.contains("v/c")),
        "the default account's repository is not this run's: {:?}",
        p.calls()
    );
    // listing errors land on the account's own pseudo rows
    run_discovery_for(p.as_ref(), &s, &second).await.unwrap();
    let st = s.lock().unwrap();
    let snap = st.cache_snapshot().unwrap();
    let ids: Vec<(u64, AccountId)> = snap
        .prs
        .iter()
        .map(|(p, _)| (p.id, p.account.clone()))
        .collect();
    assert_eq!(ids, vec![(1, second.clone())], "stamped with its account");
    assert!(st.get_diffstat(&mine, 1).unwrap().is_some());
    assert_eq!(
        snap.errors,
        vec![(repo("w", "*"), "access denied".to_string())]
    );
    // removing the account takes exactly its rows
    let removed = st.remove_account(&second).unwrap();
    assert_eq!((removed.prs, removed.repo_errors), (1, 1));
    assert!(st.cache_snapshot().unwrap().errors.is_empty());
}

#[tokio::test]
async fn sync_touches_only_selected_repos() {
    let f = Fake::default();
    *f.workspaces.lock().unwrap() = Some(Ok(vec!["w".into(), "v".into()]));
    f.ws_repos
        .lock()
        .unwrap()
        .insert("w".into(), Ok(vec![repo("w", "a"), repo("w", "b")]));
    f.ws_repos
        .lock()
        .unwrap()
        .insert("v".into(), Ok(vec![repo("v", "c")]));
    for (r, id) in [
        (repo("w", "a"), 1),
        (repo("w", "b"), 2),
        (repo("v", "c"), 3),
    ] {
        let open = on(pr(&r, id, PrState::Open, "c"), "child", "parent");
        f.set_open(&r, Ok(vec![open]));
    }
    let p = Arc::new(f);
    let s = new_store();
    run_discovery(p.as_ref(), &s).await.unwrap();
    // an unselected repo's PRs from an earlier selection stay cached, untouched
    let b = repo("w", "b");
    let old = pr(&b, 9, PrState::Open, "old");
    s.lock()
        .unwrap()
        .replace_prs(&b, std::slice::from_ref(&old))
        .unwrap();
    s.lock().unwrap().set_selection(&[repo("w", "a")]).unwrap();
    p.log.lock().unwrap().clear();
    let (res, ev) = run_selected(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    assert_eq!(
        p.calls(),
        vec!["prs:w/a", "merged:w/a", "diffstat:w/a#1"],
        "no listing and no call for an unselected repo"
    );
    assert!(ev.contains(&SyncEvent::Discovered { repos: 1 }));
    let st = s.lock().unwrap();
    let mut stored: Vec<(String, u64)> = st
        .list_prs()
        .unwrap()
        .iter()
        .map(|p| (p.repo.full_name(), p.id))
        .collect();
    stored.sort();
    assert_eq!(stored, vec![("w/a".into(), 1), ("w/b".into(), 9)]);
    // the inbox shows the selected repo only
    let shown: Vec<u64> = st
        .cache_snapshot()
        .unwrap()
        .prs
        .iter()
        .map(|(p, _)| p.id)
        .collect();
    assert_eq!(shown, vec![1]);
}

#[tokio::test]
async fn nothing_selected_syncs_nothing() {
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &repo("w", "a"),
        Ok(vec![pr(&repo("w", "a"), 1, PrState::Open, "c")]),
    );
    let p = Arc::new(f);
    let s = new_store();
    run_discovery(p.as_ref(), &s).await.unwrap();
    p.log.lock().unwrap().clear();
    let (res, ev) = run_selected(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    assert!(p.calls().is_empty());
    assert_eq!(
        ev,
        vec![
            SyncEvent::Started,
            SyncEvent::Discovered { repos: 0 },
            SyncEvent::Finished
        ]
    );
}

#[tokio::test]
async fn zero_repos_and_zero_prs_finish_cleanly() {
    let p = Arc::new(Fake::default());
    let s = new_store();
    let (res, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    assert_eq!(
        ev,
        vec![
            SyncEvent::Started,
            SyncEvent::Discovered { repos: 0 },
            SyncEvent::Finished
        ]
    );

    let p = Arc::new(Fake::with_repos("w", &["a"]));
    let (res, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    assert!(ev.contains(&SyncEvent::RepoDone {
        repo: repo("w", "a"),
        open: 0
    }));
    assert_eq!(ev.last(), Some(&SyncEvent::Finished));
}

#[tokio::test]
async fn auth_error_from_workspaces_aborts_the_discovery() {
    let f = Fake::default();
    *f.workspaces.lock().unwrap() = Some(Err(PE::Auth));
    let s = new_store();
    assert_eq!(run_discovery(&f, &s).await, Err(DiscoveryError::Auth));
    let f = Fake::default();
    *f.workspaces.lock().unwrap() = Some(Ok(vec!["w".into()]));
    f.ws_repos.lock().unwrap().insert("w".into(), Err(PE::Auth));
    assert_eq!(run_discovery(&f, &s).await, Err(DiscoveryError::Auth));
    assert!(s.lock().unwrap().repo_choices().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn auth_error_mid_run_aborts() {
    let slugs: Vec<String> = (0..10).map(|i| format!("r{i}")).collect();
    let refs: Vec<&str> = slugs.iter().map(|s| s.as_str()).collect();
    let mut f = Fake::with_repos("w", &refs);
    f.delay = Duration::from_millis(10);
    f.set_open(&repo("w", "r0"), Err(PE::Auth));
    let p = Arc::new(f);
    let s = new_store();
    let opts = SyncOptions {
        concurrency: 1,
        ..Default::default()
    };
    let (res, ev) = run(&p, &s, opts).await;
    assert_eq!(res, Err(PE::Auth));
    assert_eq!(ev.last(), Some(&SyncEvent::Finished));
    let done = ev
        .iter()
        .filter(|e| matches!(e, SyncEvent::RepoDone { .. }))
        .count();
    assert!(done < 9, "expected abort, got {done} RepoDone");
    assert!(s.lock().unwrap().repo_errors().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn concurrency_never_exceeds_limit() {
    let slugs: Vec<String> = (0..12).map(|i| format!("r{i}")).collect();
    let refs: Vec<&str> = slugs.iter().map(|s| s.as_str()).collect();
    let mut f = Fake::with_repos("w", &refs);
    f.delay = Duration::from_millis(50);
    let p = Arc::new(f);
    let s = new_store();
    let opts = SyncOptions {
        concurrency: 3,
        ..Default::default()
    };
    let (res, _) = run(&p, &s, opts).await;
    assert_eq!(res, Ok(()));
    assert_eq!(p.high_water.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn zero_concurrency_is_treated_as_one() {
    let p = Arc::new(Fake::with_repos("w", &["a", "b"]));
    let s = new_store();
    let opts = SyncOptions {
        concurrency: 0,
        ..Default::default()
    };
    let (res, ev) = run(&p, &s, opts).await;
    assert_eq!(res, Ok(()));
    assert!(ev.contains(&SyncEvent::RepoDone {
        repo: repo("w", "b"),
        open: 0
    }));
    assert_eq!(ev.last(), Some(&SyncEvent::Finished));
}

#[tokio::test]
async fn diffstat_skipped_when_source_commit_unchanged() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(&r, Ok(vec![pr(&r, 1, PrState::Open, "c1")]));
    let p = Arc::new(f);
    let s = new_store();
    s.lock()
        .unwrap()
        .put_diffstat(&r, 1, "c1", &stat(9))
        .unwrap();
    let (res, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    assert_eq!(p.diffstat_calls.load(Ordering::SeqCst), 0);
    assert!(!ev
        .iter()
        .any(|e| matches!(e, SyncEvent::DiffstatDone { .. })));
}

#[tokio::test]
async fn diffstat_refetched_when_source_commit_changed() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(&r, Ok(vec![pr(&r, 1, PrState::Open, "c2")]));
    let p = Arc::new(f);
    let s = new_store();
    s.lock()
        .unwrap()
        .put_diffstat(&r, 1, "c1", &stat(9))
        .unwrap();
    let (_, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(p.diffstat_calls.load(Ordering::SeqCst), 1);
    assert!(ev.contains(&SyncEvent::DiffstatDone {
        repo: r.clone(),
        id: 1
    }));
    let got = s.lock().unwrap().get_diffstat(&r, 1).unwrap().unwrap();
    assert_eq!(got, ("c2".to_string(), stat(3)));
}

#[tokio::test]
async fn diffstats_disabled_by_option() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(&r, Ok(vec![pr(&r, 1, PrState::Open, "c")]));
    let p = Arc::new(f);
    let s = new_store();
    let opts = SyncOptions {
        fetch_diffstats: false,
        ..Default::default()
    };
    let (res, _) = run(&p, &s, opts).await;
    assert_eq!(res, Ok(()));
    assert_eq!(p.diffstat_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn diffstat_failure_does_not_fail_repo_but_auth_aborts() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &r,
        Ok(vec![
            pr(&r, 1, PrState::Open, "c"),
            pr(&r, 2, PrState::Open, "c"),
        ]),
    );
    f.diffstat_err
        .lock()
        .unwrap()
        .insert((r.full_name(), 1), PE::Http(500));
    let p = Arc::new(f);
    let s = new_store();
    let (res, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    assert!(s.lock().unwrap().repo_errors().unwrap().is_empty());
    assert!(!ev.contains(&SyncEvent::DiffstatDone {
        repo: r.clone(),
        id: 1
    }));
    assert!(ev.contains(&SyncEvent::DiffstatDone {
        repo: r.clone(),
        id: 2
    }));

    p.diffstat_err
        .lock()
        .unwrap()
        .insert((r.full_name(), 1), PE::Auth);
    let (res, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Err(PE::Auth));
    assert_eq!(ev.last(), Some(&SyncEvent::Finished));
}

#[tokio::test]
async fn merged_parent_stored_and_marks_child_parent_merged() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &r,
        Ok(vec![on(pr(&r, 2, PrState::Open, "c2"), "child", "parent")]),
    );
    f.set_merged(
        &r,
        "parent",
        Ok(vec![on(pr(&r, 1, PrState::Merged, "c1"), "parent", "main")]),
    );
    let p = Arc::new(f);
    let s = new_store();
    let (_, ev) = run(&p, &s, SyncOptions::default()).await;
    let prs = s.lock().unwrap().list_prs().unwrap();
    assert_eq!(prs.len(), 2);
    assert!(prs.iter().any(|p| p.id == 1 && p.state == PrState::Merged));
    assert!(ev.contains(&SyncEvent::RepoDone {
        repo: r.clone(),
        open: 1
    }));
    // only the open PR gets a diffstat
    assert_eq!(p.diffstat_calls.load(Ordering::SeqCst), 1);
    // the existing advice path still fires
    let stacks = prcore::stacks::detect_stacks(&prs);
    assert_eq!(stacks.len(), 1);
    let advice = prcore::advice::advise(&stacks[0], &prs, &HashMap::new());
    assert!(
        advice.contains(&prcore::advice::Advice::ParentMergedRetarget {
            parent: 1,
            child: 2
        }),
        "{advice:?}"
    );
}

#[tokio::test]
async fn no_merged_calls_when_every_target_is_an_open_source() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &r,
        Ok(vec![
            on(pr(&r, 1, PrState::Open, "c1"), "parent", "main"),
            on(pr(&r, 2, PrState::Open, "c2"), "child", "parent"),
            on(pr(&r, 3, PrState::Open, "c3"), "main", "release"),
            on(pr(&r, 4, PrState::Open, "c4"), "release", "main"),
        ]),
    );
    let p = Arc::new(f);
    let s = new_store();
    let (res, _) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    assert_eq!(p.merged_calls(), vec![]);
    assert_eq!(s.lock().unwrap().list_prs().unwrap().len(), 4);
}

#[tokio::test]
async fn merged_looked_up_once_per_distinct_missing_target() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &r,
        Ok(vec![
            on(pr(&r, 1, PrState::Open, "c1"), "x", "main"),
            on(pr(&r, 2, PrState::Open, "c2"), "y", "main"),
            on(pr(&r, 3, PrState::Open, "c3"), "z", "gone"),
            on(pr(&r, 4, PrState::Open, "c4"), "w", "x"),
        ]),
    );
    let p = Arc::new(f);
    let s = new_store();
    let _ = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(
        p.merged_calls(),
        vec![
            ("w/a".to_string(), "gone".to_string()),
            ("w/a".to_string(), "main".to_string())
        ]
    );
}

#[tokio::test]
async fn over_returned_merged_prs_are_filtered_by_source_branch() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &r,
        Ok(vec![on(pr(&r, 2, PrState::Open, "c2"), "child", "parent")]),
    );
    // A lookup that returns extra merged PRs (other source branches) keeps only the target's.
    f.set_merged(
        &r,
        "parent",
        Ok(vec![
            on(pr(&r, 1, PrState::Merged, "c1"), "parent", "main"),
            on(pr(&r, 9, PrState::Merged, "c9"), "other", "main"),
        ]),
    );
    let p = Arc::new(f);
    let s = new_store();
    let _ = run(&p, &s, SyncOptions::default()).await;
    let mut ids: Vec<u64> = s
        .lock()
        .unwrap()
        .list_prs()
        .unwrap()
        .iter()
        .map(|p| p.id)
        .collect();
    ids.sort();
    assert_eq!(ids, vec![1, 2]);
    assert_eq!(p.merged_calls().len(), 1);
}

#[tokio::test]
async fn merged_lookup_failure_keeps_open_prs() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &r,
        Ok(vec![on(pr(&r, 2, PrState::Open, "c2"), "child", "parent")]),
    );
    f.set_merged(
        &r,
        "parent",
        Ok(vec![on(pr(&r, 1, PrState::Merged, "c1"), "parent", "main")]),
    );
    let p = Arc::new(f);
    let s = new_store();
    let _ = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(s.lock().unwrap().list_prs().unwrap().len(), 2);

    p.set_merged(&r, "parent", Err(PE::Network("boom secret".into())));
    let (res, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    let st = s.lock().unwrap();
    // the open PR and the previously stored targeted merged PR both survive
    let prs = st.list_prs().unwrap();
    assert!(prs.iter().any(|p| p.id == 2 && p.state == PrState::Open));
    assert!(prs.iter().any(|p| p.id == 1 && p.state == PrState::Merged));
    assert_eq!(
        st.repo_errors().unwrap(),
        vec![(r.clone(), "network error".to_string())]
    );
    assert!(ev.contains(&SyncEvent::RepoFailed {
        repo: r,
        error: "network error".into()
    }));
}

fn stored_ids(s: &Arc<Mutex<Store>>) -> Vec<u64> {
    let mut v: Vec<u64> = s
        .lock()
        .unwrap()
        .list_prs()
        .unwrap()
        .iter()
        .map(|p| p.id)
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn stale_untargeted_merged_pr_dropped_on_success() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &r,
        Ok(vec![on(pr(&r, 2, PrState::Open, "c2"), "child", "old")]),
    );
    f.set_merged(
        &r,
        "old",
        Ok(vec![on(pr(&r, 1, PrState::Merged, "c1"), "old", "main")]),
    );
    let p = Arc::new(f);
    let s = new_store();
    let _ = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(stored_ids(&s), vec![1, 2]);

    // The child is retargeted: "old" is no longer a target.
    p.set_open(
        &r,
        Ok(vec![on(pr(&r, 2, PrState::Open, "c2"), "child", "main")]),
    );
    let _ = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(stored_ids(&s), vec![2]);
}

#[tokio::test]
async fn stale_untargeted_merged_pr_dropped_when_lookup_fails() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &r,
        Ok(vec![on(pr(&r, 2, PrState::Open, "c2"), "child", "old")]),
    );
    f.set_merged(
        &r,
        "old",
        Ok(vec![on(pr(&r, 1, PrState::Merged, "c1"), "old", "main")]),
    );
    let p = Arc::new(f);
    let s = new_store();
    let _ = run(&p, &s, SyncOptions::default()).await;

    p.set_open(
        &r,
        Ok(vec![on(pr(&r, 2, PrState::Open, "c2"), "child", "other")]),
    );
    p.set_merged(&r, "other", Err(PE::Http(500)));
    let _ = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(stored_ids(&s), vec![2]);
}

#[tokio::test]
async fn partial_failure_keeps_stored_pr_only_for_failed_target() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &r,
        Ok(vec![
            on(pr(&r, 10, PrState::Open, "c"), "c1", "pa"),
            on(pr(&r, 11, PrState::Open, "c"), "c2", "pb"),
        ]),
    );
    f.set_merged(
        &r,
        "pa",
        Ok(vec![on(pr(&r, 1, PrState::Merged, "c"), "pa", "main")]),
    );
    f.set_merged(
        &r,
        "pb",
        Ok(vec![on(pr(&r, 2, PrState::Merged, "c"), "pb", "main")]),
    );
    let p = Arc::new(f);
    let s = new_store();
    let _ = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(stored_ids(&s), vec![1, 2, 10, 11]);

    // "pa" now yields a different merged PR; "pb" fails.
    p.set_merged(
        &r,
        "pa",
        Ok(vec![on(pr(&r, 3, PrState::Merged, "c"), "pa", "main")]),
    );
    p.set_merged(&r, "pb", Err(PE::Http(500)));
    let _ = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(stored_ids(&s), vec![2, 3, 10, 11]);
}

#[tokio::test]
async fn merged_lookup_auth_error_aborts() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(
        &r,
        Ok(vec![on(pr(&r, 2, PrState::Open, "c2"), "child", "parent")]),
    );
    f.set_merged(&r, "parent", Err(PE::Auth));
    let p = Arc::new(f);
    let s = new_store();
    let (res, _) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Err(PE::Auth));
}

#[tokio::test]
async fn skipped_prs_are_recorded_and_good_ones_stored() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open_list(
        &r,
        Ok(PrList {
            prs: vec![pr(&r, 1, PrState::Open, "c1")],
            skipped: vec![skip(Some(2), "author.uuid")],
        }),
    );
    f.set_merged_list(
        &r,
        "main",
        Ok(PrList {
            prs: vec![],
            skipped: vec![skip(None, "id")],
        }),
    );
    let p = Arc::new(f);
    let s = new_store();
    let (res, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    let st = s.lock().unwrap();
    assert_eq!(st.list_prs().unwrap().len(), 1);
    let note = "2 PRs skipped: decode (#2 field: author.uuid; field: id)".to_string();
    assert_eq!(st.repo_errors().unwrap(), vec![(r.clone(), note.clone())]);
    assert!(ev.contains(&SyncEvent::RepoFailed {
        repo: r,
        error: note
    }));
}

#[test]
fn skip_note_names_ids_and_paths_and_caps_the_list() {
    assert_eq!(
        skip_note(&[skip(Some(5), "source.branch.name")]),
        "1 PR skipped: decode (#5 field: source.branch.name)"
    );
    let many: Vec<SkippedPr> = (1..=5).map(|i| skip(Some(i), "title")).collect();
    assert_eq!(
        skip_note(&many),
        "5 PRs skipped: decode (#1 field: title; #2 field: title; #3 field: title; ...)"
    );
}

#[tokio::test]
async fn every_open_entry_skipped_keeps_old_data() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(&r, Ok(vec![pr(&r, 1, PrState::Open, "c")]));
    let p = Arc::new(f);
    let s = new_store();
    let _ = run(&p, &s, SyncOptions::default()).await;
    p.set_open_list(
        &r,
        Ok(PrList {
            prs: vec![],
            skipped: vec![skip(Some(1), "title"), skip(Some(3), "id")],
        }),
    );
    let (res, _) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(res, Ok(()));
    let st = s.lock().unwrap();
    assert_eq!(st.list_prs().unwrap().len(), 1);
    assert_eq!(
        st.repo_errors().unwrap(),
        vec![(
            r,
            "2 PRs skipped: decode (#1 field: title; #3 field: id)".to_string()
        )]
    );
}

/// The audit's sync bench (section 1.5): 100 repos x 5 PRs, a provider that answers at
/// once, a file store, the shipped runtime shape. Run with
/// `cargo test -p sync --release -- --ignored --nocapture bench_sync_file_store`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn bench_sync_file_store() {
    let dir = std::env::var_os("NIT_BENCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let tmp = tempfile::tempdir_in(dir).unwrap();
    let slugs: Vec<String> = (0..100).map(|i| format!("r{i}")).collect();
    let refs: Vec<&str> = slugs.iter().map(|s| s.as_str()).collect();
    let f = Fake::with_repos("w", &refs);
    for s in &slugs {
        let r = repo("w", s);
        let prs = (1..=5)
            .map(|i| pr(&r, i, PrState::Open, &format!("c{i}")))
            .collect();
        f.set_open(&r, Ok(prs));
    }
    let p = Arc::new(f);
    let s = Arc::new(Mutex::new(
        Store::open(&tmp.path().join("cache.sqlite")).unwrap(),
    ));
    for label in ["first sync", "second sync (nothing changed)"] {
        let t = std::time::Instant::now();
        let (res, _) = run(&p, &s, SyncOptions::default()).await;
        assert_eq!(res, Ok(()));
        eprintln!(
            "{label}: {:.1} ms ({} PRs) in {}",
            t.elapsed().as_secs_f64() * 1000.0,
            s.lock().unwrap().list_prs().unwrap().len(),
            tmp.path().display()
        );
    }
}

/// A sync of one repo with one open PR whose diffstat waits for `gate`, running on its own
/// task; returns once that diffstat was asked for.
async fn sync_held_at_diffstat(
    s: &Arc<Mutex<Store>>,
) -> (Arc<Semaphore>, tokio::task::JoinHandle<Vec<SyncEvent>>) {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(&r, Ok(vec![pr(&r, 1, PrState::Open, "c1")]));
    let gate = Arc::new(Semaphore::new(0));
    *f.diffstat_gate.lock().unwrap() = Some(gate.clone());
    let p = Arc::new(f);
    let (p2, s2) = (p.clone(), s.clone());
    let task = tokio::spawn(async move { run(&p2, &s2, SyncOptions::default()).await.1 });
    while p.diffstat_calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (gate, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prs_are_stored_before_the_diffstats_finish() {
    let r = repo("w", "a");
    let s = new_store();
    let (gate, task) = sync_held_at_diffstat(&s).await;
    {
        let st = s.lock().unwrap();
        assert_eq!(
            st.list_prs().unwrap().len(),
            1,
            "PR list waits for diffstats"
        );
        assert_eq!(st.get_diffstat(&r, 1).unwrap(), None);
    }
    gate.add_permits(1);
    let ev = task.await.unwrap();
    let done = ev
        .iter()
        .position(|e| matches!(e, SyncEvent::RepoDone { .. }));
    let diffstat = ev
        .iter()
        .position(|e| matches!(e, SyncEvent::DiffstatDone { .. }));
    assert!(done.unwrap() < diffstat.unwrap(), "{ev:?}");
    let got = s.lock().unwrap().get_diffstat(&r, 1).unwrap();
    assert_eq!(got, Some(("c1".to_string(), stat(3))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sync_aborted_during_diffstats_keeps_the_pr_list() {
    let s = new_store();
    let (_gate, task) = sync_held_at_diffstat(&s).await;
    task.abort();
    let _ = task.await;
    let st = s.lock().unwrap();
    assert_eq!(st.list_prs().unwrap().len(), 1);
    assert!(st.repo_errors().unwrap().is_empty());
}

#[tokio::test]
async fn events_end_with_finished() {
    let f = Fake::with_repos("w", &["a", "b"]);
    f.set_open(&repo("w", "a"), Err(PE::NotFound));
    let p = Arc::new(f);
    let s = new_store();
    let (_, ev) = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(ev.first(), Some(&SyncEvent::Started));
    assert_eq!(ev[1], SyncEvent::Discovered { repos: 2 });
    assert_eq!(ev.last(), Some(&SyncEvent::Finished));
    assert_eq!(ev.iter().filter(|e| **e == SyncEvent::Finished).count(), 1);
}

#[tokio::test]
async fn repo_error_cleared_on_next_success() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(&r, Err(PE::Http(500)));
    let p = Arc::new(f);
    let s = new_store();
    let _ = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(
        s.lock().unwrap().repo_errors().unwrap(),
        vec![(r.clone(), "http 500".to_string())]
    );
    p.open.lock().unwrap().clear();
    let _ = run(&p, &s, SyncOptions::default()).await;
    assert!(s.lock().unwrap().repo_errors().unwrap().is_empty());
}

#[tokio::test]
async fn workspace_listing_error_cleared_on_next_discovery() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    let s = new_store();
    f.ws_repos
        .lock()
        .unwrap()
        .insert("w".into(), Err(PE::Forbidden));
    run_discovery(&f, &s).await.unwrap();
    assert_eq!(s.lock().unwrap().repo_errors().unwrap().len(), 1);
    f.ws_repos.lock().unwrap().insert("w".into(), Ok(vec![r]));
    run_discovery(&f, &s).await.unwrap();
    assert!(s.lock().unwrap().repo_errors().unwrap().is_empty());
}

#[tokio::test]
async fn receiver_dropped_does_not_panic() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(&r, Ok(vec![pr(&r, 1, PrState::Open, "c")]));
    let p = Arc::new(f);
    let s = new_store();
    s.lock().unwrap().set_selection(&[r]).unwrap();
    let (tx, rx) = unbounded_channel();
    drop(rx);
    let res = run_sync(p, s.clone(), SyncOptions::default(), tx).await;
    assert_eq!(res, Ok(()));
    assert_eq!(s.lock().unwrap().list_prs().unwrap().len(), 1);
}

#[tokio::test]
async fn open_fetch_failure_keeps_old_data() {
    let r = repo("w", "a");
    let f = Fake::with_repos("w", &["a"]);
    f.set_open(&r, Ok(vec![pr(&r, 1, PrState::Open, "c")]));
    let p = Arc::new(f);
    let s = new_store();
    let _ = run(&p, &s, SyncOptions::default()).await;
    assert_eq!(s.lock().unwrap().list_prs().unwrap().len(), 1);

    p.set_open(&r, Err(PE::Network("boom secret".into())));
    let _ = run(&p, &s, SyncOptions::default()).await;
    {
        let st = s.lock().unwrap();
        assert_eq!(st.list_prs().unwrap().len(), 1);
        assert_eq!(
            st.repo_errors().unwrap(),
            vec![(r.clone(), "network error".to_string())]
        );
    }

    // Merged-only failure still stores the Open list and records the error.
    p.set_open(
        &r,
        Ok(vec![
            pr(&r, 1, PrState::Open, "c"),
            pr(&r, 2, PrState::Open, "c"),
        ]),
    );
    p.set_merged(&r, "main", Err(PE::Decode("x".into())));
    let _ = run(&p, &s, SyncOptions::default()).await;
    let st = s.lock().unwrap();
    assert_eq!(st.list_prs().unwrap().len(), 2);
    assert_eq!(
        st.repo_errors().unwrap(),
        vec![(r, "decode error".to_string())]
    );
}

#[tokio::test]
async fn workspaces_error_cleared_on_next_success() {
    let f = Fake::with_repos("w", &["a"]);
    *f.workspaces.lock().unwrap() = Some(Err(PE::Network("boom".into())));
    let s = new_store();
    let res = run_discovery(&f, &s).await;
    assert_eq!(res, Err(DiscoveryError::Workspaces("network error".into())));
    assert_eq!(
        s.lock().unwrap().repo_errors().unwrap(),
        vec![(repo("*", "*"), "network error".to_string())]
    );

    *f.workspaces.lock().unwrap() = Some(Ok(vec!["w".into()]));
    assert!(run_discovery(&f, &s).await.is_ok());
    assert!(s.lock().unwrap().repo_errors().unwrap().is_empty());
}
