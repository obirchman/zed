//! The system tray icon, shown while `on_last_window_closed` is
//! `keep_running`: it brings a window back when none is open, and shows how
//! many pull requests changed since they were handled.

use crate::restore_or_create_workspace;
use bitbucket::PrWatch;
use git_ui::pr_inbox::PrInboxPanel;
use gpui::{App, AsyncApp, TaskExt as _};
use settings::{Settings as _, SettingsStore};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use workspace::{AppState, MultiWorkspace, WorkspaceSettings};

enum Command {
    Open,
    PullRequests,
    Quit,
}

struct ZedTray {
    id: String,
    changed: usize,
    icon: Vec<ksni::Icon>,
    commands: smol::channel::Sender<Command>,
}

impl ZedTray {
    fn send(&self, command: Command) {
        self.commands.try_send(command).ok();
    }
}

impl ksni::Tray for ZedTray {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn title(&self) -> String {
        "Zed".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }

    fn status(&self) -> ksni::Status {
        if self.changed > 0 {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Zed".into(),
            description: format!("{} pull requests changed since handled", self.changed),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::Open);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Open Zed".into(),
                activate: Box::new(|tray: &mut Self| tray.send(Command::Open)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("Pull requests ({} changed)", self.changed),
                activate: Box::new(|tray: &mut Self| tray.send(Command::PullRequests)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit Zed".into(),
                activate: Box::new(|tray: &mut Self| tray.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        log::warn!(
            "tray: no StatusNotifier host ({reason:?}); on GNOME enable the AppIndicator extension to see the icon"
        );
        true
    }
}

/// The app icon as 64×64 ARGB32, the format StatusNotifierItem expects.
fn app_icon() -> Vec<ksni::Icon> {
    let Ok(image) = image::load_from_memory(include_bytes!("../resources/app-icon-dev.png")) else {
        return Vec::new();
    };
    let image = image::imageops::resize(&image.to_rgba8(), 64, 64, image::imageops::FilterType::Triangle);
    let data = image
        .pixels()
        .flat_map(|p| {
            let [r, g, b, a] = p.0;
            [a, r, g, b]
        })
        .collect();
    vec![ksni::Icon {
        width: 64,
        height: 64,
        data,
    }]
}

type TrayHandle = ksni::blocking::Handle<ZedTray>;

/// Show the tray icon while `on_last_window_closed` is `keep_running`, and
/// let pull request notifications open a window when none is open.
pub fn init(app_state: Arc<AppState>, cx: &mut App) {
    let (commands, received) = smol::channel::unbounded();
    let handle: Rc<RefCell<Option<TrayHandle>>> = Rc::default();

    let sync = {
        let handle = handle.clone();
        move |cx: &mut App| {
            let wanted = WorkspaceSettings::get_global(cx)
                .on_last_window_closed
                .is_keep_running();
            let mut handle = handle.borrow_mut();
            match (wanted, handle.is_some()) {
                (true, false) => {
                    use ksni::blocking::TrayMethods as _;
                    let tray = ZedTray {
                        id: release_channel::RELEASE_CHANNEL.app_id().to_string(),
                        changed: PrWatch::try_global(cx).map_or(0, |w| w.read(cx).changed_count()),
                        icon: app_icon(),
                        commands: commands.clone(),
                    };
                    match tray.spawn() {
                        Ok(spawned) => *handle = Some(spawned),
                        Err(e) => log::error!("tray: could not start the tray icon: {e}"),
                    }
                }
                (false, true) => {
                    if let Some(old) = handle.take() {
                        old.shutdown();
                    }
                }
                _ => {}
            }
        }
    };
    sync(cx);
    cx.observe_global::<SettingsStore>(sync).detach();

    if let Some(watch) = PrWatch::try_global(cx) {
        let handle = handle.clone();
        cx.observe(&watch, move |watch, cx| {
            let changed = watch.read(cx).changed_count();
            if let Some(handle) = handle.borrow().as_ref() {
                handle.update(|tray| tray.changed = changed);
            }
        })
        .detach();
    }

    git_ui::pr_inbox::set_open_without_window({
        let app_state = app_state.clone();
        move |key, cx| {
            let app_state = app_state.clone();
            cx.spawn(async move |cx| {
                restore_or_create_workspace(app_state, cx).await?;
                cx.update(|cx| git_ui::pr_inbox::open_review_in_existing_window(key, cx));
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }
    });

    cx.spawn(async move |cx| {
        while let Ok(command) = received.recv().await {
            if let Err(e) = run(command, app_state.clone(), cx).await {
                log::error!("tray: {e:#}");
            }
        }
    })
    .detach();
}

async fn run(command: Command, app_state: Arc<AppState>, cx: &mut AsyncApp) -> anyhow::Result<()> {
    match command {
        Command::Quit => cx.update(|cx| cx.quit()),
        Command::Open => {
            if workspace::activate_any_workspace_window(cx).is_none() {
                restore_or_create_workspace(app_state, cx).await?;
            }
        }
        Command::PullRequests => {
            let window = match workspace::activate_any_workspace_window(cx) {
                Some(window) => window,
                None => {
                    restore_or_create_workspace(app_state, cx).await?;
                    workspace::activate_any_workspace_window(cx)
                        .ok_or_else(|| anyhow::anyhow!("no window opened"))?
                }
            };
            window.update(cx, |multi_workspace: &mut MultiWorkspace, window, cx| {
                multi_workspace.workspace().clone().update(cx, |workspace, cx| {
                    workspace.focus_panel::<PrInboxPanel>(window, cx);
                });
            })?;
        }
    }
    Ok(())
}
