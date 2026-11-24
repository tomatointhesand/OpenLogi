//! The report-rate row while the mouse's stored profile owns it: the same
//! pills the live row shows, but picking one only sets a draft — a
//! "Save to mouse" button, after its confirmation, sends it to the agent.
//!
//! Nothing here writes the mouse by itself. Only the button, after its
//! confirmation, sends the picked rate to the agent.

use gpui::{AnyElement, App, IntoElement, ParentElement, Styled, Window, div, px};
use gpui_kit::component::{
    Disableable as _, Selectable as _, WindowExt as _, button::Button, h_flex, v_flex,
};
use openlogi_core::config::OnboardMemory;
use openlogi_core::hid::{ReportRate, ReportRateInfo};

use crate::state::{AppState, DeviceRecord, ProfileSave, ReportRateLoad, ReportRateProfilePanel};
use crate::ui::components::{Pill, save_cancel_footer};
use crate::ui::status::{retry_line, status_line};
use crate::ui::theme::{self, Palette, Typography as _};

const CHIP_H: f32 = 28.;

/// The section for a mouse whose profile owns the report rate, or `None` when
/// the mouse's own read is not reachable yet (the main card above already
/// says it is offline).
pub(super) fn profile_section(pal: Palette, cx: &App) -> AnyElement {
    let Some(state) = AppState::try_read(cx) else {
        return div().into_any_element();
    };
    let Some(key) = state.current_record().map(DeviceRecord::device_key) else {
        return div().into_any_element();
    };
    let reachable = state
        .current_record()
        .is_some_and(|record| record.route.is_some());
    let number = match state.current_onboard_active() {
        Some(OnboardMemory::Profile(index)) => index.to_string(),
        _ => String::new(),
    };
    let panel = state.current_report_rate_profile_panel().cloned();
    let body = match state.current_report_rate_load() {
        ReportRateLoad::Ready(info) => editor(&info, panel.as_ref(), number, pal),
        ReportRateLoad::Unknown | ReportRateLoad::Loading if !reachable => {
            return div().into_any_element();
        }
        ReportRateLoad::Unknown | ReportRateLoad::Loading => {
            status_line(tr!("pointer.reading_report_rate"), pal).into_any_element()
        }
        ReportRateLoad::Failed(_) => retry_line(
            "report-rate-profile-retry",
            tr!("pointer.couldnt_read_report_rate_click_to_retry"),
            move |cx| AppState::apply(cx, |state| state.retry_report_rate_read(&key)),
        )
        .into_any_element(),
        ReportRateLoad::Unsupported(_) => return div().into_any_element(),
    };
    v_flex()
        .gap_2()
        .child(
            div()
                .text_body()
                .text_color(pal.text_primary)
                .child(tr!("pointer.report_rate")),
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
    info: &ReportRateInfo,
    panel: Option<&ReportRateProfilePanel>,
    number: String,
    pal: Palette,
) -> AnyElement {
    let draft = panel.and_then(ReportRateProfilePanel::draft);
    let selected_rate = draft.unwrap_or(info.current);
    let saving = panel.is_some_and(|panel| matches!(panel.save(), ProfileSave::Saving));
    let can_save = panel.is_some_and(|panel| panel.can_save(info.current));
    let pills: Vec<_> = info
        .supported
        .iter()
        .map(|&rate| rate_pill(rate, rate == selected_rate))
        .collect();
    v_flex()
        .gap_2()
        .child(h_flex().gap_2().flex_wrap().children(pills))
        .child(
            Button::new("profile-report-rate-save")
                .compact()
                .h(px(CHIP_H))
                .label(tr!("pointer.profile_dpi_save"))
                .disabled(!can_save || saving)
                .on_click(move |_, window, cx| open_confirm_dialog(window, cx, &number)),
        )
        .child(save_status(panel, can_save, pal))
        .into_any_element()
}

/// One rate pill. Clicking it only picks the draft; it does not write the
/// mouse.
fn rate_pill(rate: ReportRate, selected: bool) -> impl IntoElement {
    Pill::new(
        ("profile-report-rate", usize::from(rate.into_inner())),
        tr!("pointer.report_rate_hz", rate => rate),
    )
    .selected(selected)
    .on_click(move |_event, _window, cx| {
        AppState::apply(cx, |state| state.pick_profile_report_rate(rate));
    })
}

/// What the last save, or an unsaved pick, has to say.
fn save_status(panel: Option<&ReportRateProfilePanel>, can_save: bool, pal: Palette) -> AnyElement {
    let text = match panel.map(ReportRateProfilePanel::save) {
        Some(ProfileSave::Saved { backup }) if !backup.is_empty() => Some(
            tr!("pointer.profile_dpi_saved", path => crate::state::short_backup_path(backup))
                .to_string(),
        ),
        Some(ProfileSave::Failed(message)) => {
            Some(tr!("pointer.profile_dpi_failed", message => message.clone()).to_string())
        }
        Some(ProfileSave::Idle) if can_save => Some(tr!("pointer.profile_dpi_unsaved").to_string()),
        _ => None,
    };
    div()
        .w_full()
        .min_w_0()
        .text_caption()
        .text_color(pal.text_muted)
        .children(text)
        .into_any_element()
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
            .on_ok(move |_, _, cx| {
                AppState::apply(cx, AppState::save_profile_report_rate);
                true
            })
    });
}
