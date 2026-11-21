//! Reading and rewriting one onboard profile's sector on a real mouse, through
//! the guarded sequence in `hidpp::feature::onboard_profiles::profile_io`.
//!
//! The rules below are pure and tested. The device calls at the bottom are
//! thin glue over the tested pieces and are proven only on a mouse.

use std::path::Path;
use std::sync::Arc;

use hidpp::{
    channel::HidppChannel,
    device::Device,
    feature::onboard_profiles::{
        OnboardMode as WireMode, OnboardProfilesFeature, ProfileEntry,
        profile::ProfileError,
        profile_io::{self, ProfileMemory, WriteError as GuardedWriteError},
    },
};
use openlogi_core::hid::{
    Dpi, OnboardMode, ProfileDpi, ProfileDpiError, ProfileDpiSaved, ProfileIndex, ProfileLed,
    ProfileLedSaved,
};

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

/// The DPI stages stored in a profile sector read from the mouse.
pub fn profile_dpi(
    profile: ProfileIndex,
    bytes: &[u8],
    sector_size: usize,
) -> Result<ProfileDpi, ProfileDpiError> {
    let sector = ProfileSector::parse(bytes, sector_size)
        .map_err(|error| ProfileDpiError::Unsupported(error.to_string()))?;
    Ok(dpi_of(profile, &sector))
}

fn dpi_of(profile: ProfileIndex, sector: &ProfileSector) -> ProfileDpi {
    ProfileDpi {
        profile,
        stages: sector.presets().into_iter().map(Dpi::new).collect(),
        default_stage: sector.default_stage(),
    }
}

/// The LED setting stored in a profile sector read from the mouse.
pub fn profile_led(
    profile: ProfileIndex,
    bytes: &[u8],
    sector_size: usize,
) -> Result<ProfileLed, ProfileDpiError> {
    let sector = ProfileSector::parse(bytes, sector_size)
        .map_err(|error| ProfileDpiError::Unsupported(error.to_string()))?;
    led_of(profile, &sector)
}

fn led_of(profile: ProfileIndex, sector: &ProfileSector) -> Result<ProfileLed, ProfileDpiError> {
    match (sector.led_color(), sector.led_brightness()) {
        (Some(color), Some(brightness)) => Ok(ProfileLed {
            profile,
            color,
            brightness,
        }),
        _ => Err(ProfileDpiError::Unsupported(
            "this profile's LED is not one fixed colour".to_owned(),
        )),
    }
}

/// Where and how a profile's DPI stages are saved.
pub struct SaveTarget<'a> {
    /// The profile being saved.
    pub profile: ProfileIndex,
    /// Its flash sector, from the mouse's directory.
    pub sector: u16,
    /// The size of every sector on this mouse.
    pub sector_size: usize,
    /// Every profile sector the directory lists: the only ones ever written.
    pub listed: &'a [u16],
    /// Who owns the mouse now; only its own profiles are rewritten.
    pub mode: OnboardMode,
    /// Where the backup of the profile as it was is saved first.
    pub backup_dir: &'a Path,
    /// The device's name, for the backup file's name.
    pub device_name: &'a str,
    /// The time, for the backup file's name.
    pub now_secs: u64,
}

/// Save `stages` (one to five) and `default` into the profile `target` names.
///
/// Order: host mode is refused; the sector is read and must be a valid
/// profile; the new bytes are worked out (nothing is written when they equal
/// the old); the old bytes are saved to a backup file and read back from disk;
/// only then is the sector written, read back and, if it did not land, put
/// back. The result is read from the mouse afterwards.
pub async fn save_dpi_with(
    memory: &dyn ProfileMemory,
    target: &SaveTarget<'_>,
    stages: &[Dpi],
    default: u8,
) -> Result<ProfileDpiSaved, ProfileDpiError> {
    let raw: Vec<u16> = stages.iter().map(|stage| stage.into_inner()).collect();
    let (after, backup) = save_edit_with(memory, target, |parsed| {
        parsed.with_dpi_stages(&raw, default)
    })
    .await?;
    Ok(ProfileDpiSaved {
        saved: dpi_of(target.profile, &after),
        backup,
    })
}

