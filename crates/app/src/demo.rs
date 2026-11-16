//! Deterministic demo data covering every inbox scenario, for `--demo` and screenshots.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use prcore::model::{DiffStat, PrState, PullRequest, RepoRef, ReviewState, Reviewer, UserRef};

const WS: &str = "acme";
const REPOS: [&str; 15] = [
    "api",
    "billing",
    "docs",
    "gateway",
    "infra",
    "mobile",
    "notifications",
    "platform",
    "search",
    "sso",
    "tools",
    "web",
    "reports",
    "payments",
    "warehouse",
];
const NOW: i64 = 1_794_000_000;

fn repo(slug: &str) -> RepoRef {
    RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: WS.into(),
        slug: slug.into(),
    }
}

/// The demo discovery cache: every `acme` repository selected (the inbox's PRs come from
/// them), one of them gone, and two more workspaces with nothing selected.
pub fn demo_repo_choices() -> Vec<store::RepoChoice> {
    let at = Some("2026-11-13 09:12:44".to_string());
    let choice =
        |ws: &str, slug: &str, name: Option<&str>, private, selected, gone| store::RepoChoice {
            repo: RepoRef {
                host: prcore::model::BITBUCKET_HOST.into(),
                namespace: ws.into(),
                slug: slug.into(),
            },
            name: name.map(str::to_string),
            private: Some(private),
            selected,
            discovered_at: at.clone(),
            gone,
            account: Default::default(),
        };
    let mut v: Vec<store::RepoChoice> = REPOS
        .iter()
        .map(|s| choice(WS, s, None, *s != "docs", true, false))
        .collect();
    v.push(choice(WS, "legacy-billing", None, true, true, true));
    for (slug, name) in [
        ("design-system", Some("Design System")),
        ("ml-pipeline", None),
        ("prototype-checkout", Some("Checkout prototype")),
        ("sandbox", None),
    ] {
        v.push(choice("acme-labs", slug, name, true, false, false));
    }
    for slug in ["dotfiles", "blog"] {
        v.push(choice("side-project", slug, None, false, false, false));
    }
    v.sort_by_key(|c| (c.repo.namespace.clone(), c.repo.slug.clone()));
    v
}

/// The demo's second account id (a Bitbucket account of another person).
pub const DEMO_SECOND_ACCOUNT: &str = "acc-demo-second";

/// The demo accounts: the signed-in one, a second Bitbucket Cloud account and a GitHub
/// account (github.com, read-only in this version).
pub fn demo_accounts() -> Vec<store::Account> {
    vec![
        store::Account::default_bitbucket("{alex-example}".into(), "Alex Example".into()),
        store::Account {
            id: prcore::model::AccountId(DEMO_SECOND_ACCOUNT.into()),
            ..store::Account::default_bitbucket("{bo-lindqvist}".into(), "Bo Lindqvist".into())
        },
        store::Account {
            id: prcore::model::AccountId(DEMO_GITHUB_ACCOUNT.into()),
            forge: prcore::model::Forge::GitHub,
            web_base: "https://github.com".into(),
            api_base: "https://api.github.com".into(),
            graphql_base: Some("https://api.github.com/graphql".into()),
            git_host: "github.com".into(),
            user_id: DEMO_GITHUB_USER.into(),
            login: "alex-example".into(),
            display_name: "Alex Example".into(),
            auth_kind: "pat".into(),
            enabled: true,
        },
        store::Account {
            id: prcore::model::AccountId(DEMO_GITLAB_ACCOUNT.into()),
            forge: prcore::model::Forge::GitLab,
            web_base: format!("https://{DEMO_GITLAB_HOST}"),
            api_base: format!("https://{DEMO_GITLAB_HOST}/api/v4"),
            graphql_base: None,
            git_host: DEMO_GITLAB_HOST.into(),
            user_id: DEMO_GITLAB_USER.into(),
            login: "alex".into(),
            display_name: "Alex Example".into(),
            auth_kind: "pat".into(),
            enabled: true,
        },
    ]
}

/// The demo accounts paused by a rate limit: the GitHub one, for 12 more minutes.
pub fn demo_paused() -> Vec<(prcore::model::AccountId, std::time::SystemTime)> {
    vec![(
        prcore::model::AccountId(DEMO_GITHUB_ACCOUNT.into()),
        std::time::SystemTime::now() + std::time::Duration::from_secs(12 * 60),
    )]
}

/// The demo GitLab account (a self-managed instance under a relative URL root).
pub const DEMO_GITLAB_ACCOUNT: &str = "acc-demo-gitlab";
pub const DEMO_GITLAB_HOST: &str = "git.acme.example/gitlab";
/// The demo user's numeric GitLab id.
pub const DEMO_GITLAB_USER: &str = "17";

/// The demo accounts' token expiry: the GitLab token runs out in 9 days (the Accounts card
/// shows its warning).
pub fn demo_token_expiry(id: &prcore::model::AccountId) -> Option<chrono::NaiveDate> {
    (id.as_str() == DEMO_GITLAB_ACCOUNT)
        .then(|| chrono::Local::now().date_naive() + chrono::Days::new(9))
}

/// `demo_repo_choices` plus the second account's repositories: its own workspace and one
/// repository both accounts see (synced through the second account).
pub fn demo_repo_choices_two_accounts() -> Vec<store::RepoChoice> {
    let second = prcore::model::AccountId(DEMO_SECOND_ACCOUNT.into());
    // the big workspace left out, so both accounts' groups fit on one screen
    let mut v: Vec<store::RepoChoice> = demo_repo_choices()
        .into_iter()
        .filter(|c| c.repo.namespace != WS)
        .collect();
    let shared = v
        .iter()
        .position(|c| c.repo.namespace == "acme-labs" && c.repo.slug == "design-system")
        .map(|i| v[i].clone());
    for (ws, slug, selected) in [
        ("bo-consulting", "client-portal", true),
        ("bo-consulting", "invoicing", false),
        ("bo-consulting", "terraform-modules", true),
    ] {
        v.push(store::RepoChoice {
            repo: RepoRef {
                host: prcore::model::BITBUCKET_HOST.into(),
                namespace: ws.into(),
                slug: slug.into(),
            },
            name: None,
            private: Some(true),
            selected,
            discovered_at: Some("2026-11-13 09:12:44".into()),
            gone: false,
            account: second.clone(),
        });
    }
    if let Some(mut c) = shared {
        c.account = second;
        c.selected = true;
        v.push(c);
    }
    // the store's order
    v.sort_by_key(|c| (c.repo.namespace.clone(), c.repo.slug.clone()));
    v
}

