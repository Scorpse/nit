use super::*;

fn choice(full: &str, selected: bool, gone: bool) -> RepoChoice {
    RepoChoice {
        repo: RepoRef::parse_bitbucket(full).unwrap(),
        name: None,
        private: None,
        selected,
        discovered_at: Some("2026-11-13 10:00:00".into()),
        gone,
        account: Default::default(),
    }
}

fn picker(rows: Vec<RepoChoice>) -> RepoPicker {
    let mut p = RepoPicker::loading();
    assert!(!p.is_loaded());
    p.load(rows);
    p
}

fn names(v: &[RepoRef]) -> Vec<String> {
    v.iter().map(|r| r.full_name()).collect()
}

fn def() -> AccountId {
    AccountId::default_bitbucket()
}

/// `repos` through the default account.
fn by_default(repos: Vec<RepoRef>) -> Vec<(AccountId, RepoRef)> {
    repos.into_iter().map(|r| (def(), r)).collect()
}

fn under(account: &str, full: &str, selected: bool) -> RepoChoice {
    RepoChoice {
        account: AccountId(account.into()),
        ..choice(full, selected, false)
    }
}

#[test]
fn picker_groups_by_account_then_namespace_and_one_account_per_repo() {
    // as the store lists them: by workspace and repository, accounts mixed
    let mut p = picker(vec![
        choice("acme/api", true, false),
        under("acc-second", "acme/api", false),
        choice("acme/web", false, false),
        under("acc-second", "beta/web", false),
    ]);
    p.account_labels = vec![(
        AccountId("acc-second".into()),
        "Bo (Bitbucket Cloud)".into(),
    )];
    assert!(p.multi_account());
    let order: Vec<(String, String)> = p
        .rows
        .iter()
        .map(|c| (c.account.as_str().to_string(), c.repo.full_name()))
        .collect();
    assert_eq!(
        order,
        vec![
            ("acc-bitbucket-default".into(), "acme/api".into()),
            ("acc-bitbucket-default".into(), "acme/web".into()),
            ("acc-second".into(), "acme/api".into()),
            ("acc-second".into(), "beta/web".into()),
        ],
        "the signed-in account first, then workspace and repository"
    );
    assert_eq!(
        p.account_label(&AccountId("acc-second".into())),
        "Bo (Bitbucket Cloud)"
    );
    assert_eq!(p.account_label(&def()), "acc-bitbucket-default");
    // ticking acme/api under the second account unticks it under the first
    p.tick(2, true);
    assert!(!p.rows[0].selected && p.rows[2].selected);
    // "All" of a workspace works per account, and also moves repositories over
    p.set_workspace(&def(), "acme", true);
    assert!(p.rows[0].selected && p.rows[1].selected && !p.rows[2].selected);
    let second = AccountId("acc-second".into());
    p.set_workspace(&second, "beta", true);
    let api = RepoRef::parse_bitbucket("acme/api").unwrap();
    assert_eq!(
        p.save(),
        Some(PickerAction::Save {
            selected: vec![
                (def(), api),
                (def(), RepoRef::parse_bitbucket("acme/web").unwrap()),
                (second, RepoRef::parse_bitbucket("beta/web").unwrap()),
            ],
            purge: vec![]
        })
    );
}

#[test]
fn search_matches_full_name_and_display_name_ignoring_case() {
    let mut named = choice("acme/api", false, false);
    named.name = Some("Payments API".into());
    let mut p = picker(vec![
        named,
        choice("acme/web", false, false),
        choice("labs/payments-ui", false, false),
    ]);
    p.query = " PAYMENTS ".into();
    assert_eq!(p.visible(), vec![0, 2]);
    p.query = "labs/".into();
    assert_eq!(p.visible(), vec![2]);
    p.query.clear();
    assert_eq!(p.visible().len(), 3);
}

#[test]
fn counts_and_workspace_toggles_follow_the_search() {
    let mut p = picker(vec![
        choice("acme/api", false, false),
        choice("acme/web", false, false),
        choice("acme/old", true, true),
        choice("labs/x", false, false),
    ]);
    assert_eq!(p.counts(), (0, 3), "a gone repo is not counted");
    p.set_workspace(&def(), "acme", true);
    assert_eq!(
        names(&p.selected()),
        vec!["acme/api", "acme/web", "acme/old"]
    );
    assert_eq!(p.counts(), (2, 3));
    // with a search, only the visible repos of the workspace change
    p.query = "web".into();
    p.set_workspace(&def(), "acme", false);
    assert_eq!(names(&p.selected()), vec!["acme/api", "acme/old"]);
    p.query.clear();
    p.set_workspace(&def(), "acme", false);
    assert!(p.selected().is_empty(), "None also unticks a gone repo");
    p.set_workspace(&def(), "acme", true);
    assert!(!p.rows[2].selected, "All never ticks a gone repo");
}

#[test]
fn adding_repos_saves_at_once() {
    let mut p = picker(vec![
        choice("acme/api", false, false),
        choice("acme/web", false, false),
    ]);
    p.rows[1].selected = true;
    assert_eq!(
        p.save(),
        Some(PickerAction::Save {
            selected: by_default(vec![RepoRef::parse_bitbucket("acme/web").unwrap()]),
            purge: vec![]
        })
    );
    assert!(p.saving && p.confirm.is_none());
}

#[test]
fn removing_repos_asks_and_keeps_the_cache_unless_told() {
    let start = || {
        let mut p = picker(vec![
            choice("acme/api", true, false),
            choice("acme/web", true, false),
        ]);
        p.rows[0].selected = false;
        assert_eq!(p.save(), None, "asks first");
        assert_eq!(names(p.confirm.as_ref().unwrap()), vec!["acme/api"]);
        p
    };
    let web = vec![RepoRef::parse_bitbucket("acme/web").unwrap()];
    let api = vec![RepoRef::parse_bitbucket("acme/api").unwrap()];
    let mut keep = start();
    assert_eq!(
        keep.answer(Some(false)),
        Some(PickerAction::Save {
            selected: by_default(web.clone()),
            purge: vec![]
        })
    );
    let mut delete = start();
    assert_eq!(
        delete.answer(Some(true)),
        Some(PickerAction::Save {
            selected: by_default(web),
            purge: api
        })
    );
    let mut cancel = start();
    assert_eq!(cancel.answer(None), None);
    assert!(cancel.confirm.is_none() && !cancel.saving);
}

#[test]
fn a_reload_keeps_what_was_ticked_and_adds_new_repos() {
    let mut p = picker(vec![
        choice("acme/api", true, false),
        choice("acme/web", false, false),
    ]);
    p.rows[0].selected = false;
    p.rows[1].selected = true;
    // a discovery added a repo and marked api gone
    p.load(vec![
        choice("acme/api", true, true),
        choice("acme/new", false, false),
        choice("acme/web", false, false),
    ]);
    assert_eq!(names(&p.selected()), vec!["acme/web"]);
    assert!(p.rows[0].gone);
    // the first load still decides what a Save stops syncing
    assert_eq!(names(&p.removed()), vec!["acme/api"]);
}

#[test]
fn nothing_is_preselected_by_the_picker() {
    let p = picker(vec![
        choice("acme/api", false, false),
        choice("acme/web", false, false),
    ]);
    assert!(p.selected().is_empty());
    assert_eq!(p.counts(), (0, 2));
}
