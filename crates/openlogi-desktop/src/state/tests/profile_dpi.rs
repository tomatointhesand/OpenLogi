//! A mouse's stored DPI stages: read once a profile owns the mouse, edited as a
//! draft, and saved to the mouse only on request.

use super::fn_lock::drain;
use super::*;
use crate::services::ipc::{Command, ReadProfileDpi, WriteProfileDpi};
use crate::state::events::StateEvent;
use crate::state::profile_dpi::{ProfileLoad, ProfileSave};
use openlogi_core::config::OnboardMemory;
use openlogi_core::hid::{
    Dpi, ProfileDpi, ProfileDpiError, ProfileDpiSaved, ProfileIndex, WriteError,
};

fn index() -> ProfileIndex {
    ProfileIndex::new(1).unwrap()
}

fn held(stages: &[u16], default_stage: u8) -> ProfileDpi {
    ProfileDpi {
        profile: index(),
        stages: stages.iter().map(|&v| Dpi::new(v)).collect(),
        default_stage,
    }
}

/// A G-series mouse that is set to run its stored profile 1.
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

fn reads(commands: Vec<Command>) -> Vec<ReadProfileDpi> {
    commands
        .into_iter()
        .filter_map(|command| match command {
            Command::ReadProfileDpi(read) => Some(read),
            _ => None,
        })
        .collect()
}

fn writes(commands: Vec<Command>) -> Vec<WriteProfileDpi> {
    commands
        .into_iter()
        .filter_map(|command| match command {
            Command::WriteProfileDpi(write) => Some(write),
            _ => None,
        })
        .collect()
}

fn key(state: &AppState) -> crate::state::DeviceKey {
    state.current_record().expect("a device").device_key()
}

#[test]
fn a_mouse_on_a_profile_is_asked_for_its_stages_once() {
    let (mut state, mut receiver) = mouse_on_profile();
    drain(&mut receiver);
    let events = state.request_current_profile_dpi();
    assert!(events.contains(&StateEvent::ProfileDpiChanged(key(&state))));
    let asked = reads(drain(&mut receiver));
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].profile, index());
    // asking again while the answer is awaited sends nothing
    assert!(state.request_current_profile_dpi().is_empty());
    assert!(reads(drain(&mut receiver)).is_empty());
}

#[test]
fn a_mouse_that_openlogi_controls_is_not_asked() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.commit_onboard_memory(OnboardMemory::Off);
    drain(&mut receiver);
    assert!(state.request_current_profile_dpi().is_empty());
    assert!(reads(drain(&mut receiver)).is_empty());
    assert!(state.current_profile_panel().is_none());
}

#[test]
fn the_answer_becomes_a_draft_to_edit() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_dpi();
    drain(&mut receiver);
    let k = key(&state);
    let events = state.apply_profile_dpi_read(&k, index(), Ok(held(&[800, 3200], 0)));
    assert!(events.contains(&StateEvent::ProfileDpiChanged(k)));
    let panel = state.current_profile_panel().unwrap();
    assert!(matches!(panel.load(), ProfileLoad::Ready(_)));
    assert_eq!(panel.draft().unwrap().stages().len(), 2);
}

#[test]
fn an_answer_for_another_profile_is_dropped() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_dpi();
    drain(&mut receiver);
    let k = key(&state);
    let other = ProfileIndex::new(2).unwrap();
    assert!(
        state
            .apply_profile_dpi_read(&k, other, Ok(held(&[800], 0)))
            .is_empty()
    );
    assert!(matches!(
        state.current_profile_panel().unwrap().load(),
        ProfileLoad::Loading
    ));
}

#[test]
fn a_failed_read_can_be_asked_again() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_dpi();
    drain(&mut receiver);
    let k = key(&state);
    let _ = state.apply_profile_dpi_read(
        &k,
        index(),
        Err(ProfileDpiError::Device(WriteError::AgentUnavailable)),
    );
    assert!(matches!(
        state.current_profile_panel().unwrap().load(),
        ProfileLoad::Failed(_)
    ));
    let _ = state.retry_profile_dpi();
    assert_eq!(reads(drain(&mut receiver)).len(), 1);
}

#[test]
fn saving_sends_the_edited_stages_and_nothing_before() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_dpi();
    let k = key(&state);
    let _ = state.apply_profile_dpi_read(&k, index(), Ok(held(&[800, 3200], 0)));
    drain(&mut receiver);
    let _ = state.edit_profile_dpi(|draft| {
        draft.set(1, Dpi::new(2400));
        draft.make_default(1);
    });
    assert!(
        writes(drain(&mut receiver)).is_empty(),
        "editing never writes the mouse"
    );
    let _ = state.save_profile_dpi();
    let sent = writes(drain(&mut receiver));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].stages, [Dpi::new(800), Dpi::new(2400)]);
    assert_eq!(sent[0].default_stage, 1);
    assert_eq!(sent[0].profile, index());
    assert!(matches!(
        state.current_profile_panel().unwrap().save(),
        ProfileSave::Saving
    ));
}

#[test]
fn saving_with_nothing_changed_sends_nothing() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_dpi();
    let k = key(&state);
    let _ = state.apply_profile_dpi_read(&k, index(), Ok(held(&[800, 3200], 0)));
    drain(&mut receiver);
    assert!(state.save_profile_dpi().is_empty());
    assert!(writes(drain(&mut receiver)).is_empty());
}

#[test]
fn a_second_save_waits_for_the_first() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_dpi();
    let k = key(&state);
    let _ = state.apply_profile_dpi_read(&k, index(), Ok(held(&[800, 3200], 0)));
    let _ = state.edit_profile_dpi(|d| {
        d.set(0, Dpi::new(900));
    });
    drain(&mut receiver);
    let _ = state.save_profile_dpi();
    let _ = state.save_profile_dpi();
    assert_eq!(writes(drain(&mut receiver)).len(), 1);
}

#[test]
fn the_saved_answer_replaces_the_draft_with_what_the_mouse_holds() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_dpi();
    let k = key(&state);
    let _ = state.apply_profile_dpi_read(&k, index(), Ok(held(&[800, 3200], 0)));
    let _ = state.edit_profile_dpi(|d| {
        d.set(1, Dpi::new(2400));
    });
    let _ = state.save_profile_dpi();
    drain(&mut receiver);
    let events = state.apply_profile_dpi_saved(
        &k,
        index(),
        Ok(ProfileDpiSaved {
            saved: held(&[800, 2400], 0),
            backup: "/b/file.txt".into(),
        }),
    );
    assert!(events.contains(&StateEvent::ProfileDpiChanged(k)));
    let panel = state.current_profile_panel().unwrap();
    assert!(matches!(panel.save(), ProfileSave::Saved { .. }));
    assert!(!panel.can_save());
}

#[test]
fn a_refused_save_keeps_the_draft_and_reports() {
    let (mut state, mut receiver) = mouse_on_profile();
    let _ = state.request_current_profile_dpi();
    let k = key(&state);
    let _ = state.apply_profile_dpi_read(&k, index(), Ok(held(&[800, 3200], 0)));
    let _ = state.edit_profile_dpi(|d| {
        d.set(1, Dpi::new(2400));
    });
    let _ = state.save_profile_dpi();
    drain(&mut receiver);
    let _ = state.apply_profile_dpi_saved(&k, index(), Err(ProfileDpiError::HostMode));
    let panel = state.current_profile_panel().unwrap();
    assert!(matches!(panel.save(), ProfileSave::Failed(_)));
    assert!(panel.can_save());
}
