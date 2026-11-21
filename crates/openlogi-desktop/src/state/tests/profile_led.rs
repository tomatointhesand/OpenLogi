//! A mouse's stored LED setting: read once a profile owns the mouse, and saved
//! to the mouse only on request.

use super::fn_lock::drain;
use super::*;
use crate::services::ipc::{Command, ReadProfileLed, WriteProfileLed};
use crate::state::events::StateEvent;
use crate::state::profile_dpi::ProfileSave;
use crate::state::profile_led::LedLoad;
use openlogi_core::config::OnboardMemory;
use openlogi_core::hid::{ProfileDpiError, ProfileIndex, ProfileLed, ProfileLedSaved};

fn index() -> ProfileIndex {
    ProfileIndex::new(1).unwrap()
}

fn held(color: [u8; 3], brightness: u8) -> ProfileLed {
    ProfileLed {
        profile: index(),
        color,
        brightness,
    }
}

fn mouse_on_profile() -> (AppState, tokio::sync::mpsc::UnboundedReceiver<Command>) {
    let resolver = AssetResolver::new();
    let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut inventory = direct_inventory([0x24, 0x23, 0x6c, 0x7a]);
    inventory.paired[0]
        .capabilities
        .as_mut()
        .expect("the fixture device has capabilities")
        .onboard_profiles = true;
    let mut state = AppState::new(Sources {
        inventories: &[inventory],
        ..Sources::in_memory(Config::ephemeral(), &resolver, commands)
    });
    let _ = state.commit_onboard_memory(OnboardMemory::Profile(index()));
    (state, receiver)
}

fn reads(commands: Vec<Command>) -> Vec<ReadProfileLed> {
    commands
        .into_iter()
        .filter_map(|command| match command {
            Command::ReadProfileLed(read) => Some(read),
            _ => None,
        })
        .collect()
}

fn writes(commands: Vec<Command>) -> Vec<WriteProfileLed> {
    commands
        .into_iter()
        .filter_map(|command| match command {
            Command::WriteProfileLed(write) => Some(write),
            _ => None,
        })
        .collect()
}

fn key(state: &AppState) -> crate::state::DeviceKey {
    state.current_record().expect("a device").device_key()
}

#[test]
fn a_mouse_on_a_profile_is_asked_for_its_led_once() {
    let (mut state, mut receiver) = mouse_on_profile();
    drain(&mut receiver);
    let events = state.request_current_profile_led();
    assert!(events.contains(&StateEvent::ProfileLedChanged(key(&state))));
    assert_eq!(reads(drain(&mut receiver)).len(), 1);
    assert!(state.request_current_profile_led().is_empty());
    assert!(reads(drain(&mut receiver)).is_empty());
}

#[test]
fn a_mouse_that_openlogi_controls_is_not_asked_for_its_led() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.commit_onboard_memory(OnboardMemory::Off);
    drain(&mut receiver);
    assert!(state.request_current_profile_led().is_empty());
    assert!(reads(drain(&mut receiver)).is_empty());
    assert!(state.current_led_panel().is_none());
}

#[test]
fn nothing_is_sent_to_the_mouse_until_a_save_is_asked_for() {
    let (mut state, mut receiver) = mouse_on_profile();
    drain(&mut receiver);
    let _ = state.request_current_profile_led();
    let k = key(&state);
    let _ = state.apply_profile_led_read(&k, index(), Ok(held([1, 2, 3], 0)));
    assert!(writes(drain(&mut receiver)).is_empty());

    let _ = state.save_profile_led([4, 5, 6], 30);
    let sent = writes(drain(&mut receiver));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].color, [4, 5, 6]);
    assert_eq!(sent[0].brightness, 30);
    assert_eq!(sent[0].profile, index());
}

#[test]
fn saving_what_the_mouse_already_holds_sends_nothing() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_led();
    let k = key(&state);
    let _ = state.apply_profile_led_read(&k, index(), Ok(held([1, 2, 3], 40)));
    drain(&mut receiver);
    assert!(state.save_profile_led([1, 2, 3], 40).is_empty());
    assert!(writes(drain(&mut receiver)).is_empty());
}

#[test]
fn the_answer_to_a_save_shows_what_the_mouse_holds() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_led();
    let k = key(&state);
    let _ = state.apply_profile_led_read(&k, index(), Ok(held([1, 2, 3], 0)));
    let _ = state.save_profile_led([4, 5, 6], 30);
    drain(&mut receiver);
    let _ = state.apply_profile_led_saved(
        &k,
        index(),
        Ok(ProfileLedSaved {
            saved: held([4, 5, 6], 30),
            backup: "x/b.txt".into(),
        }),
    );
    let panel = state.current_led_panel().unwrap();
    assert_eq!(panel.load(), &LedLoad::Ready(held([4, 5, 6], 30)));
    assert!(matches!(panel.save(), ProfileSave::Saved { .. }));
}

#[test]
fn a_refused_save_is_reported_and_the_held_setting_is_kept() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_led();
    let k = key(&state);
    let _ = state.apply_profile_led_read(&k, index(), Ok(held([1, 2, 3], 0)));
    let _ = state.save_profile_led([4, 5, 6], 30);
    drain(&mut receiver);
    let _ = state.apply_profile_led_saved(&k, index(), Err(ProfileDpiError::HostMode));
    let panel = state.current_led_panel().unwrap();
    assert!(matches!(panel.save(), ProfileSave::Failed(_)));
    assert_eq!(panel.load(), &LedLoad::Ready(held([1, 2, 3], 0)));
}

#[test]
fn an_answer_for_another_profile_is_dropped() {
    let (mut state, _receiver) = mouse_on_profile();
    let _ = state.request_current_profile_led();
    let k = key(&state);
    let other = ProfileIndex::new(2).unwrap();
    assert!(
        state
            .apply_profile_led_read(&k, other, Ok(held([1, 2, 3], 0)))
            .is_empty()
    );
    assert_eq!(state.current_led_panel().unwrap().load(), &LedLoad::Loading);
}
