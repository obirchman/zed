//! Creating a Bitbucket pull request from the current branch: title,
//! description, destination and reviewers, with a push first when the branch
//! is not on Bitbucket yet. The new pull request opens as a review.

use crate::pr_review::PrReview;
use bitbucket::{
    BitbucketStore, PrKey, PrWatch, RepoRef,
    api::NewPullRequest,
    create::{CreateContext, resolve_names},
};
use collections::HashSet;
use editor::Editor;
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    WeakEntity,
};
use std::path::PathBuf;
use ui::{Checkbox, ToggleState, Tooltip, prelude::*};
use workspace::{ModalView, Workspace};

pub struct CreatePrModal {
    repo: RepoRef,
    repo_root: PathBuf,
    workspace: WeakEntity<Workspace>,
    source: String,
    /// The local branch head, to compare with Bitbucket's.
    local_head: Option<String>,
    context: Option<CreateContext>,
    loading: bool,
    title: Entity<Editor>,
    description: Entity<Editor>,
    destination: Entity<Editor>,
    other_reviewers: Entity<Editor>,
    selected_defaults: HashSet<String>,
    close_source_branch: bool,
    draft: bool,
    creating: bool,
    error: Option<String>,
    focus_handle: FocusHandle,
}

/// Whether the active repository is on Bitbucket, so creating a pull request
/// opens this form instead of the browser.
pub fn active_repository_is_on_bitbucket(workspace: &Workspace, cx: &App) -> bool {
    workspace.project().read(cx).active_repository(cx).is_some_and(|repo| {
        let repo = repo.read(cx);
        repo.remote_origin_url
            .as_deref()
            .or(repo.remote_upstream_url.as_deref())
            .and_then(BitbucketStore::repo_for_remote)
            .is_some()
    })
}

