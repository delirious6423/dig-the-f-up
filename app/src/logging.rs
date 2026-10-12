//! A small local log file for the desktop app, std only.
//!
//! - One file: `<data>/logs/dtfu.log`. When it reaches [`MAX_BYTES`] (1 MiB) it is renamed to `dtfu.log.1`
//!   (replacing the previous `.1`) and a new file starts. So at most about 2 MiB is kept.
//! - One line per event: `2026-10-12T09:30:00Z ERROR message`, UTC.
//! - Local only. Nothing is sent anywhere. Messages can contain file paths (the folders you index), so the file is
//!   private to you; attach it to a bug report only if you choose to.
//! - Logging never panics and never fails the app: write errors are ignored.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// The active log file name.
pub const FILE_NAME: &str = "dtfu.log";
/// Rotate once the active file reaches this size.
pub const MAX_BYTES: u64 = 1024 * 1024;

struct Log {
    path: PathBuf,
    write: Mutex<()>,
}

static LOG: OnceLock<Log> = OnceLock::new();

/// Sets the log folder and creates the log file. Call once at startup; later calls are ignored. Returns the log
/// file path, or the error when the folder cannot be created.
pub fn init(dir: &Path) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(FILE_NAME);
    OpenOptions::new().create(true).append(true).open(&path)?;
    let _ = LOG.set(Log { path: path.clone(), write: Mutex::new(()) });
    Ok(path)
}

/// The active log file, once [`init`] has succeeded.
pub fn log_path() -> Option<PathBuf> {
    LOG.get().map(|l| l.path.clone())
}

/// Appends one timestamped line. Does nothing before [`init`].
pub fn log(level: &str, message: &str) {
    let Some(log) = LOG.get() else { return };
    let Ok(_guard) = log.write.lock() else { return };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let line = format_line(now, level, message);
    let _ = append(&log.path, &line, MAX_BYTES);
}

/// Logs an error line.
pub fn error(message: &str) {
    log("ERROR", message);
}

/// Logs an informational line.
pub fn info(message: &str) {
    log("INFO", message);
}

/// Appends `line` to `path`, first rotating `path` to `path.1` if it already holds `max_bytes` or more.
pub fn append(path: &Path, line: &str, max_bytes: u64) -> std::io::Result<()> {
    rotate_if_needed(path, max_bytes)?;
    let mut file: File = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

/// If `path` is at least `max_bytes` long, moves it to `<path>.1` (replacing any previous `.1`).
/// Returns true when it rotated.
pub fn rotate_if_needed(path: &Path, max_bytes: u64) -> std::io::Result<bool> {
    let len = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if len < max_bytes {
        return Ok(false);
    }
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    fs::rename(path, PathBuf::from(rotated))?;
    Ok(true)
}

/// `"<UTC timestamp> <LEVEL> <message>\n"` for the Unix time `secs`. Newlines in `message` become spaces so one
/// event is always one line.
pub fn format_line(secs: u64, level: &str, message: &str) -> String {
    let one_line = message.replace(['\r', '\n'], " ");
    format!("{} {level} {one_line}\n", iso_utc(secs))
}

/// ISO 8601 UTC, second precision, for Unix time `secs` (proleptic Gregorian, no leap seconds).
pub fn iso_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_correct_utc() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        // 2000-02-29 is a leap day.
        assert_eq!(iso_utc(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn line_is_one_line_and_keeps_level() {
        let line = format_line(0, "ERROR", "bad\nthing\r\nhere");
        assert_eq!(line, "1970-01-01T00:00:00Z ERROR bad thing  here\n");
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn rotates_at_threshold_and_keeps_one_backup() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        fs::write(&p, vec![b'a'; 10]).unwrap();
        // 10 bytes against a 10-byte limit: rotates.
        assert!(rotate_if_needed(&p, 10).unwrap());
        assert!(!p.exists());
        assert_eq!(fs::read(d.path().join("dtfu.log.1")).unwrap().len(), 10);

        // Second rotation replaces the old backup.
        append(&p, "first\n", 10).unwrap();
        append(&p, "0123456789abcdef\n", 10).unwrap();
        append(&p, "after\n", 10).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "after\n");
        // "first" (6 bytes) stays below the limit, so it shares a file with the next line; the third append rotates.
        assert_eq!(fs::read_to_string(d.path().join("dtfu.log.1")).unwrap(), "first\n0123456789abcdef\n");
        assert!(!d.path().join("dtfu.log.2").exists());
    }

    #[test]
    fn below_threshold_does_not_rotate() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE_NAME);
        append(&p, "small\n", 1024).unwrap();
        assert!(!rotate_if_needed(&p, 1024).unwrap());
        assert!(!d.path().join("dtfu.log.1").exists());
        assert_eq!(fs::read_to_string(&p).unwrap(), "small\n");
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(!rotate_if_needed(&d.path().join("nope.log"), 1).unwrap());
    }
}
