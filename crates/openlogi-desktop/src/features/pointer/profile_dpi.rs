//! The DPI row while the mouse's stored profile owns its DPI: the profile's
//! stages as chips to edit, and a button that saves them to the mouse.
//!
//! Nothing here writes the mouse by itself. Editing changes a draft held by
//! [`AppState`](crate::state::AppState); only the "Save to mouse" button, after
//! its confirmation, sends the draft to the agent.

use gpui::{
    AnyElement, App, AppContext as _, IntoElement, ParentElement, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::component::{
    Disableable as _, IconName, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::InputState,
    v_flex,
};
use openlogi_core::hid::{Dpi, DpiCapabilities};

use super::dpi::parse_dpi_entry;
use crate::state::{
    AppState, ProfileLoad, ProfilePanel, ProfileSave, active_stage, short_backup_path,
};
use crate::ui::components::{PresetChip, control_input, save_cancel_footer};
use crate::ui::status::{retry_line, status_line};
use crate::ui::theme::{self, Palette, Typography as _};

const CHIP_H: f32 = 28.;

/// The section for a mouse whose profile owns the DPI.
pub(super) fn profile_section(pal: Palette, cx: &App) -> AnyElement {
    let panel = AppState::try_read(cx).and_then(|state| state.current_profile_panel().cloned());
    let capabilities =
        AppState::try_read(cx).and_then(|state| state.active_dpi_capabilities().cloned());
    let live = AppState::try_read(cx).and_then(AppState::live_dpi);
    let body = match &panel {
        None => status_line(tr!("pointer.reading_supported_dpi_values"), pal).into_any_element(),
        Some(panel) => match panel.load() {
            ProfileLoad::Idle | ProfileLoad::Loading => {
                status_line(tr!("pointer.reading_supported_dpi_values"), pal).into_any_element()
            }
            ProfileLoad::Failed(_) => retry_line(
                "profile-dpi-retry",
                tr!("pointer.couldnt_read_dpi_click_to_retry"),
                |cx| AppState::apply(cx, AppState::retry_profile_dpi),
            )
            .into_any_element(),
            ProfileLoad::Ready(_) => editor(panel, capabilities, live, pal),
        },
    };
    v_flex()
        .gap_2()
        .w_full()
        .child(
            div()
                .text_body()
                .text_color(pal.text_primary)
                .child(tr!("pointer.dpi")),
        )
        .child(
            div()
                .text_caption()
                .text_color(pal.text_muted)
                .child(tr!("pointer.report_rate_onboard")),
        )
        .child(body)
        .into_any_element()
}

fn editor(
    panel: &ProfilePanel,
    capabilities: Option<DpiCapabilities>,
    live: Option<Dpi>,
    pal: Palette,
) -> AnyElement {
    let Some(draft) = panel.draft() else {
        return div().into_any_element();
    };
    let saving = matches!(panel.save(), ProfileSave::Saving);
    let default = usize::from(draft.default_stage());
    let active = live.and_then(|current| active_stage(draft.stages(), current));
    let chips: Vec<_> = draft
        .stages()
        .iter()
        .enumerate()
        .map(|(index, &stage)| {
            stage_chip(
                index,
                stage,
                StageFlags {
                    active: active == Some(index),
                    default: index == default,
                    can_remove: draft.can_remove(),
                },
                capabilities.clone(),
            )
        })
        .collect();
    let add_caps = capabilities;
    let last = draft.stages().last().copied();
    let number = panel.profile().to_string();

    v_flex()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .children(chips)
                .when(draft.can_add(), |row| {
                    row.child(
                        Button::new("profile-dpi-add")
                            .compact()
                            .outline()
                            .h(px(CHIP_H))
                            .icon(IconName::Plus)
                            .label(tr!("common.add"))
                            .on_click(move |_, window, cx| {
                                if let (Some(caps), Some(last)) = (add_caps.clone(), last) {
                                    open_stage_dialog(window, cx, None, last, caps);
                                }
                            }),
                    )
                }),
        )
        .child(
            Button::new("profile-dpi-save")
                .compact()
                .h(px(CHIP_H))
                .label(tr!("pointer.profile_dpi_save"))
                .disabled(!panel.can_save() || saving)
                .on_click(move |_, window, cx| open_confirm_dialog(window, cx, &number)),
        )
        .child(save_status(panel, pal))
        .into_any_element()
}

/// What the last save, or an unsaved edit, has to say.
fn save_status(panel: &ProfilePanel, pal: Palette) -> AnyElement {
    let text = match panel.save() {
        ProfileSave::Saved { backup } if !backup.is_empty() => {
            Some(tr!("pointer.profile_dpi_saved", path => short_backup_path(backup)).to_string())
        }
        ProfileSave::Failed(message) => {
            Some(tr!("pointer.profile_dpi_failed", message => message.clone()).to_string())
        }
        ProfileSave::Idle if panel.can_save() => {
            Some(tr!("pointer.profile_dpi_unsaved").to_string())
        }
        ProfileSave::Idle | ProfileSave::Saving | ProfileSave::Saved { .. } => None,
    };
    div()
        .w_full()
        .min_w_0()
        .text_caption()
        .text_color(pal.text_muted)
        .children(text)
        .into_any_element()
}

