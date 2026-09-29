//! The Pull requests panel: open pull requests the user authored or reviews,
//! each with what changed since the user last marked it handled. A change
//! after a handled mark raises one desktop notification.

use crate::pr_review::PrReview;
use anyhow::Result;
use bitbucket::{BitbucketSettings, BitbucketStore, Connection, PrKey, PrWatch, WatchEvent, WatchedPr, find_local_clone};
use bitbucket::models::ParticipantState;
use editor::Editor;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle,
    Focusable, Render, Subscription, SystemNotification, SystemNotificationAction,
    WeakEntity, actions,
};
use settings::Settings as _;
use std::time::SystemTime;
use ui::{Tooltip, prelude::*};
use workspace::{
    MultiWorkspace, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotifyTaskExt,
};

actions!(
    pr_inbox,
    [
        /// Opens or focuses the Pull requests panel.
        ToggleFocus,
    ]
);

const NOTIFICATION_TAG_PREFIX: &str = "pr:";

pub(crate) fn init(cx: &mut App) {
    let channel = *release_channel::RELEASE_CHANNEL;
    cx.set_app_identity(channel.app_id(), channel.display_name());
    let Some(watch) = PrWatch::try_global(cx) else {
        return;
    };
    cx.subscribe(&watch, |watch, event, cx| {
        let WatchEvent::BecameChanged(key) = event else {
            return;
        };
        if !BitbucketSettings::get_global(cx).notifications {
            return;
        }
        let Some(pr) = watch.read(cx).get(key).cloned() else {
            return;
        };
        cx.show_system_notification(SystemNotification {
            tag: tag_for(key).into(),
            title: format!("{} #{}: {}", key.repo.slug, key.id, pr.pull_request.title).into(),
            body: pr.summary().into(),
            actions: vec![
                SystemNotificationAction {
                    id: "open".into(),
                    label: "Open review".into(),
                },
                SystemNotificationAction {
                    id: "handled".into(),
                    label: "Mark handled".into(),
                },
            ],
        });
    })
    .detach();
    cx.on_system_notification_response(|response, cx| {
        let Some(key) = key_from_tag(&response.tag) else {
            return;
        };
        if response.action_id.as_deref() == Some("handled") {
            if let Some(watch) = PrWatch::try_global(cx) {
                watch.update(cx, |watch, cx| watch.mark_handled(&key, cx));
            }
        } else {
            open_review_in_any_window(key, cx);
        }
    });
}

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<PrInboxPanel>(window, cx);
    });
}

fn tag_for(key: &PrKey) -> String {
    format!("{NOTIFICATION_TAG_PREFIX}{}#{}", key.repo, key.id)
}

fn key_from_tag(tag: &str) -> Option<PrKey> {
    let (repo, id) = tag.strip_prefix(NOTIFICATION_TAG_PREFIX)?.split_once('#')?;
    Some(PrKey {
        repo: repo.parse().ok()?,
        id: id.parse().ok()?,
    })
}

/// Open the review in the first window that has a workspace, and bring that
/// window forward. Without any window, the hook set by
/// `set_open_without_window` opens one.
pub fn open_review_in_any_window(key: PrKey, cx: &mut App) {
    if open_review_in_existing_window(key.clone(), cx) {
        return;
    }
    OPEN_WITHOUT_WINDOW.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(key, cx);
        }
    });
}

/// Open the review in the first window that has a workspace. False when no
/// such window is open.
pub fn open_review_in_existing_window(key: PrKey, cx: &mut App) -> bool {
    for window in cx.windows() {
        let Some(multi_workspace) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        let opened = multi_workspace.update(cx, |multi_workspace, window, cx| {
            window.activate_window();
            let workspace = multi_workspace.workspace().clone();
            open_review(workspace.downgrade(), key.clone(), window, cx);
        });
        if opened.is_ok() {
            return true;
        }
    }
    false
}

thread_local! {
    /// How to open a review when no window is open. The app sets this; it
    /// needs `AppState` to open a new window, which this crate does not have.
    static OPEN_WITHOUT_WINDOW: std::cell::RefCell<Option<Box<dyn Fn(PrKey, &mut App)>>> = const { std::cell::RefCell::new(None) };
}

