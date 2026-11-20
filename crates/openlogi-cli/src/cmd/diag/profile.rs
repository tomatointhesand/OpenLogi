//! `openlogi diag profile-dump|profile-write|profile-restore` — look at, and
//! cautiously rewrite, one onboard profile's DPI stages on a G-series mouse.
//!
//! `profile-dump` only reads. `profile-write` and `profile-restore` write the
//! mouse's flash, so each is a dry run until `--yes`, and a write is refused
//! unless the profile's current bytes have first been saved to a backup file
//! that was read back and found identical. Stop the OpenLogi agent first: the
//! mouse must run its own profiles, not be held in host mode.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use openlogi_core::hid::{OnboardMode, ProfileIndex};
use openlogi_hid::write::profile_edit::{
    DPI_STAGE_COUNT, ProfileLayoutError, ProfileSector, SectorBackup,
};

use crate::cmd::diag::select_device;

/// Parse `--dpi 400,800,1600`: whole numbers above zero, separated by commas.
pub fn parse_dpi_list(text: &str) -> Result<Vec<u16>, String> {
    text.split(',')
        .map(|item| {
            let item = item.trim();
            if item.is_empty() {
                return Err("an empty entry in the DPI list".to_string());
            }
            match item.parse::<u16>() {
                Ok(0) => Err("a DPI of 0 is not usable".to_string()),
                Ok(dpi) => Ok(dpi),
                Err(_) => Err(format!("{item:?} is not a whole DPI between 1 and 65535")),
            }
        })
        .collect()
}

/// The file a backup of `profile` on `device` taken at `unix_secs` is saved
/// as: a plain name with no path separators, whatever the device is called.
#[must_use]
pub fn backup_file_name(device: &str, profile: u8, unix_secs: u64) -> String {
    let mut slug = String::new();
    for c in device.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "device" } else { slug };
    format!("profile-backup-{slug}-p{profile}-{unix_secs}.txt")
}

