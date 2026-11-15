use super::*;

fn second_account() -> Account {
    Account {
        id: AccountId("acc-second".into()),
        ..Account::default_bitbucket("{bo}".into(), "Bo".into())
    }
}

/// A store with the default and a second Bitbucket account, each with one selected
/// repository, and a registry answering with the given fakes (no credentials needed).
fn two_accounts(
    default: &Arc<FakeProvider>,
    second: Option<&Arc<FakeProvider>>,
) -> (Arc<Mutex<Store>>, ProviderRegistry, RepoRef, RepoRef) {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (
        RepoRef::parse_bitbucket("acme/api").unwrap(),
        RepoRef::parse_bitbucket("beta/web").unwrap(),
    );
    store
        .upsert_account(&Account::default_bitbucket("{ana}".into(), "Ana".into()))
        .unwrap();
    store.upsert_account(&second_account()).unwrap();
    store.set_selection(std::slice::from_ref(&a)).unwrap();
    store
        .set_selection_for(&second_account().id, std::slice::from_ref(&b))
        .unwrap();
    let store = Arc::new(Mutex::new(store));
    let reg = ProviderRegistry::new(Arc::new(MemorySecrets::new()), store.clone());
    {
        let mut map = lock_ok(&reg.by_account);
        map.insert(AccountId::default_bitbucket(), default.clone());
        if let Some(p) = second {
            map.insert(second_account().id, p.clone());
        }
    }
    (store, reg, a, b)
}

fn run_accounts(
    reg: &ProviderRegistry,
    store: &Arc<Mutex<Store>>,
) -> (Result<(), ProviderError>, Vec<SyncEvent>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (tx, mut rx) = unbounded_channel();
    let accounts = sync_targets(store, true);
    let res = rt.block_on(sync_accounts(reg, store, accounts, &tx));
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    (res, events)
}

#[test]
fn two_accounts_sync_independently_and_auth_failure_stops_only_one() {
    // the default account's token is rejected: the second account still syncs
    let (bad, good) = (FakeProvider::new(true), FakeProvider::new(false));
    let (store, reg, a, b) = two_accounts(&bad, Some(&good));
    let (res, events) = run_accounts(&reg, &store);
    assert_eq!(
        res,
        Err(ProviderError::Auth),
        "the sign-in screen is asked for"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SyncEvent::Started | SyncEvent::Finished))
            .count(),
        2,
        "one sync for the UI: {events:?}"
    );
    assert_eq!(
        lock_ok(&bad.calls).clone(),
        vec![format!("prs:{}", a.full_name())]
    );
    assert_eq!(
        lock_ok(&good.calls).clone(),
        vec![format!("prs:{}", b.full_name())]
    );
    let snap = read_snapshot(&lock_ok(&store).reader()).unwrap();
    assert_eq!(snap.prs.len(), 1);
    assert_eq!(snap.prs[0].repo, b);
    assert_eq!(snap.prs[0].account, second_account().id);
    // the other way round: the second account's 401 is its own error row only
    let (good, bad) = (FakeProvider::new(false), FakeProvider::new(true));
    let (store, reg, a, _) = two_accounts(&good, Some(&bad));
    let (res, events) = run_accounts(&reg, &store);
    assert_eq!(res, Ok(()));
    assert!(
        events.contains(&SyncEvent::Discovered { repos: 2 }),
        "{events:?}"
    );
    let snap = read_snapshot(&lock_ok(&store).reader()).unwrap();
    assert_eq!(snap.prs.len(), 1);
    assert_eq!(snap.prs[0].repo, a);
    assert_eq!(
        snap.errors
            .iter()
            .map(|(_, m)| m.as_str())
            .collect::<Vec<_>>(),
        vec!["authentication failed"]
    );
}

#[test]
fn an_account_without_credentials_does_not_block_the_others() {
    let good = FakeProvider::new(false);
    let (store, reg, a, _) = two_accounts(&good, None);
    let (res, _) = run_accounts(&reg, &store);
    assert_eq!(res, Ok(()));
    let snap = read_snapshot(&lock_ok(&store).reader()).unwrap();
    assert_eq!(snap.prs.len(), 1);
    assert_eq!(snap.prs[0].repo, a);
    assert_eq!(
        snap.errors
            .iter()
            .map(|(_, m)| m.as_str())
            .collect::<Vec<_>>(),
        vec!["No stored credentials"]
    );
    // a disabled account is not synced at all
    lock_ok(&store)
        .set_account_enabled(&second_account().id, false)
        .unwrap();
    assert_eq!(
        sync_targets(&store, true),
        vec![AccountId::default_bitbucket()]
    );
    assert_eq!(sync_targets(&store, false), Vec::<AccountId>::new());
}

