//! Picking and saving a report rate into a mouse's onboard profile, while the
//! profile owns the mouse's report rate.
//!
//! Unlike DPI and LED, there is no separate read of what the profile holds:
//! `0x8061`'s live report rate already reflects the profile's whenever a
//! profile is in control (`crate::state::onboard::current_report_rate_locked`
//! is exactly that condition), so [`super::report_rate::ReportRateLoad`]
//! already answers "what does the profile hold" — this module only adds a
//! new way to *write* it: a draft pick and a "Save to mouse" request, through
//! the same [`ProfileSave`] status DPI and LED already use.

use openlogi_core::hid::{ProfileDpiError, ReportRate, ReportRateSaved};
use tracing::{info, warn};

use super::device_key::DeviceKey;
use super::events::{StateEvent, StateEvents};
use super::load::ReportRateLoad;
use super::profile_dpi::ProfileSave;
use super::{AppState, devices::DeviceRecord};
use crate::services::ipc::WriteProfileReportRate;

/// One mouse's draft pick and the state of the last save into its profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRateProfilePanel {
    draft: Option<ReportRate>,
    save: ProfileSave,
}

impl ReportRateProfilePanel {
    /// A panel with nothing picked yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            draft: None,
            save: ProfileSave::Idle,
        }
    }

    /// The rate picked in the UI, not yet necessarily saved.
    #[must_use]
    pub fn draft(&self) -> Option<ReportRate> {
        self.draft
    }

    /// How the last save stands.
    #[must_use]
    pub fn save(&self) -> &ProfileSave {
        &self.save
    }

    /// Pick `rate` as the draft. Does not write the mouse.
    pub fn pick(&mut self, rate: ReportRate) {
        self.draft = Some(rate);
        self.save = ProfileSave::Idle;
    }

    /// Whether a save can be asked for now: a draft that differs from `held`
    /// (what the profile currently reports live), with no save running.
    #[must_use]
    pub fn can_save(&self, held: ReportRate) -> bool {
        !matches!(self.save, ProfileSave::Saving) && self.draft.is_some_and(|rate| rate != held)
    }

    /// Note that a save is starting and say what to write; `None` when there
    /// is nothing to save or one is already running.
    pub fn begin_save(&mut self, held: ReportRate) -> Option<ReportRate> {
        if !self.can_save(held) {
            return None;
        }
        self.save = ProfileSave::Saving;
        self.draft
    }

    /// Take the answer to a save.
    pub fn saved(&mut self, result: &Result<ReportRateSaved, ProfileDpiError>) {
        match result {
            Ok(done) => {
                self.save = ProfileSave::Saved {
                    backup: done.backup.clone(),
                };
                self.draft = None;
            }
            Err(error) => self.save = ProfileSave::Failed(error.to_string()),
        }
    }
}

impl Default for ReportRateProfilePanel {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    /// The selected mouse's draft pick and save state, once one exists.
    #[must_use]
    pub fn current_report_rate_profile_panel(&self) -> Option<&ReportRateProfilePanel> {
        let key = self.current_record()?.device_key();
        self.pointer.report_rate_profile_panels.get(&key)
    }

    /// Pick `rate` as the selected mouse's draft, creating its panel if this
    /// is the first pick. Does not write the mouse.
    pub(crate) fn pick_profile_report_rate(&mut self, rate: ReportRate) -> StateEvents {
        let Some(key) = self.current_record().map(DeviceRecord::device_key) else {
            return StateEvents::none();
        };
        self.pointer
            .report_rate_profile_panels
            .entry(key.clone())
            .or_default()
            .pick(rate);
        StateEvent::ReportRateChanged(key).into()
    }

