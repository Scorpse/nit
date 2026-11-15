//! Accounts page: the signed-in account, other accounts and the add-account form.

use super::*;

/// Up to two capital initials of a display name for the account badge; `?` when empty.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let pick = match words.as_slice() {
        [] => return "?".into(),
        [one] => vec![*one],
        [first, .., last] => vec![*first, *last],
    };
    pick.iter()
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

/// The account's connection, as the Account card shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Conn {
    /// `--demo`: no network.
    Demo,
    Connecting,
    Syncing {
        done: usize,
        total: usize,
    },
    Connected,
    /// Signed in, but the last connect or sync failed with this message.
    Problem(String),
    /// No answer from Bitbucket yet this run (cached data only).
    #[default]
    Offline,
}

/// Picks the connection state from the inbox's sync status.
pub fn connection(
    demo: bool,
    connecting: bool,
    running: bool,
    progress: (usize, usize),
    note: Option<&str>,
    me_known: bool,
) -> Conn {
    if demo {
        Conn::Demo
    } else if connecting {
        Conn::Connecting
    } else if running {
        Conn::Syncing {
            done: progress.0,
            total: progress.1,
        }
    } else if let Some(n) = note {
        Conn::Problem(n.to_string())
    } else if me_known {
        Conn::Connected
    } else {
        Conn::Offline
    }
}

/// (chip text, tone, detail line) of a connection state.
pub fn conn_label(c: &Conn) -> (&'static str, Tone, String) {
    match c {
        Conn::Demo => ("Demo", Tone::Muted, "Demo data, no network".into()),
        Conn::Connecting => (
            "Connecting",
            Tone::Busy,
            "Signing in to Bitbucket...".into(),
        ),
        Conn::Syncing { done, total } => (
            "Syncing",
            Tone::Busy,
            format!("Connected \u{00b7} syncing {done} of {total} repositories"),
        ),
        Conn::Connected => (
            "Connected",
            Tone::Good,
            "Connected to Bitbucket Cloud".into(),
        ),
        Conn::Problem(e) => ("Problem", Tone::Bad, e.clone()),
        Conn::Offline => (
            "Offline",
            Tone::Warn,
            "Not connected: showing cached data".into(),
        ),
    }
}

/// The command the token check runs as.
pub const CHECK_TOKEN_CMD: &str = "nit --check-token";

// ---- accounts ---------------------------------------------------------------------------

/// An account besides the signed-in one, as the Accounts card lists it. Never holds a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherAccount {
    pub id: AccountId,
    pub forge: Forge,
    pub host: String,
    pub name: String,
    pub enabled: bool,
    /// When the account's token expires, where the forge says (GitLab).
    pub expires: Option<chrono::NaiveDate>,
    /// Until when the account's client waits out a rate limit (set by the caller).
    pub paused_until: Option<std::time::SystemTime>,
    /// Reached over plain http (a private address): the token travels unencrypted.
    pub cleartext: bool,
}

/// The chip of an account reached over plain http, None for an https one.
pub fn cleartext_chip(a: &OtherAccount) -> Option<(&'static str, Tone)> {
    a.cleartext.then_some(("http", Tone::Warn))
}

/// The accounts other than the signed-in default one, in store order.
#[cfg(test)]
pub fn other_accounts(all: &[store::Account]) -> Vec<OtherAccount> {
    other_accounts_with(all, |_| None)
}

/// `other_accounts` with each account's token expiry (`expiry`, from the store).
pub fn other_accounts_with(
    all: &[store::Account],
    expiry: impl Fn(&AccountId) -> Option<chrono::NaiveDate>,
) -> Vec<OtherAccount> {
    all.iter()
        .filter(|a| a.id != AccountId::default_bitbucket())
        .map(|a| OtherAccount {
            id: a.id.clone(),
            forge: a.forge,
            host: a.git_host.clone(),
            name: a.display_name.clone(),
            enabled: a.enabled,
            expires: expiry(&a.id),
            paused_until: None,
            cleartext: [&a.web_base, &a.api_base]
                .iter()
                .any(|u| u.to_ascii_lowercase().starts_with("http://")),
        })
        .collect()
}

