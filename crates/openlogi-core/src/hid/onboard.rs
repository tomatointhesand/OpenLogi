//! Gaming-mouse onboard memory. Wire types: field order matters.

use std::fmt;
use std::num::NonZeroU8;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::dpi::Dpi;
use super::error::WriteError;

/// A 1-based onboard profile number. Config and wire carry the plain number;
/// the firmware has no profile 0, so it never deserializes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileIndex(NonZeroU8);

impl ProfileIndex {
    /// The profile numbered `index`; `None` for 0.
    #[must_use]
    pub const fn new(index: u8) -> Option<Self> {
        match NonZeroU8::new(index) {
            Some(index) => Some(Self(index)),
            None => None,
        }
    }

    /// The number as the firmware takes it.
    #[must_use]
    pub const fn get(self) -> NonZeroU8 {
        self.0
    }
}

impl From<NonZeroU8> for ProfileIndex {
    fn from(index: NonZeroU8) -> Self {
        Self(index)
    }
}

impl fmt::Display for ProfileIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Who owns a gaming mouse's buttons, DPI levels and report rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OnboardMode {
    /// A stored profile.
    Onboard,
    /// OpenLogi.
    Host,
}

/// One onboard profile slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardProfile {
    /// Slot number.
    pub index: ProfileIndex,
    /// Stored name.
    pub name: Option<String>,
    /// Whether it can be made active.
    pub enabled: bool,
}

/// Onboard memory as read from the mouse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardState {
    /// Current owner.
    pub mode: OnboardMode,
    /// Active profile; `None` in host mode.
    pub active_profile: Option<ProfileIndex>,
    /// Profile slots.
    pub profiles: Vec<OnboardProfile>,
}

/// The DPI stages one onboard profile stores: what its DPI button cycles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDpi {
    /// The profile.
    pub profile: ProfileIndex,
    /// The stages in use, one to five, in cycling order.
    pub stages: Vec<Dpi>,
    /// The stage the mouse starts on, an index into `stages`.
    pub default_stage: u8,
}

/// A profile's DPI stages after a save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDpiSaved {
    /// What the mouse holds now, read back.
    pub saved: ProfileDpi,
    /// The file the profile as it was before was saved to; empty when nothing
    /// needed writing.
    pub backup: String,
}

/// Why a profile's DPI stages could not be read or saved.
///
/// Append new variants only: the enum crosses the bincode IPC.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum ProfileDpiError {
    /// The mouse could not be reached or answered with an error.
    #[error(transparent)]
    Device(WriteError),
    /// The mouse is in host mode, so its profile cannot be rewritten.
    #[error("the mouse is under OpenLogi's control; switch it to a profile first")]
    HostMode,
    /// The mouse has no profile with this number.
    #[error("the mouse has no profile {0}")]
    NoSuchProfile(ProfileIndex),
    /// The profile exists but is switched off.
    #[error("profile {0} is switched off")]
    ProfileDisabled(ProfileIndex),
    /// The stored profile is not one this version understands, so it is left
    /// alone.
    #[error("this profile is not one OpenLogi can edit: {0}")]
    Unsupported(String),
    /// The stages asked for are not storable.
    #[error("{0}")]
    Invalid(String),
    /// The backup file could not be written, so nothing was changed.
    #[error("could not save a backup first, so nothing was changed: {0}")]
    BackupFailed(String),
    /// The write went wrong; `restored` says whether the original is back.
    #[error("the save did not land ({reason}); original restored: {restored}")]
    Failed {
        /// What went wrong.
        reason: String,
        /// Whether the original profile was written back and checked.
        restored: bool,
        /// The file holding the original.
        backup: String,
    },
}