#[test]
fn two_github_accounts_same_host_coexist() {
    let secrets = MemorySecrets::new();
    let store = Mutex::new(Store::open_in_memory().unwrap());
    let bases = providers::github::bases_for("https://github.com").unwrap();
    let user = |id: &str, login: &str| UserRef {
        id: id.into(),
        display_name: format!("{login} name"),
        login: Some(login.into()),
    };
    let (work, _) = add_github(&secrets, &store, &bases, "tok-work", &user("1", "mona")).unwrap();
    let (home, _) =
        add_github(&secrets, &store, &bases, "tok-home", &user("2", "octocat")).unwrap();
    assert_ne!(work, home);
    assert_eq!(
        secrets.get(&token_key(&work)).unwrap().as_deref(),
        Some("tok-work")
    );
    assert_eq!(
        secrets.get(&token_key(&home)).unwrap().as_deref(),
        Some("tok-home")
    );
    let accounts = lock_ok(&store).accounts().unwrap();
    assert_eq!(accounts.len(), 2);
    assert!(accounts
        .iter()
        .all(|a| a.forge == Forge::GitHub && a.git_host == "github.com"));
    // each has its own label: the login until renamed; a rename touches one only
    let label = |id: &AccountId| {
        let a = accounts.iter().find(|a| a.id == *id).unwrap();
        account_label(&lock_ok(&store), a)
    };
    assert_eq!(
        (label(&work), label(&home)),
        ("mona".into(), "octocat".into())
    );
    set_account_label(&lock_ok(&store), &work, "  Work  ").unwrap();
    assert_eq!(
        (label(&work), label(&home)),
        ("Work".into(), "octocat".into())
    );
    set_account_label(&lock_ok(&store), &work, "").unwrap();
    assert_eq!(
        label(&work),
        "mona",
        "an empty label falls back to the login"
    );
    set_account_label(&lock_ok(&store), &work, "Work").unwrap();
    // both sync, each through its own provider
    let store = Arc::new(store);
    assert_eq!(sync_targets(&store, false).len(), 2);
    for a in &accounts {
        assert_eq!(build_provider(a, &secrets).unwrap().forge(), Forge::GitHub);
    }
    // removing one leaves the other's token, row and label
    let reg = ProviderRegistry::new(Arc::new(MemorySecrets::new()), store.clone());
    remove_account(&secrets, &store, &reg, &work).unwrap();
    assert_eq!(secrets.get(&token_key(&work)).unwrap(), None);
    assert_eq!(
        secrets.get(&token_key(&home)).unwrap().as_deref(),
        Some("tok-home")
    );
    assert_eq!(lock_ok(&store).get_meta(&label_key(&work)).unwrap(), None);
    let left = lock_ok(&store).accounts().unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(account_label(&lock_ok(&store), &left[0]), "octocat");
}

#[test]
fn disabled_account_not_synced_but_selection_kept() {
    let (first, second) = (FakeProvider::new(false), FakeProvider::new(false));
    let (store, reg, a, b) = two_accounts(&first, Some(&second));
    let id = second_account().id;
    set_enabled(&lock_ok(&store), &id, false).unwrap();
    let (res, _) = run_accounts(&reg, &store);
    assert_eq!(res, Ok(()));
    assert!(
        lock_ok(&second.calls).is_empty(),
        "a disabled account sends nothing"
    );
    assert_eq!(
        lock_ok(&first.calls).clone(),
        vec![format!("prs:{}", a.full_name())]
    );
    // its selection is still there, still through it
    let selected = lock_ok(&store).selected_repos_by_account().unwrap();
    assert!(selected.contains(&(id.clone(), b.clone())), "{selected:?}");
    assert!(!lock_ok(&store)
        .accounts()
        .unwrap()
        .iter()
        .any(|x| x.id == id && x.enabled));
    // enabled again: it syncs its kept selection
    set_enabled(&lock_ok(&store), &id, true).unwrap();
    assert_eq!(run_accounts(&reg, &store).0, Ok(()));
    assert_eq!(
        lock_ok(&second.calls).clone(),
        vec![format!("prs:{}", b.full_name())]
    );
    assert!(set_enabled(&lock_ok(&store), &AccountId("acc-nope".into()), false).is_err());
}

