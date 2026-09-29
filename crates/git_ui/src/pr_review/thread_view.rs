//! One pull request comment thread as a card: comments, replies, the reply
//! editor, and Reply / Resolve / rating buttons. The same card is shown in
//! editors next to the commented line and in the review tab's conversation.

use super::editor_threads::EditorThreads;
use super::threads::{self, Place, Thread};
use super::PrReview;
use anyhow::Result;
use bitbucket::{
    BitbucketStore, PrKey,
    api::{NewComment, plan},
};
use editor::Editor;
use gpui::{AnyElement, App, AppContext as _, Entity, Focusable as _, Task, WeakEntity};
use ui::{Tooltip, prelude::*};

/// An open reply editor under a thread.
pub(crate) struct Reply {
    pub root_id: u64,
    pub editor: Entity<Editor>,
}

impl Reply {
    pub fn new(root_id: u64, prefill: Option<String>, window: &mut Window, cx: &mut App) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text("Reply…", window, cx);
            if let Some(text) = &prefill {
                editor.set_text(text.as_str(), window, cx);
            }
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        Self { root_id, editor }
    }

    /// Post the reply's text under its thread.
    pub fn send(&self, key: &PrKey, cx: &mut App) -> Task<Result<()>> {
        let body = self.editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return Task::ready(Ok(()));
        }
        let request = plan::comment(
            &key.repo,
            key.id,
            &NewComment {
                body,
                inline: None,
                parent: Some(self.root_id),
            },
        );
        let key = key.clone();
        let send = BitbucketStore::global(cx).update(cx, |store, cx| store.execute(request, Some(key), cx));
        cx.background_spawn(async move { send.await.map(|_| ()) })
    }
}

pub(crate) fn set_resolved(key: &PrKey, root_id: u64, resolved: bool, cx: &mut App) -> Task<Result<()>> {
    let request = plan::resolve_comment(&key.repo, key.id, root_id, resolved);
    let key = key.clone();
    let send = BitbucketStore::global(cx).update(cx, |store, cx| store.execute(request, Some(key), cx));
    cx.background_spawn(async move { send.await.map(|_| ()) })
}

/// Who owns a card's reply editor: the review tab, or the controller that
/// puts cards into one editor.
#[derive(Clone)]
pub(crate) enum ThreadHost {
    Review(WeakEntity<PrReview>),
    Editor(WeakEntity<EditorThreads>),
}

impl ThreadHost {
    fn start_reply(&self, root_id: u64, prefill: Option<String>, window: &mut Window, cx: &mut App) {
        match self {
            ThreadHost::Review(this) => {
                this.update(cx, |this, cx| this.start_reply(root_id, prefill, window, cx)).ok();
            }
            ThreadHost::Editor(this) => {
                this.update(cx, |this, cx| this.start_reply(root_id, prefill, window, cx)).ok();
            }
        };
    }

    fn cancel_reply(&self, cx: &mut App) {
        match self {
            ThreadHost::Review(this) => {
                this.update(cx, |this, cx| this.cancel_reply(cx)).ok();
            }
            ThreadHost::Editor(this) => {
                this.update(cx, |this, cx| this.cancel_reply(cx)).ok();
            }
        };
    }

    fn send_reply(&self, cx: &mut App) {
        match self {
            ThreadHost::Review(this) => {
                this.update(cx, |this, cx| this.send_reply(cx)).ok();
            }
            ThreadHost::Editor(this) => {
                this.update(cx, |this, cx| this.send_reply(cx)).ok();
            }
        };
    }
}

/// Lines a card needs in an editor block: a header line and the wrapped
/// body per comment, the button row, and room for the reply editor.
pub(crate) fn card_height(thread: &Thread, replying: bool) -> u32 {
    const WRAP_COLUMNS: usize = 110;
    let wrapped = |text: &str| {
        text.lines()
            .map(|line| line.chars().count().div_ceil(WRAP_COLUMNS).max(1) as u32)
            .sum::<u32>()
            .max(1)
    };
    let comments: u32 = std::iter::once(&thread.root)
        .chain(&thread.replies)
        .map(|c| 1 + wrapped(&c.content.raw))
        .sum();
    comments + 3 + if replying { 5 } else { 0 }
}

pub(crate) fn render_card(
    key: &PrKey,
    thread: &Thread,
    host: ThreadHost,
    reply_editor: Option<Entity<Editor>>,
    extra_button: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let can_write = BitbucketStore::try_global(cx).is_some_and(|s| s.read(cx).is_connected());
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
                    .child(
                        Label::new(c.user.display_name.clone())
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(Label::new(when).size(LabelSize::XSmall).color(Color::Muted))
                    .when(c.deleted, |row| {
                        row.child(Label::new("deleted").size(LabelSize::XSmall).color(Color::Muted))
                    }),
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
                        let host = host.clone();
                        move |_, window, cx| host.start_reply(root_id, None, window, cx)
                    }),
            )
            .child(
                Button::new(("resolve", root_id), if resolved { "Reopen" } else { "Resolve" })
                    .style(ButtonStyle::Subtle)
                    .disabled(!can_write)
                    .on_click({
                        let key = key.clone();
                        move |_, _, cx| set_resolved(&key, root_id, !resolved, cx).detach_and_log_err(cx)
                    }),
            )
            .when(finding, |row| {
                row.child(Label::new("Rate finding:").size(LabelSize::Small).color(Color::Muted))
                    .children((1..=5u8).map(|score| {
                        let host = host.clone();
                        Button::new(("score", root_id * 10 + score as u64), score.to_string())
                            .style(ButtonStyle::Subtle)
                            .disabled(!can_write)
                            .tooltip(Tooltip::text(
                                "Reply with SCORE=<n> Comment=<why>; add the reason before sending",
                            ))
                            .on_click(move |_, window, cx| {
                                host.start_reply(root_id, Some(format!("SCORE={score} Comment=")), window, cx)
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
            .children(extra_button)
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
                    .when(thread.outdated, |row| {
                        row.child(Label::new("outdated").size(LabelSize::XSmall).color(Color::Warning))
                    })
                    .when(resolved, |row| {
                        row.child(Label::new("resolved").size(LabelSize::XSmall).color(Color::Success))
                    }),
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
                                let host = host.clone();
                                move |_, _, cx| host.send_reply(cx)
                            }))
                            .child(Button::new(("cancel", root_id), "Cancel").on_click({
                                let host = host.clone();
                                move |_, _, cx| host.cancel_reply(cx)
                            })),
                    ),
            )
        })
        .child(buttons)
        .into_any_element()
}
