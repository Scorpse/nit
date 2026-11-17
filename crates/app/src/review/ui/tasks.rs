//! The Tasks tab: the PR's tasks and a box to add one.

use prcore::comments::TaskState;

use super::*;

pub(super) fn draw_tasks(ui: &mut Ui, s: &mut ReviewScreen) {
    let read_only = s.composer.read_only;
    let tasks = &mut s.tasks;
    egui::ScrollArea::vertical()
        .id_salt("review-tasks")
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.label(RichText::new("Tasks").strong().size(15.0));
            ui.add_space(4.0);
            match &tasks.list {
                None if s.comments.loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading tasks...");
                    });
                }
                None => {
                    ui.colored_label(AMBER, "The tasks could not be read.");
                }
                Some(list) if list.is_empty() => {
                    ui.colored_label(GREY, "No tasks yet.");
                }
                Some(list) => {
                    // open ones first; each group in the order the server lists them
                    let open = list.iter().filter(|t| t.state == TaskState::Open);
                    let done = list.iter().filter(|t| t.state == TaskState::Resolved);
                    for t in open.chain(done) {
                        let (mark, color) = match t.state {
                            TaskState::Open => ("open", AMBER),
                            TaskState::Resolved => ("done", GREEN),
                        };
                        ui.horizontal_top(|ui| {
                            ui.label(RichText::new(mark).color(color).strong());
                            ui.vertical(|ui| {
                                ui.add(egui::Label::new(&t.content).wrap());
                                let who = &t.creator.display_name;
                                let note = match t.comment_id {
                                    Some(_) => format!("{who}, on a comment"),
                                    None => who.clone(),
                                };
                                ui.label(RichText::new(note).small().color(GREY));
                            });
                        });
                        ui.add_space(4.0);
                    }
                }
            }
            ui.separator();
            ui.label(RichText::new("New task").strong());
            let can = !read_only && !tasks.creating;
            ui.add_enabled(
                can,
                egui::TextEdit::multiline(&mut tasks.draft)
                    .hint_text("What needs doing?")
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            ui.horizontal(|ui| {
                let ready = can && !tasks.draft.trim().is_empty();
                if ui
                    .add_enabled(ready, egui::Button::new("Add task"))
                    .on_disabled_hover_text(if read_only {
                        super::super::composer::READ_ONLY_TIP
                    } else {
                        "Write the task first"
                    })
                    .clicked()
                {
                    tasks.submit();
                }
                if tasks.creating {
                    ui.spinner();
                    ui.label("Adding...");
                }
            });
            if let Some(e) = &tasks.error {
                ui.colored_label(RED, e);
            }
        });
}
