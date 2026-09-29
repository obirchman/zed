//! A Bitbucket pull request under review.
//!
//! The review tab shows the pull request itself: metadata, reviewers, builds,
//! review buttons and the whole conversation. The pull request's head is
//! checked out as a worktree whose repository is diffed against the pull
//! request's destination commit, so Zed's own git panel and diff view show
//! the pull request's changes, and comment threads appear next to their
//! lines in any editor showing those files.

mod checkout;
mod editor_threads;
mod thread_view;
mod threads;

use crate::{branch_diff::BranchDiff, picker_prompt, pr_pipelines::PrPipelines};
use anyhow::{Context as _, Result, anyhow};
use bitbucket::{
    BitbucketStore, PrKey, PrWatch, StoreEvent,
    api::plan,
    models::{ParticipantState, PullRequest},
};
use checkout::{Checkout, CheckoutRequest};
use gpui::{
    AnyElement, App, AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListState, Render, SharedString, Subscription, Task, WeakEntity, actions, list,
};
use language::Point;
use project::{Project, WorktreeId, git_store::Repository};
use std::{path::PathBuf, sync::Arc, time::Duration};
use thread_view::{Reply, ThreadHost};
use threads::{Place, Thread};
use ui::{Tooltip, prelude::*};
use workspace::{
    SaveIntent, Workspace,
    item::{Item, ItemEvent},
    notifications::NotifyTaskExt,
};

actions!(
    pr_review,
    [
        /// Picks one of the current repository's open Bitbucket pull requests
        /// and opens it for review.
        OpenPullRequest,
    ]
);

pub(crate) fn init(cx: &mut App) {
    editor_threads::init(cx);
}

pub(crate) fn register(workspace: &mut Workspace, _cx: &mut gpui::Context<Workspace>) {
    workspace.register_action(|workspace, _: &OpenPullRequest, window, cx| {
        PrReview::pick_and_open(workspace, window, cx)
    });
}

enum CheckoutState {
    /// Waiting for the pull request's commits from Bitbucket.
    Waiting,
    Preparing,
    Ready(Checkout),
    Failed(String),
}

pub enum PrReviewEvent {}

pub struct PrReview {
    key: PrKey,
    /// The user's own clone, which the review's worktree is created from.
    repo_root: PathBuf,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    store: Entity<BitbucketStore>,
    checkout: CheckoutState,
    worktree_id: Option<WorktreeId>,
    repository: Option<Entity<Repository>>,
    threads: Vec<Arc<Thread>>,
    list: ListState,
    reply: Option<Reply>,
    show_description: bool,
    write_error: Option<String>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

impl PrReview {
    fn pick_and_open(workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
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
                prs.sort_by_key(|pr| !awaits_review_from(pr, me.as_ref().and_then(|m| m.uuid.as_deref())));
                let options = prs
                    .iter()
                    .map(|pr| {
                        SharedString::from(format!(
                            "#{} {}  ({} → {}, {})",
                            pr.id, pr.title, pr.source.branch.name, pr.destination.branch.name, pr.author.display_name
                        ))
                    })
                    .collect();
                let choice = cx
                    .update(|window, cx| {
                        picker_prompt::prompt("Pull request to review", options, workspace_handle.clone(), window, cx)
                    })?
                    .await;
                let Some(ix) = choice else {
                    return Ok(());
                };
                let key = PrKey { repo, id: prs[ix].id };
                workspace_handle.update_in(cx, |workspace, window, cx| Self::open(workspace, key, repo_root, window, cx))?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(error_handle, window, cx);
    }

