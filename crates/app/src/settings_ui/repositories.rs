//! Repositories page: git status and the chosen-repositories card.

use super::*;

/// The oldest git nit works with.
pub const GIT_MIN: (u32, u32) = (2, 31);

/// (major, minor, patch) of a `git --version` line such as `git version 2.47.1.windows.1`.
pub fn parse_git_version(line: &str) -> Option<(u32, u32, u32)> {
    let rest = line.trim().strip_prefix("git version ")?;
    let mut nums = rest.split('.').map(|p| p.parse::<u32>().ok());
    let major = nums.next()??;
    let minor = nums.next()??;
    let patch = nums.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// What the About card says about git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitInfo {
    Checking,
    /// Usable; the version as `2.47.1` (or the raw line when it has an unusual shape).
    Ok(String),
    /// Found but older than `GIT_MIN`.
    Old(String),
    Missing,
}

pub fn git_info(status: Option<&Result<String, String>>) -> GitInfo {
    match status {
        None => GitInfo::Checking,
        Some(Err(_)) => GitInfo::Missing,
        Some(Ok(line)) => match parse_git_version(line) {
            Some((a, b, c)) if (a, b) < GIT_MIN => GitInfo::Old(format!("{a}.{b}.{c}")),
            Some((a, b, c)) => GitInfo::Ok(format!("{a}.{b}.{c}")),
            None => GitInfo::Ok(line.trim().to_string()),
        },
    }
}

/// (value, chip text and tone, hint under it) for the git row.
pub fn git_row(info: &GitInfo) -> (String, Option<(&'static str, Tone)>, Option<&'static str>) {
    const INSTALL: &str = "Reviews need Git for Windows 2.31 or newer. Install or update it, \
                           then restart nit.";
    match info {
        GitInfo::Checking => ("checking...".into(), None, None),
        GitInfo::Ok(v) => (v.clone(), Some(("OK", Tone::Good)), None),
        GitInfo::Old(v) => (v.clone(), Some(("too old", Tone::Warn)), Some(INSTALL)),
        GitInfo::Missing => (
            "not found".into(),
            Some(("missing", Tone::Bad)),
            Some(INSTALL),
        ),
    }
}

/// (headline, explanation) of the Repositories card.
pub fn repos_summary(selected: usize, known: usize) -> (String, &'static str) {
    if known == 0 {
        (
            "No repositories listed yet".into(),
            "Refresh the list to see the repositories of your workspaces.",
        )
    } else if selected == 0 {
        (
            format!("None of {known} chosen"),
            "Nothing is synced until you choose repositories.",
        )
    } else {
        (
            format!("{selected} of {known} chosen"),
            "Their pull requests, sizes and comments are synced; the others are not read.",
        )
    }
}

/// Filled part of the progress chip, 0 to 1.
pub fn repos_fraction(selected: usize, known: usize) -> f32 {
    if known == 0 {
        0.0
    } else {
        (selected as f32 / known as f32).clamp(0.0, 1.0)
    }
}

/// wgpu's backend names as people know them.
pub(super) fn backend_name(raw: &str) -> &str {
    match raw {
        "Dx12" => "DirectX 12",
        "Vulkan" => "Vulkan",
        "Gl" => "OpenGL",
        "Metal" => "Metal",
        "BrowserWebGpu" => "WebGPU",
        other => other,
    }
}

/// `"<adapter> (<Backend>)"` from `gpu::adapter_line` as (adapter, backend); `None` before
/// the window started.
pub fn adapter_parts(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    let open = line.rfind(" (")?;
    let backend = line[open + 2..].strip_suffix(')')?;
    let name = line[..open].trim();
    if name.is_empty() || backend.is_empty() {
        return None;
    }
    Some((name.to_string(), backend_name(backend).to_string()))
}

/// The progress-style chip: the chosen share as a filled bar with "3 / 12" on it.
pub(super) fn repos_meter(ui: &mut egui::Ui, selected: usize, known: usize) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(132.0, 24.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 12.0, theme::BG_DEEP);
    let frac = repos_fraction(selected, known);
    if frac > 0.0 {
        let w = (rect.width() * frac).max(24.0);
        let fill = egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height()));
        p.rect_filled(fill, 12.0, theme::ACCENT_FILL);
    }
    p.rect_stroke(
        rect,
        12.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Inside,
    );
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{selected} / {known}"),
        egui::FontId::proportional(12.5),
        Color32::WHITE,
    );
}

pub(super) fn repos_card(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    action: &mut Option<SettingsAction>,
) {
    let (selected, known) = state.repos;
    let (headline, detail) = repos_summary(selected, known);
    card(
        ui,
        "Repositories",
        "Which of your workspaces' repositories nit keeps in sync.",
        None,
        |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&headline)
                            .size(22.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                    let tone = if known > 0 && selected == 0 {
                        Tone::Warn
                    } else {
                        Tone::Muted
                    };
                    ui.label(RichText::new(detail).color(tone_color(tone)));
                });
                if known > 0 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        repos_meter(ui, selected, known)
                    });
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if primary_button(ui, true, "Choose repositories...")
                    .on_hover_text("Tick the repositories to sync")
                    .clicked()
                {
                    *action = Some(SettingsAction::ChooseRepos);
                }
                if button(ui, !state.discovering, "Refresh list", 0.0)
                    .on_hover_text(
                        "List your workspaces and repositories again (no pull requests are read)",
                    )
                    .clicked()
                {
                    state.discovering = true;
                    state.repos_error = None;
                    *action = Some(SettingsAction::RefreshRepos);
                }
                if state.discovering {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(RichText::new("Listing repositories...").color(theme::MUTED));
                }
            });
            if let Some(e) = &state.repos_error {
                ui.add_space(4.0);
                status_line(ui, Tone::Bad, e);
            }
        },
    );
}