#[test]
fn paused_status_shown_per_account() {
    let until = std::time::SystemTime::now() + std::time::Duration::from_secs(600);
    let calm = FakeProvider::new(false);
    let paused = Arc::new(FakeProvider {
        auth_fails: false,
        calls: Mutex::new(Vec::new()),
        paused: Some(until),
    });
    let (store, reg, _, _) = two_accounts(&calm, Some(&paused));
    let second = second_account().id;
    assert_eq!(paused_accounts(&reg), vec![(second.clone(), until)]);
    // the Accounts card: the paused account says so, the other one does not
    let accounts = lock_ok(&store).accounts().unwrap();
    let mut others = crate::settings_ui::other_accounts(&accounts);
    for o in &mut others {
        o.paused_until = paused_accounts(&reg)
            .into_iter()
            .find(|(id, _)| *id == o.id)
            .map(|(_, t)| t);
    }
    let hhmm = chrono::DateTime::<chrono::Local>::from(until)
        .format("%H:%M")
        .to_string();
    let (text, tone) = crate::settings_ui::other_account_status(&others[0]);
    assert_eq!(text, format!("Paused by the rate limit until {hhmm}"));
    assert_eq!(tone, crate::settings_ui::Tone::Warn);
    // the inbox status line names the account
    let line = paused_line(&[(Forge::GitHub, "mona".into(), until)]).unwrap();
    assert_eq!(line, format!("GitHub (mona): paused until {hhmm}"));
    let two = paused_line(&[
        (Forge::GitHub, "mona".into(), until),
        (Forge::GitLab, "alex".into(), until),
    ])
    .unwrap();
    assert_eq!(
        two,
        format!("GitHub (mona): paused until {hhmm}; GitLab (alex): paused until {hhmm}")
    );
    assert_eq!(paused_line(&[]), None);
    // a pause that ended is not shown
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(5);
    assert_eq!(paused_line(&[(Forge::GitHub, "mona".into(), past)]), None);
}

#[test]
fn remove_confirm_counts_match_store() {
    let store = Store::open_in_memory().unwrap();
    let gh = Account {
        id: AccountId("acc-gh".into()),
        forge: Forge::GitHub,
        git_host: "github.com".into(),
        ..second_account()
    };
    store.upsert_account(&gh).unwrap();
    let mine = RepoRef::parse("github.com", "octo/api").unwrap();
    let other = RepoRef::parse_bitbucket("acme/web").unwrap();
    store
        .record_discovery_for(
            &gh.id,
            &[prcore::model::RepoInfo::from(mine.clone())],
            &[],
            None,
        )
        .unwrap();
    store
        .set_selection_for(&gh.id, std::slice::from_ref(&mine))
        .unwrap();
    store.set_selection(std::slice::from_ref(&other)).unwrap();
    let template = crate::demo::demo_data().1[0].clone();
    let prs: Vec<PullRequest> = (1..=3)
        .map(|id| {
            let mut p = template.clone();
            p.repo = mine.clone();
            p.id = id;
            p.account = gh.id.clone();
            p
        })
        .collect();
    let stat = DiffStat {
        files: 1,
        lines_added: 1,
        lines_removed: 0,
        max_file_lines: 1,
    };
    let stats = vec![(1, "c1".to_string(), stat.clone()), (2, "c2".into(), stat)];
    store
        .apply_repo_sync_for(&gh.id, &mine, Some(&prs), &stats, None)
        .unwrap();
    store
        .apply_repo_sync(&other, Some(std::slice::from_ref(&template)), &[], None)
        .unwrap();
    // two open drafts on the account's repository, one posted, one of another account
    let draft = |repo: &RepoRef, text: &str| prcore::draft::Draft {
        id: 0,
        repo: repo.clone(),
        pr_id: 1,
        kind: prcore::draft::DraftKind::Pr,
        path: None,
        line: None,
        side: None,
        start: None,
        reply_to: None,
        text: text.into(),
        source_commit: "c1".into(),
        comment_type: None,
    };
    store.save_draft(&draft(&mine, "a")).unwrap();
    store.save_draft(&draft(&mine, "b")).unwrap();
    let posted = store.save_draft(&draft(&mine, "c")).unwrap();
    store.mark_draft_posted(posted).unwrap();
    store.save_draft(&draft(&other, "d")).unwrap();

    let preview = store.removal_counts(&gh.id).unwrap();
    assert_eq!(
        crate::settings_ui::removal_summary(&preview),
        "Removes 3 cached PRs, keeps 2 drafts marked"
    );
    let before = store.list_prs().unwrap().len();
    let removed = store.remove_account(&gh.id).unwrap();
    assert_eq!(
        removed, preview,
        "the confirmation said exactly what happened"
    );
    assert_eq!(store.list_prs().unwrap().len(), before - preview.prs);
    assert_eq!(store.list_drafts(&mine, 1).unwrap().len(), 3, "drafts stay");
    // nothing of it is left but the kept drafts
    assert_eq!(
        store.removal_counts(&gh.id).unwrap(),
        store::RemovedCounts {
            drafts_kept: 2,
            ..Default::default()
        }
    );
    assert_eq!(
        crate::settings_ui::removal_summary(&store::RemovedCounts {
            prs: 1,
            drafts_kept: 1,
            ..Default::default()
        }),
        "Removes 1 cached PR, keeps 1 draft marked"
    );
}