fn user(name: &str) -> UserRef {
    UserRef {
        id: format!("{{{}}}", name.to_lowercase().replace(' ', "-")),
        display_name: name.into(),
        login: None,
    }
}

fn ts(minutes_ago: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(NOW - minutes_ago * 60, 0).unwrap()
}

fn review(u: &UserRef, state: ReviewState) -> Reviewer {
    Reviewer {
        user: u.clone(),
        state,
    }
}

/// Open PR `id` in `slug`, branch `feat/{id}` -> `main`, no reviewers.
fn pr(slug: &str, id: u64, title: &str, author: &UserRef, age_min: i64) -> PullRequest {
    PullRequest {
        repo: repo(slug),
        id,
        title: title.into(),
        state: PrState::Open,
        draft: false,
        author: author.clone(),
        source_branch: format!("feat/{id}"),
        dest_branch: "main".into(),
        source_commit: format!("c{id}"),
        dest_commit: "main-head".into(),
        source_repo: repo(slug),
        comment_count: 0,
        task_count: 0,
        reviewers: vec![],
        created_on: ts(age_min + 600),
        updated_on: ts(age_min),
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

/// Makes `child` target `parent`'s branch, based on the parent's current head.
fn stacked_on(mut child: PullRequest, parent: &PullRequest) -> PullRequest {
    child.dest_branch = parent.source_branch.clone();
    child.dest_commit = parent.source_commit.clone();
    child
}

fn stat(files: u32, added: u32, removed: u32, max_file: u32) -> DiffStat {
    DiffStat {
        files,
        lines_added: added,
        lines_removed: removed,
        max_file_lines: max_file,
    }
}

struct Builder {
    prs: Vec<PullRequest>,
    stats: HashMap<(RepoRef, u64), DiffStat>,
}

impl Builder {
    fn add(&mut self, p: PullRequest, d: DiffStat) {
        self.stats.insert((p.repo.clone(), p.id), d);
        self.prs.push(p);
    }
}

/// Returns (me, PRs, diffstats). A few PRs have no diffstat yet ("size pending").
pub fn demo_data() -> (UserRef, Vec<PullRequest>, HashMap<(RepoRef, u64), DiffStat>) {
    let me = user("Alex Example");
    let ana = user("Ana Souza");
    let bo = user("Bo Lindqvist");
    let chen = user("Chen Wei");
    let dmitri = user("Дмитрий Орлов");
    let mut b = Builder {
        prs: vec![],
        stats: HashMap::new(),
    };

    // acme/api: a 3-deep stack; the newest ungrouped scenario PRs sort right below it.
    let mut a1 = pr("api", 101, "Add rate limiter core", &ana, 300);
    a1.reviewers = vec![review(&me, ReviewState::Pending)];
    let mut a2 = stacked_on(
        pr("api", 102, "Rate limiter: per-token buckets", &ana, 290),
        &a1,
    );
    let a3 = stacked_on(
        pr("api", 103, "Rate limiter: expose metrics", &ana, 280),
        &a2,
    );
    a2.checks = prcore::model::CheckState::Failing;
    a2.reviewers = vec![review(&chen, ReviewState::ChangesRequested)];
    a1.checks = prcore::model::CheckState::Passing;
    a1.has_conflicts = true;
    b.add(a1, stat(6, 220, 14, 120));
    b.add(a2, stat(4, 140, 30, 90));
    b.add(a3, stat(3, 80, 5, 60));

    // fork PR (not supported)
    let mut fork = pr("api", 110, "Fix typo in README (from fork)", &chen, 5);
    fork.source_repo = RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: "chen-w".into(),
        slug: "api".into(),
    };
    b.add(fork, stat(1, 1, 1, 2));
    // over the Bitbucket limits: more than 8000 changed lines
    let mut huge = pr("api", 111, "Regenerate protobuf bindings", &bo, 10);
    huge.reviewers = vec![review(&me, ReviewState::Pending)];
    b.add(huge, stat(60, 7200, 1900, 1500));
    // draft
    let mut d = pr("api", 112, "WIP: pagination rework", &me, 20);
    d.draft = true;
    b.add(d, stat(5, 90, 10, 60));
    // unicode titles
    b.add(
        pr(
            "api",
            113,
            "Überarbeitung der Zahlungs-Schnittstelle — naïve café",
            &dmitri,
            30,
        ),
        stat(7, 210, 120, 140),
    );
    b.add(
        pr(
            "api",
            114,
            "Исправить обработку пустых значений (Ελληνικά ok)",
            &dmitri,
            40,
        ),
        stat(2, 30, 8, 30),
    );

    // acme/billing: the parent's head moved after the child was opened (ParentChanged)
    let mut p = pr("billing", 201, "Invoice PDF renderer", &bo, 120);
    p.source_commit = "c201-force-pushed".into();
    let mut c = pr("billing", 202, "Invoice PDF: add tax lines", &bo, 110);
    c.dest_branch = p.source_branch.clone();
    c.dest_commit = "c201-old".into();
    c.reviewers = vec![review(&me, ReviewState::Pending)];
    b.add(p, stat(8, 400, 60, 180));
    b.add(c, stat(3, 70, 10, 40));
    // over the limits: more than 200 files
    let wide = pr(
        "billing",
        210,
        "Rename currency module across services",
        &me,
        15,
    );
    b.add(wide, stat(240, 900, 880, 120));
    b.add(
        pr("billing", 211, "Bump deps", &chen, 90),
        stat(2, 12, 12, 8),
    );

    // acme/docs: merged parent, the child still targets the old branch
    let mut mp = pr("docs", 301, "Docs site: new navigation", &chen, 600);
    mp.state = PrState::Merged;
    let mut mc = stacked_on(pr("docs", 302, "Docs site: sidebar search", &chen, 60), &mp);
    mc.reviewers = vec![review(&me, ReviewState::Pending)];
    b.add(mp, stat(9, 300, 120, 150));
    b.add(mc, stat(3, 90, 10, 70));

    // acme/gateway: ChangesRequested parent, approved child
    let mut gp = pr("gateway", 401, "Auth middleware refactor", &ana, 200);
    gp.reviewers = vec![review(&bo, ReviewState::ChangesRequested)];
    gp.comment_count = 7;
    gp.task_count = 2;
    let mut gc = stacked_on(
        pr("gateway", 402, "Auth middleware: token cache", &ana, 190),
        &gp,
    );
    gc.reviewers = vec![
        review(&chen, ReviewState::Approved),
        review(&me, ReviewState::Pending),
    ];
    gc.comment_count = 2;
    b.add(gp, stat(5, 180, 90, 100));
    b.add(gc, stat(2, 60, 5, 45));

    // acme/infra: a stack reviewable only per PR (cumulative over 8000 lines, none over alone)
    let i1 = pr("infra", 501, "Terraform: network module", &bo, 400);
    let i2 = stacked_on(
        pr("infra", 502, "Terraform: database module", &bo, 390),
        &i1,
    );
    let i3 = stacked_on(pr("infra", 503, "Terraform: compute module", &bo, 380), &i2);
    b.add(i1, stat(90, 3000, 500, 400));
    b.add(i2, stat(80, 2800, 400, 350));
    b.add(i3, stat(70, 2500, 300, 300));
    // one file over 2000 lines
    b.add(
        pr("infra", 510, "Vendored lockfile update", &chen, 25),
        stat(1, 2600, 100, 2700),
    );

    // my own PRs and PRs that need my review
    for (i, slug) in ["mobile", "search", "web"].iter().enumerate() {
        let id = 600 + i as u64;
        let age = i as i64;
        let mut mine = pr(slug, id, &format!("My change {id}"), &me, 50 + age);
        mine.reviewers = vec![
            review(&ana, ReviewState::Approved),
            review(&bo, ReviewState::Pending),
        ];
        b.add(mine, stat(3, 40, 10, 30));
        let mut theirs = pr(
            slug,
            id + 50,
            &format!("Please review {}", id + 50),
            &ana,
            70 + age,
        );
        theirs.reviewers = vec![review(&me, ReviewState::Pending)];
        b.add(theirs, stat(4, 100, 20, 50));
    }
    let mut draft2 = pr("web", 660, "Draft: dark mode", &ana, 35);
    draft2.draft = true;
    draft2.reviewers = vec![review(&me, ReviewState::Pending)];
    b.add(draft2, stat(6, 150, 40, 90));
    // a stack whose diffstats are not fetched yet
    let sp = pr("sso", 701, "SAML: metadata endpoint", &chen, 500);
    let sc = stacked_on(pr("sso", 702, "SAML: assertion consumer", &chen, 490), &sp);
    b.prs.push(sp);
    b.prs.push(sc);

    // 600 filler PRs spread over the 15 repos (deterministic LCG)
    let people = [&ana, &bo, &chen, &dmitri, &me];
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (x >> 33) as u32
    };
    for n in 0..600u64 {
        // The first five repos hold the scenarios; keep them small so they sort and show first.
        let slug = REPOS[5 + (n as usize) % (REPOS.len() - 5)];
        let id = 1000 + n;
        let author = people[(next() % 5) as usize];
        let title = format!("Task {id}: update module {}", next() % 97);
        let mut p = pr(slug, id, &title, author, 1000 + (next() % 40_000) as i64);
        p.draft = next() % 17 == 0;
        p.comment_count = next() % 9;
        if next() % 9 == 0 {
            p.reviewers = vec![review(&me, ReviewState::Pending)];
        } else if next() % 5 == 0 {
            p.reviewers = vec![review(&ana, ReviewState::Approved)];
        }
        let files = 1 + next() % 40;
        let added = files * (3 + next() % 60);
        let removed = files * (next() % 20);
        b.add(p, stat(files, added, removed, 20 + next() % 400));
    }

    // A GitHub account (read-only in this version): its PRs list first (`acme-oss/` sorts
    // before `acme/`), with a forge tag.
    for (p, s) in demo_github_prs() {
        b.add(p, s);
    }
    // A GitLab account, a nested group on a self-managed host.
    for (p, s) in demo_gitlab_mrs() {
        b.add(p, s);
    }

    (me, b.prs, b.stats)
}

