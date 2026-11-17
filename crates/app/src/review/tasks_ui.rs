//! The PR's tasks (Bitbucket) and the text of the one being added.

use prcore::comments::{PrTask, TaskState};

/// What the Tasks tab shows.
#[derive(Debug, Default)]
pub struct Tasks {
    /// `None` until fetched, or when the tasks could not be read.
    pub list: Option<Vec<PrTask>>,
    /// The text of the task being written.
    pub draft: String,
    /// A task is being created: its text is on its way.
    pub creating: bool,
    pub error: Option<String>,
    wants: Vec<String>,
}

impl Tasks {
    pub fn set_list(&mut self, list: Option<Vec<PrTask>>) {
        self.list = list;
    }

    pub fn open_count(&self) -> usize {
        self.list
            .iter()
            .flatten()
            .filter(|t| t.state == TaskState::Open)
            .count()
    }

    /// Asks for the draft to become a task. Nothing happens for an empty draft or while one
    /// is on its way (the draft stays until the server has answered).
    pub fn submit(&mut self) {
        let text = self.draft.trim();
        if text.is_empty() || self.creating {
            return;
        }
        self.wants.push(text.to_string());
        self.creating = true;
        self.error = None;
    }

    /// The server's answer: the new task joins the list and the draft is cleared, or the
    /// draft is kept with the reason.
    pub fn created(&mut self, result: Result<PrTask, String>) {
        self.creating = false;
        match result {
            Ok(task) => {
                self.draft.clear();
                let list = self.list.get_or_insert_with(Vec::new);
                if !list.iter().any(|t| t.id == task.id) {
                    list.push(task);
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn take_wants(&mut self) -> Vec<String> {
        std::mem::take(&mut self.wants)
    }
}

#[cfg(test)]
mod tests;