/// The removal confirmation's summary of what `Store::removal_counts` found.
pub fn removal_summary(c: &store::RemovedCounts) -> String {
    let plural = |n: usize, one: &str| {
        if n == 1 {
            format!("{n} {one}")
        } else {
            format!("{n} {one}s")
        }
    };
    format!(
        "Removes {}, keeps {} marked",
        plural(c.prs, "cached PR"),
        plural(c.drafts_kept, "draft")
    )
}

/// The warning under an account whose token expires within 14 days (spec 5.3), or None.
pub fn account_expiry_status(a: &OtherAccount, today: chrono::NaiveDate) -> Option<(String, Tone)> {
    crate::checktoken::gitlab::expiry_warning(a.expires, today).map(|w| {
        let mut w = w;
        if let Some(first) = w.get(..1) {
            w = format!("{}{}", first.to_uppercase(), &w[1..]);
        }
        (
            format!("{w}: create a new token and add the account again"),
            Tone::Warn,
        )
    })
}

/// (status text, tone) of another account's row.
pub fn other_account_status(a: &OtherAccount) -> (String, Tone) {
    let paused = a.paused_until.filter(|t| *t > std::time::SystemTime::now());
    match (a.enabled, paused) {
        (false, _) => (
            "Disabled: not synced, its repository choice is kept".into(),
            Tone::Muted,
        ),
        (true, Some(t)) => (
            format!(
                "Paused by the rate limit until {}",
                crate::backend::clock(t)
            ),
            Tone::Warn,
        ),
        (true, None) => ("Synced with the other accounts".into(), Tone::Good),
    }
}

/// Shown next to a forge that cannot be added yet.
pub const COMING_SOON: &str = "coming soon";

/// The forges "Add account" offers, and whether each can be picked yet (GitLab is
/// read-only in this version).
pub fn forge_choices() -> [(Forge, bool); 3] {
    [
        (Forge::BitbucketCloud, true),
        (Forge::GitHub, true),
        (Forge::GitLab, true),
    ]
}

/// What a GitLab token needs (spec 5.3), shown under the form.
pub const GITLAB_TOKEN_HELP: &str = "Use a personal access token with the api scope to \
    comment and approve (it also covers git), or read_api and read_repository to only read. \
    Enter gitlab.com, or your self-managed GitLab's web address, including its path if it \
    runs under one (e.g. https://git.example.com/gitlab).";

/// Shown under an http address in the form and on such an account in the list.
pub const CLEARTEXT_WARNING: &str = prcore::origin::CLEARTEXT_WARNING;

/// The web address prefilled for a GitHub account.
pub const GITHUB_DEFAULT_URL: &str = "https://github.com";

/// What a GitHub token needs (spec 5.3), shown under the form.
pub const GITHUB_TOKEN_HELP: &str = "Use a fine-grained personal access token with access to \
    the repositories you review and the permissions Pull requests: read and write, Contents: read (for \
    git) and Metadata: read. A fine-grained token belongs to one owner: add one account per \
    organization, or use a classic token with the repo scope. With SAML single sign-on, \
    authorize the token for the organization first.";

/// A checked "Add account" entry. `Debug` never shows the token.
#[derive(Clone, PartialEq, Eq)]
pub enum NewAccount {
    Bitbucket { email: String, token: String },
    GitHub { bases: GitHubBases, token: String },
    GitLab { bases: GitLabBases, token: String },
}

impl std::fmt::Debug for NewAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NewAccount::Bitbucket { email, .. } => f
                .debug_struct("Bitbucket")
                .field("email", email)
                .field("token", &"***")
                .finish(),
            NewAccount::GitHub { bases, .. } => f
                .debug_struct("GitHub")
                .field("bases", bases)
                .field("token", &"***")
                .finish(),
            NewAccount::GitLab { bases, .. } => f
                .debug_struct("GitLab")
                .field("bases", bases)
                .field("token", &"***")
                .finish(),
        }
    }
}