pub fn set_open_without_window(hook: impl Fn(PrKey, &mut App) + 'static) {
    OPEN_WITHOUT_WINDOW.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
}

/// Find the user's clone of the pull request's repository, then open the
/// review. Without a clone, say where it was looked for.
pub fn open_review(workspace: WeakEntity<Workspace>, key: PrKey, window: &mut Window, cx: &mut App) {
    let roots = BitbucketSettings::get_global(cx).clone_roots.clone();
    let repo = key.repo.clone();
    let search = cx.background_spawn(async move { find_local_clone(&repo, &roots).ok_or(roots) });
    let error_workspace = workspace.clone();
    window
        .spawn(cx, async move |cx| {
            let root = match search.await {
                Ok(root) => root,
                Err(roots) => anyhow::bail!(
                    "No clone of {} found under {}. Clone it there to review its pull requests in Zed.",
                    key.repo,
                    roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")
                ),
            };
            workspace.update_in(cx, |workspace, window, cx| PrReview::open(workspace, key, root, window, cx))?;
            anyhow::Ok(())
        })
        .detach_and_notify_err(error_workspace, window, cx);
}

/// The XDG autostart entry that starts this Zed at login with `--background`.
fn autostart_file() -> std::path::PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".config"));
    config
        .join("autostart")
        .join(format!("{}.desktop", release_channel::RELEASE_CHANNEL.app_id()))
}

