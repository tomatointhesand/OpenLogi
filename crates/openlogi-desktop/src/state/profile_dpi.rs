//! The DPI stages of a mouse's stored profile as the Pointer tab edits them:
//! a draft of the stages that is saved to the mouse only on request.
//!
//! Pure: nothing here talks to the agent. [`ProfilePanel`] is what the view
//! reads, and the answers to its requests are fed back through
//! [`ProfilePanel::loaded`] and [`ProfilePanel::saved`].

use openlogi_core::config::OnboardMemory;
use openlogi_core::hid::{Dpi, ProfileDpi, ProfileDpiError, ProfileDpiSaved};
use tracing::{info, warn};

use super::AppState;
use super::device_key::DeviceKey;
use super::devices::DeviceRecord;
use super::events::{StateEvent, StateEvents};
use crate::services::ipc::{ReadProfileDpi, WriteProfileDpi};

/// How many stages a profile stores.
pub const MAX_STAGES: usize = 5;

/// The stages being edited, and which of them the mouse starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageDraft {
    stages: Vec<Dpi>,
    default_stage: u8,
}

impl StageDraft {
    /// A draft that starts as `held`, what the mouse stores.
    #[must_use]
    pub fn from_profile(held: &ProfileDpi) -> Self {
        Self {
            stages: held.stages.clone(),
            default_stage: held.default_stage,
        }
    }

    /// The stages in cycling order.
    #[must_use]
    pub fn stages(&self) -> &[Dpi] {
        &self.stages
    }

    /// The stage the mouse starts on, an index into [`Self::stages`].
    #[must_use]
    pub fn default_stage(&self) -> u8 {
        self.default_stage
    }

    /// Whether another stage fits in the profile.
    #[must_use]
    pub fn can_add(&self) -> bool {
        self.stages.len() < MAX_STAGES
    }

    /// Whether a stage can be taken out: one always stays.
    #[must_use]
    pub fn can_remove(&self) -> bool {
        self.stages.len() > 1
    }

    /// Append a stage. `false` when the profile is full or `dpi` is zero.
    pub fn add(&mut self, dpi: Dpi) -> bool {
        if !self.can_add() || dpi.into_inner() == 0 {
            return false;
        }
        self.stages.push(dpi);
        true
    }

    /// Remove stage `index`, keeping the default on the same DPI where it
    /// still exists and on the first stage where it was removed.
    pub fn remove(&mut self, index: usize) -> bool {
        if !self.can_remove() || index >= self.stages.len() {
            return false;
        }
        self.stages.remove(index);
        let default = usize::from(self.default_stage);
        self.default_stage = match default.cmp(&index) {
            std::cmp::Ordering::Less => self.default_stage,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => self.default_stage - 1,
        };
        true
    }

    /// Change stage `index` to `dpi`. `false` when there is no such stage or
    /// `dpi` is zero.
    pub fn set(&mut self, index: usize, dpi: Dpi) -> bool {
        match self.stages.get_mut(index) {
            Some(stage) if dpi.into_inner() != 0 => {
                *stage = dpi;
                true
            }
            _ => false,
        }
    }

    /// Start the mouse on stage `index`. `false` when there is no such stage.
    pub fn make_default(&mut self, index: usize) -> bool {
        match u8::try_from(index) {
            Ok(index) if usize::from(index) < self.stages.len() => {
                self.default_stage = index;
                true
            }
            _ => false,
        }
    }

    /// Whether this draft would change what `held` stores.
    #[must_use]
    pub fn differs_from(&self, held: &ProfileDpi) -> bool {
        self.stages != held.stages || self.default_stage != held.default_stage
    }
}

/// How reading the profile from the mouse stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileLoad {
    /// Not asked yet.
    Idle,
    /// Asked, no answer yet.
    Loading,
    /// What the mouse holds.
    Ready(ProfileDpi),
    /// The read failed, with a sentence saying why.
    Failed(String),
}

/// How saving to the mouse stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileSave {
    /// Nothing to report.
    Idle,
    /// A save is running.
    Saving,
    /// The last save landed; `backup` names the file kept of what was there.
    Saved {
        /// The backup file, empty when nothing needed writing.
        backup: String,
    },
    /// The last save did not land, with a sentence saying why.
    Failed(String),
}