/// The demo GitHub account.
pub const DEMO_GITHUB_ACCOUNT: &str = "acc-demo-github";
/// The demo user's numeric GitHub id (the GitHub account's `me`).
pub const DEMO_GITHUB_USER: &str = "4242";

/// Each demo account's own user, for per-account "Mine" / "Needs me".
pub fn demo_account_users() -> HashMap<prcore::model::AccountId, String> {
    demo_accounts()
        .into_iter()
        .map(|a| (a.id, a.user_id))
        .collect()
}

/// The demo GitHub account's open PRs in `acme-oss/widgets` (github.com): a team review
/// request, a bot PR, my own PR over GitHub's web diff limits, and a fork PR.
fn demo_github_prs() -> Vec<(PullRequest, DiffStat)> {
    let repo = RepoRef {
        host: "github.com".into(),
        namespace: "acme-oss".into(),
        slug: "widgets".into(),
    };
    let gh_user = |id: &str, login: &str, name: &str| UserRef {
        id: id.into(),
        display_name: name.into(),
        login: Some(login.into()),
    };
    let me = gh_user(DEMO_GITHUB_USER, "alex-example", "Alex Example");
    let mona = gh_user("7", "mona", "Mona Lisa");
    let bot = gh_user("49699333", "dependabot[bot]", "dependabot[bot]");
    let contrib = gh_user("99", "contrib", "Casey Contributor");
    let make = |id: u64, title: &str, author: &UserRef, age: i64| {
        let mut p = pr("widgets", id, title, author, age);
        p.repo = repo.clone();
        p.source_repo = repo.clone();
        p.forge = prcore::model::Forge::GitHub;
        p.account = prcore::model::AccountId(DEMO_GITHUB_ACCOUNT.into());
        p.web_url = format!("https://github.com/acme-oss/widgets/pull/{id}");
        p.node_id = Some(format!("PR_demo{id}"));
        p
    };
    let mut theme = make(31, "Add dark theme tokens", &mona, 15);
    theme.review_requested_from_me = true;
    theme.reviewers = vec![Reviewer {
        user: UserRef {
            id: "team:acme-oss/design".into(),
            display_name: "Design".into(),
            login: Some("design".into()),
        },
        state: ReviewState::Pending,
    }];
    theme.comment_count = 4;
    let bump = make(32, "Bump serde from 1.0.210 to 1.0.215", &bot, 25);
    let port = make(33, "Port the parser to nom 8", &me, 35);
    let mut fork = make(34, "Fix typo in the getting-started guide", &contrib, 45);
    fork.source_repo = RepoRef {
        host: "github.com".into(),
        namespace: "contrib".into(),
        slug: "widgets".into(),
    };
    fork.review_requested_from_me = true;
    vec![
        (theme, stat(12, 340, 40, 0)),
        (bump, stat(2, 8, 8, 0)),
        (port, stat(3400, 9000, 7000, 0)),
        (fork, stat(1, 1, 1, 0)),
    ]
}

