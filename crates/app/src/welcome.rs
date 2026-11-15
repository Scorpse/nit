//! Welcome and sign-in screen: logo, app name, the email / API token form, help on creating
//! a token, and a footer with the version and whether git was found. Pure UI: the caller
//! owns the login state and acts on the returned `Action`.

use eframe::egui::{self, epaint::Shadow, Color32, RichText, Stroke};

use crate::theme;

/// Atlassian's API token page (a constant: nothing user-supplied goes into it).
pub const TOKEN_PAGE: &str = "https://id.atlassian.com/manage-profile/security/api-tokens";

const CARD_W: f32 = 460.0;
const CARD_PAD: i8 = 28;
const WEAK: Color32 = Color32::from_rgb(150, 156, 168);

/// The email as it is saved: surrounding whitespace removed.
pub fn clean_email(email: &str) -> &str {
    email.trim()
}

/// The token as it is saved: whitespace and one pair of surrounding quotes removed (a
/// token copied from a quoted config or a terminal often carries them).
pub fn clean_token(token: &str) -> &str {
    let t = token.trim();
    let unquoted = ['"', '\'']
        .iter()
        .find_map(|q| t.strip_prefix(*q).and_then(|r| r.strip_suffix(*q)));
    unquoted.unwrap_or(t).trim()
}

/// Whether Connect can be pressed, or why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectState {
    Ready,
    Disabled(&'static str),
}

pub fn connect_state(email: &str, token: &str, busy: bool) -> ConnectState {
    let email = clean_email(email);
    if busy {
        ConnectState::Disabled("Connecting...")
    } else if email.is_empty() {
        ConnectState::Disabled("Enter your Atlassian email")
    } else if !email.contains('@') {
        ConnectState::Disabled("The email needs an @")
    } else if clean_token(token).is_empty() {
        ConnectState::Disabled("Paste your API token")
    } else {
        ConnectState::Ready
    }
}

/// Footer line for the startup git check: None while it runs, then the `git --version`
/// output (None when git could not be used).
pub fn git_line(check: Option<Option<&str>>) -> String {
    match check {
        None => "checking for git...".into(),
        Some(None) => "git not found: install Git for Windows".into(),
        Some(Some(v)) => {
            let num = v.trim().strip_prefix("git version ").unwrap_or("");
            let mut parts = num.split('.');
            match (parts.next(), parts.next()) {
                (Some(a), Some(b))
                    if !a.is_empty()
                        && a.bytes().all(|c| c.is_ascii_digit())
                        && b.bytes().all(|c| c.is_ascii_digit())
                        && !b.is_empty() =>
                {
                    format!("git {a}.{b} found")
                }
                _ => "git found".into(),
            }
        }
    }
}

/// "nit 0.1.0 · git 2.53 found"
pub fn footer_line(version: &str, git: &str) -> String {
    format!("nit {version} \u{00b7} {git}")
}

/// What the form holds this frame (borrowed from the caller's login state).
pub struct Form<'a> {
    pub email: &'a mut String,
    pub token: &'a mut String,
    pub error: Option<&'a str>,
    pub busy: bool,
    pub has_saved: bool,
    /// `git_line` input.
    pub git: Option<Option<&'a str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Connect,
    Retry,
    SignOut,
    OpenTokenPage,
}

/// Paints a soft vertical gradient behind the card.
fn paint_background(ui: &egui::Ui) {
    let rect = ui.max_rect();
    let top = Color32::from_rgb(30, 34, 44);
    let bottom = Color32::from_rgb(14, 15, 19);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    ui.painter().add(mesh);
}

fn error_callout(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::ERROR.gamma_multiply(0.14))
        .stroke(Stroke::new(1.0, theme::ERROR.gamma_multiply(0.6)))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(RichText::new(text).color(Color32::from_rgb(255, 160, 160)))
                    .wrap(),
            );
        });
}