/// The "Add account" form. Not `Debug`: it holds the typed token until it is submitted.
#[derive(Default)]
pub struct AddAccountForm {
    pub open: bool,
    pub forge: Forge,
    pub email: String,
    /// GitHub's web address (github.com or a GitHub Enterprise host); empty is github.com.
    pub web_url: String,
    pub token: String,
    /// Checking the token right now (the caller clears it when the answer arrives).
    pub busy: bool,
    /// The last outcome to show (e.g. "Added Bo"), or why an add or removal failed.
    pub result: Option<Result<String, String>>,
    /// The answer of "Test connection" (who the token signs in as, or why not).
    pub tested: Option<Result<String, String>>,
    /// "I understand" ticked for a plain-http address.
    pub cleartext_ok: bool,
}

impl AddAccountForm {
    /// A GitHub or GitLab address typed as plain `http://` (shows the cleartext warning).
    pub fn cleartext(&self) -> bool {
        matches!(self.forge, Forge::GitHub | Forge::GitLab)
            && self
                .web_url
                .trim()
                .to_ascii_lowercase()
                .starts_with("http://")
    }

    /// Add and Test connection are offered: an http address needs "I understand" ticked.
    pub fn can_submit(&self) -> bool {
        !self.cleartext() || self.cleartext_ok
    }

    pub(super) fn check_cleartext(&self) -> Result<(), String> {
        match self.can_submit() {
            true => Ok(()),
            false => Err("Tick \"I understand\" to use an unencrypted http connection".into()),
        }
    }

    /// The cleaned account to add (or test), or why not.
    pub fn submit(&self) -> Result<NewAccount, String> {
        let (enabled, label) = forge_choices()
            .into_iter()
            .find(|(f, _)| *f == self.forge)
            .map_or((false, ""), |(f, on)| (on, f.label()));
        if !enabled {
            return Err(format!("{label} accounts are {COMING_SOON}"));
        }
        let token = crate::welcome::clean_token(&self.token).to_string();
        match self.forge {
            Forge::GitHub => {
                let url = match self.web_url.trim() {
                    "" => GITHUB_DEFAULT_URL,
                    u => u,
                };
                let bases = providers::github::bases_for(url)?;
                self.check_cleartext()?;
                if token.is_empty() {
                    return Err("Paste a personal access token".into());
                }
                Ok(NewAccount::GitHub { bases, token })
            }
            Forge::GitLab => {
                let url = match self.web_url.trim() {
                    "" => providers::gitlab::GITLAB_COM,
                    u => u,
                };
                let bases = providers::gitlab::bases_for(url)?;
                self.check_cleartext()?;
                if token.is_empty() {
                    return Err("Paste a personal access token".into());
                }
                Ok(NewAccount::GitLab { bases, token })
            }
            Forge::BitbucketCloud => {
                let email = crate::welcome::clean_email(&self.email);
                if !email.contains('@') {
                    return Err("Enter the email of your Atlassian account".into());
                }
                if token.is_empty() {
                    return Err("Paste an API token".into());
                }
                Ok(NewAccount::Bitbucket {
                    email: email.to_string(),
                    token,
                })
            }
        }
    }
}

