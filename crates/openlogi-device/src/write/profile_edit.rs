//! Reading and rewriting one onboard profile's sector on a real mouse, through
//! the guarded sequence in `hidpp::feature::onboard_profiles::profile_io`.
//!
//! The rules below are pure and tested. The device calls at the bottom are
//! thin glue over the tested pieces and are proven only on a mouse.

use std::sync::Arc;

use hidpp::{
    channel::HidppChannel,
    device::Device,
    feature::onboard_profiles::{
        OnboardMode as WireMode, OnboardProfilesFeature, ProfileEntry,
        profile::ProfileError,
        profile_io::{self, WriteError as GuardedWriteError},
    },
};
use openlogi_core::hid::{OnboardMode, ProfileIndex};

pub use hidpp::feature::onboard_profiles::profile::{
    DPI_STAGE_COUNT, ProfileError as ProfileLayoutError, ProfileSector, SectorBackup,
};

use crate::backend::HidBackend;
use crate::channel::route::DeviceRoute;

use super::{HidppOperation, WriteError, classify_hidpp_error, open_feature, with_route};

/// Why a profile could not be read or rewritten.
#[derive(Debug, thiserror::Error)]
pub enum ProfileEditError {
    /// The mouse could not be reached or answered with an error.
    #[error(transparent)]
    Device(#[from] WriteError),
    /// The mouse has no profile with this number.
    #[error("the mouse has no profile {0}")]
    NoSuchProfile(ProfileIndex),
    /// The profile exists but is switched off.
    #[error("profile {0} is switched off")]
    ProfileDisabled(ProfileIndex),
    /// An OpenLogi agent (or another program) has the mouse in host mode.
    #[error("the mouse is in host mode; stop the OpenLogi agent and power-cycle the mouse first")]
    HostMode,
    /// The sector is not a profile we understand.
    #[error(transparent)]
    Profile(#[from] ProfileError),
    /// The guarded write refused or failed.
    #[error(transparent)]
    Write(#[from] GuardedWriteError),
}

/// The sector holding profile `index`, from the mouse's directory.
pub fn profile_sector_for(
    entries: &[ProfileEntry],
    index: ProfileIndex,
) -> Result<u16, ProfileEditError> {
    let entry = entries
        .iter()
        .find(|entry| entry.index == index.get())
        .ok_or(ProfileEditError::NoSuchProfile(index))?;
    if entry.enabled {
        Ok(entry.sector)
    } else {
        Err(ProfileEditError::ProfileDisabled(index))
    }
}

/// Every profile sector the directory lists, enabled or not, leaving out
/// anything in the ROM area. This is the whole of what may ever be written.
#[must_use]
pub fn listed_profile_sectors(entries: &[ProfileEntry]) -> Vec<u16> {
    entries
        .iter()
        .map(|entry| entry.sector)
        .filter(|&sector| sector != 0 && sector < ROM_AREA)
        .collect()
}

/// Flash is only written while the mouse runs its own profiles: in host mode
/// something else owns it.
pub fn require_onboard_mode(mode: OnboardMode) -> Result<(), ProfileEditError> {
    match mode {
        OnboardMode::Onboard => Ok(()),
        OnboardMode::Host => Err(ProfileEditError::HostMode),
    }
}

/// The first sector of the ROM area, as in `profile_io`.
const ROM_AREA: u16 = 0x0100;

/// What a read-only look at one profile found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileDump {
    /// The profile asked for.
    pub index: ProfileIndex,
    /// The flash sector it lives in.
    pub sector: u16,
    /// The size of every sector on this mouse.
    pub sector_size: usize,
    /// Whether the mouse runs its own profiles or is owned by a program.
    pub mode: OnboardMode,
    /// The sector's bytes, as read.
    pub bytes: Vec<u8>,
}

/// Read profile `index` without writing anything.
pub async fn dump_profile(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
    index: ProfileIndex,
) -> Result<ProfileDump, ProfileEditError> {
    let device_index = route.device_index();
    with_route(backend, route, move |channel| async move {
        Ok(dump_on(&channel, device_index, index).await)
    })
    .await?
}

async fn dump_on(
    channel: &Arc<HidppChannel>,
    device_index: u8,
    index: ProfileIndex,
) -> Result<ProfileDump, ProfileEditError> {
    let (onboard, sector_size, entries) = open(channel, device_index).await?;
    let mode = mode_of(&onboard).await?;
    let sector = profile_sector_for(&entries, index)?;
    let bytes = profile_io::read_sector(&*onboard, sector, sector_size)
        .await
        .map_err(|error| ProfileEditError::Write(GuardedWriteError::Memory(error)))?;
    Ok(ProfileDump {
        index,
        sector,
        sector_size,
        mode,
        bytes,
    })
}

/// Replace profile `index` with `new`, through the guarded write: refused
/// unless the mouse runs its own profiles, and the sector is one the directory
/// lists. Returns the backup of what was there.
pub async fn write_profile(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
    index: ProfileIndex,
    new: &ProfileSector,
) -> Result<SectorBackup, ProfileEditError> {
    let device_index = route.device_index();
    with_route(backend, route, move |channel| async move {
        Ok(write_on(&channel, device_index, index, new).await)
    })
    .await?
}

async fn write_on(
    channel: &Arc<HidppChannel>,
    device_index: u8,
    index: ProfileIndex,
    new: &ProfileSector,
) -> Result<SectorBackup, ProfileEditError> {
    let (onboard, sector_size, entries) = open(channel, device_index).await?;
    require_onboard_mode(mode_of(&onboard).await?)?;
    let sector = profile_sector_for(&entries, index)?;
    let listed = listed_profile_sectors(&entries);
    Ok(profile_io::write_profile_sector(&*onboard, sector, sector_size, &listed, new).await?)
}

/// Put `backup` back, through the same guards.
pub async fn restore_profile(
    backend: &dyn HidBackend,
    route: &DeviceRoute,
    backup: &SectorBackup,
) -> Result<(), ProfileEditError> {
    let device_index = route.device_index();
    with_route(backend, route, move |channel| async move {
        Ok(restore_on(&channel, device_index, backup).await)
    })
    .await?
}

async fn restore_on(
    channel: &Arc<HidppChannel>,
    device_index: u8,
    backup: &SectorBackup,
) -> Result<(), ProfileEditError> {
    let (onboard, sector_size, entries) = open(channel, device_index).await?;
    require_onboard_mode(mode_of(&onboard).await?)?;
    let listed = listed_profile_sectors(&entries);
    Ok(profile_io::restore_backup(&*onboard, backup, sector_size, &listed).await?)
}

async fn open(
    channel: &Arc<HidppChannel>,
    index: u8,
) -> Result<(Arc<OnboardProfilesFeature>, usize, Vec<ProfileEntry>), WriteError> {
    let mut device = Device::new(Arc::clone(channel), index)
        .await
        .map_err(|_| WriteError::DeviceUnreachable { index })?;
    let onboard = open_feature::<OnboardProfilesFeature>(&mut device).await?;
    let err = |e| classify_hidpp_error(e, HidppOperation::ReadOnboard, 0x8100);
    let info = onboard.get_info().await.map_err(err)?;
    let entries = onboard.read_directory(&info).await.map_err(err)?;
    Ok((onboard, usize::from(info.sector_size), entries))
}

async fn mode_of(onboard: &OnboardProfilesFeature) -> Result<OnboardMode, WriteError> {
    let err = |e| classify_hidpp_error(e, HidppOperation::ReadOnboard, 0x8100);
    Ok(match onboard.get_mode().await.map_err(err)? {
        WireMode::Onboard => OnboardMode::Onboard,
        WireMode::Host => OnboardMode::Host,
    })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU8;

    use super::*;

    fn entry(index: u8, sector: u16, enabled: bool) -> ProfileEntry {
        ProfileEntry {
            index: NonZeroU8::new(index).unwrap(),
            sector,
            enabled,
        }
    }

    fn index(n: u8) -> ProfileIndex {
        ProfileIndex::new(n).unwrap()
    }

    /// As the real mouse lists them: five profiles, only the first enabled.
    fn directory() -> Vec<ProfileEntry> {
        vec![
            entry(1, 1, true),
            entry(2, 2, false),
            entry(3, 3, false),
            entry(4, 4, false),
            entry(5, 5, false),
        ]
    }

    #[test]
    fn an_enabled_profile_maps_to_its_sector() {
        assert_eq!(profile_sector_for(&directory(), index(1)).unwrap(), 1);
    }

    /// The sector follows the entry, not the number: a directory may list
    /// profiles out of order.
    #[test]
    fn the_sector_comes_from_the_entry_not_from_the_index() {
        let shuffled = vec![entry(2, 9, true), entry(1, 4, true)];
        assert_eq!(profile_sector_for(&shuffled, index(1)).unwrap(), 4);
        assert_eq!(profile_sector_for(&shuffled, index(2)).unwrap(), 9);
    }

    #[test]
    fn a_profile_the_directory_lacks_is_refused() {
        assert!(matches!(
            profile_sector_for(&directory(), index(6)),
            Err(ProfileEditError::NoSuchProfile(_))
        ));
        assert!(matches!(
            profile_sector_for(&[], index(1)),
            Err(ProfileEditError::NoSuchProfile(_))
        ));
    }

    #[test]
    fn a_switched_off_profile_is_refused() {
        assert!(matches!(
            profile_sector_for(&directory(), index(2)),
            Err(ProfileEditError::ProfileDisabled(_))
        ));
    }

    #[test]
    fn every_listed_profile_sector_counts_enabled_or_not() {
        assert_eq!(listed_profile_sectors(&directory()), vec![1, 2, 3, 4, 5]);
    }

    /// A blank user directory falls back to the factory one, whose sectors
    /// sit in the ROM area. None of those may be written.
    #[test]
    fn rom_area_sectors_are_never_listed_as_writable() {
        let factory = vec![
            entry(1, 0x0101, true),
            entry(2, 0x0102, true),
            entry(3, 3, true),
        ];
        assert_eq!(listed_profile_sectors(&factory), vec![3]);
    }

    #[test]
    fn the_directory_sector_itself_is_never_listed() {
        assert_eq!(
            listed_profile_sectors(&[entry(1, 0, true), entry(2, 2, true)]),
            vec![2]
        );
    }

    #[test]
    fn onboard_mode_may_be_written() {
        require_onboard_mode(OnboardMode::Onboard).unwrap();
    }

    #[test]
    fn host_mode_is_refused_with_advice() {
        let error = require_onboard_mode(OnboardMode::Host).unwrap_err();
        assert!(matches!(error, ProfileEditError::HostMode));
        assert!(error.to_string().contains("power-cycle"));
    }
}
