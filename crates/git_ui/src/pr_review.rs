//! A Bitbucket pull request reviewed inside Zed: the pull request's metadata
//! and review buttons above its diff, with comment threads and reviewer
//! findings shown at their lines. The diff's files are a checkout of the pull
//! request's head, so code navigation works as in any project file.

mod checkout;
mod threads;

use crate::{diff_multibuffer::DiffMultibuffer, picker_prompt, pr_pipelines::PrPipelines};
use anyhow::{Context as _, Result, anyhow};
use bitbucket::{
    BitbucketStore, PrKey, PrWatch, StoreEvent,
    bbt::write::{InlineAnchor, NewComment, plan},
    models::{ParticipantState, PullRequest},
};
use checkout::{Checkout, CheckoutRequest};
use collections::{HashMap, HashSet};
use editor::{
    Editor, EditorEvent, HiddenDiffHunkRenderer, SplittableEditor,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use gpui::{
    AnyElement, App, AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, Task, WeakEntity, actions,
};
use language::{Capability, Point};
use project::{
    Project, ProjectPath, WorktreeId,
    git_store::{
        Repository,
        diff_buffer_list::{DiffBase, DiffBufferList},
    },
};
use std::{
    any::{Any, TypeId},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use threads::{Place, Thread};
use ui::{Divider, Tooltip, prelude::*};
use workspace::{
    ItemNavHistory, SaveIntent, Workspace,
    item::{Item, ItemEvent, SaveOptions},
    notifications::NotifyTaskExt,
    searchable::SearchableItemHandle,
};

actions!(
    pr_review,
    [
        /// Picks one of the current repository's open Bitbucket pull requests
        /// and opens it for review: its diff, comments and builds in one tab.
        OpenPullRequest,
    ]
);

pub(crate) fn register(workspace: &mut Workspace, _cx: &mut gpui::Context<Workspace>) {
    workspace.register_action(|workspace, _: &OpenPullRequest, window, cx| {
        PrReview::pick_and_open(workspace, window, cx)
    });
}

/// Characters per line assumed when estimating a comment block's height.
const WRAP_COLUMNS: usize = 110;

enum CheckoutState {
    /// Waiting for the pull request's commits from Bitbucket.
    Waiting,
    Preparing,
    Ready(Checkout),
    Failed(String),
}

struct Reply {
    root_id: u64,
    editor: Entity<Editor>,
}

pub struct PrReview {
    key: PrKey,
    /// The user's own clone, which the review's worktree is created from.
    repo_root: PathBuf,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    store: Entity<BitbucketStore>,
    checkout: CheckoutState,
    worktree_id: Option<WorktreeId>,
    diff: Option<Entity<DiffMultibuffer>>,
    threads: Vec<Arc<Thread>>,
    /// Root comment ids of the threads shown inline in the diff.
    placed: HashSet<u64>,
    thread_blocks: Vec<CustomBlockId>,
    reply: Option<Reply>,
    show_description: bool,
    show_conversation: bool,
    write_error: Option<String>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

impl PrReview {
    fn pick_and_open(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) {
        let workspace_handle = cx.weak_entity();
        let error_handle = workspace_handle.clone();
        let target = Self::active_bitbucket_repo(workspace, cx);
        let store = BitbucketStore::global(cx);
        let list = match &target {
            Ok((repo, _)) => store.update(cx, |store, cx| store.open_pull_requests(repo.clone(), cx)),
            Err(_) => Task::ready(Ok(Vec::new())),
        };
        window
            .spawn(cx, async move |cx| {
                let (repo, repo_root) = target?;
                let mut prs = list.await?;
                if prs.is_empty() {
                    anyhow::bail!("{repo} has no open pull requests");
                }
                let me = store.read_with(cx, |store, _| store.me().cloned());
                // Pull requests waiting for my review first, then the rest as Bitbucket ordered them.
                prs.sort_by_key(|pr| !awaits_review_from(pr, me.as_ref().and_then(|m| m.uuid.as_deref())));
                let options = prs
                    .iter()
                    .map(|pr| {
                        SharedString::from(format!(
                            "#{} {}  ({} → {}, {})",
                            pr.id,
                            pr.title,
                            pr.source.branch.name,
                            pr.destination.branch.name,
                            pr.author.display_name
                        ))
                    })
                    .collect();
                let choice = cx
                    .update(|window, cx| {
                        picker_prompt::prompt(
                            "Pull request to review",
                            options,
                            workspace_handle.clone(),
                            window,
                            cx,
                        )
                    })?
                    .await;
                let Some(ix) = choice else {
                    return Ok(());
                };
                let key = PrKey {
                    repo,
                    id: prs[ix].id,
                };
                workspace_handle.update_in(cx, |workspace, window, cx| {
                    Self::open(workspace, key, repo_root, window, cx)
                })?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(error_handle, window, cx);
    }

    /// The active repository's Bitbucket identity and its directory.
    fn active_bitbucket_repo(
        workspace: &Workspace,
        cx: &App,
    ) -> Result<(bitbucket::RepoRef, PathBuf)> {
        let repo = workspace
            .project()
            .read(cx)
            .active_repository(cx)
            .context("no active git repository")?;
        let repo = repo.read(cx);
        let remote = repo
            .remote_origin_url
            .as_deref()
            .or(repo.remote_upstream_url.as_deref())
            .context("the active repository has no remote")?;
        let repo_ref = BitbucketStore::repo_for_remote(remote)
            .with_context(|| format!("the active repository's remote is not on bitbucket.org: {remote}"))?;
        Ok((repo_ref, repo.work_directory_abs_path.to_path_buf()))
    }

    pub fn open(
        workspace: &mut Workspace,
        key: PrKey,
        repo_root: PathBuf,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) {
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|item| item.read(cx).key == key);
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let project = workspace.project().clone();
        let workspace_handle = cx.weak_entity();
        let review = cx.new(|cx| Self::new(key, repo_root, project, workspace_handle, window, cx));
        workspace.add_item_to_active_pane(Box::new(review), None, true, window, cx);
    }

    fn new(
        key: PrKey,
        repo_root: PathBuf,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let store = BitbucketStore::global(cx);
        let subscription = cx.subscribe_in(&store, window, |this, _, event, window, cx| {
            if matches!(event, StoreEvent::PrUpdated(key) if *key == this.key) {
                this.pull_request_updated(window, cx);
            }
        });
        store.update(cx, |store, cx| store.refresh_pull_request(key.clone(), cx));
        let mut subscriptions = vec![subscription];
        if let Some(watch) = PrWatch::try_global(cx) {
            subscriptions.push(cx.observe(&watch, |_, _, cx| cx.notify()));
        }
        let mut this = Self {
            key,
            repo_root,
            project,
            workspace,
            store,
            checkout: CheckoutState::Waiting,
            worktree_id: None,
            diff: None,
            threads: Vec::new(),
            placed: HashSet::default(),
            thread_blocks: Vec::new(),
            reply: None,
            show_description: false,
            show_conversation: true,
            write_error: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
            _tasks: Vec::new(),
        };
        this.pull_request_updated(window, cx);
        this
    }

    fn pull_request(&self, cx: &App) -> Option<PullRequest> {
        self.store
            .read(cx)
            .pull_request(&self.key)
            .and_then(|data| data.pull_request.clone())
    }

    fn pull_request_updated(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(data) = self.store.read(cx).pull_request(&self.key).cloned() else {
            return;
        };
        self.threads = threads::group(&data.comments).into_iter().map(Arc::new).collect();
        if matches!(self.checkout, CheckoutState::Waiting) {
            if let Some(pr) = &data.pull_request {
                self.start_checkout(pr, window, cx);
            }
        }
        self.sync_thread_blocks(cx);
        cx.notify();
    }

    fn start_checkout(&mut self, pr: &PullRequest, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let (Some(head), Some(base)) = (
            pr.source.commit.as_ref().map(|c| c.hash.clone()),
            pr.destination.commit.as_ref().map(|c| c.hash.clone()),
        ) else {
            self.checkout = CheckoutState::Failed("Bitbucket sent no commit hashes for this pull request".into());
            return;
        };
        let source_is_fork = pr
            .source
            .repository
            .as_ref()
            .zip(pr.destination.repository.as_ref())
            .is_some_and(|(s, d)| s.full_name != d.full_name);
        if source_is_fork {
            self.checkout = CheckoutState::Failed(
                "The source branch is in another repository (a fork); only same-repository pull requests can be checked out".into(),
            );
            return;
        }
        self.checkout = CheckoutState::Preparing;
        let request = CheckoutRequest {
            repo_root: self.repo_root.clone(),
            path: checkout::worktree_path(&self.key),
            source_branch: pr.source.branch.name.clone(),
            destination_branch: pr.destination.branch.name.clone(),
            head,
            base,
        };
        let prepare = cx.background_spawn(checkout::prepare(request));
        let task = cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let checkout = prepare.await?;
                let add = this.update(cx, |this, cx| {
                    this.project.update(cx, |project, cx| {
                        project.find_or_create_worktree(&checkout.path, true, cx)
                    })
                })?;
                let (worktree, _) = add.await?;
                let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
                let repository = wait_for_repository(&this, &checkout.path, cx).await?;
                this.update_in(cx, |this, window, cx| {
                    this.worktree_id = Some(worktree_id);
                    let base = checkout.base.clone();
                    this.checkout = CheckoutState::Ready(checkout);
                    this.create_diff(repository, base, window, cx);
                })?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                this.update(cx, |this, cx| {
                    this.checkout = CheckoutState::Failed(format!("{e:#}"));
                    cx.notify();
                })
                .ok();
            }
        });
        self._tasks.push(task);
    }

    fn create_diff(
        &mut self,
        repository: Entity<Repository>,
        base: String,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let git_store = self.project.read(cx).git_store().clone();
        let list = cx.new(|cx| {
            DiffBufferList::new(
                DiffBase::Merge {
                    base_ref: base.into(),
                },
                git_store,
                Some(repository),
                cx,
            )
        });
        let diff = cx.new(|cx| {
            DiffMultibuffer::new(
                list,
                Capability::ReadWrite,
                "No changes in this pull request",
                |editor, cx| {
                    editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
                    editor.rhs_editor().update(cx, |rhs, _| rhs.set_read_only(false));
                },
                self.project.clone(),
                workspace,
                window,
                cx,
            )
        });
        let multibuffer = diff.read(cx).editor().read(cx).rhs_editor().read(cx).buffer().clone();
        self._subscriptions.push(cx.subscribe(
            &multibuffer,
            |this, _, event: &multi_buffer::Event, cx| match event {
                multi_buffer::Event::BufferRangesUpdated { .. }
                | multi_buffer::Event::BuffersRemoved { .. } => this.sync_thread_blocks(cx),
                _ => {}
            },
        ));
        self._subscriptions
            .push(cx.subscribe(&diff, |_, _, event: &EditorEvent, cx| cx.emit(event.clone())));
        // The diff's review button stores comments in the editor; in a pull
        // request review each one is posted to Bitbucket as an inline comment.
        let rhs = diff.read(cx).editor().read(cx).rhs_editor().clone();
        self._subscriptions.push(cx.subscribe(&rhs, |this, rhs, event: &EditorEvent, cx| {
            if let EditorEvent::ReviewCommentsChanged { total_count } = event
                && *total_count > 0
            {
                let comments = rhs.update(cx, |editor, cx| editor.take_review_comments_by_line(cx));
                this.post_inline_comments(comments, cx);
            }
        }));
        self.diff = Some(diff);
        self.sync_thread_blocks(cx);
        cx.notify();
    }

    fn rhs_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        Some(self.diff.as_ref()?.read(cx).editor().read(cx).rhs_editor().clone())
    }

    /// Put each thread anchored to a new-file line that is shown in the diff
    /// into a block below that line. Threads on lines outside the shown hunks,
    /// on removed lines, on whole files or on the pull request are listed in
    /// the Conversation section instead.
    fn sync_thread_blocks(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(rhs) = self.rhs_editor(cx) else {
            return;
        };
        let old_blocks: HashSet<CustomBlockId> = self.thread_blocks.drain(..).collect();
        let this = cx.weak_entity();
        let can_write = self.store.read(cx).can_write();
        let reply = self.reply.as_ref().map(|r| (r.root_id, r.editor.clone()));
        let threads = self.threads.clone();
        let (ids, placed) = rhs.update(cx, |editor, cx| {
            editor.remove_blocks(old_blocks, None, cx);
            let mut blocks = Vec::new();
            let mut placed = HashSet::default();
            {
                let multibuffer = editor.buffer().read(cx);
                let snapshot = multibuffer.snapshot(cx);
                let buffers: HashMap<String, Entity<language::Buffer>> = multibuffer
                    .all_buffers()
                    .into_iter()
                    .filter_map(|buffer| {
                        let path = buffer.read(cx).file()?.path().as_unix_str().to_string();
                        Some((path, buffer))
                    })
                    .collect();
                for thread in &threads {
                    let Place::NewLine { path, line } = &thread.place else {
                        continue;
                    };
                    let Some(buffer) = buffers.get(path) else {
                        continue;
                    };
                    let buffer = buffer.read(cx);
                    let row = line.saturating_sub(1).min(buffer.max_point().row);
                    let text_anchor = buffer.anchor_before(Point::new(row, 0));
                    let Some(anchor) = snapshot.anchor_in_excerpt(text_anchor) else {
                        continue;
                    };
                    placed.insert(thread.root.id);
                    let thread = thread.clone();
                    let this = this.clone();
                    let reply_editor = reply
                        .as_ref()
                        .filter(|(root_id, _)| *root_id == thread.root.id)
                        .map(|(_, editor)| editor.clone());
                    let height = thread_height(&thread, reply_editor.is_some());
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
                                .child(render_thread(
                                    &thread,
                                    this.clone(),
                                    can_write,
                                    reply_editor.clone(),
                                    block_cx.app,
                                ))
                                .into_any_element()
                        }),
                        priority: 0,
                    });
                }
            }
            (editor.insert_blocks(blocks, None, cx), placed)
        });
        self.thread_blocks = ids;
        self.placed = placed;
    }

    fn write(&mut self, request: bitbucket::bbt::write::WriteRequest, cx: &mut gpui::Context<Self>) -> Task<Result<()>> {
        self.write_error = None;
        let key = self.key.clone();
        let send = self
            .store
            .update(cx, |store, cx| store.execute(request, Some(key), cx));
        cx.spawn(async move |this, cx| {
            let result = send.await;
            this.update(cx, |this, cx| {
                if let Err(e) = &result {
                    this.write_error = Some(format!("{e:#}"));
                }
                cx.notify();
            })?;
            result.map(|_| ())
        })
    }

    fn post_inline_comments(
        &mut self,
        comments: Vec<(Arc<util::rel_path::RelPath>, u32, String)>,
        cx: &mut gpui::Context<Self>,
    ) {
        for (path, row, body) in comments {
            if body.trim().is_empty() {
                continue;
            }
            let request = plan::comment(
                &self.key.repo,
                self.key.id,
                &NewComment {
                    body,
                    inline: Some(InlineAnchor {
                        path: path.as_unix_str().to_string(),
                        line: row as u64 + 1,
                        old_side: false,
                    }),
                    parent: None,
                },
            );
            self.write(request, cx).detach();
        }
    }

    fn set_approved(&mut self, approved: bool, cx: &mut gpui::Context<Self>) {
        let request = plan::approve(&self.key.repo, self.key.id, approved);
        self.write(request, cx).detach();
    }

    fn set_changes_requested(&mut self, requested: bool, cx: &mut gpui::Context<Self>) {
        let request = plan::request_changes(&self.key.repo, self.key.id, requested);
        self.write(request, cx).detach();
    }

    fn set_resolved(&mut self, root_id: u64, resolved: bool, cx: &mut gpui::Context<Self>) {
        let request = plan::resolve_comment(&self.key.repo, self.key.id, root_id, resolved);
        self.write(request, cx).detach();
    }

    fn start_reply(&mut self, root_id: u64, prefill: Option<String>, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text("Reply…", window, cx);
            if let Some(text) = &prefill {
                editor.set_text(text.as_str(), window, cx);
            }
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.reply = Some(Reply { root_id, editor });
        self.sync_thread_blocks(cx);
        cx.notify();
    }

    fn cancel_reply(&mut self, cx: &mut gpui::Context<Self>) {
        self.reply = None;
        self.sync_thread_blocks(cx);
        cx.notify();
    }

    fn send_reply(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(reply) = &self.reply else {
            return;
        };
        let body = reply.editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        let request = plan::comment(
            &self.key.repo,
            self.key.id,
            &NewComment {
                body,
                inline: None,
                parent: Some(reply.root_id),
            },
        );
        let send = self.write(request, cx);
        cx.spawn(async move |this, cx| {
            if send.await.is_ok() {
                this.update(cx, |this, cx| this.cancel_reply(cx)).ok();
            }
        })
        .detach();
    }

    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        self.store
            .update(cx, |store, cx| store.refresh_pull_request(self.key.clone(), cx));
    }

    fn open_pipelines(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(pr) = self.pull_request(cx) else {
            return;
        };
        let repo = self.key.repo.clone();
        let branch = pr.source.branch.name;
        self.workspace
            .update(cx, |workspace, cx| PrPipelines::open(workspace, repo, branch, window, cx))
            .ok();
    }

    /// Remove the review's worktree from the project and from disk, then close
    /// the tab. A worktree with local changes is kept, and git's refusal shown.
    fn close_review(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let CheckoutState::Ready(checkout) = &self.checkout else {
            return;
        };
        let path = checkout.path.clone();
        let repo_root = self.repo_root.clone();
        if let Some(id) = self.worktree_id.take() {
            self.project.update(cx, |project, cx| project.remove_worktree(id, cx));
        }
        let item_id = cx.entity_id();
        let workspace = self.workspace.clone();
        let remove = cx.background_spawn(checkout::remove(repo_root, path));
        window
            .spawn(cx, async move |cx| {
                remove.await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                            .detach_and_log_err(cx);
                    });
                })?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    fn render_header(&self, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let data = store.pull_request(&self.key).cloned().unwrap_or_default();
        let can_write = store.can_write();
        let me = store.me().cloned();
        let config_error = store.config_error().map(str::to_string);
        let colors = cx.theme().colors();

        let Some(pr) = data.pull_request.clone() else {
            let message = config_error
                .or(data.error)
                .unwrap_or_else(|| format!("Loading pull request #{}…", self.key.id));
            return v_flex().p_3().child(Label::new(message).color(Color::Muted)).into_any_element();
        };

        let my_state = me.as_ref().and_then(|me| {
            pr.participants
                .iter()
                .find(|p| p.user.uuid.is_some() && p.user.uuid == me.uuid)
        });
        let i_approved = my_state.is_some_and(|p| p.approved);
        let i_requested_changes = my_state.is_some_and(|p| p.state == Some(ParticipantState::ChangesRequested));
        let web_url = pr.links.html.as_ref().map(|h| h.href.clone());

        let reviewers = pr
            .participants
            .iter()
            .filter(|p| p.role == "REVIEWER" || p.approved || p.state.is_some())
            .map(|p| {
                let (icon, color, tooltip) = match (p.approved, p.state) {
                    (true, _) => (IconName::Check, Color::Success, "Approved"),
                    (_, Some(ParticipantState::ChangesRequested)) => {
                        (IconName::XCircle, Color::Error, "Requested changes")
                    }
                    _ => (IconName::Circle, Color::Muted, "No decision yet"),
                };
                h_flex()
                    .id(SharedString::from(format!("reviewer-{}", p.user.display_name)))
                    .gap_1()
                    .child(Icon::new(icon).size(IconSize::Small).color(color))
                    .child(Label::new(p.user.display_name.clone()).size(LabelSize::Small))
                    .tooltip(Tooltip::text(tooltip))
            });

        let builds = data.statuses.iter().map(|s| {
            let color = match s.state.as_str() {
                "SUCCESSFUL" => Color::Success,
                "FAILED" => Color::Error,
                "INPROGRESS" => Color::Warning,
                _ => Color::Muted,
            };
            let name = if s.name.is_empty() { s.key.clone() } else { s.name.clone() };
            Label::new(format!("{name}: {}", s.state.to_lowercase()))
                .size(LabelSize::Small)
                .color(color)
        });

        let checkout_line = match &self.checkout {
            CheckoutState::Waiting => "Waiting for the pull request's commits…".to_string(),
            CheckoutState::Preparing => "Fetching and checking out the pull request's head…".to_string(),
            CheckoutState::Ready(c) if c.kept_local_changes => format!(
                "Checked out in {} at {} (kept: the worktree has local changes)",
                c.path.display(),
                short(&c.head)
            ),
            CheckoutState::Ready(c) => format!("Checked out in {} at {}", c.path.display(), short(&c.head)),
            CheckoutState::Failed(e) => format!("Checkout failed: {e}"),
        };
        let checkout_failed = matches!(self.checkout, CheckoutState::Failed(_));
        let head_moved = match (&self.checkout, pr.source.commit.as_ref()) {
            (CheckoutState::Ready(c), Some(commit)) => !c.head.starts_with(&commit.hash),
            _ => false,
        };

        let write_tooltip = if can_write {
            None
        } else {
            Some("Needs a Bitbucket write credential: run `bbt auth set write`")
        };
        let approve = Button::new("approve", if i_approved { "Unapprove" } else { "Approve" })
            .style(ButtonStyle::Filled)
            .disabled(!can_write)
            .when_some(write_tooltip, |b, t| b.tooltip(Tooltip::text(t)))
            .on_click(cx.listener(move |this, _, _, cx| this.set_approved(!i_approved, cx)));
        let request_changes = Button::new(
            "request-changes",
            if i_requested_changes { "Withdraw change request" } else { "Request changes" },
        )
        .style(ButtonStyle::Filled)
        .disabled(!can_write)
        .when_some(write_tooltip, |b, t| b.tooltip(Tooltip::text(t)))
        .on_click(cx.listener(move |this, _, _, cx| this.set_changes_requested(!i_requested_changes, cx)));

        let unplaced: Vec<Arc<Thread>> = self
            .threads
            .iter()
            .filter(|t| !self.placed.contains(&t.root.id))
            .cloned()
            .collect();
        let reply = self.reply.as_ref().map(|r| (r.root_id, r.editor.clone()));
        let this = cx.weak_entity();
        let watched = PrWatch::try_global(cx).and_then(|watch| watch.read(cx).get(&self.key).cloned());
        let handled_line = watched.as_ref().map(|w| {
            if w.is_changed() {
                (format!("Since you handled it: {}", w.summary()), Color::Accent)
            } else {
                ("Handled: nothing new since".to_string(), Color::Muted)
            }
        });
        let can_mark_handled = watched.as_ref().is_some_and(|w| w.is_changed());

        v_flex()
            .id("pr-header")
            .p_3()
            .gap_2()
            .max_h(rems(28.))
            .overflow_y_scroll()
            .bg(colors.panel_background)
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(Headline::new(format!("#{} {}", pr.id, pr.title)).size(HeadlineSize::Small))
                    .child(Label::new(pr.state.to_lowercase()).color(Color::Muted))
                    .when(pr.draft, |row| row.child(Label::new("draft").color(Color::Warning))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(pr.author.display_name.clone()).size(LabelSize::Small))
                    .child(
                        Label::new(format!("{} → {}", pr.source.branch.name, pr.destination.branch.name))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!(
                            "{} comments · {} files",
                            pr.comment_count,
                            data.diffstat.len()
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .when_some(handled_line, |header, (text, color)| {
                header.child(Label::new(text).size(LabelSize::Small).color(color))
            })
            .child(h_flex().gap_3().flex_wrap().children(reviewers))
            .when(!data.statuses.is_empty(), |header| {
                header.child(h_flex().gap_3().flex_wrap().children(builds))
            })
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .when(can_mark_handled, |row| {
                        row.child(
                            Button::new("mark-handled", "Mark handled")
                                .style(ButtonStyle::Filled)
                                .tooltip(Tooltip::text(
                                    "No more notifications for this pull request until something new happens",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(watch) = PrWatch::try_global(cx) {
                                        let key = this.key.clone();
                                        watch.update(cx, |watch, cx| watch.mark_handled(&key, cx));
                                    }
                                })),
                        )
                    })
                    .child(approve)
                    .child(request_changes)
                    .child(
                        Button::new("pipelines", "Pipelines")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| this.open_pipelines(window, cx))),
                    )
                    .child(
                        Button::new("refresh", if data.loading { "Refreshing…" } else { "Refresh" })
                            .disabled(data.loading)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .when_some(web_url, |row, url| {
                        row.child(
                            Button::new("open-web", "Open in Bitbucket")
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    })
                    .when(matches!(self.checkout, CheckoutState::Ready(_)), |row| {
                        row.child(
                            Button::new("close-review", "Close review")
                                .tooltip(Tooltip::text(
                                    "Remove the review's worktree from the project and from disk",
                                ))
                                .on_click(cx.listener(|this, _, window, cx| this.close_review(window, cx))),
                        )
                    }),
            )
            .child(
                Label::new(checkout_line)
                    .size(LabelSize::Small)
                    .color(if checkout_failed { Color::Error } else { Color::Muted }),
            )
            .when(head_moved, |header| {
                header.child(
                    Label::new(
                        "The pull request has new commits since this checkout; close and reopen the review to see them",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Warning),
                )
            })
            .when_some(self.write_error.clone().or(data.error.clone()), |header, error| {
                header.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .when(!pr.description.trim().is_empty(), |header| {
                header.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Button::new(
                                "toggle-description",
                                if self.show_description { "Hide description" } else { "Show description" },
                            )
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_description = !this.show_description;
                                cx.notify();
                            })),
                        )
                        .when(self.show_description, |col| {
                            col.child(div().text_sm().child(pr.description.clone()))
                        }),
                )
            })
            .when(!unplaced.is_empty(), |header| {
                header.child(Divider::horizontal()).child(
                    v_flex()
                        .gap_2()
                        .child(
                            Button::new(
                                "toggle-conversation",
                                format!(
                                    "{} conversation ({} not shown in the diff)",
                                    if self.show_conversation { "Hide" } else { "Show" },
                                    unplaced.len()
                                ),
                            )
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_conversation = !this.show_conversation;
                                cx.notify();
                            })),
                        )
                        .when(self.show_conversation, |col| {
                            col.children(unplaced.iter().map(|thread| {
                                let reply_editor = reply
                                    .as_ref()
                                    .filter(|(id, _)| *id == thread.root.id)
                                    .map(|(_, e)| e.clone());
                                render_thread(thread, this.clone(), can_write, reply_editor, cx)
                            }))
                        }),
                )
            })
            .into_any_element()
    }
}