fn set_start_at_login(enabled: bool) -> Result<()> {
    let file = autostart_file();
    if !enabled {
        if file.exists() {
            std::fs::remove_file(&file)?;
        }
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let entry = [
        "[Desktop Entry]".to_string(),
        "Type=Application".to_string(),
        "Name=Zed (background)".to_string(),
        "Comment=Watches Bitbucket pull requests".to_string(),
        format!("Exec=\"{}\" --background", exe.display()),
        "NoDisplay=true".to_string(),
        "X-GNOME-Autostart-enabled=true".to_string(),
    ]
    .join("\n");
    std::fs::write(&file, entry + "\n")?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RoleFilter {
    Both,
    Reviewer,
    Author,
}

pub struct PrInboxPanel {
    watch: Entity<PrWatch>,
    store: Entity<BitbucketStore>,
    email: Entity<Editor>,
    token: Entity<Editor>,
    connecting: bool,
    connect_error: Option<String>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    changed_only: bool,
    role: RoleFilter,
    search: Entity<Editor>,
    _subscriptions: Vec<Subscription>,
}

impl PrInboxPanel {
    pub async fn load(workspace: WeakEntity<Workspace>, mut cx: AsyncWindowContext) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let workspace_handle = workspace.weak_handle();
            cx.new(|cx| Self::new(workspace_handle, window, cx))
        })
    }

    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut gpui::Context<Self>) -> Self {
        let watch = PrWatch::global(cx);
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter by repository, title, author or #id", window, cx);
            editor
        });
        let store = BitbucketStore::global(cx);
        let email = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Atlassian account email (empty for an access token)", window, cx);
            editor
        });
        let token = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("API token", window, cx);
            editor.set_masked(true, cx);
            editor
        });
        let subscriptions = vec![
            cx.observe(&watch, |_, _, cx| cx.notify()),
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |_, _, event: &editor::EditorEvent, cx| {
                if matches!(event, editor::EditorEvent::BufferEdited) {
                    cx.notify();
                }
            }),
        ];
        Self {
            watch,
            store,
            email,
            token,
            connecting: false,
            connect_error: None,
            workspace,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            changed_only: false,
            role: RoleFilter::Both,
            search,
            _subscriptions: subscriptions,
        }
    }

    fn visible(&self, cx: &App) -> Vec<WatchedPr> {
        let query = self.search.read(cx).text(cx).to_lowercase();
        let mut rows: Vec<WatchedPr> = self
            .watch
            .read(cx)
            .prs()
            .iter()
            .filter(|p| !self.changed_only || p.is_changed())
            .filter(|p| match self.role {
                RoleFilter::Both => true,
                RoleFilter::Reviewer => p.reviewer,
                RoleFilter::Author => p.author,
            })
            .filter(|p| {
                query.is_empty() || {
                    let haystack = format!(
                        "{} #{} {} {}",
                        p.key.repo, p.key.id, p.pull_request.title, p.pull_request.author.display_name
                    )
                    .to_lowercase();
                    query.split_whitespace().all(|word| haystack.contains(word))
                }
            })
            .cloned()
            .collect();
        // Changed pull requests first, each group most recently updated first.
        rows.sort_by_key(|p| !p.is_changed());
        rows
    }

    fn render_row(&self, pr: &WatchedPr, ix: usize, cx: &mut gpui::Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let key = pr.key.clone();
        let me = bitbucket::BitbucketStore::global(cx).read(cx).me().cloned();
        let my_state = me.as_ref().and_then(|me| {
            pr.pull_request
                .participants
                .iter()
                .find(|p| p.user.uuid.is_some() && p.user.uuid == me.uuid)
        });
        let decision = match my_state {
            Some(p) if p.approved => Some(("you approved", Color::Success)),
            Some(p) if p.state == Some(ParticipantState::ChangesRequested) => Some(("you requested changes", Color::Error)),
            _ => None,
        };
        let role = match (pr.author, pr.reviewer) {
            (true, true) => "author, reviewer",
            (true, false) => "author",
            _ => "reviewer",
        };
        let url = pr.pull_request.links.html.as_ref().map(|h| h.href.clone());
        let changed = pr.is_changed();
        v_flex()
            .id(("pr-row", ix))
            .px_2()
            .py_1p5()
            .gap_0p5()
            .border_b_1()
            .border_color(colors.border_variant)
            .when(changed, |row| row.border_l_2().border_color(colors.text_accent))
            .hover(|row| row.bg(colors.element_hover))
            .cursor_pointer()
            .on_click(cx.listener({
                let key = key.clone();
                move |this, _, window, cx| {
                    open_review(this.workspace.clone(), key.clone(), window, cx);
                }
            }))
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new(format!("{} #{}", pr.key.repo.slug, pr.key.id)).size(LabelSize::Small).color(Color::Muted))
                    .child(Label::new(pr.pull_request.title.clone()).size(LabelSize::Small).truncate()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(pr.pull_request.author.display_name.clone()).size(LabelSize::XSmall).color(Color::Muted))
                    .child(Label::new(role).size(LabelSize::XSmall).color(Color::Muted))
                    .when_some(decision, |row, (text, color)| row.child(Label::new(text).size(LabelSize::XSmall).color(color))),
            )
            .when(changed, |col| {
                col.child(Label::new(pr.summary()).size(LabelSize::Small).color(Color::Accent))
            })
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(("open-review", ix), "Open review")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener({
                                let key = key.clone();
                                move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    open_review(this.workspace.clone(), key.clone(), window, cx);
                                }
                            })),
                    )
                    .when(changed, |row| {
                        row.child(
                            Button::new(("mark-handled", ix), "Mark handled")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text("No more notifications for this pull request until something new happens"))
                                .on_click(cx.listener({
                                    let key = key.clone();
                                    move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.watch.update(cx, |watch, cx| watch.mark_handled(&key, cx));
                                    }
                                })),
                        )
                    })
                    .when_some(url, |row, url| {
                        row.child(
                            Button::new(("open-web", ix), "Bitbucket")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.open_url(&url)
                                }),
                        )
                    }),
            )
            .into_any_element()
    }
}

fn describe_age(time: SystemTime) -> String {
    let secs = time.elapsed().map(|d| d.as_secs()).unwrap_or_default();
    match secs {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", secs / 60),
        _ => format!("{} h ago", secs / 3600),
    }
}

impl EventEmitter<PanelEvent> for PrInboxPanel {}

