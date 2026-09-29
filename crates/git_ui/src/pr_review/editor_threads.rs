//! Pull request comment threads inside ordinary editors. Every full editor
//! gets an `EditorThreads` controller; for each of its buffers that is a file
//! in a pull request's review worktree, the controller puts that pull
//! request's threads below their lines. Comments written with the diff's
//! review button are posted to the pull request as inline comments.

use super::thread_view::{self, Reply, ThreadHost};
use super::threads::{self, Place};
use bitbucket::{
    BitbucketStore, PrKey, StoreEvent,
    api::{InlineAnchor, NewComment, plan},
};
use collections::{HashMap, HashSet};
use editor::{
    Addon, Editor, EditorEvent,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use gpui::{App, AppContext as _, Context, Entity, Global, Subscription, WeakEntity};
use language::Point;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use ui::prelude::*;

/// Review worktrees on disk and the pull request each holds.
#[derive(Default)]
pub(crate) struct PrWorktrees(HashMap<PathBuf, PrKey>);

impl Global for PrWorktrees {}

pub(crate) fn register_worktree(root: PathBuf, key: PrKey, cx: &mut App) {
    cx.default_global::<PrWorktrees>().0.insert(root, key);
}

pub(crate) fn unregister_worktree(root: &Path, cx: &mut App) {
    cx.default_global::<PrWorktrees>().0.remove(root);
}

/// The pull request whose review worktree holds `abs_path`, and the path
/// relative to that worktree (the path Bitbucket uses).
fn pull_request_for(abs_path: &Path, cx: &App) -> Option<(PrKey, String)> {
    let worktrees = cx.try_global::<PrWorktrees>()?;
    worktrees.0.iter().find_map(|(root, key)| {
        let rel = abs_path.strip_prefix(root).ok()?;
        Some((key.clone(), rel.to_string_lossy().replace('\\', "/")))
    })
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _window, cx: &mut Context<Editor>| {
        if !editor.mode().is_full() || editor.project().is_none() {
            return;
        }
        let handle = cx.entity();
        let multibuffer = editor.buffer().clone();
        let controller = cx.new(|cx| EditorThreads::new(&handle, &multibuffer, cx));
        editor.register_addon(EditorThreadsAddon { _controller: controller });
    })
    .detach();
}

struct EditorThreadsAddon {
    _controller: Entity<EditorThreads>,
}

impl Addon for EditorThreadsAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub(crate) struct EditorThreads {
    editor: WeakEntity<Editor>,
    blocks: Vec<CustomBlockId>,
    reply: Option<(PrKey, Reply)>,
    _subscriptions: Vec<Subscription>,
}