/// The round initials badge of an account row.
pub(super) fn account_badge(ui: &mut egui::Ui, name: &str, size: f32) {
    let (badge, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    ui.painter()
        .circle_filled(badge.center(), size / 2.0, theme::ACCENT_FILL);
    ui.painter().text(
        badge.center(),
        egui::Align2::CENTER_CENTER,
        initials(name),
        egui::FontId::proportional(size * 0.36),
        Color32::WHITE,
    );
}

/// The forge tag and host of an account, on one line.
pub(super) fn forge_line(ui: &mut egui::Ui, forge: Forge, host: &str) {
    ui.horizontal(|ui| {
        chip(ui, forge.label(), Tone::Muted);
        ui.label(RichText::new(host).color(theme::MUTED));
    });
}

/// The Accounts card: the signed-in account, the others, and "Add account". Returns an
/// action for the caller (an account to add).
pub(super) fn account_card(ui: &mut egui::Ui, state: &mut SettingsState) -> Option<SettingsAction> {
    let (chip_text, tone, detail) = conn_label(&state.account.conn);
    let mut action = None;
    card(
        ui,
        "Accounts",
        "The accounts nit reads and comments as. Each repository syncs through one of them.",
        Some((chip_text, tone)),
        |ui| {
            ui.horizontal(|ui| {
                let name = state.account.name.trim();
                account_badge(ui, name, 44.0);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    let shown = if name.is_empty() {
                        "Signed-in user not known yet"
                    } else {
                        name
                    };
                    ui.label(
                        RichText::new(shown)
                            .size(15.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                    let email = state.account.email.trim();
                    if !email.is_empty() {
                        ui.label(RichText::new(email).color(theme::MUTED));
                    }
                    forge_line(ui, Forge::BitbucketCloud, prcore::model::BITBUCKET_HOST);
                    status_line(ui, tone, &detail);
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button(ui, true, "Sign out...", 0.0)
                        .on_hover_text(format!(
                            "Delete the saved email and token from {}",
                            store::CREDENTIAL_STORE
                        ))
                        .clicked()
                    {
                        state.confirm_sign_out = true;
                    }
                });
            });
            for other in state.others.clone() {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                let name = if other.name.trim().is_empty() {
                    other.id.as_str()
                } else {
                    other.name.trim()
                };
                ui.horizontal(|ui| {
                    account_badge(ui, &other.name, 36.0);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        match state.renaming.as_mut().filter(|(id, _)| *id == other.id) {
                            Some((_, text)) => {
                                let mut done = None;
                                ui.horizontal(|ui| {
                                    let edit = ui.add(
                                        egui::TextEdit::singleline(text)
                                            .desired_width(200.0)
                                            .margin(egui::vec2(8.0, 4.0))
                                            .hint_text("Label (empty: the login)"),
                                    );
                                    let enter = edit.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    if button(ui, true, "Save", 0.0).clicked() || enter {
                                        done = Some(true);
                                    }
                                    if button(ui, true, "Cancel", 0.0).clicked() {
                                        done = Some(false);
                                    }
                                });
                                match done {
                                    Some(true) => {
                                        action = Some(SettingsAction::RenameAccount(
                                            other.id.clone(),
                                            text.trim().to_string(),
                                        ));
                                        state.renaming = None;
                                    }
                                    Some(false) => state.renaming = None,
                                    None => {}
                                }
                            }
                            None => {
                                ui.label(RichText::new(name).strong().color(Color32::WHITE));
                            }
                        }
                        ui.horizontal(|ui| {
                            forge_line(ui, other.forge, &other.host);
                            if let Some((text, tone)) = cleartext_chip(&other) {
                                chip(ui, text, tone);
                            }
                        });
                        if other.cleartext {
                            status_line(ui, Tone::Warn, CLEARTEXT_WARNING);
                        }
                        let (text, tone) = other_account_status(&other);
                        status_line(ui, tone, &text);
                        let today = chrono::Local::now().date_naive();
                        if let Some((warn, tone)) = account_expiry_status(&other, today) {
                            status_line(ui, tone, &warn);
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, true, "Remove...", 0.0)
                            .on_hover_text(
                                "Delete this account's saved credentials and its cached data",
                            )
                            .clicked()
                        {
                            state.confirm_remove = Some(other.clone());
                            state.removal_counts = None;
                        }
                        let (toggle, hint) = if other.enabled {
                            ("Disable", "Stop syncing it; its repository choice is kept")
                        } else {
                            ("Enable", "Sync it again with its kept repository choice")
                        };
                        if button(ui, true, toggle, 0.0).on_hover_text(hint).clicked() {
                            action =
                                Some(SettingsAction::SetEnabled(other.id.clone(), !other.enabled));
                        }
                        if button(ui, state.renaming.is_none(), "Rename", 0.0)
                            .on_hover_text("The name this account is shown by")
                            .clicked()
                        {
                            state.renaming = Some((other.id.clone(), name.to_string()));
                        }
                    });
                });
            }
            ui.add_space(10.0);
            if state.add.open {
                action = add_account_form(ui, &mut state.add);
            } else {
                ui.horizontal(|ui| {
                    if button(ui, true, "Add account", 0.0)
                        .on_hover_text("Sign in with another account")
                        .clicked()
                    {
                        state.add.open = true;
                        state.add.result = None;
                    }
                    match &state.add.result {
                        Some(Ok(note)) => status_line(ui, Tone::Good, note),
                        Some(Err(e)) => status_line(ui, Tone::Bad, e),
                        None => {}
                    }
                });
            }
        },
    );
    action
}

