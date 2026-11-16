//! Opens a PR against local git on std threads and serves parsed per-file patches.
//!
//! Nothing here runs on the egui thread except `open`/`request_patch`/`close`/`poll`,
//! which only touch a mutex and a channel. Every event carries the generation it was
//! produced for; `open` and `close` bump the generation, so late results are dropped.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use gitlocal::{
    batch_patches_cancellable, commit_log, discover_clones, fetch_pr, file_counts, file_list,
    file_patch, find_clone, needs_clone, pick_local, ref_prefix, resolve_commits, FetchSpec, Git,
    GitAuth, GitError, GitProgress, LocalClone, LocalRepo, PrRefs, RepoKind,
};
use prcore::comments::{build_threads, Thread};
use prcore::commit::same_commit_or_unknown;
use prcore::diff::{apply_counts, parse_patch, DiffFile, FileCounts, Row, RowKind};
use prcore::history::{CommitInfo, ReviewMark};
use prcore::model::{Forge, PrState, PullRequest, RepoRef};
use providers::provider::CommentList;
use store::Store;

use super::cache::{PatchCache, PATCH_CACHE_BYTES};
use super::composer::{FileRows, RowSource};
use super::diffview::PatchLayout;
use super::highlight::{
    highlight_chunked, Chunking, HighlightQueue, HlEnd, HlJob, Next, RowSpans, Spans, CHUNK_ROWS,
    HIGHLIGHT_BUDGET,
};
use crate::lock::lock_ok;
use crate::settings_ui::default_download_dir;

mod api;
mod clones;
mod commits;
mod highlights;
mod loading;
mod patches;
mod queue;
mod shared;
mod types;
mod workers;

#[cfg(test)]
mod tests;

pub use api::*;
pub use clones::*;
pub use types::*;

use commits::*;
use highlights::*;
use loading::*;
use patches::*;
use queue::*;
use shared::*;
use workers::*;
