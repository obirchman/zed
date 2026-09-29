//! Settings > Bitbucket > API Token: the token the Bitbucket client uses,
//! kept in the system keychain rather than in settings.json.

use bitbucket::{BitbucketStore, Connection};
use gpui::{ScrollHandle, TaskExt as _, prelude::*};
use ui::{ButtonLink, ConfiguredApiCard, prelude::*};

use crate::SettingsWindow;
use crate::components::{SettingsInputField, SettingsSectionHeader};

const TOKEN_PAGE_URL: &str = "https://id.atlassian.com/manage-profile/security/api-tokens";

pub(crate) fn render_bitbucket_token_page(
    _settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(store) = BitbucketStore::try_global(cx) else {
        return div().into_any_element();
    };
    let weak_page = cx.weak_entity();
    let observed = store.clone();
    _ = window.use_keyed_state("bitbucket-store-observer", cx, move |_, cx| {
        cx.observe(&observed, move |_, _, cx| {
            weak_page.update(cx, |_, cx| cx.notify()).ok();
        })
    });

    let (has_token, connection, account, error) = store.read_with(cx, |store, _| {
        (
            store.has_token(),
            store.connection().clone(),
            store.me().map(|me| me.display_name.clone()),
            store.connection_error().map(str::to_string),
        )
    });
    let status = match (&connection, &account, &error) {
        (_, _, Some(error)) => Some((error.clone(), Color::Error)),
        (Connection::Connected, Some(name), None) => Some((format!("Connected as {name}"), Color::Success)),
        (Connection::Connected, None, None) => Some(("Checking the token…".to_string(), Color::Muted)),
        (Connection::Loading, _, _) => Some(("Reading the saved token…".to_string(), Color::Muted)),
        (Connection::Disconnected, _, _) => None,
    };

    let token_field = if has_token {
        ConfiguredApiCard::new("bitbucket-reset-token", "API Token Configured")
            .button_label("Reset Token")
            .button_tab_index(0)
            .on_click({
                move |_, _, cx| {
                    store.update(cx, |store, cx| store.set_token(None, cx)).detach_and_log_err(cx);
                }
            })
            .into_any_element()
    } else {
        h_flex()
            .pt_2p5()
            .w_full()
            .min_w_0()
            .justify_between()
            .child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .max_w_1_2()
                    .gap_0p5()
                    .child(Label::new("API Token"))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_0p5()
                            .child(Label::new("Create one on the").size(LabelSize::Small).color(Color::Muted))
                            .child(
                                ButtonLink::new("Atlassian API tokens page", TOKEN_PAGE_URL)
                                    .no_icon(true)
                                    .label_size(LabelSize::Small)
                                    .label_color(Color::Muted),
                            )
                            .child(
                                Label::new(
                                    "with Create API token with scopes, app Bitbucket, and the scopes read:user, read:workspace, read:repository, read:pullrequest, read:pipeline, write:pullrequest, write:pipeline. Set your account email on the Bitbucket page.",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    ),
            )
            .child(
                SettingsInputField::new("bitbucket-api-token-input")
                    .tab_index(0)
                    .with_placeholder("ATATT…")
                    .aria_label("Bitbucket API Token")
                    .clear_on_confirm()
                    .on_confirm({
                        move |token, _window, cx| {
                            let Some(token) = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) else {
                                return;
                            };
                            store.update(cx, |store, cx| store.set_token(Some(token), cx)).detach_and_log_err(cx);
                        }
                    }),
            )
            .into_any_element()
    };

    v_flex()
        .id("bitbucket-token-page")
        .size_full()
        .px_8()
        .pb_16()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .child(
            v_flex()
                .min_w_0()
                .pt_8()
                .gap_1p5()
                .child(SettingsSectionHeader::new("Bitbucket").icon(IconName::Bitbucket).no_padding(true))
                .child(token_field)
                .when_some(status, |col, (text, color)| {
                    col.child(Label::new(text).size(LabelSize::Small).color(color))
                }),
        )
        .into_any_element()
}