fn help_block(ui: &mut egui::Ui, act: &mut Option<Action>) {
    egui::Frame::new()
        .fill(theme::BG_DEEP)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(6)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Create an API token").strong());
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Atlassian account settings > Security > API tokens: \
                         \"Create API token with scopes\", choose Bitbucket.",
                    )
                    .color(WEAK),
                )
                .wrap(),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Scopes: read repository, read pull request, write pull request.",
                    )
                    .color(WEAK),
                )
                .wrap(),
            );
            if ui
                .button("Open token page")
                .on_hover_text(TOKEN_PAGE)
                .clicked()
            {
                *act = Some(Action::OpenTokenPage);
            }
        });
}

/// Draws the screen; returns what the user asked for this frame.
pub fn show(root: &mut egui::Ui, form: Form<'_>) -> Option<Action> {
    let mut act = None;
    egui::CentralPanel::default()
        .frame(egui::Frame::new())
        .show(root, |ui| {
            paint_background(ui);
            // short windows (down to the 600 px minimum) get a tighter card
            let compact = ui.available_height() < 720.0;
            let pad: i8 = if compact { 20 } else { CARD_PAD };
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // centre the card vertically using its height from the last frame
                    let id = egui::Id::unique("welcome-card-h");
                    let last_h: f32 = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(560.0);
                    ui.add_space(((ui.available_height() - last_h) / 2.0).max(16.0));
                    ui.vertical_centered(|ui| {
                        let width = CARD_W.min(ui.available_width() - 32.0);
                        let card = egui::Frame::new()
                            .fill(theme::SURFACE)
                            .stroke(Stroke::new(1.0, theme::BORDER))
                            .corner_radius(10)
                            .inner_margin(egui::Margin::same(pad))
                            .shadow(Shadow {
                                offset: [0, 10],
                                blur: 32,
                                spread: 0,
                                color: Color32::from_black_alpha(110),
                            })
                            .show(ui, |ui| {
                                ui.set_width(width - 2.0 * pad as f32);
                                // the form reads left to right; only the header is centred
                                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                    card(ui, form, compact, &mut act)
                                });
                            });
                        let h = card.response.rect.height();
                        ui.ctx().data_mut(|d| d.insert_temp(id, h));
                    });
                    ui.add_space(16.0);
                });
        });
    act
}