/// What a save asks the agent to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveRequest {
    /// The profile to rewrite.
    pub profile: openlogi_core::hid::ProfileIndex,
    /// Its new stages.
    pub stages: Vec<Dpi>,
    /// The stage to start on.
    pub default_stage: u8,
}

/// One profile's DPI stages: what the mouse holds, the draft being edited and
/// the state of the last read and save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePanel {
    profile: openlogi_core::hid::ProfileIndex,
    load: ProfileLoad,
    draft: Option<StageDraft>,
    save: ProfileSave,
}

impl ProfilePanel {
    /// A panel for `profile`, with nothing read.
    #[must_use]
    pub fn new(profile: openlogi_core::hid::ProfileIndex) -> Self {
        Self {
            profile,
            load: ProfileLoad::Idle,
            draft: None,
            save: ProfileSave::Idle,
        }
    }

    /// The profile this panel edits.
    #[must_use]
    pub fn profile(&self) -> openlogi_core::hid::ProfileIndex {
        self.profile
    }

    /// How the read stands.
    #[must_use]
    pub fn load(&self) -> &ProfileLoad {
        &self.load
    }

    /// How the last save stands.
    #[must_use]
    pub fn save(&self) -> &ProfileSave {
        &self.save
    }

    /// The stages being edited, once the profile has been read.
    #[must_use]
    pub fn draft(&self) -> Option<&StageDraft> {
        self.draft.as_ref()
    }

    /// Note that the read has been asked for.
    pub fn begin_load(&mut self) {
        self.load = ProfileLoad::Loading;
    }

    /// Take the answer to a read. A good one replaces the draft.
    pub fn loaded(&mut self, result: Result<ProfileDpi, ProfileDpiError>) {
        match result {
            Ok(held) => {
                self.draft = Some(StageDraft::from_profile(&held));
                self.load = ProfileLoad::Ready(held);
            }
            Err(error) => {
                self.draft = None;
                self.load = ProfileLoad::Failed(error.to_string());
            }
        }
    }

    /// Edit the draft; clears the last save's message. Does nothing before
    /// the profile has been read.
    pub fn edit(&mut self, change: impl FnOnce(&mut StageDraft)) {
        if let Some(draft) = &mut self.draft {
            change(draft);
            if !matches!(self.save, ProfileSave::Saving) {
                self.save = ProfileSave::Idle;
            }
        }
    }

    /// Whether there is something to save and nothing is being saved.
    #[must_use]
    pub fn can_save(&self) -> bool {
        match (&self.load, &self.draft, &self.save) {
            (ProfileLoad::Ready(held), Some(draft), save) => {
                !matches!(save, ProfileSave::Saving) && draft.differs_from(held)
            }
            _ => false,
        }
    }

    /// Start a save: the stages to write, or `None` when [`Self::can_save`]
    /// is false.
    pub fn begin_save(&mut self) -> Option<SaveRequest> {
        if !self.can_save() {
            return None;
        }
        let draft = self.draft.as_ref()?;
        let request = SaveRequest {
            profile: self.profile,
            stages: draft.stages().to_vec(),
            default_stage: draft.default_stage(),
        };
        self.save = ProfileSave::Saving;
        Some(request)
    }

    /// Take the answer to a save. A landed save makes what the mouse read
    /// back both the held profile and the draft; a failed one keeps the draft.
    pub fn saved(&mut self, result: Result<ProfileDpiSaved, ProfileDpiError>) {
        match result {
            Ok(done) => {
                self.draft = Some(StageDraft::from_profile(&done.saved));
                self.load = ProfileLoad::Ready(done.saved);
                self.save = ProfileSave::Saved {
                    backup: done.backup,
                };
            }
            Err(error) => self.save = ProfileSave::Failed(error.to_string()),
        }
    }
}

/// The stage the mouse is using: the first of `stages` equal to the DPI it
/// reports, or `None` when it reports none of them.
#[must_use]
pub fn active_stage(stages: &[Dpi], current: Dpi) -> Option<usize> {
    stages.iter().position(|&stage| stage == current)
}

/// A backup file's folder and name, for a line of text: the full path is long
/// and says nothing the folder and name do not.
#[must_use]
pub fn short_backup_path(path: &str) -> String {
    let parts: Vec<&str> = path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    parts[parts.len().saturating_sub(2)..].join("/")
}

