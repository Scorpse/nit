//! Review screen: a background service opens a PR from local git and serves per-file
//! patches (worker pool, LRU cache, neighbour prefetch); the screen shows the file list
//! and a virtualized diff of the selected file.

pub mod actions_ui;
pub mod authors;
mod cache;
pub mod columns;
pub mod comments_ui;
pub mod composer;
pub mod diffview;
pub mod export_ui;
pub mod files;
mod highlight;
pub mod keymap;
mod memory_poster;
pub mod search;
pub mod selection;
mod service;
pub mod split;
pub mod types_ui;
pub mod ui;
mod worddiff;

#[allow(unused_imports)]
pub use cache::{PatchCache, PATCH_CACHE_BYTES};
#[allow(unused_imports)]
pub use service::{
    remember_clones, CommentFetcher, DepsFn, HeadProbe, ReviewDeps, ReviewEvent, ReviewService,
    PLAIN_CHANGED_LINES, PREFETCH_RADIUS,
};