/// The inline "Add account" form: forge, email, API token (masked), Add / Cancel.
pub(super) fn add_account_form(
    ui: &mut egui::Ui,
    form: &mut AddAccountForm,
) -> Option<SettingsAction> {
    let mut action = None;
    ui.label(RichText::new("Add account").strong());
    ui.horizontal(|ui| {
        for (forge, enabled) in forge_choices() {
            let text = if enabled {
                forge.label().to_string()
            } else {
                format!("{} ({COMING_SOON})", forge.label())
            };
            ui.add_enabled_ui(enabled && !form.busy, |ui| {
                ui.radio_value(&mut form.forge, forge, text)
                    .on_disabled_hover_text(format!(
                        "{} accounts arrive in a later version",
                        forge.label()
                    ));
            });
        }
    });
    ui.add_space(4.0);
    let github = form.forge == Forge::GitHub;
    let gitlab = form.forge == Forge::GitLab;
    let pat = github || gitlab;
    ui.add_enabled_ui(!form.busy, |ui| {
        if github {
            ui.add(
                egui::TextEdit::singleline(&mut form.web_url)
                    .desired_width(320.0)
                    .margin(egui::vec2(8.0, 5.0))
                    .hint_text(GITHUB_DEFAULT_URL),
            )
            .on_hover_text(
                "github.com, or the https:// address of your GitHub Enterprise server \
                 (http:// only on a private or local address)",
            );
        } else if gitlab {
            ui.add(
                egui::TextEdit::singleline(&mut form.web_url)
                    .desired_width(320.0)
                    .margin(egui::vec2(8.0, 5.0))
                    .hint_text(providers::gitlab::GITLAB_COM),
            )
            .on_hover_text(
                "gitlab.com, or the https:// address of your self-managed GitLab (with its \
                 path, e.g. https://git.example.com/gitlab; http:// only on a private or \
                 local address)",
            );
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut form.email)
                    .desired_width(320.0)
                    .margin(egui::vec2(8.0, 5.0))
                    .hint_text("Atlassian account email"),
            );
        }
        ui.add(
            egui::TextEdit::singleline(&mut form.token)
                .password(true)
                .desired_width(320.0)
                .margin(egui::vec2(8.0, 5.0))
                .hint_text(if pat {
                    "Personal access token"
                } else {
                    "API token"
                }),
        );
    });
    if form.cleartext() {
        ui.add_space(2.0);
        ui.add(
            egui::Label::new(
                RichText::new(CLEARTEXT_WARNING)
                    .strong()
                    .color(theme::AMBER),
            )
            .wrap(),
        );
        ui.add_enabled_ui(!form.busy, |ui| {
            ui.checkbox(
                &mut form.cleartext_ok,
                "I understand: use this unencrypted connection anyway",
            );
        });
    }
    if github {
        ui.add(egui::Label::new(RichText::new(GITHUB_TOKEN_HELP).small().weak()).wrap());
    } else if gitlab {
        ui.add(egui::Label::new(RichText::new(GITLAB_TOKEN_HELP).small().weak()).wrap());
    }
    ui.add_space(4.0);
    let can = !form.busy && form.can_submit();
    ui.horizontal(|ui| {
        if primary_button(ui, can, "Add").clicked() {
            match form.submit() {
                Ok(new) => {
                    form.busy = true;
                    form.result = None;
                    form.tested = None;
                    form.token.clear();
                    action = Some(SettingsAction::AddAccount(new));
                }
                Err(e) => form.result = Some(Err(e)),
            }
        }
        if pat && button(ui, can, "Test connection", 0.0).clicked() {
            match form.submit() {
                Ok(new) => {
                    form.busy = true;
                    form.result = None;
                    form.tested = None;
                    action = Some(SettingsAction::TestConnection(new));
                }
                Err(e) => form.result = Some(Err(e)),
            }
        }
        if button(ui, !form.busy, "Cancel", 0.0).clicked() {
            form.open = false;
            form.cleartext_ok = false;
            form.token.clear();
            form.result = None;
            form.tested = None;
        }
        if form.busy {
            status_line(ui, Tone::Busy, "Checking the token...");
        }
    });
    match &form.tested {
        Some(Ok(m)) => status_line(ui, Tone::Good, m),
        Some(Err(e)) => status_line(ui, Tone::Bad, e),
        None => {}
    }
    if let Some(Err(e)) = &form.result {
        status_line(ui, Tone::Bad, e);
    }
    action
}