/// Save a fixed LED `color` and `brightness` (percent) into the profile
/// `target` names, through the same guards as [`save_dpi_with`]. A profile
/// whose LED is not one fixed colour is left alone.
pub async fn save_led_with(
    memory: &dyn ProfileMemory,
    target: &SaveTarget<'_>,
    color: [u8; 3],
    brightness: u8,
) -> Result<ProfileLedSaved, ProfileDpiError> {
    let (after, backup) = save_edit_with(memory, target, |parsed| {
        parsed
            .with_led_color(color)?
            .with_led_brightness(brightness)
    })
    .await?;
    Ok(ProfileLedSaved {
        saved: led_of(target.profile, &after)?,
        backup,
    })
}

/// The shared guarded save: refuse host mode, read and parse the sector, work
/// out the new bytes with `edit`, and (unless they equal the old) save a
/// verified backup, write, read back and restore on a mismatch. Returns the
/// profile as the mouse holds it afterwards and the backup file's path (empty
/// when nothing was written).
async fn save_edit_with(
    memory: &dyn ProfileMemory,
    target: &SaveTarget<'_>,
    edit: impl FnOnce(&ProfileSector) -> Result<ProfileSector, ProfileError>,
) -> Result<(ProfileSector, String), ProfileDpiError> {
    if target.mode == OnboardMode::Host {
        return Err(ProfileDpiError::HostMode);
    }
    let current = profile_io::read_sector(memory, target.sector, target.sector_size)
        .await
        .map_err(|error| ProfileDpiError::Device(memory_failure(&error)))?;
    let parsed = ProfileSector::parse(&current, target.sector_size)
        .map_err(|error| ProfileDpiError::Unsupported(error.to_string()))?;
    let new = edit(&parsed).map_err(edit_refusal)?;
    if new == parsed {
        return Ok((parsed, String::new()));
    }

    let backup = SectorBackup {
        sector: target.sector,
        bytes: current,
    };
    let file = super::profile_backup::backup_file_name(
        target.device_name,
        target.profile.get().get(),
        target.now_secs,
    );
    let path = super::profile_backup::save_and_verify_backup(target.backup_dir, &file, &backup)
        .map_err(ProfileDpiError::BackupFailed)?;
    let path = path.display().to_string();

    match profile_io::write_profile_sector(
        memory,
        target.sector,
        target.sector_size,
        target.listed,
        &new,
    )
    .await
    {
        Ok(_) => {}
        Err(GuardedWriteError::Refused(error)) => {
            return Err(ProfileDpiError::Unsupported(error.to_string()));
        }
        Err(GuardedWriteError::Memory(error)) => {
            return Err(ProfileDpiError::Failed {
                reason: error.to_string(),
                restored: true,
                backup: path,
            });
        }
        Err(GuardedWriteError::Failed {
            reason, restored, ..
        }) => {
            return Err(ProfileDpiError::Failed {
                reason,
                restored,
                backup: path,
            });
        }
    }

    let after = profile_io::read_sector(memory, target.sector, target.sector_size)
        .await
        .map_err(|error| ProfileDpiError::Failed {
            reason: format!("the profile was written but could not be read back: {error}"),
            restored: false,
            backup: path.clone(),
        })?;
    let after = ProfileSector::parse(&after, target.sector_size)
        .map_err(|error| ProfileDpiError::Unsupported(error.to_string()))?;
    Ok((after, path))
}

fn edit_refusal(error: ProfileError) -> ProfileDpiError {
    match error {
        ProfileError::SplitAxes(_) | ProfileError::LedNotFixed(_) | ProfileError::NoLedSetting => {
            ProfileDpiError::Unsupported(error.to_string())
        }
        other => ProfileDpiError::Invalid(other.to_string()),
    }
}

fn memory_failure(error: &profile_io::MemoryError) -> WriteError {
    WriteError::Hid(error.to_string())
}

/// Read the DPI stages of profile `profile` on an open channel. Read-only.
pub async fn read_profile_dpi_on(
    shared: &crate::SharedChannel,
    profile: ProfileIndex,
) -> Result<ProfileDpi, ProfileDpiError> {
    let (onboard, sector_size, entries) = open(shared.channel(), shared.device_index())
        .await
        .map_err(ProfileDpiError::Device)?;
    let sector = sector_of(&entries, profile)?;
    let bytes = profile_io::read_sector(&*onboard, sector, sector_size)
        .await
        .map_err(|error| ProfileDpiError::Device(memory_failure(&error)))?;
    profile_dpi(profile, &bytes, sector_size)
}

