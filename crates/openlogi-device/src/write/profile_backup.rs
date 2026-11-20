//! The backup file every profile write saves first, and reading one back.
//!
//! Pure file handling, tested here; the CLI's `profile-write` and the agent's
//! profile save both go through it.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use hidpp::feature::onboard_profiles::profile::SectorBackup;

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

#[cfg(test)]
mod tests {
    use super::*;

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