/// The Remove question for another account; `Some(true)` confirmed.
pub(super) fn remove_confirm(
    ctx: &egui::Context,
    other: &OtherAccount,
    counts: Option<&Result<store::RemovedCounts, String>>,
) -> Option<bool> {
    let mut answer = None;
    let resp = egui::Modal::new(egui::Id::unique("settings-remove-account")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading(format!("Remove {}?", other.name));
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(format!(
                "nit deletes this {} account's saved credentials from Windows Credential \
                 Manager, its repository choices and its cached pull requests. Drafts stay on \
                 this computer. The other accounts are not touched.",
                other.forge.label()
            ))
            .wrap(),
        );
        ui.add_space(6.0);
        match counts {
            Some(Ok(c)) => status_line(ui, Tone::Warn, &removal_summary(c)),
            Some(Err(e)) => status_line(ui, Tone::Bad, e),
            None => status_line(ui, Tone::Busy, "Counting what it removes..."),
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let remove = ui.add(
                egui::Button::new(RichText::new("Remove").strong().color(Color32::WHITE))
                    .fill(theme::RED.gamma_multiply(0.75))
                    .min_size(egui::vec2(0.0, ROW_H)),
            );
            if remove.clicked() {
                answer = Some(true);
            }
            if button(ui, true, "Cancel", 0.0).clicked() {
                answer = Some(false);
            }
        });
    });
    if answer.is_none() && resp.should_close() {
        answer = Some(false);
    }
    answer
}

/// The Sign out question; `Some(true)` confirmed, `Some(false)` cancelled.
pub(super) fn sign_out_confirm(ctx: &egui::Context) -> Option<bool> {
    let mut answer = None;
    let resp = egui::Modal::new(egui::Id::unique("settings-sign-out")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading("Sign out?");
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(format!(
                "nit deletes the saved email and API token from {} and returns to the \
                 sign-in screen. Cached pull requests, drafts and these settings stay on this \
                 computer.",
                store::CREDENTIAL_STORE
            ))
            .wrap(),
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let sign_out = ui.add(
                egui::Button::new(RichText::new("Sign out").strong().color(Color32::WHITE))
                    .fill(theme::RED.gamma_multiply(0.75))
                    .min_size(egui::vec2(0.0, ROW_H)),
            );
            if sign_out.clicked() {
                answer = Some(true);
            }
            if button(ui, true, "Cancel", 0.0).clicked() {
                answer = Some(false);
            }
        });
    });
    if answer.is_none() && resp.should_close() {
        answer = Some(false);
    }
    answer
}