/// Wait until the git store has picked up the repository at `path`.
async fn wait_for_repository(
    this: &WeakEntity<PrReview>,
    path: &std::path::Path,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<Entity<Repository>> {
    for _ in 0..100 {
        let found = this.update(cx, |this, cx| {
            this.project
                .read(cx)
                .git_store()
                .read(cx)
                .repositories()
                .values()
                .find(|repo| repo.read(cx).work_directory_abs_path.as_ref() == path)
                .cloned()
        })?;
        if let Some(repo) = found {
            return Ok(repo);
        }
        cx.background_executor().timer(Duration::from_millis(100)).await;
    }
    Err(anyhow!("git did not report a repository at {}", path.display()))
}

fn awaits_review_from(pr: &PullRequest, my_uuid: Option<&str>) -> bool {
    let Some(my_uuid) = my_uuid else {
        return false;
    };
    pr.participants.iter().any(|p| {
        p.user.uuid.as_deref() == Some(my_uuid) && p.role == "REVIEWER" && !p.approved && p.state.is_none()
    })
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn wrapped_lines(text: &str) -> u32 {
    text.lines()
        .map(|line| line.chars().count().div_ceil(WRAP_COLUMNS).max(1) as u32)
        .sum::<u32>()
        .max(1)
}

/// Lines a thread block needs: a header line and the wrapped body per
/// comment, the button row, and room for the reply editor when open.
fn thread_height(thread: &Thread, replying: bool) -> u32 {
    let comments: u32 = std::iter::once(&thread.root)
        .chain(&thread.replies)
        .map(|c| 1 + wrapped_lines(&c.content.raw))
        .sum();
    comments + 2 + if replying { 5 } else { 0 }
}

fn render_thread(
    thread: &Thread,
    this: WeakEntity<PrReview>,
    can_write: bool,
    reply_editor: Option<Entity<Editor>>,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let root_id = thread.root.id;
    let finding = thread.finding_key().is_some();
    let resolved = thread.is_resolved();
    let place = match &thread.place {
        Place::NewLine { path, line } => Some(format!("{path}:{line}")),
        Place::OldLine { path, line } => Some(format!("{path}: removed line {line}")),
        Place::File { path } => Some(path.clone()),
        Place::General => None,
    };
    let scores = thread.scores();

    let comment = |c: &bitbucket::models::Comment, is_root: bool| {
        let when = c
            .created_on
            .as_deref()
            .map(|t| t.get(..16).unwrap_or(t).replace('T', " "))
            .unwrap_or_default();
        let body = if is_root && finding {
            let headline = threads::finding_headline(&c.content.raw);
            let rest: String = c
                .content
                .raw
                .lines()
                .skip_while(|l| l.trim().is_empty())
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n");
            v_flex()
                .child(Label::new(headline).weight(gpui::FontWeight::BOLD).color(Color::Warning))
                .child(div().text_sm().child(rest.trim().to_string()))
        } else {
            v_flex().child(div().text_sm().child(c.content.raw.clone()))
        };
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(c.user.display_name.clone()).size(LabelSize::Small).weight(gpui::FontWeight::SEMIBOLD))
                    .child(Label::new(when).size(LabelSize::XSmall).color(Color::Muted))
                    .when(c.deleted, |row| row.child(Label::new("deleted").size(LabelSize::XSmall).color(Color::Muted))),
            )
            .child(body)
    };

    let replying = reply_editor.is_some();
    let buttons = h_flex()
        .gap_1()
        .when(!replying, |row| {
            row.child(
                Button::new(("reply", root_id), "Reply")
                    .style(ButtonStyle::Subtle)
                    .disabled(!can_write)
                    .on_click({
                        let this = this.clone();
                        move |_, window, cx| {
                            this.update(cx, |this, cx| this.start_reply(root_id, None, window, cx)).ok();
                        }
                    }),
            )
            .child(
                Button::new(("resolve", root_id), if resolved { "Reopen" } else { "Resolve" })
                    .style(ButtonStyle::Subtle)
                    .disabled(!can_write)
                    .on_click({
                        let this = this.clone();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| this.set_resolved(root_id, !resolved, cx)).ok();
                        }
                    }),
            )
            .when(finding, |row| {
                row.child(Label::new("Rate finding:").size(LabelSize::Small).color(Color::Muted))
                    .children((1..=5u8).map(|score| {
                        let this = this.clone();
                        Button::new(("score", root_id * 10 + score as u64), score.to_string())
                            .style(ButtonStyle::Subtle)
                            .disabled(!can_write)
                            .tooltip(Tooltip::text("Reply with SCORE=<n> Comment=<why>; add the reason before sending"))
                            .on_click(move |_, window, cx| {
                                this.update(cx, |this, cx| {
                                    this.start_reply(root_id, Some(format!("SCORE={score} Comment=")), window, cx)
                                })
                                .ok();
                            })
                    }))
                    .when(!scores.is_empty(), |row| {
                        row.child(
                            Label::new(format!(
                                "rated {}",
                                scores.iter().map(u8::to_string).collect::<Vec<_>>().join(", ")
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    })
            })
        });

    v_flex()
        .w_full()
        .p_2()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(if finding { colors.border_focused } else { colors.border })
        .bg(colors.editor_background)
        .when_some(place, |col, place| {
            col.child(
                h_flex()
                    .gap_2()
                    .child(Label::new(place).size(LabelSize::XSmall).color(Color::Muted))
                    .when(thread.outdated, |row| row.child(Label::new("outdated").size(LabelSize::XSmall).color(Color::Warning)))
                    .when(resolved, |row| row.child(Label::new("resolved").size(LabelSize::XSmall).color(Color::Success))),
            )
        })
        .child(comment(&thread.root, true))
        .children(thread.replies.iter().map(|c| comment(c, false).pl_4()))
        .when_some(reply_editor, |col, editor| {
            col.child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .p_1()
                            .border_1()
                            .border_color(colors.border)
                            .rounded_sm()
                            .child(editor),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Button::new(("send", root_id), "Send").style(ButtonStyle::Filled).on_click({
                                let this = this.clone();
                                move |_, _, cx| {
                                    this.update(cx, |this, cx| this.send_reply(cx)).ok();
                                }
                            }))
                            .child(Button::new(("cancel", root_id), "Cancel").on_click({
                                let this = this.clone();
                                move |_, _, cx| {
                                    this.update(cx, |this, cx| this.cancel_reply(cx)).ok();
                                }
                            })),
                    ),
            )
        })
        .child(buttons)
        .into_any_element()
}