#[test]
fn hint_lists_only_unselected_repos() {
    let store = Store::open_in_memory().unwrap();
    let r = |n: &str| RepoRef::parse("github.com", n).unwrap();
    let gh = AccountId("acc-gh".into());
    store
        .record_discovery_for(
            &gh,
            &[
                prcore::model::RepoInfo::from(r("octo/api")),
                prcore::model::RepoInfo::from(r("other/lib")),
            ],
            &[],
            None,
        )
        .unwrap();
    store.set_selection_for(&gh, &[r("octo/api")]).unwrap();
    // selected through another account (D8): not "elsewhere" either
    store
        .set_selection_for(&AccountId("acc-gh2".into()), &[r("octo/web")])
        .unwrap();
    let before = store.repo_choices().unwrap();
    let ghe = RepoRef::parse("ghe.corp.example", "octo/api").unwrap();
    let found = vec![
        r("octo/api"),
        r("other/lib"),
        r("octo/web"),
        r("other/lib"),
        r("zed/app"),
        ghe.clone(),
    ];
    let hint = requested_elsewhere(&store, &found);
    assert_eq!(
        hint,
        vec![(ghe, 1), (r("other/lib"), 2), (r("zed/app"), 1)],
        "only unselected repositories (the same path on another host is another one)"
    );
    assert_eq!(
        store.repo_choices().unwrap(),
        before,
        "the hint never selects (or lists) anything"
    );
    assert_eq!(
        elsewhere_hint(&hint).as_deref(),
        Some(
            "4 open pull requests request your review in repositories you have not \
             selected: octo/api (ghe.corp.example), other/lib, zed/app"
        )
    );
    let many: Vec<(RepoRef, usize)> = (0..7).map(|i| (r(&format!("o/r{i}")), 1)).collect();
    assert!(elsewhere_hint(&many).unwrap().ends_with("o/r4 and 2 more"));
    let one = [(r("zed/app"), 1)];
    assert_eq!(
        elsewhere_hint(&one).as_deref(),
        Some("1 open pull request requests your review in a repository you have not selected: zed/app")
    );
    assert_eq!(elsewhere_hint(&[]), None);
    assert!(requested_elsewhere(&store, &[r("octo/api")]).is_empty());
}

