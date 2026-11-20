//! Editing a stored onboard profile without touching anything but its DPI
//! stages. Pure byte logic: nothing here talks to a device.
//!
//! The layout comes from a read-only dump of profile 1 on a G PRO 2 LIGHTSPEED
//! (profile format 6, 255-byte sectors); the dump is the `REAL_PROFILE_1`
//! fixture below:
//!
//! * byte 1 is the default DPI stage, an index into the five stages;
//! * bytes 4..29 are five stages of 5 bytes each: X DPI (little-endian `u16`),
//!   Y DPI (little-endian `u16`), lift-off distance (`u8`);
//! * the last two bytes of the sector are a CRC-16/CCITT-FALSE of every byte
//!   before them, big-endian (the fixture's `dc b5` is exactly that).
//!
//! Every other byte (report rate, button mapping, LED settings, name) is left
//! as it is. Formats other than 6 are not known to share this layout.

/// How many DPI stages a profile holds.
pub const DPI_STAGE_COUNT: usize = 5;

const DEFAULT_STAGE_AT: usize = 1;
const STAGES_AT: usize = 4;
const STAGE_LEN: usize = 5;
const CHECKSUM_LEN: usize = 2;
/// The first sector number of the ROM area; nothing from here up is written.
const ROM_AREA: u16 = 0x0100;

/// Why a profile could not be read, edited or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    /// The sector is not the size the mouse reports.
    #[error("profile sector is {found} bytes, expected {expected}")]
    WrongLength {
        /// The size the mouse reports.
        expected: usize,
        /// The size we were given.
        found: usize,
    },
    /// The stored checksum does not match the sector's bytes.
    #[error("profile checksum is {stored:#06x}, its bytes sum to {computed:#06x}")]
    BadChecksum {
        /// The checksum stored in the sector.
        stored: u16,
        /// The checksum of its bytes.
        computed: u16,
    },
    /// An edit with no stages at all.
    #[error("a profile needs at least one DPI stage")]
    NoStages,
    /// An edit with more stages than a profile holds.
    #[error("a profile holds at most {DPI_STAGE_COUNT} DPI stages, got {0}")]
    TooManyStages(usize),
    /// A stage value no sensor takes.
    #[error("DPI stage {0} is not a usable value")]
    BadStage(u16),
    /// A default stage that is not one of the stages given.
    #[error("default stage {default} is not one of the {count} stages")]
    DefaultOutOfRange {
        /// The default stage asked for.
        default: u8,
        /// How many stages were given.
        count: usize,
    },
    /// A list shorter than the profile's five slots, whose unused slots we
    /// cannot yet encode the way the mouse does.
    #[error("{0} DPI stages given, but only a full set of {DPI_STAGE_COUNT} can be stored for now")]
    FewerThanFiveStages(usize),
    /// A stored stage whose X and Y differ, which an edit would flatten.
    #[error("stage {0} has different X and Y DPI; refusing to overwrite it")]
    SplitAxes(usize),
    /// A sector outside the mouse's profile sectors.
    #[error("sector {0:#06x} is not a profile sector this mouse lets us write")]
    SectorNotWritable(u16),
    /// What the mouse returned is not what was written.
    #[error("read back {found} bytes that differ from the {expected} written")]
    ReadBackMismatch {
        /// How many bytes were written.
        expected: usize,
        /// How many bytes came back.
        found: usize,
    },
    /// A backup line that is not `sector=<hex> bytes=<hex>`.
    #[error("backup text is malformed")]
    BadBackup,
}

/// A profile sector whose checksum has been verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSector(Vec<u8>);

/// CRC-16/CCITT-FALSE: polynomial 0x1021, initial value 0xFFFF, no reflection.
#[must_use]
pub fn crc16_ccitt(data: &[u8]) -> u16 {
    data.iter().fold(0xffff_u16, |mut crc, &byte| {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x1021
            };
        }
        crc
    })
}

impl ProfileSector {
    /// Accept `bytes` as a profile sector of `sector_size` bytes with a valid
    /// checksum, and nothing else.
    pub fn parse(bytes: &[u8], sector_size: usize) -> Result<Self, ProfileError> {
        let table_end = STAGES_AT + DPI_STAGE_COUNT * STAGE_LEN;
        if bytes.len() != sector_size || sector_size < table_end + CHECKSUM_LEN {
            return Err(ProfileError::WrongLength {
                expected: sector_size,
                found: bytes.len(),
            });
        }
        let (body, stored) = bytes.split_at(bytes.len() - CHECKSUM_LEN);
        let stored = u16::from_be_bytes([stored[0], stored[1]]);
        let computed = crc16_ccitt(body);
        if stored != computed {
            return Err(ProfileError::BadChecksum { stored, computed });
        }
        Ok(Self(bytes.to_vec()))
    }