/// What a stage chip shows besides its value.
#[derive(Debug, Clone, Copy)]
struct StageFlags {
    /// The mouse is using this stage now (its DPI button selected it).
    active: bool,
    /// The mouse starts on this stage.
    default: bool,
    /// The stage can be taken out of the profile.
    can_remove: bool,
}

/// One stage: its value (click to type another), a button to start on it, and
/// a button to remove it. The stage in use is drawn selected; the one the
/// mouse starts on says so.
fn stage_chip(
    index: usize,
    stage: Dpi,
    flags: StageFlags,
    capabilities: Option<DpiCapabilities>,
) -> impl IntoElement {
    let StageFlags {
        active,
        default: is_default,
        can_remove,
    } = flags;
    let remove_label = tr!("common.remove_named_item", name => format!("{stage} DPI"));
    let label = if is_default {
        format!("{stage} · {}", tr!("common.default"))
    } else {
        format!("{stage}")
    };
    PresetChip::new(("profile-dpi-chip", index))
        .selected(active)
        .child(
            Button::new(("profile-dpi-stage", index))
                .compact()
                .ghost()
                .h_full()
                .flex()
                .items_center()
                .label(label)
                .selected(active)
                .on_click(move |_, window, cx| {
                    if let Some(caps) = capabilities.clone() {
                        open_stage_dialog(window, cx, Some(index), stage, caps);
                    }
                }),
        )
        .when(!is_default, |chip| {
            chip.child(
                Button::new(("profile-dpi-default", index))
                    .small()
                    .ghost()
                    .label(tr!("common.default"))
                    .on_click(move |_, _, cx| {
                        AppState::apply(cx, |state| {
                            state.edit_profile_dpi(|draft| {
                                draft.make_default(index);
                            })
                        });
                    }),
            )
        })
        .when(can_remove, |chip| {
            chip.child(
                Button::new(("profile-dpi-remove", index))
                    .small()
                    .ghost()
                    .icon(IconName::Close)
                    .accessibility_label(remove_label.clone())
                    .tooltip(remove_label)
                    .on_click(move |_, _, cx| {
                        AppState::apply(cx, |state| {
                            state.edit_profile_dpi(|draft| {
                                draft.remove(index);
                            })
                        });
                    }),
            )
        })
}

/// Ask for a stage's DPI: `index` is the stage to change, `None` for a new one.
fn open_stage_dialog(
    window: &mut Window,
    cx: &mut App,
    index: Option<usize>,
    initial: Dpi,
    capabilities: DpiCapabilities,
) {
    let input = cx.new(|cx| {
        let mut input = InputState::new(window, cx);
        input.set_value(initial.to_string(), window, cx);
        input
    });
    let range = (capabilities.min(), capabilities.max());
    window.open_dialog(cx, move |dialog, window, cx| {
        input.update(cx, |input, cx| input.focus(window, cx));
        dialog
            .w(px(320.))
            .title(tr!("camera.set_value_title", label => tr!("pointer.dpi")))
            .child(
                v_flex().gap_2().child(control_input(&input)).child(
                    div()
                        .text_caption()
                        .text_color(theme::palette(cx).text_muted)
                        .child(tr!(
                            "camera.supported_range",
                            min => range.0.to_string(),
                            max => range.1.to_string()
                        )),
                ),
            )
            .footer(save_cancel_footer())
            .on_ok({
                let input = input.clone();
                let capabilities = capabilities.clone();
                move |_, _, cx| {
                    let Some(dpi) = parse_dpi_entry(&input.read(cx).value(), &capabilities) else {
                        return false;
                    };
                    AppState::apply(cx, |state| {
                        state.edit_profile_dpi(|draft| {
                            match index {
                                Some(index) => draft.set(index, dpi),
                                None => draft.add(dpi),
                            };
                        })
                    });
                    true
                }
            })
    });
}

/// Confirm before the one action that rewrites the mouse's flash.
fn open_confirm_dialog(window: &mut Window, cx: &mut App, number: &str) {
    let number = number.to_string();
    window.open_dialog(cx, move |dialog, _, cx| {
        dialog
            .w(px(380.))
            .title(tr!("pointer.profile_dpi_confirm_title"))
            .child(
                div()
                    .text_body()
                    .text_color(theme::palette(cx).text_primary)
                    .child(tr!("pointer.profile_dpi_confirm_body", number => number.clone())),
            )
            .footer(save_cancel_footer())
            .on_ok(|_, _, cx| {
                AppState::apply(cx, AppState::save_profile_dpi);
                true
            })
    });
}
