//! Bitbucket Cloud data for Zed, fetched with the `bbt` library.
//!
//! `BitbucketStore` is the one place Zed reads Bitbucket from: every view asks
//! the store, the store caches the latest answer per pull request, comment list
//! and pipeline list, and emits `StoreEvent` when one changes.

mod http;
mod store;

pub use bbt;
pub use bbt::models;
pub use bbt::repo::RepoRef;
pub use http::ZedHttp;
pub use store::{BitbucketStore, PipelineKey, PrData, PrKey, StoreEvent};

pub fn init(cx: &mut gpui::App) {
    BitbucketStore::init_global(cx);
}
