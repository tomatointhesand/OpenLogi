//! The fixed LED setting of a mouse's stored profile as the Lighting tab shows
//! it: what the mouse keeps for itself, and a request to keep the colour and
//! brightness chosen in the app.
//!
//! Pure: nothing here talks to the agent. [`LedPanel`] is what the view reads,
//! and the answers to its requests are fed back through [`LedPanel::loaded`]
//! and [`LedPanel::saved`].

use openlogi_core::hid::{ProfileDpiError, ProfileIndex, ProfileLed, ProfileLedSaved};
use tracing::{info, warn};

use super::AppState;
use super::device_key::DeviceKey;
use super::events::{StateEvent, StateEvents};
use super::profile_dpi::ProfileSave;
use crate::services::ipc::{ReadProfileLed, WriteProfileLed};

/// How reading the stored LED setting stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedLoad {
    /// Not asked yet.
    Idle,
    /// Asked, no answer yet.
    Loading,
    /// What the mouse holds.
    Ready(ProfileLed),
    /// The read failed or the profile's LED is not one fixed colour, with a
    /// sentence saying why.
    Failed(String),
}

/// What a save asks the agent to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedSaveRequest {
    /// The profile to rewrite.
    pub profile: ProfileIndex,
    /// The colour as red, green, blue.
    pub color: [u8; 3],
    /// The brightness in percent.
    pub brightness: u8,
}

/// One profile's stored LED setting and the state of the last read and save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedPanel {
    profile: ProfileIndex,
    load: LedLoad,
    save: ProfileSave,
}

impl LedPanel {
    /// A panel for `profile`, with nothing read.
    #[must_use]
    pub fn new(profile: ProfileIndex) -> Self {
        Self {
            profile,
            load: LedLoad::Idle,
            save: ProfileSave::Idle,
        }
    }

    /// The profile this panel shows.
    #[must_use]
    pub fn profile(&self) -> ProfileIndex {
        self.profile
    }

    /// How the read stands.
    #[must_use]
    pub fn load(&self) -> &LedLoad {
        &self.load
    }

    /// How the last save stands.
    #[must_use]
    pub fn save(&self) -> &ProfileSave {
        &self.save
    }

    /// Note that the read has been asked for.
    pub fn begin_load(&mut self) {
        self.load = LedLoad::Loading;
    }

    /// Take the answer to a read.
    pub fn loaded(&mut self, result: Result<ProfileLed, ProfileDpiError>) {
        self.load = match result {
            Ok(held) => LedLoad::Ready(held),
            Err(error) => LedLoad::Failed(error.to_string()),
        };
    }

    /// Whether keeping `color` and `brightness` would change what the mouse
    /// holds. False until the profile has been read.
    #[must_use]
    pub fn differs_from(&self, color: [u8; 3], brightness: u8) -> bool {
        matches!(&self.load, LedLoad::Ready(held)
            if held.color != color || held.brightness != brightness)
    }

    /// Whether a save can be asked for now.
    #[must_use]
    pub fn can_save(&self, color: [u8; 3], brightness: u8) -> bool {
        !matches!(self.save, ProfileSave::Saving) && self.differs_from(color, brightness)
    }

    /// Note that a save is starting and say what to write; `None` when there
    /// is nothing to save or one is already running.
    pub fn begin_save(&mut self, color: [u8; 3], brightness: u8) -> Option<LedSaveRequest> {
        if !self.can_save(color, brightness) {
            return None;
        }
        self.save = ProfileSave::Saving;
        Some(LedSaveRequest {
            profile: self.profile,
            color,
            brightness,
        })
    }

    /// Take the answer to a save. A good one shows what the mouse holds now.
    pub fn saved(&mut self, result: Result<ProfileLedSaved, ProfileDpiError>) {
        match result {
            Ok(done) => {
                self.load = LedLoad::Ready(done.saved);
                self.save = ProfileSave::Saved {
                    backup: done.backup,
                };
            }
            Err(error) => self.save = ProfileSave::Failed(error.to_string()),
        }
    }
}

impl AppState {
    /// The selected mouse's stored LED panel, once it has been asked for.
    #[must_use]
    pub fn current_led_panel(&self) -> Option<&LedPanel> {
        let key = self.current_record()?.device_key();
        self.pointer.led_panels.get(&key)
    }

    /// Ask the agent for the selected mouse's stored LED setting when a
    /// profile owns it and it has not been asked for (or the active profile
    /// changed).
    pub(crate) fn request_current_profile_led(&mut self) -> StateEvents {
        let Some(profile) = self.current_stored_profile() else {
            return StateEvents::none();
        };
        let Some((key, route)) = self
            .current_record()
            .and_then(|record| Some((record.device_key(), record.route.clone()?)))
        else {
            return StateEvents::none();
        };
        if self.pointer.led_panels.get(&key).is_some_and(|panel| {
            panel.profile() == profile && !matches!(panel.load(), LedLoad::Idle)
        }) {
            return StateEvents::none();
        }
        let mut panel = LedPanel::new(profile);
        panel.begin_load();
        self.pointer.led_panels.insert(key.clone(), panel);
        self.send_ipc(ReadProfileLed {
            key: key.clone(),
            route,
            profile,
        });
        StateEvent::ProfileLedChanged(key).into()
    }

