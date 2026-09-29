//! The Bitbucket pipelines of one branch: their state, steps and logs, with
//! rerun and stop buttons. Refreshes on its own while a pipeline is running.

use anyhow::Result;
use bitbucket::{
    BitbucketStore, PipelineKey, RepoRef, StoreEvent,
    api::{NewPipeline, plan},
    models::{Pipeline, Step},
};
use collections::HashSet;
use editor::Editor;
use gpui::{
    App, AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, WeakEntity,
};
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use std::time::Duration;
use ui::{Tooltip, prelude::*};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
    notifications::NotifyTaskExt,
};

/// How often the list is refetched while any pipeline is pending or running.
const POLL_INTERVAL: Duration = Duration::from_secs(15);

pub struct PrPipelines {
    key: PipelineKey,
    store: Entity<BitbucketStore>,
    workspace: WeakEntity<Workspace>,
    expanded: HashSet<String>,
    error: Option<String>,
    focus_handle: FocusHandle,
    _poll: Task<()>,
    _subscription: Subscription,
}

pub enum PrPipelinesEvent {}

impl EventEmitter<PrPipelinesEvent> for PrPipelines {}

impl PrPipelines {
    pub fn open(
        workspace: &mut Workspace,
        repo: RepoRef,
        branch: String,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) {
        let key = PipelineKey { repo, branch };
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|item| item.read(cx).key == key);
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let workspace_handle = cx.weak_entity();
        let view = cx.new(|cx| Self::new(key, workspace_handle, cx));
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    fn new(key: PipelineKey, workspace: WeakEntity<Workspace>, cx: &mut gpui::Context<Self>) -> Self {
        let store = BitbucketStore::global(cx);
        let subscription = cx.subscribe(&store, |this, _, event, cx| match event {
            StoreEvent::PipelinesUpdated(key) if *key == this.key => cx.notify(),
            StoreEvent::StepsUpdated { repo, .. } if *repo == this.key.repo => cx.notify(),
            _ => {}
        });
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let refresh = this.update(cx, |this, cx| this.refresh(cx));
                let Ok(refresh) = refresh else {
                    break;
                };
                let result = refresh.await;
                let keep_polling = this
                    .update(cx, |this, cx| {
                        this.error = result.err().map(|e| format!("{e:#}"));
                        cx.notify();
                        this.any_running(cx)
                    })
                    .unwrap_or(false);
                // Without a running pipeline, poll at a quarter of the rate.
                let wait = if keep_polling { POLL_INTERVAL } else { POLL_INTERVAL * 4 };
                cx.background_executor().timer(wait).await;
            }
        });
        Self {
            key,
            store,
            workspace,
            expanded: HashSet::default(),
            error: None,
            focus_handle: cx.focus_handle(),
            _poll: poll,
            _subscription: subscription,
        }
    }

    fn pipelines(&self, cx: &App) -> Vec<Pipeline> {
        self.store
            .read(cx)
            .pipelines(&self.key)
            .map(<[Pipeline]>::to_vec)
            .unwrap_or_default()
    }

    fn any_running(&self, cx: &App) -> bool {
        self.pipelines(cx).iter().any(|p| p.state.name != "COMPLETED")
    }

    /// Refetch the list, and the steps of every expanded pipeline.
    fn refresh(&mut self, cx: &mut gpui::Context<Self>) -> Task<Result<()>> {
        let key = self.key.clone();
        let expanded: Vec<String> = self.expanded.iter().cloned().collect();
        let list = self.store.update(cx, |store, cx| store.refresh_pipelines(key.clone(), cx));
        let steps: Vec<_> = expanded
            .into_iter()
            .map(|uuid| {
                self.store
                    .update(cx, |store, cx| store.refresh_steps(key.repo.clone(), uuid, cx))
            })
            .collect();
        cx.background_spawn(async move {
            list.await?;
            for step in steps {
                step.await?;
            }
            Ok(())
        })
    }

    fn toggle(&mut self, uuid: String, cx: &mut gpui::Context<Self>) {
        if !self.expanded.remove(&uuid) {
            self.expanded.insert(uuid.clone());
            self.store
                .update(cx, |store, cx| store.refresh_steps(self.key.repo.clone(), uuid, cx))
                .detach_and_log_err(cx);
        }
        cx.notify();
    }

    fn write(&mut self, request: bitbucket::api::WriteRequest, cx: &mut gpui::Context<Self>) {
        let send = self.store.update(cx, |store, cx| store.execute(request, None, cx));
        cx.spawn(async move |this, cx| {
            let result = send.await;
            this.update(cx, |this, cx| {
                this.error = result.err().map(|e| format!("{e:#}"));
                this.refresh(cx).detach_and_log_err(cx);
            })
            .ok();
        })
        .detach();
    }

    fn rerun(&mut self, cx: &mut gpui::Context<Self>) {
        let request = plan::trigger_pipeline(
            &self.key.repo,
            &NewPipeline {
                branch: self.key.branch.clone(),
                commit: None,
                custom: None,
            },
        );
        self.write(request, cx);
    }

    fn stop(&mut self, uuid: &str, cx: &mut gpui::Context<Self>) {
        let request = plan::stop_pipeline(&self.key.repo, uuid);
        self.write(request, cx);
    }

    /// Open a step's log as a read-only buffer in its own tab, so search and
    /// go-to-line work on it.
    fn open_log(&mut self, pipeline: &Pipeline, step: &Step, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let title = format!("#{} {} log", pipeline.build_number, step.name);
        let fetch = self.store.update(cx, |store, cx| {
            store.step_log(self.key.repo.clone(), pipeline.uuid.clone(), step.uuid.clone(), cx)
        });
        let workspace = self.workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let log = fetch.await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    let project = workspace.project().clone();
                    let buffer = cx.new(|cx| {
                        let mut buffer = Buffer::local(log, cx);
                        buffer.set_capability(Capability::ReadOnly, cx);
                        buffer
                    });
                    let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));
                    let editor = cx.new(|cx| Editor::for_multibuffer(multibuffer, Some(project), window, cx));
                    workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
                })?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    fn render_pipeline(&self, pipeline: &Pipeline, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let can_write = self.store.read(cx).is_connected();
        let label = pipeline.state.label().to_string();
        let color = state_color(&label);
        let running = pipeline.state.name != "COMPLETED";
        let expanded = self.expanded.contains(&pipeline.uuid);
        let uuid = pipeline.uuid.clone();
        let url = format!(
            "{}/pipelines/results/{}",
            self.key.repo.web_url(),
            pipeline.build_number
        );
        let commit = pipeline
            .target
            .commit
            .as_ref()
            .map(|c| c.hash.get(..8).unwrap_or(&c.hash).to_string())
            .unwrap_or_default();
        let trigger = pipeline
            .creator
            .as_ref()
            .map(|a| a.display_name.clone())
            .or_else(|| pipeline.trigger.as_ref().map(|t| t.name.to_lowercase()))
            .unwrap_or_default();
        let custom = pipeline
            .target
            .selector
            .as_ref()
            .and_then(|s| s.pattern.clone())
            .filter(|_| pipeline.target.selector.as_ref().is_some_and(|s| s.kind == "custom"));
        let steps: Vec<Step> = if expanded {
            self.store
                .read(cx)
                .steps(&self.key.repo, &pipeline.uuid)
                .map(<[Step]>::to_vec)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let pipeline_for_logs = pipeline.clone();
        let colors = cx.theme().colors();

        v_flex()
            .p_2()
            .gap_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("toggle-{uuid}")), format!("#{}", pipeline.build_number))
                            .start_icon(Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight }).size(IconSize::Small))
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener({
                                let uuid = uuid.clone();
                                move |this, _, _, cx| this.toggle(uuid.clone(), cx)
                            })),
                    )
                    .child(Label::new(label.to_lowercase()).color(color))
                    .when_some(custom, |row, name| row.child(Label::new(format!("custom: {name}")).color(Color::Muted)))
                    .child(Label::new(commit).size(LabelSize::Small).color(Color::Muted))
                    .child(Label::new(trigger).size(LabelSize::Small).color(Color::Muted))
                    .when_some(pipeline.duration_in_seconds, |row, secs| {
                        row.child(Label::new(format!("{}m {:02}s", secs / 60, secs % 60)).size(LabelSize::Small).color(Color::Muted))
                    })
                    .child(div().flex_1())
                    .when(running, |row| {
                        row.child(
                            Button::new(SharedString::from(format!("stop-{uuid}")), "Stop")
                                .disabled(!can_write)
                                .on_click(cx.listener({
                                    let uuid = uuid.clone();
                                    move |this, _, _, cx| this.stop(&uuid, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("web-{uuid}")), "Open in Bitbucket")
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            )
            .when(expanded, |col| {
                col.children(steps.into_iter().map(|step| {
                    let step_label = step.state.label().to_string();
                    let pipeline = pipeline_for_logs.clone();
                    let step_for_log = step.clone();
                    h_flex()
                        .pl_6()
                        .gap_2()
                        .child(Label::new(step.name.clone()).size(LabelSize::Small))
                        .child(Label::new(step_label.to_lowercase()).size(LabelSize::Small).color(state_color(&step_label)))
                        .when_some(step.duration_in_seconds, |row, secs| {
                            row.child(Label::new(format!("{}m {:02}s", secs / 60, secs % 60)).size(LabelSize::Small).color(Color::Muted))
                        })
                        .child(
                            Button::new(SharedString::from(format!("log-{}", step.uuid)), "Log")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_log(&pipeline, &step_for_log, window, cx)
                                })),
                        )
                }))
            })
            .into_any_element()
    }
}