impl EventEmitter<EditorEvent> for PrReview {}

impl Focusable for PrReview {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.diff {
            Some(diff) => diff.read(cx).focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl Render for PrReview {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let body = match &self.diff {
            Some(diff) => diff.clone().into_any_element(),
            None => div()
                .p_3()
                .child(Label::new("The diff appears once the checkout is ready.").color(Color::Muted))
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Item for PrReview {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("PR #{} {}", self.key.id, self.key.repo.slug).into()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let title = self.pull_request(cx).map(|pr| pr.title).unwrap_or_default();
        Some(format!("{} #{}: {}", self.key.repo, self.key.id, title).into())
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if let Some(diff) = &self.diff {
            diff.update(cx, |diff, cx| diff.deactivated(window, cx));
        }
    }

    fn navigate(&mut self, data: Arc<dyn Any + Send>, window: &mut Window, cx: &mut gpui::Context<Self>) -> bool {
        match &self.diff {
            Some(diff) => diff.update(cx, |diff, cx| diff.navigate(data, window, cx)),
            None => false,
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.diff.as_ref()?.read(cx).editor().clone()))
    }

    fn for_each_project_item(&self, cx: &App, f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem)) {
        if let Some(diff) = &self.diff {
            diff.read(cx).for_each_project_item(cx, f);
        }
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.diff.as_ref()?.read(cx).active_project_path(cx)
    }

