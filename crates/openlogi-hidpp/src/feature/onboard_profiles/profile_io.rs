//! Writing one profile sector to a mouse, with the safeguards that make a
//! flash write recoverable: only sectors the directory lists, nothing written
//! over a sector we cannot read as a valid profile, a backup taken first, a
//! read-back after, and an automatic restore if the read-back disagrees.
//!
//! The sequence (begin write, 16-byte chunks, end write) is libratbag's
//! description of HID++ `0x8100` functions 6, 7 and 8 and has NOT been tried on
//! a mouse; the tests run it against a fake that enforces the order. Nothing in
//! the app calls this yet.

use async_trait::async_trait;

use super::profile::{
    ProfileError, ProfileSector, SectorBackup, check_writable_sector, verify_written,
};

/// Bytes moved by one memory read or write.
pub const CHUNK: usize = 16;

/// Something that went wrong talking to the mouse, as text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct MemoryError(pub String);

/// The mouse's profile memory, as the write sequence sees it.
#[async_trait]
pub trait ProfileMemory: Send + Sync {
    /// Read the 16 bytes at `offset` in `sector`.
    async fn read(&self, sector: u16, offset: u16) -> Result<[u8; CHUNK], MemoryError>;
    /// Start writing `len` bytes at `offset` in `sector`.
    async fn begin_write(&self, sector: u16, offset: u16, len: u16) -> Result<(), MemoryError>;
    /// Send the next 16 bytes of the write that was begun.
    async fn write_chunk(&self, chunk: [u8; CHUNK]) -> Result<(), MemoryError>;
    /// Finish the write that was begun.
    async fn end_write(&self) -> Result<(), MemoryError>;
}

/// Why a sector was not written, or not left written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    /// Refused before anything was written.
    #[error(transparent)]
    Refused(#[from] ProfileError),
    /// Talking to the mouse failed before anything was written.
    #[error("could not talk to the mouse: {0}")]
    Memory(MemoryError),
    /// The sector is not what the caller read earlier, so something else
    /// wrote it meanwhile; nothing was written over that.
    #[error("the profile changed on the mouse after it was read, so nothing was written")]
    Changed,
    /// The write went wrong; `restored` says whether the original is back.
    #[error("the write did not land ({reason}); original restored: {restored}")]
    Failed {
        /// What went wrong.
        reason: String,
        /// Whether the backup was written back and verified.
        restored: bool,
        /// The sector as it was before, for the caller to keep.
        backup: SectorBackup,
    },
}

/// Read all of `sector`, `sector_size` bytes: whole chunks, then the last
/// chunk aligned to the end so the final bytes are read too.
pub async fn read_sector(
    memory: &dyn ProfileMemory,
    sector: u16,
    sector_size: usize,
) -> Result<Vec<u8>, MemoryError> {
    let tail_at = sector_size
        .checked_sub(CHUNK)
        .ok_or_else(|| MemoryError(format!("sector size {sector_size} is under {CHUNK}")))?;
    let mut bytes = Vec::with_capacity(sector_size);
    for offset in (0..tail_at).step_by(CHUNK) {
        bytes.extend_from_slice(&memory.read(sector, offset_of(offset)?).await?);
    }
    bytes.truncate(tail_at);
    bytes.extend_from_slice(&memory.read(sector, offset_of(tail_at)?).await?);
    Ok(bytes)
}

fn offset_of(offset: usize) -> Result<u16, MemoryError> {
    u16::try_from(offset).map_err(|_| MemoryError(format!("offset {offset} does not fit")))
}

/// Replace profile `sector` with `new`, returning the backup of what was
/// there. Writes nothing unless every safeguard passes.
///
/// The order matters: the sector must be listed, `new` must be the mouse's
/// sector size, the sector as it is must parse as a valid profile (a blank or
/// unreadable one is not understood, so it is left alone), and only then is it
/// backed up and written. The write is read back; if it did not land, the
/// backup is written back and checked the same way.
pub async fn write_profile_sector(
    memory: &dyn ProfileMemory,
    sector: u16,
    sector_size: usize,
    profile_sectors: &[u16],
    new: &ProfileSector,
) -> Result<SectorBackup, WriteError> {
    write_checked(memory, sector, sector_size, profile_sectors, None, new).await
}