impl CreatePrModal {
    pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
        let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
            return;
        };
        let (repo, repo_root, source, local_head, last_subject) = {
            let r = repository.read(cx);
            let Some(repo) = r
                .remote_origin_url
                .as_deref()
                .or(r.remote_upstream_url.as_deref())
                .and_then(BitbucketStore::repo_for_remote)
            else {
                return;
            };
            let Some(branch) = r.branch.as_ref() else {
                return;
            };
            (
                repo,
                r.work_directory_abs_path.to_path_buf(),
                branch.name().to_string(),
                r.head_commit.as_ref().map(|c| c.sha.to_string()),
                branch.most_recent_commit.as_ref().map(|c| c.subject.to_string()),
            )
        };
        let workspace_handle = cx.weak_entity();
        workspace.toggle_modal(window, cx, |window, cx| {
            Self::new(repo, repo_root, source, local_head, last_subject, workspace_handle, window, cx)
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        repo: RepoRef,
        repo_root: PathBuf,
        source: String,
        local_head: Option<String>,
        last_subject: Option<String>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let single_line = |placeholder: &'static str, text: Option<String>, window: &mut Window, cx: &mut gpui::Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                if let Some(text) = text {
                    editor.set_text(text, window, cx);
                }
                editor
            })
        };
        let title = single_line("Title", last_subject, window, cx);
        let destination = single_line("Destination branch", None, window, cx);
        let other_reviewers = single_line("More reviewers: names, comma-separated", None, window, cx);
        let description = cx.new(|cx| {
            let mut editor = Editor::auto_height(4, 16, window, cx);
            editor.set_placeholder_text("Description", window, cx);
            editor
        });
        window.focus(&title.focus_handle(cx), cx);
        let mut this = Self {
            repo,
            repo_root,
            workspace,
            source,
            local_head,
            context: None,
            loading: false,
            title,
            description,
            destination,
            other_reviewers,
            selected_defaults: HashSet::default(),
            close_source_branch: true,
            draft: false,
            creating: false,
            error: None,
            focus_handle: cx.focus_handle(),
        };
        this.load(window, cx);
        this
    }

    /// Ask Bitbucket for the main branch, whether the branch is pushed, and
    /// the reviewers; then fill the empty fields.
    fn load(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        self.loading = true;
        self.error = None;
        let load = BitbucketStore::global(cx).update(cx, |store, cx| {
            store.create_context(self.repo.clone(), self.source.clone(), cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Ok(context) => this.context_loaded(context, window, cx),
                    Err(e) => this.error = Some(format!("{e:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn context_loaded(&mut self, context: CreateContext, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.destination.read(cx).text(cx).is_empty()
            && let Some(main) = &context.main_branch
        {
            self.destination.update(cx, |editor, cx| editor.set_text(main.as_str(), window, cx));
            self.fill_description(main.clone(), window, cx);
        }
        let me = BitbucketStore::global(cx).read(cx).me().and_then(|me| me.uuid.clone());
        if self.context.is_none() {
            // Bitbucket refuses the author as a reviewer.
            self.selected_defaults = context
                .default_reviewers
                .iter()
                .filter_map(|r| r.uuid.clone())
                .filter(|uuid| Some(uuid) != me.as_ref())
                .collect();
        }
        self.context = Some(context);
    }

    /// The subjects of the commits the pull request would bring, as a list.
    fn fill_description(&mut self, destination: String, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if !self.description.read(cx).text(cx).is_empty() {
            return;
        }
        let root = self.repo_root.clone();
        let range = format!("origin/{destination}..HEAD");
        let log = cx.background_spawn(async move {
            let output = util::command::new_command("git")
                .current_dir(&root)
                .args(["log", "--reverse", "--format=- %s", &range])
                .output()
                .await?;
            anyhow::Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        });
        let description = self.description.clone();
        cx.spawn_in(window, async move |_, cx| {
            if let Ok(text) = log.await
                && !text.is_empty()
            {
                description
                    .update_in(cx, |editor, window, cx| {
                        if editor.text(cx).is_empty() {
                            editor.set_text(text.as_str(), window, cx);
                        }
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn create(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(context) = &self.context else {
            return;
        };
        let title = self.title.read(cx).text(cx).trim().to_string();
        if title.is_empty() {
            self.error = Some("A title is needed".into());
            cx.notify();
            return;
        }
        let destination = self.destination.read(cx).text(cx).trim().to_string();
        if destination.is_empty() {
            self.error = Some("A destination branch is needed".into());
            cx.notify();
            return;
        }
        let mut reviewers: Vec<String> = self.selected_defaults.iter().cloned().collect();
        match resolve_names(&self.other_reviewers.read(cx).text(cx), &context.members) {
            Ok(more) => reviewers.extend(more),
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                cx.notify();
                return;
            }
        }
        reviewers.sort();
        reviewers.dedup();
        let pr = NewPullRequest {
            title,
            description: self.description.read(cx).text(cx),
            source_branch: self.source.clone(),
            destination_branch: destination,
            close_source_branch: self.close_source_branch,
            draft: self.draft,
            reviewers,
        };
        self.creating = true;
        self.error = None;
        let repo = self.repo.clone();
        let create = BitbucketStore::global(cx).update(cx, |store, cx| store.create_pull_request(repo.clone(), pr, cx));
        let (workspace, repo_root) = (self.workspace.clone(), self.repo_root.clone());
        cx.spawn_in(window, async move |this, cx| {
            match create.await {
                Ok((pr, _created)) => {
                    let key = PrKey { repo, id: pr.id };
                    this.update(cx, |_, cx| {
                        if let Some(watch) = PrWatch::try_global(cx) {
                            watch.update(cx, |watch, cx| watch.check_now(cx));
                        }
                        cx.emit(DismissEvent);
                    })
                    .ok();
                    workspace
                        .update_in(cx, |workspace, window, cx| PrReview::open(workspace, key, repo_root, window, cx))
                        .ok();
                }
                Err(e) => {
                    this.update(cx, |this, cx| {
                        this.creating = false;
                        this.error = Some(format!("{e:#}"));
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn dismiss(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut gpui::Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_push_status(&self, cx: &mut gpui::Context<Self>) -> Option<AnyElement> {
        let context = self.context.as_ref()?;
        let (text, color, needs_push) = match (&context.source_remote_head, &self.local_head) {
            (None, _) => (format!("{} is not on Bitbucket yet.", self.source), Color::Warning, true),
            (Some(remote), Some(local)) if !local.starts_with(remote.as_str()) && !remote.starts_with(local.as_str()) => (
                format!(
                    "Bitbucket's {} is at {}, your local branch at {}; the pull request uses Bitbucket's.",
                    self.source,
                    &remote[..remote.len().min(8)],
                    &local[..local.len().min(8)]
                ),
                Color::Warning,
                true,
            ),
            _ => (format!("{} is pushed.", self.source), Color::Muted, false),
        };
        Some(
            h_flex()
                .gap_2()
                .child(Label::new(text).size(LabelSize::Small).color(color))
                .when(needs_push, |row| {
                    row.child(
                        Button::new("push-branch", "Push")
                            .style(ButtonStyle::Filled)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(git::Push), cx)),
                    )
                    .child(
                        Button::new("recheck-branch", "Check again")
                            .on_click(cx.listener(|this, _, window, cx| this.load(window, cx))),
                    )
                })
                .into_any_element(),
        )
    }
}

impl EventEmitter<DismissEvent> for CreatePrModal {}

impl ModalView for CreatePrModal {}

impl Focusable for CreatePrModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.title.focus_handle(cx)
    }
}

impl Render for CreatePrModal {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let push_status = self.render_push_status(cx);
        let border = cx.theme().colors().border;
        let field = |label: &'static str, editor: Entity<Editor>| {
            v_flex()
                .gap_0p5()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .child(
                    div()
                        .px_1p5()
                        .py_1()
                        .border_1()
                        .border_color(border)
                        .rounded_sm()
                        .child(editor),
                )
        };
        let defaults: Vec<(String, String)> = self
            .context
            .as_ref()
            .map(|c| {
                c.default_reviewers
                    .iter()
                    .filter_map(|r| Some((r.uuid.clone()?, r.display_name.clone())))
                    .collect()
            })
            .unwrap_or_default();
        let can_create = self.context.is_some() && !self.creating;

        v_flex()
            .key_context("CreatePullRequest")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::dismiss))
            .elevation_3(cx)
            .w(rems(42.))
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(Headline::new("Create pull request").size(HeadlineSize::Small))
                    .child(Label::new(self.repo.to_string()).color(Color::Muted)),
            )
            .child(
                Label::new(format!("From {}", self.source))
                    .size(LabelSize::Small),
            )
            .when(self.loading, |col| {
                col.child(Label::new("Checking the branch on Bitbucket…").size(LabelSize::Small).color(Color::Muted))
            })
            .children(push_status)
            .child(field("Into", self.destination.clone()))
            .child(field("Title", self.title.clone()))
            .child(field("Description", self.description.clone()))
            .when(!defaults.is_empty(), |col| {
                col.child(
                    v_flex()
                        .gap_0p5()
                        .child(Label::new("Default reviewers").size(LabelSize::Small).color(Color::Muted))
                        .child(h_flex().gap_3().flex_wrap().children(defaults.into_iter().map(|(uuid, name)| {
                            let checked = self.selected_defaults.contains(&uuid);
                            let this = cx.weak_entity();
                            Checkbox::new(format!("reviewer-{uuid}"), ToggleState::from(checked))
                                .label(name)
                                .on_click(move |_, _, cx| {
                                    this.update(cx, |this, cx| {
                                        if !this.selected_defaults.remove(&uuid) {
                                            this.selected_defaults.insert(uuid.clone());
                                        }
                                        cx.notify();
                                    })
                                    .ok();
                                })
                        }))),
                )
            })
            .child(field("More reviewers", self.other_reviewers.clone()))
            .child(
                h_flex()
                    .gap_4()
                    .child({
                        let this = cx.weak_entity();
                        Checkbox::new("close-source-branch", ToggleState::from(self.close_source_branch))
                            .label("Close source branch after merge")
                            .on_click(move |_, _, cx| {
                                this.update(cx, |this, cx| {
                                    this.close_source_branch = !this.close_source_branch;
                                    cx.notify();
                                })
                                .ok();
                            })
                    })
                    .child({
                        let this = cx.weak_entity();
                        Checkbox::new("draft", ToggleState::from(self.draft))
                            .label("Draft")
                            .on_click(move |_, _, cx| {
                                this.update(cx, |this, cx| {
                                    this.draft = !this.draft;
                                    cx.notify();
                                })
                                .ok();
                            })
                    }),
            )
            .when_some(self.error.clone(), |col, error| {
                col.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(Button::new("cancel-create-pr", "Cancel").on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))))
                    .child(
                        Button::new("create-pr", if self.creating { "Creating…" } else { "Create pull request" })
                            .style(ButtonStyle::Filled)
                            .disabled(!can_create)
                            .tooltip(Tooltip::text(
                                "Returns the already-open pull request between these branches instead of opening a second one",
                            ))
                            .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                    ),
            )
    }
}