/// What an edit would change, for a person to read before saying `--yes`.
#[must_use]
pub fn render_plan(before: &ProfileSector, after: &ProfileSector) -> String {
    let list = |stages: [u16; DPI_STAGE_COUNT]| {
        stages
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let changed = before
        .as_bytes()
        .iter()
        .zip(after.as_bytes())
        .filter(|(a, b)| a != b)
        .count();
    let mut plan = String::new();
    let _ = writeln!(
        plan,
        "DPI stages:    {} -> {}",
        list(before.dpi_stages()),
        list(after.dpi_stages())
    );
    let _ = writeln!(
        plan,
        "default stage: {} -> {}",
        before.default_stage(),
        after.default_stage()
    );
    let total = after.as_bytes().len();
    if changed == 0 {
        let _ = write!(plan, "0 of {total} bytes change: nothing to write");
    } else {
        let _ = write!(plan, "{changed} of {total} bytes change");
    }
    plan
}

/// Save `backup` in `dir` as `name` and read it back: the write may proceed
/// only if what is on disk parses to exactly `backup`. Returns the path.
/// An existing file is never replaced.
pub fn save_and_verify_backup(
    dir: &Path,
    name: &str,
    backup: &SectorBackup,
) -> Result<PathBuf, String> {
    if name.is_empty() || name == "." || Path::new(name).file_name() != Some(name.as_ref()) {
        return Err(format!("{name:?} is not a plain file name"));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    writeln!(file, "{}", backup.to_text())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let read_back = load_backup(&path)?;
    if &read_back != backup {
        return Err(format!("{} does not read back identical", path.display()));
    }
    Ok(path)
}

/// Load a backup file written by [`save_and_verify_backup`].
pub fn load_backup(path: &Path) -> Result<SectorBackup, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    SectorBackup::from_text(text.trim_end()).map_err(|e| format!("{}: {e}", path.display()))
}

#[derive(Debug, Args)]
pub struct ProfileDumpArgs {
    /// Run against the device whose name contains this string.
    #[arg(long, value_name = "NAME")]
    pub device: Option<String>,
    /// Which onboard profile to read (1-based).
    #[arg(long, default_value_t = 1)]
    pub profile: u8,
    /// Also save the profile's bytes as a backup file in this directory.
    #[arg(long, value_name = "DIR")]
    pub backup_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ProfileWriteArgs {
    /// Run against the device whose name contains this string.
    #[arg(long, value_name = "NAME")]
    pub device: Option<String>,
    /// Which onboard profile to rewrite (1-based).
    #[arg(long, default_value_t = 1)]
    pub profile: u8,
    /// One to five DPI stages, comma-separated, e.g. 800,3200 or 400,800,1600,3200,6400.
    #[arg(long, value_name = "LIST")]
    pub dpi: String,
    /// The stage the mouse starts on, 0-based.
    #[arg(long = "default-stage", value_name = "N")]
    pub default_stage: u8,
    /// Where the backup of the current profile is saved first.
    #[arg(long, value_name = "DIR")]
    pub backup_dir: Option<PathBuf>,
    /// Actually write the mouse. Without it, only show what would change.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct ProfileRestoreArgs {
    /// Run against the device whose name contains this string.
    #[arg(long, value_name = "NAME")]
    pub device: Option<String>,
    /// A backup file saved by `profile-dump --backup-dir` or `profile-write`.
    #[arg(long, value_name = "FILE")]
    pub from: PathBuf,
    /// Actually write the mouse. Without it, only check the backup.
    #[arg(long)]
    pub yes: bool,
}

fn profile_index(profile: u8) -> Result<ProfileIndex> {
    ProfileIndex::new(profile).ok_or_else(|| anyhow!("profiles are numbered from 1"))
}

fn default_backup_dir() -> Result<PathBuf> {
    Ok(openlogi_core::paths::state_dir()
        .context("find the state directory")?
        .join("profile-backups"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn print_hex(bytes: &[u8]) {
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        println!("  {:04x}  {}", row * 16, hex.join(" "));
    }
}

pub async fn run_dump(args: ProfileDumpArgs) -> Result<()> {
    let index = profile_index(args.profile)?;
    let (route, name) = select_device(args.device.as_deref(), &[0x8100]).await?;
    println!("device: {name} ({route})");
    let dump = openlogi_hid::dump_profile(&route, index).await?;
    let mode = match dump.mode {
        OnboardMode::Onboard => "onboard (runs its own profiles)",
        OnboardMode::Host => "host (a program holds it)",
    };
    println!("mode: {mode}");
    println!(
        "profile {} lives in sector {:#06x}, {} bytes",
        dump.index, dump.sector, dump.sector_size
    );
    print_hex(&dump.bytes);
    match ProfileSector::parse(&dump.bytes, dump.sector_size) {
        Ok(profile) => {
            println!("checksum: valid");
            println!("DPI stages: {:?}", profile.dpi_stages());
            println!("default stage: {}", profile.default_stage());
            if let Some(dir) = args.backup_dir {
                let backup = SectorBackup {
                    sector: dump.sector,
                    bytes: dump.bytes.clone(),
                };
                let file = backup_file_name(&name, args.profile, now_secs());
                let path = save_and_verify_backup(&dir, &file, &backup).map_err(|e| anyhow!(e))?;
                println!("backup saved and verified: {}", path.display());
            }
        }
        Err(error) => println!("not a profile this tool understands: {error}"),
    }
    Ok(())
}

pub async fn run_write(args: ProfileWriteArgs) -> Result<()> {
    let index = profile_index(args.profile)?;
    let stages = parse_dpi_list(&args.dpi).map_err(|e| anyhow!(e))?;
    let (route, name) = select_device(args.device.as_deref(), &[0x8100]).await?;
    println!("device: {name} ({route})");

    let dump = openlogi_hid::dump_profile(&route, index).await?;
    if dump.mode == OnboardMode::Host {
        bail!("the mouse is in host mode: stop the OpenLogi agent and power-cycle the mouse first");
    }
    let current =
        ProfileSector::parse(&dump.bytes, dump.sector_size).map_err(|e: ProfileLayoutError| {
            anyhow!(
                "profile {} is not one this tool understands: {e}",
                args.profile
            )
        })?;
    let new = current.with_dpi_stages(&stages, args.default_stage)?;
    println!("profile {} (sector {:#06x}):", args.profile, dump.sector);
    println!("{}", render_plan(&current, &new));
    if current == new {
        return Ok(());
    }
    if !args.yes {
        println!("dry run: nothing written. Re-run with --yes to write it.");
        return Ok(());
    }

    let backup = SectorBackup {
        sector: dump.sector,
        bytes: dump.bytes.clone(),
    };
    let dir = match args.backup_dir {
        Some(dir) => dir,
        None => default_backup_dir()?,
    };
    let file = backup_file_name(&name, args.profile, now_secs());
    let path = save_and_verify_backup(&dir, &file, &backup).map_err(|e| anyhow!(e))?;
    println!("backup saved and verified: {}", path.display());

    match openlogi_hid::write_profile(&route, index, &new).await {
        Ok(_) => {
            println!(
                "written and read back: profile {} now has the new stages.",
                args.profile
            );
            println!(
                "to undo: openlogi diag profile-restore --from {} --yes",
                path.display()
            );
            Ok(())
        }
        Err(error) => {
            println!("the write did not succeed: {error}");
            println!("the backup is at {}", path.display());
            Err(error.into())
        }
    }
}

pub async fn run_restore(args: ProfileRestoreArgs) -> Result<()> {
    let backup = load_backup(&args.from).map_err(|e| anyhow!(e))?;
    println!(
        "backup of sector {:#06x}, {} bytes",
        backup.sector,
        backup.bytes.len()
    );
    let (route, name) = select_device(args.device.as_deref(), &[0x8100]).await?;
    println!("device: {name} ({route})");
    if !args.yes {
        println!("dry run: nothing written. Re-run with --yes to put this backup back.");
        return Ok(());
    }
    openlogi_hid::restore_profile(&route, &backup).await?;
    println!("restored and read back.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- --dpi -------------------------------------------------------------

    #[test]
    fn a_dpi_list_is_parsed_in_order() {
        assert_eq!(
            parse_dpi_list("400,800,1600,3200,6400").unwrap(),
            [400, 800, 1600, 3200, 6400]
        );
    }

    #[test]
    fn spaces_around_the_numbers_are_ignored() {
        assert_eq!(parse_dpi_list(" 400 , 800 ").unwrap(), [400, 800]);
    }

    #[test]
    fn a_bad_dpi_list_is_refused_with_a_reason() {
        for text in [
            "", ",", "400,,800", "400,", "abc", "400,x", "0", "400,0", "-5", "70000", "1.5",
        ] {
            let error = parse_dpi_list(text).unwrap_err();
            assert!(!error.is_empty(), "{text:?} had no reason");
        }
    }

    // -- the backup file name ----------------------------------------------

    #[test]
    fn the_backup_name_says_device_profile_and_time() {
        assert_eq!(
            backup_file_name("PRO 2 LIGHTSPEED", 1, 1_700_000_000),
            "profile-backup-pro-2-lightspeed-p1-1700000000.txt"
        );
    }

    #[test]
    fn a_hostile_device_name_cannot_escape_the_directory() {
        for name in ["../../etc/passwd", "a/b\\c", "..", "\0evil", "   ", ""] {
            let file = backup_file_name(name, 2, 5);
            assert!(
                !file.contains('/') && !file.contains('\\') && !file.contains('\0'),
                "{file:?}"
            );
            assert!(!file.contains(".."), "{file:?}");
            assert!(file.starts_with("profile-backup-"), "{file:?}");
            assert!(file.ends_with("-p2-5.txt"), "{file:?}");
        }
    }

    // -- the plan ----------------------------------------------------------

    fn sector(stages: [u16; 5], default: u8) -> ProfileSector {
        // Build through the same public path the command uses: a real
        // fixture edited with `with_dpi_stages`.
        let real = REAL
            .as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<u8>>();
        ProfileSector::parse(&real, 255)
            .unwrap()
            .with_dpi_stages(&stages, default)
            .unwrap()
    }

    /// Profile 1 of a G PRO 2 LIGHTSPEED, as in `profile.rs` of `openlogi-hidpp`.
    const REAL: &str = concat!(
        "03030100900190010220032003024006400602800c800c0200190019020000",
        "0000ff00ffffffffffffffffffffff2c0180010001800100028001000480010008",
        "80010010900500008001000880010010ffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffffffffffffffffffffffffffff0194e6ff000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000dcb5",
    );

    #[test]
    fn the_plan_shows_old_and_new_stages_and_default() {
        let before = sector([400, 800, 1600, 3200, 6400], 3);
        let after = sector([800, 1600, 2400, 3200, 4800], 1);
        let plan = render_plan(&before, &after);
        assert!(plan.contains("400, 800, 1600, 3200, 6400"), "{plan}");
        assert!(plan.contains("800, 1600, 2400, 3200, 4800"), "{plan}");
        assert!(plan.contains("default stage: 3 -> 1"), "{plan}");
    }

    #[test]
    fn the_plan_counts_the_bytes_that_change() {
        let same = sector([400, 800, 1600, 3200, 6400], 3);
        assert!(render_plan(&same, &same).contains("0 of 255 bytes change"));
        let after = sector([800, 1600, 2400, 3200, 4800], 1);
        let plan = render_plan(&same, &after);
        assert!(!plan.contains("nothing to write"), "{plan}");
        assert!(plan.contains("of 255 bytes change"), "{plan}");
    }

    #[test]
    fn an_unchanged_edit_says_there_is_nothing_to_write() {
        let same = sector([400, 800, 1600, 3200, 6400], 3);
        assert!(render_plan(&same, &same).contains("nothing to write"));
    }

    // -- the backup file ---------------------------------------------------

    fn backup() -> SectorBackup {
        SectorBackup {
            sector: 1,
            bytes: (0..255).map(|i| u8::try_from(i).unwrap()).collect(),
        }
    }

    #[test]
    fn a_backup_is_saved_and_read_back_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = save_and_verify_backup(dir.path(), "b.txt", &backup()).unwrap();
        assert_eq!(path, dir.path().join("b.txt"));
        assert_eq!(load_backup(&path).unwrap(), backup());
    }

    #[test]
    fn a_missing_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        let path = save_and_verify_backup(&nested, "b.txt", &backup()).unwrap();
        assert!(path.is_file());
    }

    /// A backup is never silently replaced: the old one may be the only copy
    /// of a profile that was already changed.
    #[test]
    fn an_existing_backup_file_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        save_and_verify_backup(dir.path(), "b.txt", &backup()).unwrap();
        let other = SectorBackup {
            sector: 2,
            bytes: vec![1, 2, 3],
        };
        save_and_verify_backup(dir.path(), "b.txt", &other).unwrap_err();
        assert_eq!(load_backup(&dir.path().join("b.txt")).unwrap(), backup());
    }

    #[test]
    fn a_name_with_a_path_in_it_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["../x.txt", "a/b.txt", "", "."] {
            assert!(
                save_and_verify_backup(dir.path(), name, &backup()).is_err(),
                "{name:?}"
            );
        }
    }

    #[test]
    fn loading_a_missing_or_damaged_file_says_why() {
        let dir = tempfile::tempdir().unwrap();
        load_backup(&dir.path().join("nope.txt")).unwrap_err();
        let bad = dir.path().join("bad.txt");
        std::fs::write(&bad, "this is not a backup\n").unwrap();
        load_backup(&bad).unwrap_err();
    }

    #[test]
    fn a_backup_file_with_a_trailing_newline_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.txt");
        std::fs::write(&path, format!("{}\n", backup().to_text())).unwrap();
        assert_eq!(load_backup(&path).unwrap(), backup());
    }
}
