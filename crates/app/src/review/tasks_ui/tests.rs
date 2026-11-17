use prcore::comments::{PrTask, TaskState};
use prcore::model::UserRef;

use super::*;

fn task(id: u64, state: TaskState) -> PrTask {
    PrTask {
        id,
        state,
        content: format!("task {id}"),
        creator: UserRef {
            id: "{a}".into(),
            display_name: "Ada".into(),
            login: None,
        },
        comment_id: None,
    }
}

fn with_draft(text: &str) -> Tasks {
    Tasks {
        draft: text.into(),
        ..Tasks::default()
    }
}

#[test]
fn only_open_tasks_count() {
    let mut t = Tasks::default();
    assert_eq!(t.open_count(), 0);
    t.set_list(Some(vec![
        task(1, TaskState::Open),
        task(2, TaskState::Resolved),
        task(3, TaskState::Open),
    ]));
    assert_eq!(t.open_count(), 2);
}

#[test]
fn an_empty_draft_or_one_in_flight_is_not_sent() {
    let mut t = with_draft("  \n ");
    t.submit();
    assert!(t.take_wants().is_empty() && !t.creating);
    t.draft = "  add a test \n".into();
    t.submit();
    t.submit();
    assert_eq!(t.take_wants(), ["add a test"], "trimmed, and sent once");
    assert!(t.creating);
}

#[test]
fn a_created_task_joins_the_list_and_clears_the_draft() {
    let mut t = with_draft("x");
    t.submit();
    t.created(Ok(task(5, TaskState::Open)));
    assert!(!t.creating && t.draft.is_empty() && t.error.is_none());
    assert_eq!(t.list.as_ref().unwrap().len(), 1);
    // an answer for a task a refresh already listed is not added twice
    t.created(Ok(task(5, TaskState::Open)));
    assert_eq!(t.list.as_ref().unwrap().len(), 1);
}

#[test]
fn a_refused_task_keeps_the_draft_and_says_why() {
    let mut t = with_draft("x");
    t.submit();
    t.take_wants();
    t.created(Err("not allowed".into()));
    assert!(!t.creating);
    assert_eq!(t.draft, "x");
    assert_eq!(t.error.as_deref(), Some("not allowed"));
    t.submit();
    assert_eq!(t.take_wants(), ["x"], "can try again");
    assert!(t.error.is_none());
}