/// The demo GitLab account's open MRs in `acme-platform/infra/terraform-modules`: one asking for
/// my review, my own draft, a bot MR whose changes count GitLab caps at "1000+", and a fork
/// MR. Sizes are file counts only (decision D6).
fn demo_gitlab_mrs() -> Vec<(PullRequest, DiffStat)> {
    let repo = RepoRef {
        host: DEMO_GITLAB_HOST.into(),
        namespace: "acme-platform/infra".into(),
        slug: "terraform-modules".into(),
    };
    let gl_user = |id: &str, login: &str, name: &str| UserRef {
        id: id.into(),
        display_name: name.into(),
        login: Some(login.into()),
    };
    let me = gl_user(DEMO_GITLAB_USER, "alex", "Alex Example");
    let jo = gl_user("23", "jo", "Jo Okafor");
    let pat = gl_user("31", "pat", "Pat Novak");
    let bot = gl_user("99", "project_12_bot_9f1c", "Renovate Bot");
    let ext = gl_user("57", "contrib", "Casey Contributor");
    let make = |id: u64, title: &str, author: &UserRef, age: i64| {
        let mut p = pr("terraform-modules", id, title, author, age);
        p.repo = repo.clone();
        p.source_repo = repo.clone();
        p.forge = prcore::model::Forge::GitLab;
        p.account = prcore::model::AccountId(DEMO_GITLAB_ACCOUNT.into());
        p.web_url = format!(
            "https://{DEMO_GITLAB_HOST}/acme-platform/infra/terraform-modules/-/merge_requests/{id}"
        );
        p.node_id = Some("12".into());
        p
    };
    let files = |n: u32| stat(n, 0, 0, 0);
    let mut vpc = make(51, "Add the VPC peering module", &jo, 12);
    vpc.review_requested_from_me = true;
    vpc.reviewers = vec![
        Reviewer {
            user: me.clone(),
            state: ReviewState::Pending,
        },
        Reviewer {
            user: pat.clone(),
            state: ReviewState::Approved,
        },
    ];
    vpc.comment_count = 5;
    let mut own = make(52, "Draft: bump provider versions", &me, 28);
    own.draft = true;
    own.reviewers = vec![Reviewer {
        user: jo.clone(),
        state: ReviewState::ChangesRequested,
    }];
    let rotate = make(53, "Regenerate the module docs", &bot, 40);
    let mut fork = make(54, "Fix a typo in the README", &ext, 55);
    fork.source_repo = RepoRef {
        host: DEMO_GITLAB_HOST.into(),
        namespace: "unknown".into(),
        slug: "project-77".into(),
    };
    fork.review_requested_from_me = true;
    fork.reviewers = vec![Reviewer {
        user: me,
        state: ReviewState::Pending,
    }];
    vec![
        (vpc, files(9)),
        (own, files(3)),
        (rotate, files(providers::gitlab::CAPPED_FILES)),
        (fork, files(1)),
    ]
}

/// A throwaway git repository holding one PR (`feature/demo` -> `main`), for opening a
/// review with no network. Dropping it deletes the repository.
// `--demo` review wiring is a later task; until then tests and benches are the users.
#[cfg_attr(not(test), allow(dead_code))]
pub struct DemoReview {
    /// Keeps the repository alive; deleted on drop.
    _dir: tempfile::TempDir,
    /// Non-bare repository with both branches; usable directly as the local repo.
    pub repo_path: std::path::PathBuf,
    /// `file://` URL of `repo_path`.
    pub url: String,
    pub repo: RepoRef,
    pub pr: PullRequest,
    /// Tip of the PR's source branch.
    pub head: String,
}

