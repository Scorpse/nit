//! Comment drafts: the composer, local-first storage, the confirmed sequential submit,
//! replies and resolve / unresolve.
//!
//! Drafts live in the `Store` until the server accepted them (2xx): a draft is deleted only
//! after its post succeeded, so a failed post or a restart never loses one. A draft is
//! marked in the store right before it is sent; when the answer is lost after sending
//! (timeout, reset, 5xx) or the app stops meanwhile, the draft is "unconfirmed" and never
//! submitted again until the user checked the comments and chose "Post again anyway".
//! Inline drafts are re-checked right before posting against the
//! server's current source commit and the file's diff rows (`inline_allowed`); a blocked
//! one is never posted and never silently turned into another kind of comment.
//!
//! `Composer` is the testable state; the `*_card` / `drafts_bar` / `confirm_dialog` /
//! `toast` functions only paint it and push `Act`s that `ReviewScreen::act` applies.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, vec2, Align2, Color32, FontId, Id, Key, Modifiers, Painter, Pos2, Rect, RichText, Sense,
    Stroke, StrokeKind, Ui,
};
use prcore::comment_type::CommentType;
use prcore::comments::{
    inline_allowed, AnchorModel, Comment, CommentId, InlineBlock, NewComment, PostRevision,
    ReviewAction, ServerRevision, Thread, ThreadId,
};
use prcore::commit::same_commit as same_commit_strict;
use prcore::diff::{Row, RowKind};
use prcore::draft::{Draft, DraftKind, Side};
use prcore::model::{Forge, PullRequest, RepoRef, UserRef};
use providers::provider::{capabilities_for, SuggestionSyntax};
use store::Store;

use crate::lock::lock_ok;

use super::comments_ui::{
    body_galley, body_width, Comments, Placement, AUTHOR_H, CARD_MARGIN, CARD_PAD, COMMENT_GAP,
    TITLE_H,
};
pub use super::memory_poster::MemoryPoster;

mod batch;
mod editing;
mod model;
mod paint;
mod plan;
mod state;
mod submit;
#[cfg(test)]
mod tests;

use batch::*;
pub use model::*;
pub use paint::*;
pub use state::*;
