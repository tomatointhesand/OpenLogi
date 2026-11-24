//! The Lighting tab's "keep it on the mouse" row, shown while the mouse's
//! stored profile holds one fixed LED colour: what the mouse shows by itself
//! (before OpenLogi starts, or with it closed), and a button that keeps the
//! colour and brightness chosen above in that profile.
//!
//! Nothing here writes the mouse by itself. Only the button, after its
//! confirmation, sends the chosen setting to the agent.

use gpui::{AnyElement, App, IntoElement, ParentElement, Styled, Window, div, px};
use gpui_kit::component::{Disableable as _, WindowExt as _, button::Button, v_flex};
use openlogi_core::config::Lighting;

use crate::state::{AppState, LedKeep, LedLoad, LedPanel, ProfileSave, short_backup_path};
use crate::ui::components::save_cancel_footer;
use crate::ui::theme::{self, Palette, Typography as _};

const BUTTON_H: f32 = 28.;

fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The row for the selected mouse, or nothing when its profile does not hold
/// one fixed colour (or has not been read yet).
pub(super) fn stored_section(lighting: &Lighting, pal: Palette, cx: &App) -> Option<AnyElement> {
    let state = AppState::try_read(cx)?;
    match state.led_keep() {
        LedKeep::Unsupported => return None,
        LedKeep::NeedsProfile => return Some(needs_profile_section(pal)),
        LedKeep::Profile => {}
    }
    let panel = state.current_led_panel()?.clone();
    let LedLoad::Ready(held) = panel.load() else {
        return None;
    };
    let (r, g, b) = lighting.color.components();
    let chosen = [r, g, b];
    let brightness = lighting.brightness;
    let number = panel.profile().to_string();
    let saving = matches!(panel.save(), ProfileSave::Saving);
    Some(
        v_flex()
            .gap_2()
            .w_full()
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .text_caption()
                    .text_color(pal.text_muted)
                    .child(tr!("lighting.profile_led_stored", color => hex(held.color))),
            )
            .child(
                Button::new("profile-led-save")
                    .compact()
                    .h(px(BUTTON_H))
                    .label(tr!("pointer.profile_dpi_save"))
                    .disabled(!panel.can_save(chosen, brightness) || saving)
                    .on_click(move |_, window, cx| {
                        open_confirm_dialog(window, cx, &number, chosen, brightness);
                    }),
            )
            .child(save_status(&panel, chosen, brightness, pal))
            .into_any_element(),
    )
}

/// The row while OpenLogi controls the mouse: the button is there but off, and
/// the note says what to do.
fn needs_profile_section(pal: Palette) -> AnyElement {
    v_flex()
        .gap_2()
        .w_full()
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_caption()
                .text_color(pal.text_muted)
                .child(tr!("lighting.profile_led_needs_profile")),
        )
        .child(
            Button::new("profile-led-save")
                .compact()
                .h(px(BUTTON_H))
                .label(tr!("pointer.profile_dpi_save"))
                .disabled(true),
        )
        .into_any_element()
}

/// What the last save, or an unsaved choice, has to say.
fn save_status(panel: &LedPanel, chosen: [u8; 3], brightness: u8, pal: Palette) -> AnyElement {
    let text = match panel.save() {
        ProfileSave::Saved { backup } if !backup.is_empty() => {
            Some(tr!("pointer.profile_dpi_saved", path => short_backup_path(backup)).to_string())
        }
        ProfileSave::Failed(message) => {
            Some(tr!("pointer.profile_dpi_failed", message => message.clone()).to_string())
        }
        ProfileSave::Idle if panel.can_save(chosen, brightness) => {
            Some(tr!("lighting.profile_led_unsaved").to_string())
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

/// Confirm before the one action that rewrites the mouse's flash.
fn open_confirm_dialog(
    window: &mut Window,
    cx: &mut App,
    number: &str,
    color: [u8; 3],
    brightness: u8,
) {
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
                AppState::apply(cx, |state| state.save_profile_led(color, brightness));
                true
            })
    });
}