    /// The sector's bytes, checksum included.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The five stored X DPI values, in order.
    #[must_use]
    pub fn dpi_stages(&self) -> [u16; DPI_STAGE_COUNT] {
        std::array::from_fn(|i| self.axis(i, 0))
    }

    /// The stage the mouse starts on, as an index into [`Self::dpi_stages`].
    #[must_use]
    pub fn default_stage(&self) -> u8 {
        self.0[DEFAULT_STAGE_AT]
    }

    /// A copy with `stages` as its DPI stages (both axes) and `default` as its
    /// default stage. Every other byte is unchanged, lift-off distances
    /// included, and the checksum is recomputed.
    pub fn with_dpi_stages(&self, stages: &[u16], default: u8) -> Result<Self, ProfileError> {
        if stages.is_empty() {
            return Err(ProfileError::NoStages);
        }
        if stages.len() > DPI_STAGE_COUNT {
            return Err(ProfileError::TooManyStages(stages.len()));
        }
        if let Some(&zero) = stages.iter().find(|&&stage| stage == 0) {
            return Err(ProfileError::BadStage(zero));
        }
        if usize::from(default) >= stages.len() {
            return Err(ProfileError::DefaultOutOfRange {
                default,
                count: stages.len(),
            });
        }
        if stages.len() < DPI_STAGE_COUNT {
            return Err(ProfileError::FewerThanFiveStages(stages.len()));
        }
        if let Some(split) = (0..DPI_STAGE_COUNT).find(|&i| self.axis(i, 0) != self.axis(i, 1)) {
            return Err(ProfileError::SplitAxes(split));
        }
        let mut bytes = self.0.clone();
        bytes[DEFAULT_STAGE_AT] = default;
        for (i, stage) in stages.iter().enumerate() {
            let at = STAGES_AT + i * STAGE_LEN;
            bytes[at..at + 2].copy_from_slice(&stage.to_le_bytes());
            bytes[at + 2..at + 4].copy_from_slice(&stage.to_le_bytes());
        }
        let end = bytes.len() - CHECKSUM_LEN;
        let crc = crc16_ccitt(&bytes[..end]);
        bytes[end..].copy_from_slice(&crc.to_be_bytes());
        Ok(Self(bytes))
    }

    /// Stage `stage`'s X (`axis` 0) or Y (`axis` 1) DPI.
    fn axis(&self, stage: usize, axis: usize) -> u16 {
        let at = STAGES_AT + stage * STAGE_LEN + axis * 2;
        u16::from_le_bytes([self.0[at], self.0[at + 1]])
    }
}

/// Whether the mouse lets us write `sector`: only one of the profile sectors
/// listed in its user directory (`profile_sectors`). Never the directory
/// itself at `0x0000`, never the ROM area from `0x0100`, and never a sector
/// that merely exists on the mouse, such as macro storage.
pub fn check_writable_sector(sector: u16, profile_sectors: &[u16]) -> Result<(), ProfileError> {
    if sector == 0 || sector >= ROM_AREA || !profile_sectors.contains(&sector) {
        return Err(ProfileError::SectorNotWritable(sector));
    }
    Ok(())
}

/// Confirm that the sector read back after a write holds what was written.
pub fn verify_written(written: &[u8], read_back: &[u8]) -> Result<(), ProfileError> {
    if written == read_back {
        return Ok(());
    }
    Err(ProfileError::ReadBackMismatch {
        expected: written.len(),
        found: read_back.len(),
    })
}

/// A copy of a sector taken before it is changed, so it can be put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectorBackup {
    /// The sector the bytes came from.
    pub sector: u16,
    /// The sector's contents.
    pub bytes: Vec<u8>,
}