/// Save DPI `stages` and `default` into profile `profile` on an open channel,
/// through [`save_dpi_with`]'s guards. `backup_dir` and `device_name` name the
/// backup file kept of the profile as it was.
pub async fn save_profile_dpi_on(
    shared: &crate::SharedChannel,
    profile: ProfileIndex,
    stages: &[Dpi],
    default: u8,
    backup_dir: &Path,
    device_name: &str,
    now_secs: u64,
) -> Result<ProfileDpiSaved, ProfileDpiError> {
    let (onboard, sector_size, entries) = open(shared.channel(), shared.device_index())
        .await
        .map_err(ProfileDpiError::Device)?;
    let mode = mode_of(&onboard).await.map_err(ProfileDpiError::Device)?;
    let sector = sector_of(&entries, profile)?;
    let listed = listed_profile_sectors(&entries);
    let target = SaveTarget {
        profile,
        sector,
        sector_size,
        listed: &listed,
        mode,
        backup_dir,
        device_name,
        now_secs,
    };
    save_dpi_with(&*onboard, &target, stages, default).await
}

/// Read the stored LED setting of profile `profile` on an open channel.
/// Read-only.
pub async fn read_profile_led_on(
    shared: &crate::SharedChannel,
    profile: ProfileIndex,
) -> Result<ProfileLed, ProfileDpiError> {
    let (onboard, sector_size, entries) = open(shared.channel(), shared.device_index())
        .await
        .map_err(ProfileDpiError::Device)?;
    let sector = sector_of(&entries, profile)?;
    let bytes = profile_io::read_sector(&*onboard, sector, sector_size)
        .await
        .map_err(|error| ProfileDpiError::Device(memory_failure(&error)))?;
    profile_led(profile, &bytes, sector_size)
}

/// Save a fixed LED `color` and `brightness` into profile `profile` on an open
/// channel, through [`save_led_with`]'s guards.
pub async fn save_profile_led_on(
    shared: &crate::SharedChannel,
    profile: ProfileIndex,
    color: [u8; 3],
    brightness: u8,
    backup_dir: &Path,
    device_name: &str,
    now_secs: u64,
) -> Result<ProfileLedSaved, ProfileDpiError> {
    let (onboard, sector_size, entries) = open(shared.channel(), shared.device_index())
        .await
        .map_err(ProfileDpiError::Device)?;
    let mode = mode_of(&onboard).await.map_err(ProfileDpiError::Device)?;
    let sector = sector_of(&entries, profile)?;
    let listed = listed_profile_sectors(&entries);
    let target = SaveTarget {
        profile,
        sector,
        sector_size,
        listed: &listed,
        mode,
        backup_dir,
        device_name,
        now_secs,
    };
    save_led_with(&*onboard, &target, color, brightness).await
}