impl Focusable for PrInboxPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PrInboxPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let connection = self.store.read(cx).connection().clone();
        if connection != Connection::Connected {
            let body = match connection {
                Connection::Loading => div()
                    .p_3()
                    .child(Label::new("Reading the saved Bitbucket credential…").color(Color::Muted))
                    .into_any_element(),
                _ => self.render_connect(cx),
            };
            return v_flex()
                .key_context("PrInboxPanel")
                .track_focus(&self.focus_handle)
                .size_full()
                .bg(cx.theme().colors().panel_background)
                .child(body)
                .child(div().flex_1())
                .child(self.render_background_controls(cx))
                .into_any_element();
        }
        let watch = self.watch.read(cx);
        let status = if watch.is_checking() {
            "Checking…".to_string()
        } else {
            match watch.last_checked() {
                Some(at) => format!("Checked {}", describe_age(at)),
                None => "Not checked yet".to_string(),
            }
        };
        let error = watch.error().map(str::to_string);
        let changed = watch.changed_count();
        let total = watch.prs().len();
        let rows: Vec<AnyElement> = self
            .visible(cx)
            .iter()
            .enumerate()
            .map(|(ix, pr)| self.render_row(pr, ix, cx))
            .collect();
        let empty = rows.is_empty();
        let colors = cx.theme().colors();
        let filter_button = |id: &'static str, label: &'static str, selected: bool| {
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .toggle_state(selected)
                .style(ButtonStyle::Subtle)
        };

        v_flex()
            .key_context("PrInboxPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background)
            .child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(format!("{changed} changed · {total} open")).size(LabelSize::Small))
                            .child(div().flex_1())
                            .child(Label::new(status).size(LabelSize::XSmall).color(Color::Muted))
                            .child(
                                Button::new("check-now", "Check now")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.watch.update(cx, |watch, cx| watch.check_now(cx))
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .flex_wrap()
                            .child(filter_button("filter-all", "All", !self.changed_only).on_click(cx.listener(|this, _, _, cx| {
                                this.changed_only = false;
                                cx.notify();
                            })))
                            .child(filter_button("filter-changed", "Changed", self.changed_only).on_click(cx.listener(|this, _, _, cx| {
                                this.changed_only = true;
                                cx.notify();
                            })))
                            .child(div().w_2())
                            .child(filter_button("role-both", "Both", self.role == RoleFilter::Both).on_click(cx.listener(|this, _, _, cx| {
                                this.role = RoleFilter::Both;
                                cx.notify();
                            })))
                            .child(filter_button("role-reviewer", "Reviewing", self.role == RoleFilter::Reviewer).on_click(cx.listener(|this, _, _, cx| {
                                this.role = RoleFilter::Reviewer;
                                cx.notify();
                            })))
                            .child(filter_button("role-author", "Mine", self.role == RoleFilter::Author).on_click(cx.listener(|this, _, _, cx| {
                                this.role = RoleFilter::Author;
                                cx.notify();
                            }))),
                    )
                    .child(div().px_1().py_0p5().border_1().border_color(colors.border).rounded_sm().child(self.search.clone()))
                    .when_some(error, |col, error| col.child(Label::new(error).size(LabelSize::Small).color(Color::Error))),
            )
            .child(
                v_flex()
                    .id("pr-rows")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(rows)
                    .when(empty, |col| {
                        col.child(
                            div()
                                .p_3()
                                .child(Label::new(if total == 0 { "No open pull requests found yet." } else { "Nothing matches the filter." }).color(Color::Muted)),
                        )
                    }),
            )
            .child(self.render_background_controls(cx))
            .into_any_element()
    }
}