    /// Keep `color` and `brightness` in the mouse's profile: the call that
    /// rewrites the mouse's flash. Does nothing without a change to save or
    /// while a save runs.
    pub(crate) fn save_profile_led(&mut self, color: [u8; 3], brightness: u8) -> StateEvents {
        let Some((key, route)) = self
            .current_record()
            .and_then(|record| Some((record.device_key(), record.route.clone()?)))
        else {
            return StateEvents::none();
        };
        let Some(request) = self
            .pointer
            .led_panels
            .get_mut(&key)
            .and_then(|panel| panel.begin_save(color, brightness))
        else {
            return StateEvents::none();
        };
        self.send_ipc(WriteProfileLed {
            key: key.clone(),
            route,
            profile: request.profile,
            color: request.color,
            brightness: request.brightness,
        });
        StateEvent::ProfileLedChanged(key).into()
    }

    /// The agent's answer to a stored-LED read. An answer for a profile the
    /// panel no longer shows is dropped.
    pub fn apply_profile_led_read(
        &mut self,
        key: &DeviceKey,
        profile: ProfileIndex,
        result: Result<ProfileLed, ProfileDpiError>,
    ) -> StateEvents {
        let Some(panel) = self.pointer.led_panels.get_mut(key) else {
            return StateEvents::none();
        };
        if panel.profile() != profile {
            return StateEvents::none();
        }
        if let Err(error) = &result {
            warn!(%error, key = %key, "reading the stored LED setting failed");
        }
        panel.loaded(result);
        StateEvent::ProfileLedChanged(key.clone()).into()
    }

    /// The agent's answer to a save into the stored profile.
    pub fn apply_profile_led_saved(
        &mut self,
        key: &DeviceKey,
        profile: ProfileIndex,
        result: Result<ProfileLedSaved, ProfileDpiError>,
    ) -> StateEvents {
        let Some(panel) = self.pointer.led_panels.get_mut(key) else {
            return StateEvents::none();
        };
        if panel.profile() != profile {
            return StateEvents::none();
        }
        match &result {
            Ok(done) => info!(key = %key, backup = %done.backup, "stored LED setting saved"),
            Err(error) => warn!(%error, key = %key, "saving the stored LED setting failed"),
        }
        panel.saved(result);
        StateEvent::ProfileLedChanged(key.clone()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> ProfileIndex {
        ProfileIndex::new(1).unwrap()
    }

    fn led(color: [u8; 3], brightness: u8) -> ProfileLed {
        ProfileLed {
            profile: index(),
            color,
            brightness,
        }
    }

    fn ready(color: [u8; 3], brightness: u8) -> LedPanel {
        let mut panel = LedPanel::new(index());
        panel.begin_load();
        panel.loaded(Ok(led(color, brightness)));
        panel
    }

    #[test]
    fn a_new_panel_has_read_nothing_and_cannot_save() {
        let panel = LedPanel::new(index());
        assert_eq!(panel.load(), &LedLoad::Idle);
        assert!(!panel.can_save([1, 2, 3], 50));
    }

    #[test]
    fn a_read_shows_what_the_mouse_holds() {
        let panel = ready([1, 2, 3], 40);
        assert_eq!(panel.load(), &LedLoad::Ready(led([1, 2, 3], 40)));
    }

    #[test]
    fn a_failed_read_says_why_and_cannot_save() {
        let mut panel = LedPanel::new(index());
        panel.loaded(Err(ProfileDpiError::Unsupported("not fixed".into())));
        assert!(matches!(panel.load(), LedLoad::Failed(text) if text.contains("not fixed")));
        assert!(!panel.can_save([1, 2, 3], 50));
    }

    #[test]
    fn only_a_different_colour_or_brightness_can_be_saved() {
        let panel = ready([1, 2, 3], 40);
        assert!(!panel.can_save([1, 2, 3], 40));
        assert!(panel.can_save([9, 2, 3], 40));
        assert!(panel.can_save([1, 2, 3], 41));
    }

    #[test]
    fn a_save_asks_for_exactly_what_was_chosen_and_blocks_a_second() {
        let mut panel = ready([1, 2, 3], 40);
        let request = panel.begin_save([4, 5, 6], 70).unwrap();
        assert_eq!(
            request,
            LedSaveRequest {
                profile: index(),
                color: [4, 5, 6],
                brightness: 70
            }
        );
        assert_eq!(panel.save(), &ProfileSave::Saving);
        assert!(panel.begin_save([7, 8, 9], 10).is_none());
    }

    #[test]
    fn nothing_is_asked_when_nothing_differs() {
        let mut panel = ready([1, 2, 3], 40);
        assert!(panel.begin_save([1, 2, 3], 40).is_none());
        assert_eq!(panel.save(), &ProfileSave::Idle);
    }

    #[test]
    fn a_good_save_shows_the_new_setting_and_the_backup() {
        let mut panel = ready([1, 2, 3], 40);
        panel.begin_save([4, 5, 6], 70);
        panel.saved(Ok(ProfileLedSaved {
            saved: led([4, 5, 6], 70),
            backup: "b.txt".into(),
        }));
        assert_eq!(panel.load(), &LedLoad::Ready(led([4, 5, 6], 70)));
        assert_eq!(
            panel.save(),
            &ProfileSave::Saved {
                backup: "b.txt".into()
            }
        );
        assert!(!panel.can_save([4, 5, 6], 70));
    }

    #[test]
    fn a_refused_save_keeps_what_the_mouse_holds_and_allows_another_try() {
        let mut panel = ready([1, 2, 3], 40);
        panel.begin_save([4, 5, 6], 70);
        panel.saved(Err(ProfileDpiError::HostMode));
        assert!(matches!(panel.save(), ProfileSave::Failed(_)));
        assert_eq!(panel.load(), &LedLoad::Ready(led([1, 2, 3], 40)));
        assert!(panel.can_save([4, 5, 6], 70));
    }
}