    /// Save the selected mouse's draft report rate into its stored profile:
    /// the call that rewrites the mouse's flash. Does nothing without a
    /// change to save, while a save runs, or once the mouse's own reading is
    /// unavailable to compare the draft against.
    pub(crate) fn save_profile_report_rate(&mut self) -> StateEvents {
        let Some(profile) = self.current_stored_profile() else {
            return StateEvents::none();
        };
        let Some((key, route)) = self
            .current_record()
            .and_then(|record| Some((record.device_key(), record.route.clone()?)))
        else {
            return StateEvents::none();
        };
        let ReportRateLoad::Ready(held) = self.current_report_rate_load() else {
            return StateEvents::none();
        };
        let Some(rate) = self
            .pointer
            .report_rate_profile_panels
            .get_mut(&key)
            .and_then(|panel| panel.begin_save(held.current))
        else {
            return StateEvents::none();
        };
        self.send_ipc(WriteProfileReportRate {
            key: key.clone(),
            route,
            profile,
            rate,
        });
        StateEvent::ReportRateChanged(key).into()
    }

    /// The agent's answer to a save into the stored profile. Also feeds the
    /// read-back into the live report-rate reading, the same cache a live
    /// write updates, so every view of the mouse's report rate agrees.
    pub fn apply_profile_report_rate_saved(
        &mut self,
        key: &DeviceKey,
        route: &openlogi_core::hid::DeviceRoute,
        result: Result<ReportRateSaved, ProfileDpiError>,
    ) -> StateEvents {
        let Some(panel) = self.pointer.report_rate_profile_panels.get_mut(key) else {
            return StateEvents::none();
        };
        match &result {
            Ok(done) => {
                info!(key = %key, backup = %done.backup, "report rate saved to the profile");
            }
            Err(error) => warn!(%error, key = %key, "saving the report rate to the profile failed"),
        }
        panel.saved(&result);
        if let Ok(done) = result {
            self.pointer
                .reads
                .set_report_rate_ready(key, route, done.saved);
        }
        StateEvent::ReportRateChanged(key.clone()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(hz: u16) -> ReportRate {
        ReportRate::new(hz)
    }

    #[test]
    fn a_new_panel_has_nothing_picked_and_cannot_save() {
        let panel = ReportRateProfilePanel::new();
        assert_eq!(panel.draft(), None);
        assert!(!panel.can_save(rate(1000)));
    }

    #[test]
    fn only_a_different_rate_from_what_is_held_can_be_saved() {
        let mut panel = ReportRateProfilePanel::new();
        panel.pick(rate(1000));
        assert!(!panel.can_save(rate(1000)));
        panel.pick(rate(125));
        assert!(panel.can_save(rate(1000)));
    }

    #[test]
    fn a_save_asks_for_exactly_the_picked_rate_and_blocks_a_second() {
        let mut panel = ReportRateProfilePanel::new();
        panel.pick(rate(125));
        assert_eq!(panel.begin_save(rate(1000)), Some(rate(125)));
        assert_eq!(panel.save(), &ProfileSave::Saving);
        assert!(panel.begin_save(rate(1000)).is_none());
    }

    #[test]
    fn nothing_is_asked_when_nothing_differs_from_what_is_held() {
        let mut panel = ReportRateProfilePanel::new();
        panel.pick(rate(1000));
        assert!(panel.begin_save(rate(1000)).is_none());
        assert_eq!(panel.save(), &ProfileSave::Idle);
    }

    #[test]
    fn a_good_save_clears_the_draft_and_shows_the_backup() {
        let mut panel = ReportRateProfilePanel::new();
        panel.pick(rate(125));
        panel.begin_save(rate(1000));
        panel.saved(&Ok(ReportRateSaved {
            saved: openlogi_core::hid::ReportRateInfo {
                current: rate(125),
                supported: vec![rate(125), rate(1000)],
            },
            backup: "b.txt".into(),
        }));
        assert_eq!(panel.draft(), None);
        assert_eq!(
            panel.save(),
            &ProfileSave::Saved {
                backup: "b.txt".into()
            }
        );
    }

    #[test]
    fn a_refused_save_keeps_the_draft_and_allows_another_try() {
        let mut panel = ReportRateProfilePanel::new();
        panel.pick(rate(125));
        panel.begin_save(rate(1000));
        panel.saved(&Err(ProfileDpiError::HostMode));
        assert!(matches!(panel.save(), ProfileSave::Failed(_)));
        assert_eq!(panel.draft(), Some(rate(125)));
        assert!(panel.can_save(rate(1000)));
    }
}
