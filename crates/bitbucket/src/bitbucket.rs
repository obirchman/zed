//! Bitbucket Cloud in Zed: the REST client, typed models, and the store and
//! watcher every Bitbucket view reads from.
//!
//! `BitbucketStore` is the one place Zed reads Bitbucket from: every view asks
//! the store, the store caches the latest answer per pull request, comment list
//! and pipeline list, and emits `StoreEvent` when one changes.

pub mod api;
pub mod changes;
mod clones;
mod http;
pub mod models;
mod persistence;
pub mod repo;
pub mod reviewer;
mod settings;
mod store;
mod watch;

pub use repo::RepoRef;
pub use clones::find_local_clone;
pub use http::ZedHttp;
pub use settings::BitbucketSettings;
pub use store::{BitbucketStore, Connection, PipelineKey, PrData, PrKey, StoreEvent};
pub use watch::{PrWatch, WatchEvent, WatchedPr};

pub fn init(cx: &mut gpui::App) {
    BitbucketStore::init_global(cx);
    PrWatch::init_global(cx);
}