fn state_color(label: &str) -> Color {
    match label {
        "SUCCESSFUL" => Color::Success,
        "FAILED" | "ERROR" => Color::Error,
        "STOPPED" => Color::Muted,
        _ => Color::Warning,
    }
}

impl Focusable for PrPipelines {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PrPipelines {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let pipelines = self.pipelines(cx);
        let can_write = self.store.read(cx).is_connected();
        let config_error = self.store.read(cx).connection_error().map(str::to_string);
        let rows: Vec<gpui::AnyElement> = pipelines.iter().map(|p| self.render_pipeline(p, cx)).collect();
        let colors = cx.theme().colors();
        v_flex()
            .id("pr-pipelines")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(colors.editor_background)
            .overflow_y_scroll()
            .child(
                h_flex()
                    .p_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Headline::new(format!("Pipelines on {}", self.key.branch)).size(HeadlineSize::Small))
                    .child(Label::new(self.key.repo.to_string()).color(Color::Muted))
                    .child(div().flex_1())
                    .child(
                        Button::new("run-branch", "Run pipeline on branch")
                            .style(ButtonStyle::Filled)
                            .disabled(!can_write)
                            .when(!can_write, |b| b.tooltip(Tooltip::text("Connect to Bitbucket in the Pull requests panel first")))
                            .on_click(cx.listener(|this, _, _, cx| this.rerun(cx))),
                    )
                    .child(
                        Button::new("refresh-pipelines", "Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx).detach_and_log_err(cx))),
                    ),
            )
            .when_some(self.error.clone().or(config_error), |col, error| {
                col.child(div().p_3().child(Label::new(error).color(Color::Error)))
            })
            .when(pipelines.is_empty() && self.error.is_none(), |col| {
                col.child(div().p_3().child(Label::new("No pipelines on this branch yet.").color(Color::Muted)))
            })
            .children(rows)
    }
}

impl Item for PrPipelines {
    type Event = PrPipelinesEvent;

    fn to_item_events(_: &PrPipelinesEvent, _: &mut dyn FnMut(ItemEvent)) {}

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PlayOutlined).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Pipelines: {}", self.key.branch).into()
    }
}