/// [`write_profile_sector`] for an edit made from bytes the caller read
/// earlier: the sector is read again just before the write, and when it is not
/// `expected` (another program, such as Logitech's own software, wrote it in
/// between) nothing is written and the answer is [`WriteError::Changed`].
pub async fn write_profile_sector_if_unchanged(
    memory: &dyn ProfileMemory,
    sector: u16,
    sector_size: usize,
    profile_sectors: &[u16],
    expected: &[u8],
    new: &ProfileSector,
) -> Result<SectorBackup, WriteError> {
    write_checked(
        memory,
        sector,
        sector_size,
        profile_sectors,
        Some(expected),
        new,
    )
    .await
}

async fn write_checked(
    memory: &dyn ProfileMemory,
    sector: u16,
    sector_size: usize,
    profile_sectors: &[u16],
    expected: Option<&[u8]>,
    new: &ProfileSector,
) -> Result<SectorBackup, WriteError> {
    check_writable_sector(sector, profile_sectors)?;
    check_size(new.as_bytes(), sector_size)?;
    let current = read_sector(memory, sector, sector_size)
        .await
        .map_err(WriteError::Memory)?;
    ProfileSector::parse(&current, sector_size)?;
    if expected.is_some_and(|expected| expected != current) {
        return Err(WriteError::Changed);
    }
    let backup = SectorBackup {
        sector,
        bytes: current,
    };
    if backup.bytes == new.as_bytes() {
        return Ok(backup);
    }
    if let Err(reason) = write_and_verify(memory, sector, sector_size, new.as_bytes()).await {
        let restored = write_and_verify(memory, sector, sector_size, &backup.bytes)
            .await
            .is_ok();
        return Err(WriteError::Failed {
            reason,
            restored,
            backup,
        });
    }
    Ok(backup)
}

/// Put `backup` back, with the same safeguards and the same read-back. The
/// sector as it is now is not checked: restoring over a damaged profile is the
/// point. The backup itself must be a valid profile of the right size.
pub async fn restore_backup(
    memory: &dyn ProfileMemory,
    backup: &SectorBackup,
    sector_size: usize,
    profile_sectors: &[u16],
) -> Result<(), WriteError> {
    check_writable_sector(backup.sector, profile_sectors)?;
    check_size(&backup.bytes, sector_size)?;
    ProfileSector::parse(&backup.bytes, sector_size)?;
    write_and_verify(memory, backup.sector, sector_size, &backup.bytes)
        .await
        .map_err(|reason| WriteError::Failed {
            reason,
            restored: false,
            backup: backup.clone(),
        })
}

fn check_size(bytes: &[u8], sector_size: usize) -> Result<(), ProfileError> {
    if bytes.len() == sector_size {
        Ok(())
    } else {
        Err(ProfileError::WrongLength {
            expected: sector_size,
            found: bytes.len(),
        })
    }
}