impl AppState {
    /// The selected mouse's stored-profile DPI panel, once it has been asked for.
    #[must_use]
    pub fn current_profile_panel(&self) -> Option<&ProfilePanel> {
        let key = self.current_record()?.device_key();
        self.pointer.profile_panels.get(&key)
    }

    /// The profile whose stages the selected mouse runs: the active one, when
    /// a profile (not OpenLogi) owns the mouse.
    pub(in crate::state) fn current_stored_profile(
        &self,
    ) -> Option<openlogi_core::hid::ProfileIndex> {
        match self.current_onboard_active()? {
            OnboardMemory::Profile(index) if self.current_onboard_supported() => Some(index),
            _ => None,
        }
    }

    /// Ask the agent for the selected mouse's stored stages when a profile owns
    /// it and they have not been asked for (or the active profile changed).
    pub(crate) fn request_current_profile_dpi(&mut self) -> StateEvents {
        let Some(profile) = self.current_stored_profile() else {
            return StateEvents::none();
        };
        let Some((key, route)) = self
            .current_record()
            .and_then(|record| Some((record.device_key(), record.route.clone()?)))
        else {
            return StateEvents::none();
        };
        if self.pointer.profile_panels.get(&key).is_some_and(|panel| {
            panel.profile() == profile && !matches!(panel.load(), ProfileLoad::Idle)
        }) {
            return StateEvents::none();
        }
        let mut panel = ProfilePanel::new(profile);
        panel.begin_load();
        self.pointer.profile_panels.insert(key.clone(), panel);
        self.send_ipc(ReadProfileDpi {
            key: key.clone(),
            route,
            profile,
        });
        StateEvent::ProfileDpiChanged(key).into()
    }

    /// Ask again after a failed read.
    pub(crate) fn retry_profile_dpi(&mut self) -> StateEvents {
        if let Some(key) = self.current_record().map(DeviceRecord::device_key) {
            self.pointer.profile_panels.remove(&key);
        }
        self.request_current_profile_dpi()
    }

    /// Read the selected mouse's live DPI again, so the stage its DPI button
    /// selected shows. Read-only.
    pub(crate) fn refresh_live_dpi(&mut self) -> StateEvents {
        if let Some(key) = self.current_record().map(DeviceRecord::device_key) {
            self.pointer.reads.retry_dpi(&key);
        }
        StateEvents::none()
    }

    /// The DPI the selected mouse reports it is using now.
    #[must_use]
    pub fn live_dpi(&self) -> Option<Dpi> {
        let key = self.current_record()?.device_key();
        match self.dpi_load_for(&key) {
            crate::state::DpiLoad::Ready(info) => Some(info.current),
            _ => None,
        }
    }

    /// Change the selected mouse's draft stages. Nothing reaches the mouse.
    pub(crate) fn edit_profile_dpi(&mut self, change: impl FnOnce(&mut StageDraft)) -> StateEvents {
        let Some(key) = self.current_record().map(DeviceRecord::device_key) else {
            return StateEvents::none();
        };
        let Some(panel) = self.pointer.profile_panels.get_mut(&key) else {
            return StateEvents::none();
        };
        panel.edit(change);
        StateEvent::ProfileDpiChanged(key).into()
    }

    /// Save the draft into the mouse's profile: the one call that rewrites the
    /// mouse's flash. Does nothing without a change to save or while a save runs.
    pub(crate) fn save_profile_dpi(&mut self) -> StateEvents {
        let Some((key, route)) = self
            .current_record()
            .and_then(|record| Some((record.device_key(), record.route.clone()?)))
        else {
            return StateEvents::none();
        };
        let Some(request) = self
            .pointer
            .profile_panels
            .get_mut(&key)
            .and_then(ProfilePanel::begin_save)
        else {
            return StateEvents::none();
        };
        self.send_ipc(WriteProfileDpi {
            key: key.clone(),
            route,
            profile: request.profile,
            stages: request.stages,
            default_stage: request.default_stage,
        });
        StateEvent::ProfileDpiChanged(key).into()
    }