impl PrInboxPanel {
    fn connect(&mut self, cx: &mut gpui::Context<Self>) {
        let email = self.email.read(cx).text(cx);
        let token = self.token.read(cx).text(cx);
        if token.trim().is_empty() {
            self.connect_error = Some("Enter an API token".into());
            cx.notify();
            return;
        }
        self.connecting = true;
        self.connect_error = None;
        let connect = self
            .store
            .update(cx, |store, cx| store.connect(email, token.trim().to_string(), cx));
        cx.spawn(async move |this, cx| {
            let result = connect.await;
            this.update(cx, |this, cx| {
                this.connecting = false;
                this.connect_error = result.err().map(|e| format!("{e:#}"));
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Shown until a credential is saved: where to get a token and the form
    /// that checks and saves it.
    fn render_connect(&self, cx: &mut gpui::Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let field = |editor: Entity<Editor>| {
            div()
                .px_1()
                .py_0p5()
                .border_1()
                .border_color(colors.border)
                .rounded_sm()
                .child(editor)
        };
        v_flex()
            .p_3()
            .gap_2()
            .child(Headline::new("Connect Bitbucket").size(HeadlineSize::Small))
            .child(
                Label::new(
                    "Create an API token at id.atlassian.com → Security → API tokens → Create API token with scopes, app Bitbucket. Scopes: read:user, read:workspace, read:repository, read:pullrequest, read:pipeline, write:pullrequest, write:pipeline.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(field(self.email.clone()))
            .child(field(self.token.clone()))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("connect-bitbucket", if self.connecting { "Checking…" } else { "Test and save" })
                            .style(ButtonStyle::Filled)
                            .disabled(self.connecting)
                            .on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
                    )
                    .child(
                        Label::new("Saved in Zed's credentials store")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .when_some(
                self.connect_error
                    .clone()
                    .or_else(|| self.store.read(cx).connection_error().map(str::to_string)),
                |col, error| col.child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
            )
            .into_any_element()
    }

    /// Keeping Zed running when its windows close, and starting it at login,
    /// so notifications arrive without a window open.
    fn render_background_controls(&self, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let keep_running = workspace::WorkspaceSettings::get_global(cx)
            .on_last_window_closed
            .is_keep_running();
        let starts_at_login = autostart_file().exists();
        let account = self
            .store
            .read(cx)
            .me()
            .map(|me| me.display_name.clone())
            .filter(|_| self.store.read(cx).is_connected());
        let colors = cx.theme().colors();
        h_flex()
            .p_2()
            .gap_1()
            .flex_wrap()
            .border_t_1()
            .border_color(colors.border)
            .when_some(account, |row, name| {
                row.child(Label::new(format!("Bitbucket: {name}")).size(LabelSize::Small).color(Color::Muted))
                    .child(
                        Button::new("disconnect-bitbucket", "Disconnect")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text("Forget the saved Bitbucket credential"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store.update(cx, |store, cx| store.disconnect(cx)).detach_and_log_err(cx);
                            })),
                    )
                    .child(div().w_2())
            })
            .child(
                Button::new("keep-running", "Keep running when closed")
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Subtle)
                    .toggle_state(keep_running)
                    .tooltip(Tooltip::text(
                        "Closing the last window leaves Zed in the tray, still watching pull requests",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let Some(fs) = this
                            .workspace
                            .upgrade()
                            .map(|w| w.read(cx).app_state().fs.clone())
                        else {
                            return;
                        };
                        settings::update_settings_file(fs, cx, move |settings, _| {
                            settings.workspace.on_last_window_closed = Some(if keep_running {
                                settings::OnLastWindowClosed::PlatformDefault
                            } else {
                                settings::OnLastWindowClosed::KeepRunning
                            });
                        });
                    })),
            )
            .child(
                Button::new("start-at-login", "Start at login")
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Subtle)
                    .toggle_state(starts_at_login)
                    .disabled(!keep_running && !starts_at_login)
                    .tooltip(Tooltip::text(if keep_running {
                        "Start Zed without a window when you log in"
                    } else {
                        "Needs Keep running when closed"
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Err(e) = set_start_at_login(!starts_at_login) {
                            this.workspace
                                .update(cx, |workspace, cx| workspace.show_error(e, cx))
                                .ok();
                        }
                        cx.notify();
                    })),
            )
    }
}

impl Panel for PrInboxPanel {
    fn persistent_name() -> &'static str {
        "PrInboxPanel"
    }

    fn panel_key() -> &'static str {
        "PrInboxPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut gpui::Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(360.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::PullRequest)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Pull requests")
    }

    fn icon_label(&self, _: &Window, cx: &App) -> Option<String> {
        let changed = self.watch.read(cx).changed_count();
        (changed > 0).then(|| changed.to_string())
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_tags_round_trip() {
        let key = PrKey {
            repo: bitbucket::RepoRef::new("cluf200", "cluf200_mcu3"),
            id: 214,
        };
        assert_eq!(tag_for(&key), "pr:cluf200/cluf200_mcu3#214");
        assert_eq!(key_from_tag(&tag_for(&key)), Some(key));
        assert_eq!(key_from_tag("other:thing"), None);
    }
}