    /// The active repository's Bitbucket identity and its directory.
    fn active_bitbucket_repo(workspace: &Workspace, cx: &App) -> Result<(bitbucket::RepoRef, PathBuf)> {
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
        let existing = workspace.items_of_type::<Self>(cx).find(|item| item.read(cx).key == key);
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
        let mut subscriptions = vec![cx.subscribe_in(&store, window, |this, _, event, window, cx| {
            if matches!(event, StoreEvent::PrUpdated(key) if *key == this.key) {
                this.pull_request_updated(window, cx);
            }
        })];
        if let Some(watch) = PrWatch::try_global(cx) {
            subscriptions.push(cx.observe(&watch, |_, _, cx| cx.notify()));
        }
        store.update(cx, |store, cx| store.refresh_pull_request(key.clone(), cx));
        let mut this = Self {
            key,
            repo_root,
            project,
            workspace,
            store,
            checkout: CheckoutState::Waiting,
            worktree_id: None,
            repository: None,
            threads: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(1000.)),
            reply: None,
            show_description: false,
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
        let mut threads: Vec<Arc<Thread>> = threads::group(&data.comments).into_iter().map(Arc::new).collect();
        // The conversation first, then each file's threads in line order.
        threads.sort_by_key(|t| match &t.place {
            Place::General => (0, String::new(), 0),
            Place::File { path } => (1, path.clone(), 0),
            Place::NewLine { path, line } | Place::OldLine { path, line } => (1, path.clone(), *line),
        });
        self.threads = threads;
        self.list.reset(self.threads.len());
        if matches!(self.checkout, CheckoutState::Waiting)
            && let Some(pr) = &data.pull_request
        {
            self.start_checkout(pr, window, cx);
        }
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
        let workspace = self.workspace.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let checkout = prepare.await?;
                let add = this.update(cx, |this, cx| {
                    this.project
                        .update(cx, |project, cx| project.find_or_create_worktree(&checkout.path, true, cx))
                })?;
                let (worktree, _) = add.await?;
                let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
                let repository = wait_for_repository(&this, &checkout.path, cx).await?;
                let base: SharedString = checkout.base.clone().into();
                let project = this.update(cx, |this, cx| {
                    editor_threads::register_worktree(checkout.path.clone(), this.key.clone(), cx);
                    let repo_id = repository.read(cx).id;
                    this.project.read(cx).git_store().clone().update(cx, |git_store, cx| {
                        git_store.set_repository_diff_base(repo_id, Some(base.clone()), cx);
                        git_store.set_active_repo_for_worktree(worktree_id, cx);
                    });
                    this.worktree_id = Some(worktree_id);
                    this.repository = Some(repository.clone());
                    this.checkout = CheckoutState::Ready(checkout);
                    cx.notify();
                    this.project.clone()
                })?;
                // Zed's own diff of the worktree against the pull request's destination.
                workspace.update_in(cx, |workspace, window, cx| {
                    open_diff(workspace, project, repository, base, window, cx)
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

    fn show_diff(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let (Some(repository), CheckoutState::Ready(checkout)) = (self.repository.clone(), &self.checkout) else {
            return;
        };
        let base: SharedString = checkout.base.clone().into();
        let (project, workspace) = (self.project.clone(), self.workspace.clone());
        // Adding a tab reads every item in the pane, this one included, so it
        // happens after this update has finished.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| open_diff(workspace, project, repository, base, window, cx))
                .ok();
        });
    }

    pub(crate) fn start_reply(&mut self, root_id: u64, prefill: Option<String>, window: &mut Window, cx: &mut gpui::Context<Self>) {
        self.reply = Some(Reply::new(root_id, prefill, window, cx));
        cx.notify();
    }

    pub(crate) fn cancel_reply(&mut self, cx: &mut gpui::Context<Self>) {
        self.reply = None;
        cx.notify();
    }

    pub(crate) fn send_reply(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(reply) = &self.reply else {
            return;
        };
        let send = reply.send(&self.key, cx);
        cx.spawn(async move |this, cx| {
            let result = send.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => this.cancel_reply(cx),
                Err(e) => {
                    this.write_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn write(&mut self, request: bitbucket::api::WriteRequest, cx: &mut gpui::Context<Self>) {
        self.write_error = None;
        let key = self.key.clone();
        let send = self.store.update(cx, |store, cx| store.execute(request, Some(key), cx));
        cx.spawn(async move |this, cx| {
            let result = send.await;
            this.update(cx, |this, cx| {
                if let Err(e) = &result {
                    this.write_error = Some(format!("{e:#}"));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_pipelines(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(pr) = self.pull_request(cx) else {
            return;
        };
        let (repo, branch) = (self.key.repo.clone(), pr.source.branch.name);
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| PrPipelines::open(workspace, repo, branch, window, cx))
                .ok();
        });
    }

    /// Open the thread's file in the review worktree at the thread's line.
    fn go_to_thread(&mut self, thread: &Thread, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Place::NewLine { path, line } = &thread.place else {
            return;
        };
        let CheckoutState::Ready(checkout) = &self.checkout else {
            return;
        };
        let abs_path = checkout.path.join(path);
        let point = Point::new(line.saturating_sub(1), 0);
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            let open = workspace.update(cx, |workspace, cx| {
                workspace.open_abs_path(abs_path, workspace::OpenOptions::default(), window, cx)
            });
            window
                .spawn(cx, async move |cx| {
                    let item = open.await?;
                    if let Some(editor) = item.downcast::<editor::Editor>() {
                        editor.update_in(cx, |editor, window, cx| {
                            editor.go_to_singleton_buffer_point(point, window, cx)
                        })?;
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
        });
    }

    /// Stop showing the pull request as the worktree's diff, remove the
    /// worktree from the project and from disk, then close the tab. A
    /// worktree with local changes is kept, and git's refusal shown.
    fn close_review(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let CheckoutState::Ready(checkout) = &self.checkout else {
            return;
        };
        let path = checkout.path.clone();
        editor_threads::unregister_worktree(&path, cx);
        if let Some(repository) = self.repository.take() {
            let repo_id = repository.read(cx).id;
            self.project.read(cx).git_store().clone().update(cx, |git_store, cx| {
                git_store.set_repository_diff_base(repo_id, None, cx)
            });
        }
        if let Some(id) = self.worktree_id.take() {
            self.project.update(cx, |project, cx| project.remove_worktree(id, cx));
        }
        let item_id = cx.entity_id();
        let workspace = self.workspace.clone();
        let remove = cx.background_spawn(checkout::remove(self.repo_root.clone(), path));
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

    fn render_header(&self, cx: &mut gpui::Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let data = store.pull_request(&self.key).cloned().unwrap_or_default();
        let can_write = store.is_connected();
        let me = store.me().cloned();
        let connection_error = store.connection_error().map(str::to_string);
        let colors = cx.theme().colors();

        let Some(pr) = data.pull_request.clone() else {
            let message = connection_error
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
        let watched = PrWatch::try_global(cx).and_then(|watch| watch.read(cx).get(&self.key).cloned());
        let handled_line = watched.as_ref().map(|w| {
            if w.is_changed() {
                (format!("Since you handled it: {}", w.summary()), Color::Accent)
            } else {
                ("Handled: nothing new since".to_string(), Color::Muted)
            }
        });
        let can_mark_handled = watched.as_ref().is_some_and(|w| w.is_changed());
        let write_tooltip = (!can_write).then_some("Set the Bitbucket API token in Settings > Bitbucket first");

        let reviewers = pr
            .participants
            .iter()
            .filter(|p| p.role == "REVIEWER" || p.approved || p.state.is_some())
            .map(|p| {
                let (icon, color, tooltip) = match (p.approved, p.state) {
                    (true, _) => (IconName::Check, Color::Success, "Approved"),
                    (_, Some(ParticipantState::ChangesRequested)) => (IconName::XCircle, Color::Error, "Requested changes"),
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
            Label::new(format!("{name}: {}", s.state.to_lowercase())).size(LabelSize::Small).color(color)
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
        let ready = matches!(self.checkout, CheckoutState::Ready(_));
        let head_moved = match (&self.checkout, pr.source.commit.as_ref()) {
            (CheckoutState::Ready(c), Some(commit)) => !c.head.starts_with(&commit.hash),
            _ => false,
        };

        v_flex()
            .p_3()
            .gap_2()
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
                        Label::new(format!("{} comments · {} files", pr.comment_count, data.diffstat.len()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when_some(handled_line, |header, (text, color)| {
                header.child(Label::new(text).size(LabelSize::Small).color(color))
            })
            .child(h_flex().gap_3().flex_wrap().children(reviewers))
            .when(!data.statuses.is_empty(), |header| header.child(h_flex().gap_3().flex_wrap().children(builds)))
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .when(can_mark_handled, |row| {
                        row.child(
                            Button::new("mark-handled", "Mark handled")
                                .style(ButtonStyle::Filled)
                                .tooltip(Tooltip::text("No more notifications for this pull request until something new happens"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(watch) = PrWatch::try_global(cx) {
                                        let key = this.key.clone();
                                        watch.update(cx, |watch, cx| watch.mark_handled(&key, cx));
                                    }
                                })),
                        )
                    })
                    .child(
                        Button::new("show-diff", "Diff")
                            .style(ButtonStyle::Filled)
                            .disabled(!ready)
                            .tooltip(Tooltip::text("Zed's diff of the pull request, in the review worktree"))
                            .on_click(cx.listener(|this, _, window, cx| this.show_diff(window, cx))),
                    )
                    .child(
                        Button::new("approve", if i_approved { "Unapprove" } else { "Approve" })
                            .style(ButtonStyle::Filled)
                            .disabled(!can_write)
                            .when_some(write_tooltip, |b, t| b.tooltip(Tooltip::text(t)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let request = plan::approve(&this.key.repo, this.key.id, !i_approved);
                                this.write(request, cx)
                            })),
                    )
                    .child(
                        Button::new(
                            "request-changes",
                            if i_requested_changes { "Withdraw change request" } else { "Request changes" },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(!can_write)
                        .when_some(write_tooltip, |b, t| b.tooltip(Tooltip::text(t)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let request = plan::request_changes(&this.key.repo, this.key.id, !i_requested_changes);
                            this.write(request, cx)
                        })),
                    )
                    .child(
                        Button::new("pipelines", "Pipelines")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| this.open_pipelines(window, cx))),
                    )
                    .child(
                        Button::new("refresh", if data.loading { "Refreshing…" } else { "Refresh" })
                            .disabled(data.loading)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let key = this.key.clone();
                                this.store.update(cx, |store, cx| store.refresh_pull_request(key, cx));
                            })),
                    )
                    .when_some(web_url, |row, url| {
                        row.child(Button::new("open-web", "Open in Bitbucket").on_click(move |_, _, cx| cx.open_url(&url)))
                    })
                    .when(ready, |row| {
                        row.child(
                            Button::new("close-review", "Close review")
                                .tooltip(Tooltip::text("Remove the review's worktree from the project and from disk"))
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
                    Label::new("The pull request has new commits since this checkout; close and reopen the review to see them")
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .when_some(self.write_error.clone().or(data.error), |header, error| {
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
                        .when(self.show_description, |col| col.child(div().text_sm().child(pr.description.clone()))),
                )
            })
            .into_any_element()
    }

    fn render_thread(&mut self, ix: usize, _window: &mut Window, cx: &mut gpui::Context<Self>) -> AnyElement {
        let Some(thread) = self.threads.get(ix).cloned() else {
            return div().into_any_element();
        };
        let reply_editor = self
            .reply
            .as_ref()
            .filter(|r| r.root_id == thread.root.id)
            .map(|r| r.editor.clone());
        // A removed line's number means nothing in the checked-out file.
        let go_to = matches!(thread.place, Place::NewLine { .. })
            .then(|| {
                let thread = thread.clone();
                Button::new(("go-to", thread.root.id), "Go to line")
                    .style(ButtonStyle::Subtle)
                    .disabled(!matches!(self.checkout, CheckoutState::Ready(_)))
                    .on_click(cx.listener(move |this, _, window, cx| this.go_to_thread(&thread, window, cx)))
                    .into_any_element()
            });
        div()
            .px_3()
            .py_1()
            .child(thread_view::render_card(
                &self.key,
                &thread,
                ThreadHost::Review(cx.weak_entity()),
                reply_editor,
                go_to,
                cx,
            ))
            .into_any_element()
    }
}

/// Zed's branch diff of the review worktree against the pull request's
/// destination commit.
fn open_diff(
    workspace: &mut Workspace,
    project: Entity<Project>,
    repository: Entity<Repository>,
    base: SharedString,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let repo_id = repository.read(cx).id;
    let display = project.read(cx).git_store().read(cx).display_diff_for_repo(repo_id);
    BranchDiff::deploy_branch_diff_with_base_ref(workspace, project, repository, base, display, window, cx);
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
    pr.participants
        .iter()
        .any(|p| p.user.uuid.as_deref() == Some(my_uuid) && p.role == "REVIEWER" && !p.approved && p.state.is_none())
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

impl EventEmitter<PrReviewEvent> for PrReview {}

impl Focusable for PrReview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PrReview {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let empty = self.threads.is_empty();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .child(Label::new(format!("Conversation ({} threads)", self.threads.len())).size(LabelSize::Small).color(Color::Muted)),
            )
            .when(empty, |col| col.child(div().p_3().child(Label::new("No comments yet.").color(Color::Muted))))
            .child(list(self.list.clone(), cx.processor(Self::render_thread)).flex_1().size_full())
    }
}

impl Item for PrReview {
    type Event = PrReviewEvent;

    fn to_item_events(_: &PrReviewEvent, _: &mut dyn FnMut(ItemEvent)) {}

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("PR #{} {}", self.key.id, self.key.repo.slug).into()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let title = self.pull_request(cx).map(|pr| pr.title).unwrap_or_default();
        Some(format!("{} #{}: {}", self.key.repo, self.key.id, title).into())
    }
}