impl EditorThreads {
    /// Runs while `editor` is being built, so `editor` must not be read here.
    fn new(editor: &Entity<Editor>, multibuffer: &Entity<multi_buffer::MultiBuffer>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(multibuffer, |this, _, event: &multi_buffer::Event, cx| {
            if matches!(
                event,
                multi_buffer::Event::BufferRangesUpdated { .. } | multi_buffer::Event::BuffersRemoved { .. }
            ) {
                this.sync(cx);
            }
        }));
        subscriptions.push(cx.subscribe(editor, |this, editor, event: &EditorEvent, cx| {
            if let EditorEvent::ReviewCommentsChanged { total_count } = event
                && *total_count > 0
            {
                this.post_review_comments(&editor, cx);
            }
        }));
        if let Some(store) = BitbucketStore::try_global(cx) {
            subscriptions.push(cx.subscribe(&store, |this, _, event, cx| {
                if matches!(event, StoreEvent::PrUpdated(_)) {
                    this.sync(cx);
                }
            }));
        }
        subscriptions.push(cx.observe_global::<PrWorktrees>(|this, cx| this.sync(cx)));
        // The editor is still being built; place threads once it is.
        cx.spawn(async move |this, cx| this.update(cx, |this, cx| this.sync(cx)).ok())
            .detach();
        Self {
            editor: editor.downgrade(),
            blocks: Vec::new(),
            reply: None,
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn start_reply(&mut self, root_id: u64, prefill: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.key_for_thread(root_id, cx) else {
            return;
        };
        self.reply = Some((key, Reply::new(root_id, prefill, window, cx)));
        self.sync(cx);
    }

    pub(crate) fn cancel_reply(&mut self, cx: &mut Context<Self>) {
        self.reply = None;
        self.sync(cx);
    }

    pub(crate) fn send_reply(&mut self, cx: &mut Context<Self>) {
        let Some((key, reply)) = &self.reply else {
            return;
        };
        let send = reply.send(key, cx);
        cx.spawn(async move |this, cx| {
            if send.await.is_ok() {
                this.update(cx, |this, cx| this.cancel_reply(cx)).ok();
            }
        })
        .detach();
    }

    fn key_for_thread(&self, root_id: u64, cx: &App) -> Option<PrKey> {
        let worktrees = cx.try_global::<PrWorktrees>()?;
        let store = BitbucketStore::try_global(cx)?;
        let store = store.read(cx);
        worktrees
            .0
            .values()
            .find(|key| {
                store
                    .pull_request(key)
                    .is_some_and(|data| data.comments.iter().any(|c| c.id == root_id))
            })
            .cloned()
    }

    /// Post comments made with the review button on a pull request's files.
    fn post_review_comments(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let has_pull_requests = cx.try_global::<PrWorktrees>().is_some_and(|w| !w.0.is_empty());
        if !has_pull_requests {
            return;
        }
        let comments = editor.update(cx, |editor, cx| editor.take_review_comments_by_line(cx));
        let buffers: Vec<_> = editor.read(cx).buffer().read(cx).all_buffers().into_iter().collect();
        for (path, row, body) in comments {
            if body.trim().is_empty() {
                continue;
            }
            let target = buffers.iter().find_map(|buffer| {
                let file = buffer.read(cx).file()?;
                if file.path().as_ref() != path.as_ref() {
                    return None;
                }
                pull_request_for(&file.as_local()?.abs_path(cx), cx)
            });
            let Some((key, rel_path)) = target else {
                continue;
            };
            let request = plan::comment(
                &key.repo,
                key.id,
                &NewComment {
                    body,
                    inline: Some(InlineAnchor {
                        path: rel_path,
                        line: row as u64 + 1,
                        old_side: false,
                    }),
                    parent: None,
                },
            );
            BitbucketStore::global(cx)
                .update(cx, |store, cx| store.execute(request, Some(key), cx))
                .detach_and_log_err(cx);
        }
    }

    /// Replace this editor's thread blocks with one per thread whose line is
    /// in one of its buffers.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let has_pull_requests = cx.try_global::<PrWorktrees>().is_some_and(|w| !w.0.is_empty());
        if !has_pull_requests && self.blocks.is_empty() {
            return;
        }
        let this = ThreadHost::Editor(cx.weak_entity());
        let reply = self
            .reply
            .as_ref()
            .map(|(_, reply)| (reply.root_id, reply.editor.clone()));
        let old_blocks: HashSet<CustomBlockId> = self.blocks.drain(..).collect();
        let store = BitbucketStore::try_global(cx);
        self.blocks = editor.update(cx, |editor, cx| {
            editor.remove_blocks(old_blocks, None, cx);
            let Some(store) = store else {
                return Vec::new();
            };
            let mut blocks = Vec::new();
            {
                let multibuffer = editor.buffer().read(cx);
                let snapshot = multibuffer.snapshot(cx);
                let mut threads_by_pr = HashMap::default();
                for buffer in multibuffer.all_buffers() {
                    let buffer = buffer.read(cx);
                    let Some(abs_path) = buffer.file().and_then(|f| Some(f.as_local()?.abs_path(cx))) else {
                        continue;
                    };
                    let Some((key, rel_path)) = pull_request_for(&abs_path, cx) else {
                        continue;
                    };
                    let threads = threads_by_pr.entry(key.clone()).or_insert_with(|| {
                        store
                            .read(cx)
                            .pull_request(&key)
                            .map(|data| threads::group(&data.comments))
                            .unwrap_or_default()
                            .into_iter()
                            .map(Arc::new)
                            .collect::<Vec<_>>()
                    });
                    for thread in threads.iter() {
                        let Place::NewLine { path, line } = &thread.place else {
                            continue;
                        };
                        if *path != rel_path {
                            continue;
                        }
                        let row = line.saturating_sub(1).min(buffer.max_point().row);
                        let Some(anchor) = snapshot.anchor_in_excerpt(buffer.anchor_before(Point::new(row, 0))) else {
                            continue;
                        };
                        let reply_editor = reply
                            .as_ref()
                            .filter(|(root_id, _)| *root_id == thread.root.id)
                            .map(|(_, editor)| editor.clone());
                        let height = thread_view::card_height(thread, reply_editor.is_some());
                        let (thread, host, key) = (thread.clone(), this.clone(), key.clone());
                        blocks.push(BlockProperties {
                            placement: BlockPlacement::Below(anchor),
                            height: Some(height),
                            style: BlockStyle::Flex,
                            render: Arc::new(move |block_cx| {
                                let indent = block_cx.margins.gutter.full_width();
                                div()
                                    .pl(indent)
                                    .pr_4()
                                    .py_1()
                                    .w_full()
                                    .child(thread_view::render_card(
                                        &key,
                                        &thread,
                                        host.clone(),
                                        reply_editor.clone(),
                                        None,
                                        block_cx.app,
                                    ))
                                    .into_any_element()
                            }),
                            priority: 0,
                        });
                    }
                }
            }
            editor.insert_blocks(blocks, None, cx)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn maps_files_in_a_review_worktree_to_their_pull_request(cx: &mut TestAppContext) {
        let key = PrKey {
            repo: bitbucket::RepoRef::new("ws", "repo"),
            id: 5,
        };
        cx.update(|cx| {
            register_worktree(PathBuf::from("/data/pr-worktrees/ws-repo-pr5"), key.clone(), cx);
            assert_eq!(
                pull_request_for(Path::new("/data/pr-worktrees/ws-repo-pr5/src/a.c"), cx),
                Some((key.clone(), "src/a.c".to_string()))
            );
            assert_eq!(pull_request_for(Path::new("/home/me/repo/src/a.c"), cx), None);
            unregister_worktree(Path::new("/data/pr-worktrees/ws-repo-pr5"), cx);
            assert_eq!(pull_request_for(Path::new("/data/pr-worktrees/ws-repo-pr5/src/a.c"), cx), None);
        });
    }
}