    fn set_nav_history(&mut self, nav_history: ItemNavHistory, _: &mut Window, cx: &mut gpui::Context<Self>) {
        if let Some(diff) = &self.diff {
            diff.update(cx, |diff, cx| diff.set_nav_history(nav_history, cx));
        }
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.diff.as_ref().is_some_and(|diff| diff.read(cx).is_dirty(cx))
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.diff.as_ref().is_some_and(|diff| diff.read(cx).has_conflict(cx))
    }

    fn can_save(&self, _cx: &App) -> bool {
        self.diff.is_some()
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Task<Result<()>> {
        match &self.diff {
            Some(diff) => diff.update(cx, |diff, cx| diff.save(options, project, window, cx)),
            None => Task::ready(Ok(())),
        }
    }

    fn reload(&mut self, project: Entity<Project>, window: &mut Window, cx: &mut gpui::Context<Self>) -> Task<Result<()>> {
        match &self.diff {
            Some(diff) => diff.update(cx, |diff, cx| diff.reload(project, window, cx)),
            None => Task::ready(Ok(())),
        }
    }

    fn act_as_type<'a>(&'a self, type_id: TypeId, self_handle: &'a Entity<Self>, cx: &'a App) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            return Some(self_handle.clone().into());
        }
        let diff = self.diff.as_ref()?;
        if type_id == TypeId::of::<DiffMultibuffer>() {
            Some(diff.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(diff.read(cx).editor().read(cx).rhs_editor().clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(diff.read(cx).editor().clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(&mut self, workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if let Some(diff) = &self.diff {
            diff.update(cx, |diff, cx| diff.added_to_workspace(workspace, window, cx));
        }
    }
}