fn sector_of(entries: &[ProfileEntry], profile: ProfileIndex) -> Result<u16, ProfileDpiError> {
    profile_sector_for(entries, profile).map_err(|error| match error {
        ProfileEditError::NoSuchProfile(index) => ProfileDpiError::NoSuchProfile(index),
        ProfileEditError::ProfileDisabled(index) => ProfileDpiError::ProfileDisabled(index),
        other => ProfileDpiError::Invalid(other.to_string()),
    })
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

    // -- saving DPI stages ---------------------------------------------------

    use std::sync::Mutex;

    use async_trait::async_trait;
    use hidpp::feature::onboard_profiles::profile::crc16_ccitt;
    use hidpp::feature::onboard_profiles::profile_io::{CHUNK, MemoryError, ProfileMemory};
    use openlogi_core::hid::{Dpi, ProfileDpiError, ProfileLed};

    const SIZE: usize = 255;

    /// A valid profile sector with the given stages (both axes) and default.
    fn sector_bytes(stages: [u16; 5], default: u8) -> Vec<u8> {
        let mut bytes = vec![0_u8; SIZE];
        bytes[0] = 3;
        bytes[1] = default;
        for (i, stage) in stages.iter().enumerate() {
            let at = 4 + i * 5;
            bytes[at..at + 2].copy_from_slice(&stage.to_le_bytes());
            bytes[at + 2..at + 4].copy_from_slice(&stage.to_le_bytes());
            bytes[at + 4] = 2;
        }
        let end = SIZE - 2;
        let crc = crc16_ccitt(&bytes[..end]);
        bytes[end..].copy_from_slice(&crc.to_be_bytes());
        bytes
    }

    /// A mouse with one sector, which can be made to fail a write.
    struct Mouse {
        flash: Mutex<Vec<u8>>,
        writes: Mutex<u32>,
        fail_writes: bool,
        open: Mutex<Option<(usize, usize)>>,
    }

    impl Mouse {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                flash: Mutex::new(bytes),
                writes: Mutex::new(0),
                fail_writes: false,
                open: Mutex::new(None),
            }
        }
        fn bytes(&self) -> Vec<u8> {
            self.flash.lock().unwrap().clone()
        }
        fn write_count(&self) -> u32 {
            *self.writes.lock().unwrap()
        }
    }

    #[async_trait]
    impl ProfileMemory for Mouse {
        async fn read(&self, _sector: u16, offset: u16) -> Result<[u8; CHUNK], MemoryError> {
            let flash = self.flash.lock().unwrap();
            let at = usize::from(offset);
            let mut chunk = [0; CHUNK];
            chunk.copy_from_slice(&flash[at..at + CHUNK]);
            Ok(chunk)
        }
        async fn begin_write(&self, _: u16, offset: u16, len: u16) -> Result<(), MemoryError> {
            *self.writes.lock().unwrap() += 1;
            if self.fail_writes {
                return Err(MemoryError("refused".into()));
            }
            *self.open.lock().unwrap() = Some((usize::from(offset), usize::from(len)));
            Ok(())
        }
        async fn write_chunk(&self, chunk: [u8; CHUNK]) -> Result<(), MemoryError> {
            let mut open = self.open.lock().unwrap();
            let (at, len) = open
                .as_mut()
                .ok_or_else(|| MemoryError("no write".into()))?;
            let mut flash = self.flash.lock().unwrap();
            let n = CHUNK.min(*len);
            flash[*at..*at + n].copy_from_slice(&chunk[..n]);
            *at += n;
            *len -= n;
            Ok(())
        }
        async fn end_write(&self) -> Result<(), MemoryError> {
            *self.open.lock().unwrap() = None;
            Ok(())
        }
    }

    fn dpis(values: &[u16]) -> Vec<Dpi> {
        values.iter().map(|&v| Dpi::new(v)).collect()
    }

    fn save_args(dir: &std::path::Path) -> SaveTarget<'_> {
        SaveTarget {
            profile: index(1),
            sector: 1,
            sector_size: SIZE,
            listed: &[1, 2, 3, 4, 5],
            mode: OnboardMode::Onboard,
            backup_dir: dir,
            device_name: "PRO 2 LIGHTSPEED",
            now_secs: 7,
        }
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn two_stages_are_written_read_back_and_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let original = sector_bytes([400, 800, 1600, 3200, 6000], 3);
        let mouse = Mouse::new(original.clone());
        let saved = block_on(save_dpi_with(
            &mouse,
            &save_args(dir.path()),
            &dpis(&[800, 3200]),
            0,
        ))
        .unwrap();
        assert_eq!(saved.saved.stages, dpis(&[800, 3200]));
        assert_eq!(saved.saved.default_stage, 0);
        assert_eq!(saved.saved.profile, index(1));
        // the mouse holds a valid two-stage profile
        let now = ProfileSector::parse(&mouse.bytes(), SIZE).unwrap();
        assert_eq!(now.dpi_stages(), [800, 3200, 0, 0, 0]);
        // the original is on disk, exactly
        let backup =
            super::super::profile_backup::load_backup(std::path::Path::new(&saved.backup)).unwrap();
        assert_eq!(backup.bytes, original);
    }

    #[test]
    fn host_mode_is_refused_before_anything_is_read_or_written() {
        let dir = tempfile::tempdir().unwrap();
        let mouse = Mouse::new(sector_bytes([400, 800, 1600, 3200, 6000], 3));
        let mut target = save_args(dir.path());
        target.mode = OnboardMode::Host;
        let error = block_on(save_dpi_with(&mouse, &target, &dpis(&[800, 3200]), 0)).unwrap_err();
        assert_eq!(error, ProfileDpiError::HostMode);
        assert_eq!(mouse.write_count(), 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_unchanged_save_writes_nothing_and_saves_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let mouse = Mouse::new(sector_bytes([800, 3200, 0, 0, 0], 0));
        let saved = block_on(save_dpi_with(
            &mouse,
            &save_args(dir.path()),
            &dpis(&[800, 3200]),
            0,
        ))
        .unwrap();
        assert_eq!(saved.backup, "");
        assert_eq!(mouse.write_count(), 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn stages_that_cannot_be_stored_are_refused_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let mouse = Mouse::new(sector_bytes([400, 800, 1600, 3200, 6000], 3));
        for (stages, default) in [
            (dpis(&[]), 0),
            (dpis(&[1, 2, 3, 4, 5, 6]), 0),
            (dpis(&[800, 0]), 0),
            (dpis(&[800, 3200]), 2),
        ] {
            let error = block_on(save_dpi_with(
                &mouse,
                &save_args(dir.path()),
                &stages,
                default,
            ))
            .unwrap_err();
            assert!(
                matches!(error, ProfileDpiError::Invalid(_)),
                "{stages:?}: {error:?}"
            );
        }
        assert_eq!(mouse.write_count(), 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_profile_that_is_not_understood_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut blank = vec![0xff_u8; SIZE];
        blank[0] = 0xff;
        let mouse = Mouse::new(blank);
        let error = block_on(save_dpi_with(
            &mouse,
            &save_args(dir.path()),
            &dpis(&[800]),
            0,
        ))
        .unwrap_err();
        assert!(
            matches!(error, ProfileDpiError::Unsupported(_)),
            "{error:?}"
        );
        assert_eq!(mouse.write_count(), 0);
    }

    #[test]
    fn a_profile_with_split_axes_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = sector_bytes([400, 800, 1600, 3200, 6000], 3);
        bytes[4 + 2 * 5 + 2..4 + 2 * 5 + 4].copy_from_slice(&1200_u16.to_le_bytes());
        let end = SIZE - 2;
        let crc = crc16_ccitt(&bytes[..end]);
        bytes[end..].copy_from_slice(&crc.to_be_bytes());
        let mouse = Mouse::new(bytes);
        let error = block_on(save_dpi_with(
            &mouse,
            &save_args(dir.path()),
            &dpis(&[800]),
            0,
        ))
        .unwrap_err();
        assert!(
            matches!(error, ProfileDpiError::Unsupported(_)),
            "{error:?}"
        );
        assert_eq!(mouse.write_count(), 0);
    }

    /// No backup, no write: a backup that cannot be saved stops everything.
    #[test]
    fn a_backup_that_cannot_be_saved_stops_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let mouse = Mouse::new(sector_bytes([400, 800, 1600, 3200, 6000], 3));
        let mut target = save_args(&blocker);
        target.backup_dir = &blocker;
        let error = block_on(save_dpi_with(&mouse, &target, &dpis(&[800, 3200]), 0)).unwrap_err();
        assert!(
            matches!(error, ProfileDpiError::BackupFailed(_)),
            "{error:?}"
        );
        assert_eq!(mouse.write_count(), 0);
    }

    #[test]
    fn a_refused_write_reports_the_backup_that_was_kept() {
        let dir = tempfile::tempdir().unwrap();
        let original = sector_bytes([400, 800, 1600, 3200, 6000], 3);
        let mut mouse = Mouse::new(original.clone());
        mouse.fail_writes = true;
        let error = block_on(save_dpi_with(
            &mouse,
            &save_args(dir.path()),
            &dpis(&[800, 3200]),
            0,
        ))
        .unwrap_err();
        let ProfileDpiError::Failed { backup, .. } = error else {
            panic!("expected Failed, got {error:?}");
        };
        assert!(!backup.is_empty());
        assert_eq!(mouse.bytes(), original);
    }

    #[test]
    fn a_sector_the_directory_does_not_list_is_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let mouse = Mouse::new(sector_bytes([400, 800, 1600, 3200, 6000], 3));
        let mut target = save_args(dir.path());
        target.listed = &[2, 3];
        let error = block_on(save_dpi_with(&mouse, &target, &dpis(&[800, 3200]), 0)).unwrap_err();
        assert!(
            matches!(error, ProfileDpiError::Unsupported(_)),
            "{error:?}"
        );
        assert_eq!(mouse.write_count(), 0);
    }

    #[test]
    fn reading_a_profile_gives_its_stages_in_use() {
        let bytes = sector_bytes([800, 3200, 0, 0, 0], 1);
        let read = profile_dpi(index(1), &bytes, SIZE).unwrap();
        assert_eq!(read.stages, dpis(&[800, 3200]));
        assert_eq!(read.default_stage, 1);
    }

    #[test]
    fn reading_a_profile_that_is_not_understood_says_so() {
        assert!(matches!(
            profile_dpi(index(1), &[0xff; SIZE], SIZE),
            Err(ProfileDpiError::Unsupported(_))
        ));
    }

    // -- the stored LED ---------------------------------------------------

    /// `sector_bytes` with the LED set to one fixed colour at `brightness`.
    fn lit_sector(color: [u8; 3], brightness: u8) -> Vec<u8> {
        let mut bytes = sector_bytes([800, 3200, 0, 0, 0], 0);
        bytes[0xd0] = 1;
        bytes[0xd1..0xd4].copy_from_slice(&color);
        bytes[0xd8] = brightness;
        let end = SIZE - 2;
        let crc = crc16_ccitt(&bytes[..end]);
        bytes[end..].copy_from_slice(&crc.to_be_bytes());
        bytes
    }

    #[test]
    fn a_stored_led_is_read_back_as_colour_and_brightness() {
        assert_eq!(
            profile_led(index(1), &lit_sector([1, 2, 3], 40), SIZE),
            Ok(ProfileLed {
                profile: index(1),
                color: [1, 2, 3],
                brightness: 40
            })
        );
    }

    #[test]
    fn a_profile_whose_led_is_not_one_fixed_colour_cannot_be_read_as_one() {
        assert!(matches!(
            profile_led(index(1), &sector_bytes([800, 0, 0, 0, 0], 0), SIZE),
            Err(ProfileDpiError::Unsupported(_))
        ));
    }

    #[test]
    fn a_new_led_is_written_read_back_and_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let original = lit_sector([0x94, 0xe6, 0xff], 0);
        let mouse = Mouse::new(original.clone());
        let saved = block_on(save_led_with(
            &mouse,
            &save_args(dir.path()),
            [0, 255, 0],
            30,
        ))
        .unwrap();
        assert_eq!(saved.saved.color, [0, 255, 0]);
        assert_eq!(saved.saved.brightness, 30);
        let now = ProfileSector::parse(&mouse.bytes(), SIZE).unwrap();
        assert_eq!(now.led_color(), Some([0, 255, 0]));
        // the DPI stages are untouched
        assert_eq!(now.dpi_stages(), [800, 3200, 0, 0, 0]);
        let backup =
            super::super::profile_backup::load_backup(std::path::Path::new(&saved.backup)).unwrap();
        assert_eq!(backup.bytes, original);
    }

    #[test]
    fn an_unchanged_led_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mouse = Mouse::new(lit_sector([5, 6, 7], 20));
        let saved = block_on(save_led_with(&mouse, &save_args(dir.path()), [5, 6, 7], 20)).unwrap();
        assert_eq!(saved.backup, "");
        assert_eq!(mouse.write_count(), 0);
    }

    #[test]
    fn the_led_is_not_saved_in_host_mode_or_over_another_effect() {
        let dir = tempfile::tempdir().unwrap();
        let mouse = Mouse::new(lit_sector([5, 6, 7], 20));
        let mut target = save_args(dir.path());
        target.mode = OnboardMode::Host;
        assert_eq!(
            block_on(save_led_with(&mouse, &target, [1, 1, 1], 5)).unwrap_err(),
            ProfileDpiError::HostMode
        );
        let other = Mouse::new(sector_bytes([800, 3200, 0, 0, 0], 0));
        let error =
            block_on(save_led_with(&other, &save_args(dir.path()), [1, 1, 1], 5)).unwrap_err();
        assert!(
            matches!(error, ProfileDpiError::Unsupported(_)),
            "{error:?}"
        );
        assert_eq!(mouse.write_count() + other.write_count(), 0);
    }

    #[test]
    fn a_brightness_above_a_hundred_is_refused_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let mouse = Mouse::new(lit_sector([5, 6, 7], 20));
        let error = block_on(save_led_with(
            &mouse,
            &save_args(dir.path()),
            [5, 6, 7],
            101,
        ))
        .unwrap_err();
        assert!(matches!(error, ProfileDpiError::Invalid(_)), "{error:?}");
        assert_eq!(mouse.write_count(), 0);
    }
}