/// One whole-sector write at offset 0, then a read-back that must match.
async fn write_and_verify(
    memory: &dyn ProfileMemory,
    sector: u16,
    sector_size: usize,
    bytes: &[u8],
) -> Result<(), String> {
    let len = u16::try_from(bytes.len()).map_err(|e| e.to_string())?;
    memory
        .begin_write(sector, 0, len)
        .await
        .map_err(|e| e.to_string())?;
    for chunk in bytes.chunks(CHUNK) {
        // The last chunk of an odd-sized sector is padded; the mouse is told
        // the real length in `begin_write` and takes only that many bytes.
        let mut padded = [0xff; CHUNK];
        padded[..chunk.len()].copy_from_slice(chunk);
        memory
            .write_chunk(padded)
            .await
            .map_err(|e| e.to_string())?;
    }
    memory.end_write().await.map_err(|e| e.to_string())?;
    let read_back = read_sector(memory, sector, sector_size)
        .await
        .map_err(|e| e.to_string())?;
    verify_written(bytes, &read_back).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::executor::block_on;

    use super::*;
    use crate::feature::onboard_profiles::profile::crc16_ccitt;

    const SECTOR_SIZE: usize = 255;
    const PROFILE_SECTORS: [u16; 5] = [1, 2, 3, 4, 5];
    const SECTOR_COUNT: u16 = 16;

    /// A profile whose stage 0 is `dpi`, sealed with a valid checksum.
    fn profile(dpi: u16) -> ProfileSector {
        let mut bytes: Vec<u8> = (0..SECTOR_SIZE)
            .map(|i| u8::try_from(i % 241).unwrap() ^ 0x33)
            .collect();
        bytes[4..6].copy_from_slice(&dpi.to_le_bytes());
        bytes[6..8].copy_from_slice(&dpi.to_le_bytes());
        let end = SECTOR_SIZE - 2;
        let crc = crc16_ccitt(&bytes[..end]);
        bytes[end..].copy_from_slice(&crc.to_be_bytes());
        ProfileSector::parse(&bytes, SECTOR_SIZE).unwrap()
    }

    /// Everything the fake mouse did, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        Read(u16, u16),
        Begin(u16, u16, u16),
        Chunk,
        End,
    }

    #[derive(Default)]
    struct Open {
        sector: u16,
        offset: usize,
        len: usize,
        received: Vec<u8>,
    }

    struct Fake {
        state: Mutex<FakeState>,
    }

    struct FakeState {
        memory: Vec<Vec<u8>>,
        open: Option<Open>,
        events: Vec<Event>,
        writes_started: usize,
        /// Flip a bit in the sector after this many writes finish (0 = the
        /// first), to model a write that lands wrongly.
        corrupt_after_write: Option<usize>,
        /// Fail the `n`th chunk (counting across the run) with an error.
        fail_chunk: Option<usize>,
        chunks_seen: usize,
        /// Fail every write once this many writes have begun.
        fail_all_writes_from: Option<usize>,
    }

    impl Fake {
        fn holding(sector: u16, bytes: &[u8]) -> Self {
            let mut memory = vec![vec![0xff; SECTOR_SIZE]; usize::from(SECTOR_COUNT)];
            memory[usize::from(sector)] = bytes.to_vec();
            Self {
                state: Mutex::new(FakeState {
                    memory,
                    open: None,
                    events: Vec::new(),
                    writes_started: 0,
                    corrupt_after_write: None,
                    fail_chunk: None,
                    chunks_seen: 0,
                    fail_all_writes_from: None,
                }),
            }
        }

        fn sector(&self, sector: u16) -> Vec<u8> {
            self.state.lock().unwrap().memory[usize::from(sector)].clone()
        }

        fn events(&self) -> Vec<Event> {
            self.state.lock().unwrap().events.clone()
        }

        fn writes_begun(&self) -> usize {
            self.state.lock().unwrap().writes_started
        }

        fn with(&self, f: impl FnOnce(&mut FakeState)) {
            f(&mut self.state.lock().unwrap());
        }
    }

    #[async_trait]
    impl ProfileMemory for Fake {
        async fn read(&self, sector: u16, offset: u16) -> Result<[u8; CHUNK], MemoryError> {
            let mut state = self.state.lock().unwrap();
            state.events.push(Event::Read(sector, offset));
            let data = &state.memory[usize::from(sector)];
            let start = usize::from(offset);
            if start + CHUNK > data.len() {
                return Err(MemoryError("read past the end of the sector".into()));
            }
            Ok(data[start..start + CHUNK].try_into().unwrap())
        }

        async fn begin_write(&self, sector: u16, offset: u16, len: u16) -> Result<(), MemoryError> {
            let mut state = self.state.lock().unwrap();
            state.events.push(Event::Begin(sector, offset, len));
            if state.open.is_some() {
                return Err(MemoryError("a write is already open".into()));
            }
            state.writes_started += 1;
            if state
                .fail_all_writes_from
                .is_some_and(|from| state.writes_started > from)
            {
                return Err(MemoryError("the mouse refused the write".into()));
            }
            state.open = Some(Open {
                sector,
                offset: usize::from(offset),
                len: usize::from(len),
                received: Vec::new(),
            });
            Ok(())
        }

        async fn write_chunk(&self, chunk: [u8; CHUNK]) -> Result<(), MemoryError> {
            let mut state = self.state.lock().unwrap();
            state.events.push(Event::Chunk);
            state.chunks_seen += 1;
            if state.fail_chunk == Some(state.chunks_seen) {
                // A chunk that fails aborts the write, as the mouse would.
                state.open = None;
                return Err(MemoryError("chunk lost".into()));
            }
            let open = state
                .open
                .as_mut()
                .ok_or_else(|| MemoryError("chunk with no write open".into()))?;
            open.received.extend_from_slice(&chunk);
            Ok(())
        }

        async fn end_write(&self) -> Result<(), MemoryError> {
            let mut state = self.state.lock().unwrap();
            state.events.push(Event::End);
            let open = state
                .open
                .take()
                .ok_or_else(|| MemoryError("end with no write open".into()))?;
            if open.received.len() < open.len {
                return Err(MemoryError("write ended before all bytes arrived".into()));
            }
            let at = open.offset;
            let data = &open.received[..open.len];
            state.memory[usize::from(open.sector)][at..at + open.len].copy_from_slice(data);
            if let Some(after) = state.corrupt_after_write
                && state.writes_started == after + 1
            {
                state.memory[usize::from(open.sector)][100] ^= 0x01;
            }
            Ok(())
        }
    }

    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        block_on(future)
    }

    // -- reading -----------------------------------------------------------

    #[test]
    fn a_whole_sector_is_read_including_the_unaligned_tail() {
        let original = profile(800);
        let fake = Fake::holding(1, original.as_bytes());
        let read = run(read_sector(&fake, 1, SECTOR_SIZE)).unwrap();
        assert_eq!(read, original.as_bytes());
        // 255 bytes: fifteen whole chunks and one aligned to the end.
        assert_eq!(fake.events().len(), 16);
        assert_eq!(fake.events().last(), Some(&Event::Read(1, 239)));
    }

    #[test]
    fn reading_never_writes() {
        let fake = Fake::holding(1, profile(800).as_bytes());
        let _ = run(read_sector(&fake, 1, SECTOR_SIZE));
        assert_eq!(fake.writes_begun(), 0);
    }

    // -- the happy path ----------------------------------------------------

    #[test]
    fn a_new_profile_replaces_the_old_one_and_the_backup_is_the_old_one() {
        let old = profile(800);
        let new = profile(1600);
        let fake = Fake::holding(1, old.as_bytes());
        let backup = run(write_profile_sector(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &new,
        ))
        .unwrap();
        assert_eq!(backup.sector, 1);
        assert_eq!(backup.bytes, old.as_bytes());
        assert_eq!(fake.sector(1), new.as_bytes());
    }

    #[test]
    fn a_sector_that_changed_since_it_was_read_is_left_alone() {
        let held = profile(900);
        let fake = Fake::holding(1, profile(800).as_bytes());
        let result = run(write_profile_sector_if_unchanged(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            held.as_bytes(),
            &profile(1600),
        ));
        assert_eq!(result, Err(WriteError::Changed));
        assert_eq!(fake.writes_begun(), 0);
        assert_eq!(fake.sector(1), profile(800).as_bytes());
    }

    #[test]
    fn a_sector_still_as_it_was_read_is_written() {
        let old = profile(800);
        let fake = Fake::holding(1, old.as_bytes());
        let backup = run(write_profile_sector_if_unchanged(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            old.as_bytes(),
            &profile(1600),
        ))
        .unwrap();
        assert_eq!(backup.bytes, old.as_bytes());
        assert_eq!(fake.sector(1), profile(1600).as_bytes());
    }

    /// One begin for the whole sector, 16-byte chunks, one end, in that order,
    /// and only ever at offset 0 of the requested sector.
    #[test]
    fn the_write_is_one_begin_sixteen_chunks_and_one_end_in_order() {
        let fake = Fake::holding(2, profile(800).as_bytes());
        run(write_profile_sector(
            &fake,
            2,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &profile(1600),
        ))
        .unwrap();
        let writes: Vec<Event> = fake
            .events()
            .into_iter()
            .filter(|event| !matches!(event, Event::Read(..)))
            .collect();
        let mut expected = vec![Event::Begin(2, 0, 255)];
        expected.extend(std::iter::repeat_n(Event::Chunk, 16));
        expected.push(Event::End);
        assert_eq!(writes, expected);
    }

    #[test]
    fn the_sector_is_read_before_it_is_written_and_again_after() {
        let fake = Fake::holding(1, profile(800).as_bytes());
        run(write_profile_sector(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &profile(1600),
        ))
        .unwrap();
        let events = fake.events();
        let first_write = events
            .iter()
            .position(|event| matches!(event, Event::Begin(..)))
            .unwrap();
        let reads_before = events[..first_write]
            .iter()
            .filter(|event| matches!(event, Event::Read(..)))
            .count();
        let reads_after = events[first_write..]
            .iter()
            .filter(|event| matches!(event, Event::Read(..)))
            .count();
        assert_eq!(reads_before, 16, "the whole sector is backed up first");
        assert_eq!(reads_after, 16, "the whole sector is read back after");
    }

    #[test]
    fn no_other_sector_is_touched() {
        let fake = Fake::holding(1, profile(800).as_bytes());
        let others_before: Vec<Vec<u8>> = (0..SECTOR_COUNT)
            .filter(|&s| s != 1)
            .map(|s| fake.sector(s))
            .collect();
        run(write_profile_sector(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &profile(1600),
        ))
        .unwrap();
        let others_after: Vec<Vec<u8>> = (0..SECTOR_COUNT)
            .filter(|&s| s != 1)
            .map(|s| fake.sector(s))
            .collect();
        assert_eq!(others_before, others_after);
    }

    #[test]
    fn writing_what_is_already_there_writes_nothing() {
        let same = profile(800);
        let fake = Fake::holding(1, same.as_bytes());
        let backup = run(write_profile_sector(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &same,
        ))
        .unwrap();
        assert_eq!(fake.writes_begun(), 0);
        assert_eq!(backup.bytes, same.as_bytes());
    }

    // -- refusals: nothing may be written ----------------------------------

    #[test]
    fn a_sector_the_directory_does_not_list_is_refused_with_no_traffic_at_all() {
        let fake = Fake::holding(7, profile(800).as_bytes());
        for sector in [0, 6, 7, 0x0100, 0xffff] {
            let result = run(write_profile_sector(
                &fake,
                sector,
                SECTOR_SIZE,
                &PROFILE_SECTORS,
                &profile(1600),
            ));
            assert_eq!(
                result,
                Err(WriteError::Refused(ProfileError::SectorNotWritable(sector)))
            );
        }
        assert!(fake.events().is_empty(), "the mouse was not even read");
    }

    /// An unreadable or blank sector is not understood, so it is not replaced.
    #[test]
    fn a_sector_that_is_not_a_valid_profile_is_never_overwritten() {
        let fake = Fake::holding(1, &[0xff; SECTOR_SIZE]);
        let result = run(write_profile_sector(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &profile(1600),
        ));
        assert!(matches!(
            result,
            Err(WriteError::Refused(ProfileError::BadChecksum { .. }))
        ));
        assert_eq!(fake.writes_begun(), 0);
        assert_eq!(fake.sector(1), vec![0xff; SECTOR_SIZE]);
    }

    #[test]
    fn a_profile_of_the_wrong_size_is_refused_before_any_write() {
        // A 256-byte profile where the mouse's sectors are 255 bytes.
        let mut bytes = vec![0x11; 256];
        let crc = crc16_ccitt(&bytes[..254]);
        bytes[254..].copy_from_slice(&crc.to_be_bytes());
        let odd = ProfileSector::parse(&bytes, 256).unwrap();
        let fake = Fake::holding(1, profile(800).as_bytes());
        let result = run(write_profile_sector(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &odd,
        ));
        assert_eq!(
            result,
            Err(WriteError::Refused(ProfileError::WrongLength {
                expected: SECTOR_SIZE,
                found: 256
            }))
        );
        assert_eq!(fake.writes_begun(), 0);
    }

    #[test]
    fn a_failed_read_before_the_write_stops_everything() {
        // The fake's sector 9 is blank, but sector 9 is not listed: ask for a
        // listed sector the fake cannot read to see the error path.
        struct Dead;
        #[async_trait]
        impl ProfileMemory for Dead {
            async fn read(&self, _: u16, _: u16) -> Result<[u8; CHUNK], MemoryError> {
                Err(MemoryError("mouse asleep".into()))
            }
            async fn begin_write(&self, _: u16, _: u16, _: u16) -> Result<(), MemoryError> {
                panic!("wrote to a mouse that could not be read");
            }
            async fn write_chunk(&self, _: [u8; CHUNK]) -> Result<(), MemoryError> {
                panic!("wrote to a mouse that could not be read");
            }
            async fn end_write(&self) -> Result<(), MemoryError> {
                panic!("wrote to a mouse that could not be read");
            }
        }
        let result = run(write_profile_sector(
            &Dead,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &profile(1600),
        ));
        assert_eq!(
            result,
            Err(WriteError::Memory(MemoryError("mouse asleep".into())))
        );
    }

    // -- a write that goes wrong -------------------------------------------

    #[test]
    fn a_read_back_that_disagrees_restores_the_original() {
        let old = profile(800);
        let fake = Fake::holding(1, old.as_bytes());
        fake.with(|state| state.corrupt_after_write = Some(0));
        let result = run(write_profile_sector(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &profile(1600),
        ));
        match result {
            Err(WriteError::Failed {
                restored, backup, ..
            }) => {
                assert!(restored);
                assert_eq!(backup.bytes, old.as_bytes());
            }
            other => panic!("expected a restored failure, got {other:?}"),
        }
        assert_eq!(fake.sector(1), old.as_bytes(), "the original is back");
    }

    #[test]
    fn a_chunk_that_fails_midway_restores_the_original() {
        let old = profile(800);
        let fake = Fake::holding(1, old.as_bytes());
        fake.with(|state| state.fail_chunk = Some(8));
        let result = run(write_profile_sector(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &profile(1600),
        ));
        assert!(matches!(
            result,
            Err(WriteError::Failed { restored: true, .. })
        ));
        assert_eq!(fake.sector(1), old.as_bytes());
    }

    /// When even the restore cannot be written, the caller is told so and is
    /// handed the backup to keep: the one situation that needs the user.
    #[test]
    fn a_failed_restore_says_so_and_hands_back_the_backup() {
        let old = profile(800);
        let fake = Fake::holding(1, old.as_bytes());
        fake.with(|state| {
            state.corrupt_after_write = Some(0);
            state.fail_all_writes_from = Some(1);
        });
        let result = run(write_profile_sector(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &profile(1600),
        ));
        match result {
            Err(WriteError::Failed {
                restored, backup, ..
            }) => {
                assert!(!restored);
                assert_eq!(backup.bytes, old.as_bytes());
            }
            other => panic!("expected an unrestored failure, got {other:?}"),
        }
    }

    // -- restoring from a backup -------------------------------------------

    #[test]
    fn a_backup_puts_the_old_profile_back() {
        let old = profile(800);
        let fake = Fake::holding(1, old.as_bytes());
        let backup = run(write_profile_sector(
            &fake,
            1,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
            &profile(1600),
        ))
        .unwrap();
        assert_ne!(fake.sector(1), old.as_bytes());
        run(restore_backup(
            &fake,
            &backup,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
        ))
        .unwrap();
        assert_eq!(fake.sector(1), old.as_bytes());
    }

    #[test]
    fn a_backup_of_an_unlisted_sector_is_not_restored() {
        let fake = Fake::holding(1, profile(800).as_bytes());
        let backup = SectorBackup {
            sector: 0,
            bytes: profile(800).as_bytes().to_vec(),
        };
        assert_eq!(
            run(restore_backup(
                &fake,
                &backup,
                SECTOR_SIZE,
                &PROFILE_SECTORS
            )),
            Err(WriteError::Refused(ProfileError::SectorNotWritable(0)))
        );
        assert_eq!(fake.writes_begun(), 0);
    }

    /// A backup file that was damaged on disk must not be written to the
    /// mouse: it has to be a valid profile.
    #[test]
    fn a_damaged_backup_is_not_restored() {
        let fake = Fake::holding(1, profile(800).as_bytes());
        let mut bytes = profile(1600).as_bytes().to_vec();
        bytes[50] ^= 0x01;
        let backup = SectorBackup { sector: 1, bytes };
        let result = run(restore_backup(
            &fake,
            &backup,
            SECTOR_SIZE,
            &PROFILE_SECTORS,
        ));
        assert!(matches!(
            result,
            Err(WriteError::Refused(ProfileError::BadChecksum { .. }))
        ));
        assert_eq!(fake.writes_begun(), 0);
    }
}