#[test]
fn poster_routes_to_owning_account() {
    let (first, second) = (FakeProvider::new(false), FakeProvider::new(false));
    let (_store, reg, a, b) = two_accounts(&first, Some(&second));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let poster = ProviderPoster {
        reg: Arc::new(reg),
        rt: rt.handle().clone(),
    };
    let text = NewComment::Pr { text: "x".into() };
    let at = PostRevision::default();
    let on_b = b.clone();
    let p = &poster;
    std::thread::scope(|s| {
        s.spawn(|| {
            let _ = p.post(&on_b, 7, &at, &text);
            let _ = p.post(&a, 3, &at, &text);
        });
    });
    assert_eq!(lock_ok(&second.calls).clone(), vec!["post:beta/web#7"]);
    assert_eq!(lock_ok(&first.calls).clone(), vec!["post:acme/api#3"]);
}

#[test]
fn review_deps_use_account_scope() {
    let secrets = MemorySecrets::new();
    let second = second_account();
    secrets
        .set(&email_key(&second.id), "bo@example.com")
        .unwrap();
    secrets.set(&token_key(&second.id), "tok-of-bo").unwrap();
    let auth = git_auth(&secrets, &second).expect("Bitbucket git auth");
    let want = GitAuth::bitbucket("tok-of-bo");
    assert_eq!(
        (auth.user, auth.token, auth.scope),
        (want.user, want.token, want.scope)
    );
    // the default account has nothing stored: no git credentials
    let default = Account::default_bitbucket(String::new(), String::new());
    assert!(git_auth(&secrets, &default).is_none());
    // never for another host than the account's own
    let elsewhere = Account {
        git_host: "bitbucket.example".into(),
        ..second.clone()
    };
    assert!(git_auth(&secrets, &elsewhere).is_none());
    // GitHub: x-access-token on the account's own git host; GitLab: oauth2 on its host
    // (a relative URL root's prefix is not part of the scope)
    let gh = Account {
        forge: Forge::GitHub,
        git_host: "ghe.corp.example".into(),
        ..second.clone()
    };
    let a = git_auth(&secrets, &gh).expect("GitHub git auth");
    assert_eq!(
        (a.user.as_str(), a.scope.as_str()),
        ("x-access-token", "https://ghe.corp.example/")
    );
    let gl = Account {
        forge: Forge::GitLab,
        git_host: "git.corp.example/gitlab".into(),
        ..second
    };
    let a = git_auth(&secrets, &gl).expect("GitLab git auth");
    assert_eq!(
        (a.user.as_str(), a.scope.as_str()),
        ("oauth2", "https://git.corp.example/")
    );
    // and a provider is never built for an http API address
    let insecure = Account {
        api_base: "http://api.bitbucket.org/2.0".into(),
        ..second_account()
    };
    assert!(build_provider(&insecure, &secrets).is_err());
}

#[test]
fn an_http_account_on_a_private_address_gets_cleartext_git_auth_and_a_provider() {
    let secrets = MemorySecrets::new();
    let gl = providers::gitlab::bases_for("http://192.168.1.20:8080").unwrap();
    let acc = Account {
        forge: Forge::GitLab,
        web_base: gl.web_base,
        api_base: gl.api_base,
        git_host: gl.git_host,
        ..second_account()
    };
    secrets.set(&token_key(&acc.id), "glpat-tok").unwrap();
    let a = git_auth(&secrets, &acc).expect("cleartext git auth");
    assert_eq!(
        (a.user.as_str(), a.scope.as_str()),
        ("oauth2", "http://192.168.1.20:8080/")
    );
    assert!(!format!("{a:?}").contains("glpat-tok"));
    assert!(build_provider(&acc, &secrets).is_ok());
    let gh = providers::github::bases_for("http://ghe.localhost:8080").unwrap();
    let ghacc = Account {
        forge: Forge::GitHub,
        web_base: gh.web_base,
        api_base: gh.api_base,
        graphql_base: Some(gh.graphql_url),
        git_host: gh.git_host,
        ..acc.clone()
    };
    let a = git_auth(&secrets, &ghacc).expect("cleartext git auth");
    assert_eq!(a.scope, "http://ghe.localhost:8080/");
    assert!(build_provider(&ghacc, &secrets).is_ok());
    // a public http address (e.g. a hand-edited cache): no git credential, no provider
    let public = Account {
        web_base: "http://gitlab.example".into(),
        api_base: "http://gitlab.example/api/v4".into(),
        git_host: "gitlab.example".into(),
        ..acc.clone()
    };
    assert!(git_auth(&secrets, &public).is_none());
    assert!(build_provider(&public, &secrets).is_err());
    // an https account on a private address stays https
    let tls = Account {
        web_base: "https://192.168.1.20:8443".into(),
        api_base: "https://192.168.1.20:8443/api/v4".into(),
        git_host: "192.168.1.20:8443".into(),
        ..acc
    };
    assert_eq!(
        git_auth(&secrets, &tls).unwrap().scope,
        "https://192.168.1.20:8443/"
    );
}