    /// The agent's answer to a stored-profile read. An answer for a profile
    /// the panel no longer shows is dropped.
    pub fn apply_profile_dpi_read(
        &mut self,
        key: &DeviceKey,
        profile: openlogi_core::hid::ProfileIndex,
        result: Result<ProfileDpi, ProfileDpiError>,
    ) -> StateEvents {
        let Some(panel) = self.pointer.profile_panels.get_mut(key) else {
            return StateEvents::none();
        };
        if panel.profile() != profile {
            return StateEvents::none();
        }
        if let Err(error) = &result {
            warn!(%error, key = %key, "reading the stored DPI stages failed");
        }
        panel.loaded(result);
        StateEvent::ProfileDpiChanged(key.clone()).into()
    }

    /// The agent's answer to a save into the stored profile.
    pub fn apply_profile_dpi_saved(
        &mut self,
        key: &DeviceKey,
        profile: openlogi_core::hid::ProfileIndex,
        result: Result<ProfileDpiSaved, ProfileDpiError>,
    ) -> StateEvents {
        let Some(panel) = self.pointer.profile_panels.get_mut(key) else {
            return StateEvents::none();
        };
        if panel.profile() != profile {
            return StateEvents::none();
        }
        match &result {
            Ok(done) => info!(key = %key, backup = %done.backup, "stored DPI stages saved"),
            Err(error) => warn!(%error, key = %key, "saving the stored DPI stages failed"),
        }
        panel.saved(result);
        StateEvent::ProfileDpiChanged(key.clone()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlogi_core::hid::ProfileIndex;

    fn dpi(value: u16) -> Dpi {
        Dpi::new(value)
    }

    fn profile(stages: &[u16], default_stage: u8) -> ProfileDpi {
        ProfileDpi {
            profile: ProfileIndex::new(1).unwrap(),
            stages: stages.iter().map(|&v| dpi(v)).collect(),
            default_stage,
        }
    }

    fn draft(stages: &[u16], default_stage: u8) -> StageDraft {
        StageDraft::from_profile(&profile(stages, default_stage))
    }

    // -- editing the stages --------------------------------------------------

    #[test]
    fn a_draft_starts_as_what_the_mouse_holds() {
        let d = draft(&[800, 3200], 1);
        assert_eq!(d.stages(), [dpi(800), dpi(3200)]);
        assert_eq!(d.default_stage(), 1);
    }

    #[test]
    fn a_stage_can_be_added_until_the_profile_is_full() {
        let mut d = draft(&[800], 0);
        for value in [1200, 1600, 2400, 3200] {
            assert!(d.can_add());
            assert!(d.add(dpi(value)));
        }
        assert_eq!(d.stages().len(), MAX_STAGES);
        assert!(!d.can_add());
        assert!(!d.add(dpi(6000)));
        assert_eq!(d.stages().len(), MAX_STAGES);
    }

    #[test]
    fn the_last_stage_cannot_be_removed() {
        let mut d = draft(&[800], 0);
        assert!(!d.can_remove());
        assert!(!d.remove(0));
        assert_eq!(d.stages(), [dpi(800)]);
    }

    #[test]
    fn removing_a_stage_before_the_default_keeps_the_same_default_stage() {
        let mut d = draft(&[400, 800, 3200], 2);
        assert!(d.remove(0));
        assert_eq!(d.stages(), [dpi(800), dpi(3200)]);
        assert_eq!(d.default_stage(), 1);
        assert_eq!(d.stages()[usize::from(d.default_stage())], dpi(3200));
    }

    #[test]
    fn removing_the_default_stage_makes_the_first_the_default() {
        let mut d = draft(&[400, 800, 3200], 1);
        assert!(d.remove(1));
        assert_eq!(d.default_stage(), 0);
    }

    #[test]
    fn removing_a_stage_after_the_default_leaves_the_default() {
        let mut d = draft(&[400, 800, 3200], 0);
        assert!(d.remove(2));
        assert_eq!(d.default_stage(), 0);
    }

    #[test]
    fn a_stage_that_is_not_there_cannot_be_removed_or_changed() {
        let mut d = draft(&[800, 3200], 0);
        assert!(!d.remove(2));
        assert!(!d.set(2, dpi(1000)));
        assert!(!d.make_default(2));
        assert_eq!(d.stages(), [dpi(800), dpi(3200)]);
        assert_eq!(d.default_stage(), 0);
    }

    #[test]
    fn a_stage_can_be_changed_and_made_the_default() {
        let mut d = draft(&[800, 3200], 0);
        assert!(d.set(1, dpi(2400)));
        assert!(d.make_default(1));
        assert_eq!(d.stages(), [dpi(800), dpi(2400)]);
        assert_eq!(d.default_stage(), 1);
    }

    /// A stage of zero would read back as "unused", so it is never accepted.
    #[test]
    fn a_zero_stage_is_never_accepted() {
        let mut d = draft(&[800], 0);
        assert!(!d.add(dpi(0)));
        assert!(!d.set(0, dpi(0)));
        assert_eq!(d.stages(), [dpi(800)]);
    }

    #[test]
    fn a_draft_knows_whether_it_differs_from_the_mouse() {
        let held = profile(&[800, 3200], 0);
        let mut d = StageDraft::from_profile(&held);
        assert!(!d.differs_from(&held));
        d.set(0, dpi(900));
        assert!(d.differs_from(&held));
        d.set(0, dpi(800));
        assert!(!d.differs_from(&held));
        d.make_default(1);
        assert!(d.differs_from(&held));
    }

    // -- the panel: loading and saving ---------------------------------------

    fn index() -> ProfileIndex {
        ProfileIndex::new(1).unwrap()
    }

    fn saved(stages: &[u16], default_stage: u8, backup: &str) -> ProfileDpiSaved {
        ProfileDpiSaved {
            saved: profile(stages, default_stage),
            backup: backup.to_string(),
        }
    }

    #[test]
    fn a_new_panel_has_read_nothing() {
        let panel = ProfilePanel::new(index());
        assert!(matches!(panel.load(), ProfileLoad::Idle));
        assert!(panel.draft().is_none());
        assert!(!panel.can_save());
    }

    #[test]
    fn reading_the_profile_gives_a_draft_to_edit() {
        let mut panel = ProfilePanel::new(index());
        panel.begin_load();
        assert!(matches!(panel.load(), ProfileLoad::Loading));
        panel.loaded(Ok(profile(&[800, 3200], 0)));
        assert!(matches!(panel.load(), ProfileLoad::Ready(_)));
        assert_eq!(panel.draft().unwrap().stages(), [dpi(800), dpi(3200)]);
        assert!(!panel.can_save(), "nothing changed yet");
    }

    #[test]
    fn a_failed_read_says_why_and_leaves_no_draft() {
        let mut panel = ProfilePanel::new(index());
        panel.begin_load();
        panel.loaded(Err(ProfileDpiError::HostMode));
        let ProfileLoad::Failed(message) = panel.load() else {
            panic!("expected a failed read");
        };
        assert!(message.contains("control"), "{message}");
        assert!(panel.draft().is_none());
    }

    #[test]
    fn an_edit_makes_the_draft_saveable() {
        let mut panel = ProfilePanel::new(index());
        panel.loaded(Ok(profile(&[800, 3200], 0)));
        panel.edit(|draft| {
            draft.set(1, dpi(2400));
        });
        assert!(panel.can_save());
    }

    #[test]
    fn editing_before_the_profile_is_read_does_nothing() {
        let mut panel = ProfilePanel::new(index());
        panel.edit(|_| panic!("no draft to edit"));
    }

    #[test]
    fn saving_cannot_start_while_one_is_running() {
        let mut panel = ProfilePanel::new(index());
        panel.loaded(Ok(profile(&[800, 3200], 0)));
        panel.edit(|d| {
            d.set(0, dpi(900));
        });
        assert!(panel.begin_save().is_some());
        assert!(matches!(panel.save(), ProfileSave::Saving));
        assert!(!panel.can_save());
        assert!(panel.begin_save().is_none());
    }

    #[test]
    fn begin_save_hands_over_the_stages_to_write() {
        let mut panel = ProfilePanel::new(index());
        panel.loaded(Ok(profile(&[800, 3200], 0)));
        panel.edit(|d| {
            d.set(1, dpi(2400));
            d.make_default(1);
        });
        let request = panel.begin_save().unwrap();
        assert_eq!(request.stages, [dpi(800), dpi(2400)]);
        assert_eq!(request.default_stage, 1);
        assert_eq!(request.profile, index());
    }

    #[test]
    fn nothing_changed_means_nothing_to_save() {
        let mut panel = ProfilePanel::new(index());
        panel.loaded(Ok(profile(&[800, 3200], 0)));
        assert!(panel.begin_save().is_none());
    }

    #[test]
    fn a_saved_profile_becomes_what_the_mouse_holds() {
        let mut panel = ProfilePanel::new(index());
        panel.loaded(Ok(profile(&[800, 3200], 0)));
        panel.edit(|d| {
            d.set(1, dpi(2400));
        });
        panel.begin_save().unwrap();
        panel.saved(Ok(saved(&[800, 2400], 0, "/b/file.txt")));
        let ProfileSave::Saved { backup } = panel.save() else {
            panic!("expected a saved profile");
        };
        assert_eq!(backup, "/b/file.txt");
        assert_eq!(panel.draft().unwrap().stages(), [dpi(800), dpi(2400)]);
        assert!(!panel.can_save(), "the draft now matches the mouse");
    }

    /// The mouse may store a value other than the one asked for; the draft
    /// shows what was stored.
    #[test]
    fn the_draft_shows_what_the_mouse_read_back() {
        let mut panel = ProfilePanel::new(index());
        panel.loaded(Ok(profile(&[800, 3200], 0)));
        panel.edit(|d| {
            d.set(1, dpi(2401));
        });
        panel.begin_save().unwrap();
        panel.saved(Ok(saved(&[800, 2400], 0, "")));
        assert_eq!(panel.draft().unwrap().stages(), [dpi(800), dpi(2400)]);
    }

    #[test]
    fn a_failed_save_keeps_the_edits_and_says_what_happened() {
        let mut panel = ProfilePanel::new(index());
        panel.loaded(Ok(profile(&[800, 3200], 0)));
        panel.edit(|d| {
            d.set(1, dpi(2400));
        });
        panel.begin_save().unwrap();
        panel.saved(Err(ProfileDpiError::Failed {
            reason: "stalled".into(),
            restored: true,
            backup: "/b/file.txt".into(),
        }));
        let ProfileSave::Failed(message) = panel.save() else {
            panic!("expected a failed save");
        };
        assert!(message.contains("stalled"), "{message}");
        assert_eq!(panel.draft().unwrap().stages(), [dpi(800), dpi(2400)]);
        assert!(panel.can_save(), "the user can try again");
    }

    #[test]
    fn editing_after_a_save_clears_its_message() {
        let mut panel = ProfilePanel::new(index());
        panel.loaded(Ok(profile(&[800, 3200], 0)));
        panel.edit(|d| {
            d.set(1, dpi(2400));
        });
        panel.begin_save().unwrap();
        panel.saved(Ok(saved(&[800, 2400], 0, "/b")));
        panel.edit(|d| {
            d.set(0, dpi(900));
        });
        assert!(matches!(panel.save(), ProfileSave::Idle));
    }

    // -- which stage is active, and how a backup is named -------------------

    #[test]
    fn the_active_stage_is_the_one_the_mouse_reports() {
        let stages = [dpi(800), dpi(3200)];
        assert_eq!(active_stage(&stages, dpi(800)), Some(0));
        assert_eq!(active_stage(&stages, dpi(3200)), Some(1));
    }

    #[test]
    fn a_dpi_that_is_no_stage_makes_none_active() {
        assert_eq!(active_stage(&[dpi(800), dpi(3200)], dpi(1030)), None);
        assert_eq!(active_stage(&[], dpi(800)), None);
    }

    /// With the same value twice there is no telling which was pressed; the
    /// first is shown.
    #[test]
    fn a_repeated_stage_shows_the_first() {
        assert_eq!(active_stage(&[dpi(800), dpi(800)], dpi(800)), Some(0));
    }

    #[test]
    fn a_backup_path_is_shortened_to_its_folder_and_file() {
        assert_eq!(
            short_backup_path("/home/me/.local/state/openlogi/profile-backups/b-p1-7.txt"),
            "profile-backups/b-p1-7.txt"
        );
        assert_eq!(
            short_backup_path(r"C:\Users\me\AppData\openlogi\profile-backups\b-p1-7.txt"),
            "profile-backups/b-p1-7.txt"
        );
    }

    #[test]
    fn a_short_path_is_left_as_it_is() {
        assert_eq!(short_backup_path("b-p1-7.txt"), "b-p1-7.txt");
        assert_eq!(short_backup_path("dir/b.txt"), "dir/b.txt");
        assert_eq!(short_backup_path(""), "");
    }
}