fn card(ui: &mut egui::Ui, form: Form<'_>, compact: bool, act: &mut Option<Action>) {
    // vertical gaps shrink in a short window
    let gap = |n: f32| if compact { (n * 0.5).round() } else { n };
    let field_margin = egui::vec2(8.0, if compact { 4.0 } else { 6.0 });
    let Form {
        email,
        token,
        error,
        busy,
        has_saved,
        git,
    } = form;
    ui.vertical_centered(|ui| {
        let (logo, _) = ui.allocate_exact_size(
            egui::Vec2::splat(if compact { 40.0 } else { 52.0 }),
            egui::Sense::hover(),
        );
        theme::paint_logo(ui.painter(), logo);
        ui.add_space(gap(6.0));
        ui.label(
            RichText::new(crate::brand::NAME)
                .size(if compact { 26.0 } else { 30.0 })
                .strong()
                .color(Color32::WHITE),
        );
        ui.label(RichText::new("Review big pull requests locally. Fast.").color(WEAK));
    });
    ui.add_space(gap(18.0));
    ui.label("Sign in to Bitbucket Cloud with your Atlassian email and an API token.");
    ui.add_space(gap(10.0));

    let field_w = ui.available_width();
    ui.label(RichText::new("Atlassian email").strong());
    ui.add(
        egui::TextEdit::singleline(email)
            .desired_width(field_w)
            .margin(field_margin)
            .hint_text("you@example.com"),
    );
    ui.add_space(gap(6.0));
    ui.label(RichText::new("API token").strong());
    let token_field = ui.add(
        egui::TextEdit::singleline(token)
            .password(true)
            .desired_width(field_w)
            .margin(field_margin)
            .hint_text("paste your API token"),
    );
    ui.add_space(gap(12.0));

    let state = connect_state(email, token, busy);
    let ready = state == ConnectState::Ready;
    let enter = token_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    let fill = if ready {
        theme::ACCENT_FILL
    } else {
        ui.visuals().widgets.inactive.weak_bg_fill
    };
    // centred layout: a button wider than its text centres the text
    let connect = ui
        .vertical_centered(|ui| {
            ui.add_enabled(
                ready,
                egui::Button::new(RichText::new("Connect").strong().color(Color32::WHITE))
                    .fill(fill)
                    .min_size(egui::vec2(field_w, if compact { 32.0 } else { 36.0 })),
            )
        })
        .inner;
    let reason = match state {
        ConnectState::Disabled(why) => Some(why),
        ConnectState::Ready => None,
    };
    if let Some(why) = reason {
        connect.on_disabled_hover_text(why);
    } else if connect.clicked() || enter {
        *act = Some(Action::Connect);
    }
    if busy {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Connecting...");
        });
    } else if let Some(why) = reason {
        ui.label(RichText::new(why).small().color(WEAK));
    }
    if has_saved {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if !busy && ui.button("Retry with saved credentials").clicked() {
                *act = Some(Action::Retry);
            }
            if ui
                .button("Sign out")
                .on_hover_text("Delete the saved email and token")
                .clicked()
            {
                *act = Some(Action::SignOut);
            }
        });
    }
    if let Some(e) = error {
        ui.add_space(8.0);
        error_callout(ui, e);
    }

    ui.add_space(gap(14.0));
    help_block(ui, act);
    ui.add_space(gap(10.0));
    ui.add(
        egui::Label::new(
            RichText::new(format!(
                "Your token is stored in {}, never in files.",
                store::CREDENTIAL_STORE
            ))
            .small()
            .color(WEAK),
        )
        .wrap(),
    );
    ui.add_space(gap(14.0));
    ui.separator();
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(footer_line(env!("CARGO_PKG_VERSION"), &git_line(git)))
                .small()
                .color(WEAK),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_is_disabled_with_a_reason_until_the_form_is_valid() {
        use ConnectState::*;
        let cases = [
            ("", "", false, Disabled("Enter your Atlassian email")),
            ("   ", "tok", false, Disabled("Enter your Atlassian email")),
            ("ana", "tok", false, Disabled("The email needs an @")),
            (
                "ana@example.com",
                "",
                false,
                Disabled("Paste your API token"),
            ),
            (
                "ana@example.com",
                "  \"\" ",
                false,
                Disabled("Paste your API token"),
            ),
            (" ana@example.com ", " tok ", false, Ready),
            ("ana@example.com", "tok", true, Disabled("Connecting...")),
        ];
        for (email, token, busy, want) in cases {
            assert_eq!(
                connect_state(email, token, busy),
                want,
                "{email:?} {token:?}"
            );
        }
    }

    #[test]
    fn email_and_token_are_cleaned_before_saving() {
        assert_eq!(clean_email("  ana@example.com\t"), "ana@example.com");
        assert_eq!(clean_token(" abc123 \n"), "abc123");
        assert_eq!(clean_token("\"abc123\""), "abc123");
        assert_eq!(clean_token(" 'abc123' "), "abc123");
        assert_eq!(clean_token("\" abc123 \""), "abc123");
        // unmatched or inner quotes are kept
        assert_eq!(clean_token("\"abc123"), "\"abc123");
        assert_eq!(clean_token("ab\"c"), "ab\"c");
        assert_eq!(clean_token("\""), "\"");
    }

    #[test]
    fn git_and_version_lines() {
        assert_eq!(git_line(None), "checking for git...");
        assert_eq!(
            git_line(Some(None)),
            "git not found: install Git for Windows"
        );
        assert_eq!(
            git_line(Some(Some("git version 2.53.0.windows.1\n"))),
            "git 2.53 found"
        );
        assert_eq!(git_line(Some(Some("git version 2.31.0"))), "git 2.31 found");
        assert_eq!(git_line(Some(Some("something else"))), "git found");
        assert_eq!(
            footer_line("0.1.0", "git 2.53 found"),
            "nit 0.1.0 \u{00b7} git 2.53 found"
        );
    }

    #[test]
    fn token_page_is_a_fixed_https_url() {
        assert!(TOKEN_PAGE.starts_with("https://id.atlassian.com/"));
        assert!(!TOKEN_PAGE.contains(char::is_whitespace));
    }
}