#[cfg_attr(not(test), allow(dead_code))]
impl DemoReview {
    /// Runs git in the fixture repository with a fixed identity; panics on failure.
    pub fn git(&self, args: &[&str]) -> String {
        fixture_git(&self.repo_path, args)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub const DEMO_REVIEW_FILES: usize = 40;

#[cfg_attr(not(test), allow(dead_code))]
fn fixture_git(dir: &std::path::Path, args: &[&str]) -> String {
    let git = gitlocal::Git::shared().expect("git is required for the demo review");
    let mut full = vec![
        "-c",
        "user.name=nit demo",
        "-c",
        "user.email=demo@nit.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.autocrlf=false",
    ];
    full.extend(args);
    let out = git
        .run(dir, &full, None)
        .unwrap_or_else(|e| panic!("demo git {args:?}: {e}"));
    String::from_utf8_lossy(&out).trim().to_string()
}

/// Ten lines unique to file `i`, so rename detection pairs the right files.
#[cfg_attr(not(test), allow(dead_code))]
fn demo_lines(i: usize, tag: &str) -> String {
    (0..10)
        .map(|l| format!("// file {i} line {l} {tag}\nfn f_{i}_{l}() -> u32 {{ {l} }}\n"))
        .collect()
}

/// Demo PR with `DEMO_REVIEW_FILES` changed files.
#[cfg_attr(not(test), allow(dead_code))]
pub fn review_fixture() -> DemoReview {
    review_fixture_with(DEMO_REVIEW_FILES)
}

/// Demo PR with `n` changed files: by index, `i % 10 == 0` added, `1` deleted, `2` renamed,
/// `i == 3` binary, the rest modified. `main` gains an unrelated commit after branching.
#[cfg_attr(not(test), allow(dead_code))]
pub fn review_fixture_with(n: usize) -> DemoReview {
    build_review_fixture(n, &[])
}

/// The demo app's review: `DEMO_REVIEW_FILES` plus `demo_tree_files()`, so the file tree
/// shows nested directories, many siblings in one directory and root files.
#[cfg_attr(not(test), allow(dead_code))]
pub fn review_fixture_demo() -> DemoReview {
    build_review_fixture(DEMO_REVIEW_FILES, &demo_tree_files())
}

/// `review_fixture_with(n)`, plus `extra` files: (path, true: added in the PR, false: modified).
#[cfg_attr(not(test), allow(dead_code))]
fn build_review_fixture(n: usize, extra: &[(String, bool)]) -> DemoReview {
    let dir = tempfile::tempdir().expect("temp dir for the demo review");
    let p = dir.path().to_path_buf();
    let path = |i: usize| format!("src/m{:02}/file_{i:04}.rs", i / 50);
    let write = |rel: &str, data: &[u8]| {
        let f = p.join(rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, data).unwrap();
    };
    fixture_git(&p, &["init", "-q", "-b", "main"]);
    write("README.md", b"demo\n");
    for i in 0..n {
        match i {
            3 => write("assets/logo.bin", &[0, 1, 2, 3, 0, 255, 0]),
            _ if i % 10 == 0 => {}
            _ => write(&path(i), demo_lines(i, "base").as_bytes()),
        }
    }
    for (k, (rel, added)) in extra.iter().enumerate() {
        if !added {
            write(rel, extra_text(rel, k, false, false).as_bytes());
        }
    }
    fixture_git(&p, &["add", "-A"]);
    fixture_git(&p, &["commit", "-qm", "base"]);
    fixture_git(&p, &["checkout", "-q", "-b", "feature/demo"]);
    for i in 0..n {
        match i {
            3 => write("assets/logo.bin", &[0, 9, 9, 0, 7, 255, 0, 1]),
            _ if i % 10 == 0 => write(&path(i), demo_lines(i, "new").as_bytes()),
            _ if i % 10 == 1 => std::fs::remove_file(p.join(path(i))).unwrap(),
            _ if i % 10 == 2 => {
                let to = path(i).replace("file_", "renamed_");
                std::fs::rename(p.join(path(i)), p.join(&to)).unwrap();
            }
            _ => {
                let text = demo_lines(i, "base").replace(
                    "line 4 base\n",
                    &format!("line 4 changed in PR\n// added {i}\n"),
                );
                write(&path(i), text.as_bytes());
            }
        }
    }
    for (k, (rel, added)) in extra.iter().enumerate() {
        write(rel, extra_text(rel, k, *added, true).as_bytes());
    }
    fixture_git(&p, &["add", "-A"]);
    fixture_git(&p, &["commit", "-qm", "feature work"]);
    let head = fixture_git(&p, &["rev-parse", "HEAD"]);
    fixture_git(&p, &["checkout", "-q", "main"]);
    write("MAIN_ONLY.md", b"main moved on\n");
    fixture_git(&p, &["add", "-A"]);
    fixture_git(&p, &["commit", "-qm", "main moved on"]);
    let dest = fixture_git(&p, &["rev-parse", "HEAD"]);
    let url = format!("file:///{}", p.to_string_lossy().replace('\\', "/"));
    let repo = RepoRef {
        host: prcore::model::BITBUCKET_HOST.into(),
        namespace: WS.into(),
        slug: "review-demo".into(),
    };
    let mut pr = pr(
        "review-demo",
        900,
        "Demo review: refactor modules",
        &user("Ana Souza"),
        30,
    );
    pr.source_branch = "feature/demo".into();
    pr.source_commit = head.clone();
    pr.dest_commit = dest;
    DemoReview {
        _dir: dir,
        repo_path: p,
        url,
        repo,
        pr,
        head,
    }
}

/// The demo PR's file that carries the sample comment threads (a modified file: old line
/// 9 is replaced by new lines 9-10, and the hunk shows new lines 6-13).
pub const DEMO_COMMENTS_FILE: &str = "src/m00/file_0004.rs";

/// A file the demo comments are anchored on that is not in the diff: one 120-char path.
pub const UNPLACED_PATH: &str = "build/module-bundles/widgets-ui/\
    d89e0f55db83b07895bd3f57db396182eb2906a747d3f2b9dd5951db4577c0bd/bundle-manifest.json";

/// A ~1500-char PR-level summary: paragraphs, "- " bullets, long hashes and inline code.
const LONG_PR_COMMENT: &str =
    "Rebased onto feat/widget-sizes (a1b2c3d4..e5f60718), four commits:\n\n\
- 3a4b5c6d packages/api/test/widget-size.test.ts: Widget.dimensions is optional in \
the schema, so the test now reads it as (widget.dimensions ?? []). The assertion is \
unchanged and still fails if the entry is missing. This was the only pnpm typecheck error.\n\
- 7e8f9a0b packages/api/openapi.json regenerated. The widget schema gained the optional colour \
field (engine widget-schema, 1c2d3e4f) without the document being regenerated, so pnpm \
openapi:check and the openapi comparison in api.test.ts failed. API_VERSION is unchanged \
(2.4.0); scripts/check-schema-drift.sh against release/2.3 passes (2.3.0 -> 2.4.0).\n\
- 5a6b7c8d packages/api/test/golden/: the golden snapshots were regenerated with \
UPDATE_GOLDEN=1. The only change is the `colour` field in every widget; reviewed by hand.\n\
- e5f60718 docs/: `pnpm docs:build` output path fixed (docs/site/ -> docs/dist/), \
https://example.invalid/acme/widgets/pipelines/0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c\n\n\
Verified locally: pnpm install --frozen-lockfile, pnpm typecheck, pnpm lint, pnpm test \
(1312 passed), pnpm openapi:check. CI is green on the rebased head.";

/// A pasted log of more than 3000 chars (collapsed with "Show more").
fn very_long_comment() -> String {
    let mut s = String::from("Full output of the failing run, for the record:\n\n");
    for i in 0..40 {
        s.push_str(&format!(
            "- step {i:02}: packages/verifiers/test/live-smoke.test.ts passed in {} ms\n",
            100 + i * 7
        ));
    }
    s.push_str("\nEnd of log.");
    s
}

/// Comments of the demo PR: an unresolved inline thread with two replies on an added line,
/// a resolved thread (with a deleted reply), a file-level thread, a PR-level thread, a
/// thread on a deleted line, an outdated thread whose line is not in the diff, one Bitbucket
/// flags outdated on a line that is in the diff, threads on files that are not in the diff,
/// and one unresolved thread on a second file.
pub fn review_comments() -> providers::provider::CommentList {
    use prcore::comments::{Anchor, Comment};
    let (ana, ben, cleo) = (user("Ana Souza"), user("Ben Okafor"), user("Cleo Park"));
    let at = |path: &str, from: Option<u32>, to: Option<u32>| {
        Some(Anchor {
            path: path.into(),
            from,
            to,
            start: None,
            outdated: false,
            old_path: None,
            commit: None,
        })
    };
    let c = |id: u64, parent: Option<u64>, who: &UserRef, mins: i64, body: &str| Comment {
        id: id.into(),
        parent_id: parent.map(Into::into),
        author: who.clone(),
        body: body.into(),
        created_on: ts(mins),
        updated_on: ts(mins),
        deleted: false,
        resolved: false,
        anchor: None,
        author_is_bot: false,
        thread: Some(prcore::comments::ThreadId::from(id)),
        can_resolve: true,
    };
    let f = DEMO_COMMENTS_FILE;
    let mut comments = vec![
        Comment {
            anchor: at(f, None, Some(10)),
            ..c(
                1,
                None,
                &ben,
                240,
                "This added comment line does not say why the value changed. Could you \
                 explain the reasoning here, or link the ticket? A short note helps the next \
                 reader a lot.",
            )
        },
        c(
            2,
            Some(1),
            &ana,
            200,
            "It follows the new config format; I will add a link.",
        ),
        c(
            3,
            Some(1),
            &ben,
            180,
            "**praise:** Thanks, a link is enough.",
        ),
        Comment {
            resolved: true,
            anchor: at(f, None, Some(7)),
            ..c(
                4,
                None,
                &cleo,
                300,
                "**nit:** Trailing whitespace on this line.",
            )
        },
        c(5, Some(4), &ana, 290, "Fixed."),
        Comment {
            deleted: true,
            ..c(6, Some(4), &cleo, 280, "")
        },
        Comment {
            anchor: at(f, None, None),
            ..c(
                7,
                None,
                &cleo,
                260,
                "**question:** Should this module get its own test file?",
            )
        },
        Comment {
            anchor: at(f, Some(9), None),
            ..c(
                8,
                None,
                &ana,
                220,
                "The old wording here was clearer; maybe keep it.",
            )
        },
        // a long multi-paragraph reply under a diff line (card height in the diff rows)
        c(20, Some(8), &ben, 215, LONG_PR_COMMENT),
        Comment {
            anchor: at(f, None, Some(20)),
            ..c(
                9,
                None,
                &ben,
                400,
                "This line moved in an earlier revision.",
            )
        },
        c(
            10,
            None,
            &ben,
            500,
            "Overall looks good. Please squash the fixup commits before merging.",
        ),
        c(11, Some(10), &ana, 450, "Will do."),
        Comment {
            anchor: at("src/m00/file_0005.rs", None, Some(10)),
            ..c(12, None, &cleo, 210, "Same question as in file_0004.")
        },
        // the shape of a real PR's conversation: a long multi-paragraph PR-level summary
        // with bullets, hashes and inline code, edited after posting
        Comment {
            updated_on: ts(100),
            ..c(13, None, &ana, 600, LONG_PR_COMMENT)
        },
        // unplaced: anchored on files that are not in the diff
        Comment {
            anchor: at(UNPLACED_PATH, None, Some(1)),
            updated_on: ts(90),
            ..c(14, None, &ben, 120, UNPLACED_PATH)
        },
        c(
            15,
            Some(14),
            &ana,
            110,
            "Generated output; it is not part of the review. \
             See 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef for the run.",
        ),
        Comment {
            anchor: at("build/module-bundles/modules.lock", None, Some(3)),
            ..c(
                16,
                None,
                &ben,
                100,
                "I think these can be ignored OR fetched from the repository artefacts. The \
                 same can be applied for the whole bundles directory.",
            )
        },
        Comment {
            updated_on: ts(60),
            ..c(
                17,
                Some(16),
                &ana,
                80,
                "Agreed. `build/module-bundles` is build output: the Dockerfile builds the \
                 bundles into the image, the release pipeline builds them again for the \
                 manifest, and the previous bundles come from the previous image, not from \
                 git. `bundles:verify` reads only `modules.lock` and `templates/`. I \
                 untracked all 35 files and added `build/` to .gitignore (ab5e2ade).\n\n\
                 `modules.lock` stays committed (one line per module, it is the gate).",
            )
        },
        c(
            18,
            Some(16),
            &ben,
            70,
            "Thanks, resolving once CI is green.",
        ),
        // a very long PR-level comment (a pasted log): collapsed with "Show more"
        c(19, None, &cleo, 50, &very_long_comment()),
        // Bitbucket flags it outdated (its line changed after a later push): shown in the
        // outdated section although new line 8 is in the diff
        Comment {
            anchor: at(f, None, Some(8)).map(|a| Anchor {
                outdated: true,
                ..a
            }),
            ..c(
                21,
                None,
                &cleo,
                350,
                "This line read differently when I commented; is the rename intended?",
            )
        },
    ];
    comments.sort_by_key(|c| c.id.clone());
    providers::provider::CommentList {
        comments,
        skipped: Vec::new(),
        tasks: None,
    }
}

/// The nested file the tree screenshot selects (it carries demo threads).
pub const DEMO_TREE_FILE: &str = "packages/engine/test/catalogue.test.ts";

/// Extra files of the demo app's review (102, for 142 in all): many siblings in
/// `packages/engine/test`, a few more test directories, one deep path and root files.
/// (path, added in the PR?); the others are modified.
#[cfg_attr(not(test), allow(dead_code))]
pub fn demo_tree_files() -> Vec<(String, bool)> {
    let stems = [
        "catalogue",
        "cart",
        "cart-totals",
        "checkout",
        "colour-palette",
        "coupon-codes",
        "currency",
        "discounts",
        "export-csv",
        "feature-flags",
        "gift-wrap",
        "import-csv",
        "inventory",
        "inventory-sync",
        "invoices",
        "labels",
        "locale",
        "loyalty-points",
        "order-history",
        "order-status",
        "packaging",
        "pagination",
        "price-calculator-with-a-rather-long-descriptive-name",
        "pricing",
        "receipts",
        "refunds",
        "reviews",
        "search-index",
        "shipping",
        "shipping-zones",
        "sizes",
        "sku-format",
        "stock-alerts",
        "suppliers",
        "tax-rates",
        "thumbnails",
        "units",
        "warehouse",
        "widget-schema",
        "wishlist",
    ];
    let mut out: Vec<String> = Vec::new();
    for s in stems {
        out.push(format!("packages/engine/test/{s}.test.ts"));
        out.push(format!("packages/engine/test/{s}-edge.test.ts"));
    }
    out.extend((0..10).map(|i| format!("packages/readers/test/a{i:02}.test.ts")));
    out.extend((0..8).map(|i| format!("packages/verifiers/test/v{i}.test.ts")));
    out.push("packages/check-library/test/support/fixture-gate.ts".into());
    out.push("CHANGELOG.md".into());
    out.push("README.md".into());
    out.push(DEMO_TSX_FILE.into());
    out.into_iter()
        .enumerate()
        .map(|(k, p)| {
            let modified = k % 5 == 4
                || p.ends_with("fixture-gate.ts")
                || p == "README.md"
                || p == DEMO_TSX_FILE;
            (p, !modified)
        })
        .collect()
}

/// A modified React component of the demo app's review (TypeScript JSX).
pub const DEMO_TSX_FILE: &str = "packages/web/src/CatalogueTable.tsx";

/// Contents of an extra file: the base version (`pr` false, modified files only) or the
/// PR's. A few files carry realistic TypeScript; the others the generated demo lines.
#[cfg_attr(not(test), allow(dead_code))]
fn extra_text(rel: &str, k: usize, added: bool, pr: bool) -> String {
    let (base, after) = match rel {
        DEMO_TREE_FILE => (String::new(), CATALOGUE_TEST_TS.to_string()),
        DEMO_TSX_FILE => (
            CATALOGUE_TABLE_TSX.to_string(),
            CATALOGUE_TABLE_TSX
                .replace(
                    "  const [open, setOpen] = useState(false);\n",
                    "  const [open, setOpen] = useState<boolean>(false);\n  const sorted = useMemo(() => sortRows(rows, by), [rows, by]);\n",
                )
                .replace("{rows.map(", "{sorted.map("),
        ),
        p if p.ends_with("fixture-gate.ts") => (
            FIXTURE_GATE_TS.to_string(),
            FIXTURE_GATE_TS.replace(
                "export const GATE_TIMEOUT_MS = 5_000;\n",
                "export const GATE_TIMEOUT_MS = 10_000;\n/** Retries before a gate counts as closed. */\nexport const GATE_RETRIES = 3;\n",
            ),
        ),
        _ if added => (String::new(), demo_lines(1000 + k, "new")),
        _ => {
            let base = demo_lines(1000 + k, "base");
            let after = base.replace("line 4 base\n", "line 4 changed in PR\n");
            (base, after)
        }
    };
    if pr {
        after
    } else {
        base
    }
}

const CATALOGUE_TEST_TS: &str = r#"import type { Catalogue, Entry } from "../src/catalogue";
import { describe, expect, it } from "vitest";
import { loadCatalogue } from "../src/load";

/** Entries of one country, in catalogue order. */
const byCountry = <T extends Entry>(xs: readonly T[], cc: string): T[] =>
  xs.filter((x) => x.country === cc);

describe("catalogue", () => {
  it("loads every entry once", async () => {
    const cat: Catalogue = await loadCatalogue({ strict: true });
    const ids = new Set(cat.entries.map((e) => e.id));
    expect(ids.size).toBe(cat.entries.length);
  });

  it("labels entries with their country", () => {
    const entry: Entry = { id: 7, name: "Widget", country: "DE" };
    const label = `entry ${entry.id}: ${entry.name.toUpperCase()} (${entry.country})`;
    expect(label).toMatch(/^entry \d+: WIDGET/);
  });

  it.each([
    ["DE", 2],
    ["FR", 1],
  ])("finds %s entries", (cc, n) => {
    const xs: Entry[] = [
      { id: 1, name: "a", country: "DE" },
      { id: 2, name: "b", country: "DE" },
      { id: 3, name: "c", country: "FR" },
    ];
    expect(byCountry(xs, cc)).toHaveLength(n);
  });
});
"#;

const CATALOGUE_TABLE_TSX: &str = r#"import React, { useMemo, useState } from "react";
import type { Entry } from "@demo/engine";
import { sortRows, type SortKey } from "./sort";

export interface CatalogueTableProps {
  rows: readonly Entry[];
  by?: SortKey;
  onPick?: (entry: Entry) => void;
}

/** The catalogue as a table; a click on a row picks it. */
export function CatalogueTable({ rows, by = "name", onPick }: CatalogueTableProps): JSX.Element {
  const [open, setOpen] = useState(false);
  return (
    <table className="catalogue" aria-expanded={open}>
      <thead>
        <tr onClick={() => setOpen(!open)}>
          <th>Id</th>
          <th>Name</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <tr key={r.id} onClick={() => onPick?.(r)}>
            <td>{`#${r.id}`}</td>
            <td>{r.name}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
"#;

const FIXTURE_GATE_TS: &str = r#"import { setTimeout as sleep } from "node:timers/promises";

/** How long a test waits for the gate before it fails. */
export const GATE_TIMEOUT_MS = 5_000;

export type Gate = { readonly name: string; open(): Promise<void> };

export async function waitForGate(gate: Gate, ms = GATE_TIMEOUT_MS): Promise<boolean> {
  const started = Date.now();
  while (Date.now() - started < ms) {
    try {
      await gate.open();
      return true;
    } catch (err: unknown) {
      console.warn(`gate ${gate.name} not open yet`, err);
      await sleep(100);
    }
  }
  return false;
}
"#;

/// `review_comments`, plus threads on nested files of `demo_tree_files` (the demo app):
/// My last review of the demo PR: a day ago, before the demo commit was made.
pub fn demo_last_review() -> prcore::history::ReviewMark {
    prcore::history::ReviewMark {
        at: chrono::Utc::now() - chrono::Duration::days(1),
        commit: None,
    }
}

/// unresolved and resolved ones at different depths, a file-level one, and one in a
/// directory the tree screenshot collapses.
pub fn review_comments_demo() -> providers::provider::CommentList {
    use prcore::comments::{Anchor, Comment};
    let mut list = review_comments();
    let ben = user("Ben Okafor");
    let mk = |id: u64, path: &str, to: Option<u32>, resolved: bool, body: &str| Comment {
        id: id.into(),
        parent_id: None,
        author: ben.clone(),
        body: body.into(),
        created_on: ts(100),
        updated_on: ts(100),
        deleted: false,
        resolved,
        anchor: Some(Anchor {
            path: path.into(),
            from: None,
            to,
            start: None,
            outdated: false,
            old_path: None,
            commit: None,
        }),
        author_is_bot: false,
        thread: Some(prcore::comments::ThreadId::from(id)),
        can_resolve: true,
    };
    list.comments.extend([
        mk(
            101,
            DEMO_TREE_FILE,
            Some(3),
            false,
            "Is this fixture still needed?",
        ),
        mk(102, DEMO_TREE_FILE, Some(12), true, "Renamed, thanks."),
        mk(
            103,
            "packages/readers/test/a03.test.ts",
            Some(5),
            true,
            "Fine now.",
        ),
        mk(
            104,
            "packages/verifiers/test/v2.test.ts",
            Some(7),
            false,
            "Flaky on CI?",
        ),
        mk(
            105,
            "packages/check-library/test/support/fixture-gate.ts",
            None,
            false,
            "Could the gate fixture live next to the pack tests?",
        ),
        mk(106, "README.md", None, true, "Docs updated."),
    ]);
    list.comments.sort_by_key(|c| c.id.clone());
    list
}

/// A line of `DEMO_COMMENTS_FILE` that is not in its diff (the drafts scene's blocked draft).
pub const DEMO_FAR_LINE: u32 = 500;

/// The demo review's context: the demo user (a pending reviewer of the PR) and a two-PR
/// stack whose parent is not approved yet, so the Approve confirmation shows a warning.
pub fn review_context(
    fixture: &DemoReview,
    pr: &mut PullRequest,
) -> crate::review::actions_ui::ReviewContext {
    let (me, _, _) = demo_data();
    if pr.reviewers.is_empty() {
        pr.reviewers = vec![
            review(&user("Bo Lindqvist"), ReviewState::Approved),
            review(&me, ReviewState::Pending),
        ];
    }
    let mut parent = self::pr(
        &fixture.repo.slug,
        pr.id - 1,
        "Demo review: shared helpers",
        &user("Ana Souza"),
        60,
    );
    parent.reviewers = vec![review(&me, ReviewState::Pending)];
    crate::review::actions_ui::ReviewContext {
        me: Some(me),
        stack: Some(prcore::Stack {
            repo: fixture.repo.clone(),
            members: vec![parent.id, pr.id],
            edges: vec![(parent.id, pr.id)],
        }),
        prs: [(parent.id, parent), (pr.id, pr.clone())]
            .into_iter()
            .collect(),
    }
}

/// Sample drafts for the drafts screenshot: inline on a shown line and on a line outside
/// the diff, a reply to the first thread, a file draft and a PR-level draft.
pub fn review_drafts(repo: &RepoRef, pr_id: u64, head: &str) -> Vec<prcore::draft::Draft> {
    use prcore::draft::{Draft, DraftKind, Side};
    let d = |kind, path: Option<&str>, line, side, parent_id: Option<u64>, text: &str| Draft {
        id: 0,
        repo: repo.clone(),
        pr_id,
        kind,
        path: path.map(Into::into),
        line,
        side,
        start: None,
        reply_to: parent_id.map(Into::into),
        text: text.into(),
        source_commit: head.into(),
        comment_type: None,
    };
    let f = Some(DEMO_COMMENTS_FILE);
    let mut drafts = vec![
        d(
            DraftKind::Inline,
            f,
            Some(12),
            Some(Side::New),
            None,
            "Consider naming this value; a bare number is easy to misread.\n\n\
             - it also appears in config/limits/0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a6978/defaults.json\n\
             - and in the generated client, so a constant keeps them in sync.",
        ),
        d(
            DraftKind::Reply,
            None,
            None,
            None,
            Some(1),
            "Linked the ticket in the description.",
        ),
        d(
            DraftKind::File,
            f,
            None,
            None,
            None,
            "A short module comment at the top would help.",
        ),
        d(
            DraftKind::Inline,
            f,
            Some(DEMO_FAR_LINE),
            Some(Side::New),
            None,
            "This depends on the old layout.",
        ),
        d(
            DraftKind::Pr,
            None,
            None,
            None,
            None,
            "Thanks, this reads much better now.",
        ),
    ];
    // comment types: posted as a "**type:** " prefix, shown as badges
    use prcore::comment_type::CommentType::*;
    for (d, t) in
        drafts
            .iter_mut()
            .zip([Some(Issue), None, Some(Nit), Some(Question), Some(Praise)])
    {
        d.comment_type = t;
    }
    drafts
}

#[cfg(test)]
mod tests;