#[test]
fn an_added_account_gets_its_own_keys_and_removal_takes_only_its_rows() {
    let secrets = MemorySecrets::new();
    let store = Mutex::new(Store::open_in_memory().unwrap());
    save_me(
        &store,
        &UserRef {
            id: "{ana}".into(),
            display_name: "Ana".into(),
            login: None,
        },
    );
    let bo = UserRef {
        id: "{bo}".into(),
        display_name: "Bo".into(),
        login: None,
    };
    let (id, name) = add_account(&secrets, &store, "bo@example.com", "tok-bo", &bo).unwrap();
    assert_eq!(name, "Bo");
    assert!(id.as_str().starts_with("acc-") && id != AccountId::default_bitbucket());
    assert_eq!(
        secrets.get(&token_key(&id)).unwrap().as_deref(),
        Some("tok-bo")
    );
    let accounts = lock_ok(&store).accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].user_id, "{bo}");
    assert!(!format!("{accounts:?}").contains("tok-bo"));
    // the same user again, or the signed-in one, is refused
    assert!(add_account(&secrets, &store, "bo@example.com", "t", &bo).is_err());
    let ana = UserRef {
        id: "{ana}".into(),
        display_name: "Ana".into(),
        login: None,
    };
    assert!(add_account(&secrets, &store, "ana@example.com", "t", &ana).is_err());
    // removal: its keys and rows, nothing of the default account
    let r = RepoRef::parse_bitbucket("beta/web").unwrap();
    let a = RepoRef::parse_bitbucket("acme/api").unwrap();
    {
        let st = lock_ok(&store);
        st.set_selection_for(&id, std::slice::from_ref(&r)).unwrap();
        st.set_selection(std::slice::from_ref(&a)).unwrap();
    }
    let reg = ProviderRegistry::new(
        Arc::new(MemorySecrets::new()),
        Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
    );
    assert!(remove_account(&secrets, &store, &reg, &AccountId::default_bitbucket()).is_err());
    let msg = remove_account(&secrets, &store, &reg, &id).unwrap();
    assert!(msg.starts_with("Removed"), "{msg}");
    assert_eq!(secrets.get(&token_key(&id)).unwrap(), None);
    assert_eq!(secrets.get(&email_key(&id)).unwrap(), None);
    assert_eq!(lock_ok(&store).selected_repos().unwrap(), vec![a]);
    assert!(lock_ok(&store).accounts().unwrap().is_empty());
}

#[test]
fn a_github_account_stores_only_its_token_and_its_derived_bases() {
    let secrets = MemorySecrets::new();
    let store = Mutex::new(Store::open_in_memory().unwrap());
    let bases = providers::github::bases_for("https://ghe.corp.example").unwrap();
    let me = UserRef {
        id: "42".into(),
        display_name: "Mona".into(),
        login: Some("mona".into()),
    };
    let (id, name) = add_github(&secrets, &store, &bases, "gh-tok", &me).unwrap();
    assert_eq!(name, "mona (ghe.corp.example)");
    assert_eq!(
        secrets.get(&token_key(&id)).unwrap().as_deref(),
        Some("gh-tok")
    );
    assert_eq!(secrets.get(&email_key(&id)).unwrap(), None);
    let a = lock_ok(&store).accounts().unwrap().pop().unwrap();
    assert_eq!(a.forge, Forge::GitHub);
    assert_eq!(a.api_base, "https://ghe.corp.example/api/v3");
    assert_eq!(
        a.graphql_base.as_deref(),
        Some("https://ghe.corp.example/api/graphql")
    );
    assert_eq!(
        (a.git_host.as_str(), a.login.as_str()),
        ("ghe.corp.example", "mona")
    );
    assert_eq!(a.auth_kind, "pat");
    assert!(!format!("{a:?}").contains("gh-tok"));
    // the same user on the same host is refused; on github.com it is another account
    assert!(add_github(&secrets, &store, &bases, "t2", &me).is_err());
    let dotcom = providers::github::bases_for("https://github.com").unwrap();
    assert!(add_github(&secrets, &store, &dotcom, "t3", &me).is_ok());
    // and its provider is built from the stored token
    let p = build_provider(&a, &secrets).unwrap();
    assert_eq!(p.forge(), Forge::GitHub);
    assert!(!p.capabilities().read_only);
    let insecure = Account {
        graphql_base: Some("http://ghe.corp.example/api/graphql".into()),
        ..a.clone()
    };
    assert!(build_provider(&insecure, &secrets).is_err());
    let _ = secrets.delete(&token_key(&a.id));
    assert!(build_provider(&a, &secrets).is_err());
}