impl SectorBackup {
    /// One line of text: `sector=<hex> bytes=<hex>`.
    #[must_use]
    pub fn to_text(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = format!("sector={:04x} bytes=", self.sector);
        for &byte in &self.bytes {
            text.push(char::from(DIGITS[usize::from(byte >> 4)]));
            text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        text
    }

    /// Parse the line [`Self::to_text`] writes.
    pub fn from_text(text: &str) -> Result<Self, ProfileError> {
        let mut fields = text.split(' ');
        let sector = fields
            .next()
            .and_then(|field| field.strip_prefix("sector="))
            .filter(|hex| hex.len() == 4)
            .and_then(|hex| u16::from_str_radix(hex, 16).ok())
            .ok_or(ProfileError::BadBackup)?;
        let hex = fields
            .next()
            .and_then(|field| field.strip_prefix("bytes="))
            .filter(|hex| hex.len() % 2 == 0)
            .ok_or(ProfileError::BadBackup)?;
        if fields.next().is_some() {
            return Err(ProfileError::BadBackup);
        }
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| {
                hex.get(i..i + 2)
                    .filter(|pair| pair.bytes().all(|b| b.is_ascii_hexdigit()))
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            })
            .collect::<Option<Vec<u8>>>()
            .ok_or(ProfileError::BadBackup)?;
        Ok(Self { sector, bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTOR_SIZE: usize = 255;
    const DEFAULT_STAGE_AT: usize = 1;
    const STAGES_AT: usize = 4;
    const STAGE_LEN: usize = 5;

    /// Profile 1 of a G PRO 2 LIGHTSPEED, read from the mouse with a read-only
    /// dump (profile format 6): stages 400/800/1600/3200/6400 DPI on both
    /// axes, lift-off 2, default stage 3. It holds profile settings only.
    const REAL_PROFILE_1: &str = concat!(
        "03030100900190010220032003024006400602800c800c0200190019020000",
        "0000ff00ffffffffffffffffffffff2c0180010001800100028001000480010008",
        "80010010900500008001000880010010ffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffff0194e6ff000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000dcb5",
    );

    fn real_profile() -> Vec<u8> {
        let hex = REAL_PROFILE_1;
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    /// A sector with `stages` stored on both axes at lift-off 2, everything
    /// else a recognisable pattern (so an edit that clobbers a neighbour is
    /// seen), and a valid checksum.
    fn sector_with(stages: [u16; DPI_STAGE_COUNT], default: u8) -> Vec<u8> {
        let mut bytes: Vec<u8> = (0..SECTOR_SIZE)
            .map(|i| u8::try_from(i % 251).unwrap() ^ 0x5a)
            .collect();
        bytes[DEFAULT_STAGE_AT] = default;
        for (i, stage) in stages.into_iter().enumerate() {
            let at = STAGES_AT + i * STAGE_LEN;
            bytes[at..at + 2].copy_from_slice(&stage.to_le_bytes());
            bytes[at + 2..at + 4].copy_from_slice(&stage.to_le_bytes());
            bytes[at + 4] = 2;
        }
        seal(&mut bytes);
        bytes
    }

    fn seal(bytes: &mut [u8]) {
        let end = bytes.len() - 2;
        let crc = crc16_ccitt(&bytes[..end]);
        bytes[end..].copy_from_slice(&crc.to_be_bytes());
    }

    fn stock() -> ProfileSector {
        ProfileSector::parse(&sector_with([400, 800, 1600, 3200, 6400], 2), SECTOR_SIZE).unwrap()
    }

    // -- the real mouse -----------------------------------------------------

    /// The one test that is not built from our own assumptions: bytes read from
    /// the mouse. It must parse, with the checksum the mouse itself stored.
    #[test]
    fn the_real_profile_parses_and_shows_the_stages_the_mouse_reports() {
        let profile = ProfileSector::parse(&real_profile(), SECTOR_SIZE).unwrap();
        assert_eq!(profile.dpi_stages(), [400, 800, 1600, 3200, 6400]);
        assert_eq!(profile.default_stage(), 3);
        assert_eq!(profile.as_bytes(), real_profile().as_slice());
    }

    #[test]
    fn the_checksum_of_the_real_profile_is_the_one_the_mouse_stored() {
        let bytes = real_profile();
        assert_eq!(bytes.len(), SECTOR_SIZE);
        assert_eq!(crc16_ccitt(&bytes[..SECTOR_SIZE - 2]), 0xdcb5);
    }

    /// Editing the real profile to the stages it already has must reproduce it
    /// to the byte, so a write that changes nothing cannot corrupt anything.
    #[test]
    fn rewriting_the_real_profile_with_its_own_stages_changes_nothing() {
        let profile = ProfileSector::parse(&real_profile(), SECTOR_SIZE).unwrap();
        let again = profile
            .with_dpi_stages(&profile.dpi_stages(), profile.default_stage())
            .unwrap();
        assert_eq!(again.as_bytes(), real_profile().as_slice());
    }

    #[test]
    fn editing_the_real_profile_leaves_buttons_leds_and_the_rest_alone() {
        let before = real_profile();
        let profile = ProfileSector::parse(&before, SECTOR_SIZE).unwrap();
        let after = profile
            .with_dpi_stages(&[800, 1600, 2400, 3200, 4800], 1)
            .unwrap();
        let stage_bytes = STAGES_AT..STAGES_AT + DPI_STAGE_COUNT * STAGE_LEN;
        for (i, (a, b)) in before.iter().zip(after.as_bytes()).enumerate() {
            let in_stage_dpi = stage_bytes.contains(&i) && (i - STAGES_AT) % STAGE_LEN != 4;
            let allowed = i == DEFAULT_STAGE_AT || in_stage_dpi || i >= SECTOR_SIZE - 2;
            assert!(
                a == b || allowed,
                "byte {i} changed from {a:#04x} to {b:#04x}"
            );
        }
    }

    // -- the checksum ------------------------------------------------------

    /// The published check value for CRC-16/CCITT-FALSE, independent of any
    /// profile: if this fails, nothing else here can be trusted.
    #[test]
    fn crc16_matches_the_published_check_value() {
        assert_eq!(crc16_ccitt(b"123456789"), 0x29b1);
    }

    #[test]
    fn crc16_of_nothing_is_the_initial_value() {
        assert_eq!(crc16_ccitt(&[]), 0xffff);
    }

    // -- reading a sector --------------------------------------------------

    #[test]
    fn a_sealed_sector_parses_and_exposes_its_stages() {
        let profile = stock();
        assert_eq!(profile.dpi_stages(), [400, 800, 1600, 3200, 6400]);
        assert_eq!(profile.default_stage(), 2);
    }

    #[test]
    fn a_sector_of_the_wrong_length_is_refused() {
        let short = &sector_with([400; 5], 0)[..SECTOR_SIZE - 1];
        assert_eq!(
            ProfileSector::parse(short, SECTOR_SIZE),
            Err(ProfileError::WrongLength {
                expected: SECTOR_SIZE,
                found: SECTOR_SIZE - 1
            })
        );
    }

    #[test]
    fn a_flipped_bit_anywhere_fails_the_checksum() {
        let good = sector_with([400, 800, 1600, 3200, 6400], 2);
        for at in [
            0,
            DEFAULT_STAGE_AT,
            STAGES_AT,
            100,
            SECTOR_SIZE - 3,
            SECTOR_SIZE - 1,
        ] {
            let mut bad = good.clone();
            bad[at] ^= 0x01;
            assert!(
                matches!(
                    ProfileSector::parse(&bad, SECTOR_SIZE),
                    Err(ProfileError::BadChecksum { .. })
                ),
                "a flipped bit at byte {at} was accepted"
            );
        }
    }

    /// A blank (erased) sector is all 0xff. It must never parse, or an
    /// unwritten slot would be treated as a profile and rewritten.
    #[test]
    fn an_erased_sector_is_refused() {
        assert!(matches!(
            ProfileSector::parse(&[0xff; SECTOR_SIZE], SECTOR_SIZE),
            Err(ProfileError::BadChecksum { .. })
        ));
    }

    // -- editing -----------------------------------------------------------

    #[test]
    fn new_stages_are_stored_and_the_result_still_parses() {
        let edited = stock()
            .with_dpi_stages(&[800, 1600, 3200, 4800, 12000], 1)
            .unwrap();
        let reparsed = ProfileSector::parse(edited.as_bytes(), SECTOR_SIZE).unwrap();
        assert_eq!(reparsed.dpi_stages(), [800, 1600, 3200, 4800, 12000]);
        assert_eq!(reparsed.default_stage(), 1);
    }

    /// Both axes are written, so a mouse never ends up with X and Y disagreeing.
    #[test]
    fn an_edit_writes_the_same_value_to_both_axes() {
        let edited = stock()
            .with_dpi_stages(&[800, 1600, 3200, 4800, 12000], 0)
            .unwrap();
        for i in 0..DPI_STAGE_COUNT {
            let at = STAGES_AT + i * STAGE_LEN;
            let b = edited.as_bytes();
            assert_eq!(b[at..at + 2], b[at + 2..at + 4], "stage {i}");
        }
    }

    /// The whole point of the safeguards: only the default stage, the X and Y
    /// DPI of the five stages, and the checksum may differ. Lift-off, report
    /// rate, buttons and everything else must not.
    #[test]
    fn an_edit_changes_no_byte_outside_the_stages_default_and_checksum() {
        let before = stock();
        let after = before
            .with_dpi_stages(&[100, 200, 300, 400, 500], 4)
            .unwrap();
        assert_eq!(before.as_bytes().len(), after.as_bytes().len());
        let allowed = |i: usize| {
            i == DEFAULT_STAGE_AT
                || ((STAGES_AT..STAGES_AT + DPI_STAGE_COUNT * STAGE_LEN).contains(&i)
                    && (i - STAGES_AT) % STAGE_LEN != 4)
                || i >= SECTOR_SIZE - 2
        };
        for (i, (a, b)) in before.as_bytes().iter().zip(after.as_bytes()).enumerate() {
            assert!(
                a == b || allowed(i),
                "byte {i} changed from {a:#04x} to {b:#04x}"
            );
        }
    }

    #[test]
    fn editing_to_the_same_stages_changes_nothing_at_all() {
        let before = stock();
        let after = before
            .with_dpi_stages(&before.dpi_stages(), before.default_stage())
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn the_original_is_left_untouched_by_an_edit() {
        let before = stock();
        let snapshot = before.clone();
        let _ = before.with_dpi_stages(&[100; 5], 0).unwrap();
        assert_eq!(before, snapshot);
    }

    #[test]
    fn no_stages_are_refused() {
        assert_eq!(stock().with_dpi_stages(&[], 0), Err(ProfileError::NoStages));
    }

    #[test]
    fn more_than_five_stages_are_refused() {
        assert_eq!(
            stock().with_dpi_stages(&[400; 6], 0),
            Err(ProfileError::TooManyStages(6))
        );
    }

    #[test]
    fn a_zero_stage_is_refused() {
        assert_eq!(
            stock().with_dpi_stages(&[400, 0, 800, 1600, 3200], 0),
            Err(ProfileError::BadStage(0))
        );
    }

    #[test]
    fn a_default_stage_past_the_last_stage_is_refused() {
        assert_eq!(
            stock().with_dpi_stages(&[400, 800, 1600, 3200, 6400], 5),
            Err(ProfileError::DefaultOutOfRange {
                default: 5,
                count: 5
            })
        );
        assert_eq!(
            stock().with_dpi_stages(&[400, 800, 1600], 3),
            Err(ProfileError::DefaultOutOfRange {
                default: 3,
                count: 3
            })
        );
    }

    /// For now a short list is refused, not guessed at: the unused slots'
    /// encoding is not known, and a wrong guess would be written to flash.
    #[test]
    fn fewer_than_five_stages_are_refused_for_now() {
        for count in 1..DPI_STAGE_COUNT {
            let stages: Vec<u16> = (1..=count)
                .map(|n| u16::try_from(n).unwrap() * 400)
                .collect();
            assert_eq!(
                stock().with_dpi_stages(&stages, 0),
                Err(ProfileError::FewerThanFiveStages(count)),
                "{count} stages"
            );
        }
    }

    /// An edit to a stage stored with different X and Y would silently flatten
    /// it, so it is refused, and says which stage.
    #[test]
    fn a_profile_with_split_axes_is_refused_not_flattened() {
        let mut bytes = sector_with([400, 800, 1600, 3200, 6400], 0);
        let y_of_stage_2 = STAGES_AT + 2 * STAGE_LEN + 2;
        bytes[y_of_stage_2..y_of_stage_2 + 2].copy_from_slice(&1200_u16.to_le_bytes());
        seal(&mut bytes);
        let split = ProfileSector::parse(&bytes, SECTOR_SIZE).unwrap();
        assert_eq!(
            split.with_dpi_stages(&[400, 800, 1600, 3200, 6400], 0),
            Err(ProfileError::SplitAxes(2))
        );
    }

    /// How a profile with fewer than five stages stores the unused slots is
    /// still unknown: the dumped profile uses all five, and G HUB may pad with
    /// zeros, a repeat of the last stage, or 0xffff. When a profile with fewer
    /// stages has been dumped, this replaces the refusal above.
    #[test]
    #[ignore = "needs a dump of a profile with fewer than five stages"]
    fn fewer_than_five_stages_fill_the_unused_slots_the_way_the_mouse_does() {
        let edited = stock().with_dpi_stages(&[800, 1600, 3200], 0).unwrap();
        let _ = edited.dpi_stages();
        todo!("assert the unused slots once the dump shows the encoding");
    }

    // -- which sector may be written ---------------------------------------

    /// As the real mouse lists them: five profile sectors in its directory,
    /// although it reports sixteen sectors in all.
    const PROFILE_SECTORS: [u16; 5] = [1, 2, 3, 4, 5];

    #[test]
    fn only_the_sectors_the_directory_lists_are_writable() {
        for sector in PROFILE_SECTORS {
            assert_eq!(
                check_writable_sector(sector, &PROFILE_SECTORS),
                Ok(()),
                "sector {sector}"
            );
        }
    }

    #[test]
    fn the_user_directory_is_never_writable() {
        assert_eq!(
            check_writable_sector(0x0000, &PROFILE_SECTORS),
            Err(ProfileError::SectorNotWritable(0x0000))
        );
        // Not even if a corrupt directory were to list itself.
        assert_eq!(
            check_writable_sector(0x0000, &[0x0000, 1]),
            Err(ProfileError::SectorNotWritable(0x0000))
        );
    }

    #[test]
    fn the_rom_area_is_never_writable() {
        for sector in [0x0100, 0x0101, 0x01ff, 0x0200, 0xffff] {
            assert_eq!(
                check_writable_sector(sector, &PROFILE_SECTORS),
                Err(ProfileError::SectorNotWritable(sector)),
                "sector {sector:#06x}"
            );
            // Not even if a corrupt directory listed it.
            assert_eq!(
                check_writable_sector(sector, &[sector]),
                Err(ProfileError::SectorNotWritable(sector)),
                "listed sector {sector:#06x}"
            );
        }
    }

    /// The mouse has sixteen sectors; only five hold profiles. The rest (macro
    /// storage and the like) are none of our business.
    #[test]
    fn a_sector_the_directory_does_not_list_is_not_writable() {
        for sector in [6, 7, 15, 16] {
            assert_eq!(
                check_writable_sector(sector, &PROFILE_SECTORS),
                Err(ProfileError::SectorNotWritable(sector)),
                "sector {sector}"
            );
        }
        assert_eq!(
            check_writable_sector(1, &[]),
            Err(ProfileError::SectorNotWritable(1))
        );
    }

    // -- verifying the write -----------------------------------------------

    #[test]
    fn an_identical_read_back_verifies() {
        let bytes = sector_with([400; 5], 0);
        assert_eq!(verify_written(&bytes, &bytes), Ok(()));
    }

    #[test]
    fn a_read_back_that_differs_in_one_byte_fails() {
        let written = sector_with([400; 5], 0);
        let mut read = written.clone();
        read[100] ^= 0x01;
        assert_eq!(
            verify_written(&written, &read),
            Err(ProfileError::ReadBackMismatch {
                expected: SECTOR_SIZE,
                found: SECTOR_SIZE
            })
        );
    }

    #[test]
    fn a_short_read_back_fails() {
        let written = sector_with([400; 5], 0);
        assert_eq!(
            verify_written(&written, &written[..written.len() - 1]),
            Err(ProfileError::ReadBackMismatch {
                expected: SECTOR_SIZE,
                found: SECTOR_SIZE - 1
            })
        );
    }

    // -- the backup --------------------------------------------------------

    #[test]
    fn a_backup_survives_a_round_trip_through_text() {
        let backup = SectorBackup {
            sector: 1,
            bytes: real_profile(),
        };
        assert_eq!(SectorBackup::from_text(&backup.to_text()), Ok(backup));
    }

    #[test]
    fn a_backup_is_a_single_line() {
        let backup = SectorBackup {
            sector: 3,
            bytes: vec![0xde, 0xad],
        };
        assert_eq!(backup.to_text(), "sector=0003 bytes=dead");
        assert!(!backup.to_text().contains('\n'));
    }

    #[test]
    fn malformed_backups_are_refused() {
        for text in [
            "",
            "sector=0001",
            "bytes=dead",
            "sector=zzzz bytes=dead",
            "sector=0001 bytes=dea",
            "sector=0001 bytes=dexd",
            "sector=0001 bytes=dead extra=1",
        ] {
            assert_eq!(
                SectorBackup::from_text(text),
                Err(ProfileError::BadBackup),
                "{text:?}"
            );
        }
    }
}