#[test]
fn a_gitlab_account_stores_only_its_token_and_its_derived_bases() {
    let secrets = MemorySecrets::new();
    let store = Mutex::new(Store::open_in_memory().unwrap());
    let bases = providers::gitlab::bases_for("https://git.corp.example/gitlab").unwrap();
    let me = UserRef {
        id: "17".into(),
        display_name: "Alex".into(),
        login: Some("alex".into()),
    };
    let expires = chrono::NaiveDate::from_ymd_opt(2027, 1, 31);
    let (id, name) = add_gitlab(&secrets, &store, &bases, "glpat-tok", &me, expires).unwrap();
    assert_eq!(name, "alex (git.corp.example/gitlab)");
    assert_eq!(
        secrets.get(&token_key(&id)).unwrap().as_deref(),
        Some("glpat-tok")
    );
    assert_eq!(secrets.get(&email_key(&id)).unwrap(), None);
    let a = lock_ok(&store).accounts().unwrap().pop().unwrap();
    assert_eq!(a.forge, Forge::GitLab);
    assert_eq!(a.api_base, "https://git.corp.example/gitlab/api/v4");
    assert_eq!(a.graphql_base, None);
    assert_eq!(a.git_host, "git.corp.example/gitlab");
    assert!(!format!("{a:?}").contains("glpat-tok"));
    // the expiry is kept (no secret) for the Accounts card's warning
    assert_eq!(token_expiry(&lock_ok(&store), &id), expires);
    assert!(add_gitlab(&secrets, &store, &bases, "t2", &me, None).is_err());
    // its provider is built from the stored token and can write
    let p = build_provider(&a, &secrets).unwrap();
    assert_eq!(p.forge(), Forge::GitLab);
    assert!(!p.capabilities().read_only);
    // removal takes the expiry with the keys
    let reg = ProviderRegistry::new(
        Arc::new(MemorySecrets::new()),
        Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
    );
    remove_account(&secrets, &store, &reg, &id).unwrap();
    assert_eq!(token_expiry(&lock_ok(&store), &id), None);
    assert!(build_provider(&a, &secrets).is_err());
}

#[test]
fn gitlab_signin_errors_and_summary_never_carry_the_token() {
    let text = gitlab_signin_error(&ProviderError::Auth);
    assert!(text.contains("did not accept"), "{text}");
    let info = providers::gitlab::TokenInfo {
        scopes: vec!["read_api".into(), "read_repository".into()],
        expires_at: chrono::NaiveDate::from_ymd_opt(2026, 11, 20),
        active: true,
    };
    let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
    let s = gitlab_token_summary("alex", &info, today, "3 projects visible");
    assert!(s.contains("Signed in as alex"), "{s}");
    assert!(s.contains("expires in 6 day(s)"), "{s}");
    assert!(s.contains("api scope"), "{s}");
}

#[test]
fn github_signin_errors_never_carry_a_url() {
    let sso = ProviderError::Rejected {
        status: 403,
        message: providers::github::SSO_HINT.into(),
    };
    for e in [sso, ProviderError::Auth, ProviderError::RateLimited] {
        let text = github_signin_error(&e);
        assert!(!text.contains("https://"), "{text}");
    }
    assert!(github_signin_error(&ProviderError::Auth).contains("did not accept"));
}

#[test]
fn new_account_ids_differ() {
    let ids: std::collections::HashSet<AccountId> = (0..50).map(|_| new_account_id()).collect();
    assert_eq!(ids.len(), 50);
}
