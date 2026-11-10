//! A size-capped log file for `tracing` (DEV-1243).
//!
//! `Allele.app` launched from Finder or with `open` has its stdout thrown
//! away, so before this the only record of a field problem was whatever the
//! user remembered. Every line now also goes to `~/.allele/logs/allele.log`.
//!
//! ```text
//!   allele.log      ← written now
//!   allele.log.1    ← the previous MAX_BYTES
//!   …
//!   allele.log.N    ← oldest kept; the next rotation deletes it
//! ```
//!
//! Rotation is by size, checked before each write, so the directory never
//! holds more than about `(KEEP + 1) × MAX_BYTES`. One `tracing` event is one
//! write, so a line is never split across two files.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// Rotate once the live file would pass this.
pub(crate) const MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Rotated files kept beside the live one: 5 MiB × (1 + 4) ≈ 25 MiB at most.
pub(crate) const KEEP: usize = 4;

/// The live file's name inside the logs directory.
pub(crate) const FILE_NAME: &str = "allele.log";

struct State {
    file: File,
    /// Bytes in the live file.
    len: u64,
}

/// A log file that rotates itself. Cheap to clone; every clone writes to the
/// same file under one lock.
#[derive(Clone)]
pub(crate) struct RotatingLog {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    state: Arc<Mutex<State>>,
}

impl RotatingLog {
    /// Open (or create) `dir/name` for appending, creating `dir` too. The
    /// file is owner-only: it names peers and their network addresses.
    pub(crate) fn open(dir: &Path, name: &str, max_bytes: u64, keep: usize) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(name);
        let file = open_append(&path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            path,
            max_bytes,
            keep,
            state: Arc::new(Mutex::new(State { file, len })),
        })
    }

    /// The live file.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `allele.log.n`.
    fn rotated(&self, n: usize) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    /// Shift `.1…` up one, dropping the oldest, and start an empty live file.
    fn rotate(&self, state: &mut State) -> io::Result<()> {
        state.file.flush()?;
        if self.keep == 0 {
            let _ = fs::remove_file(&self.path);
        } else {
            let _ = fs::remove_file(self.rotated(self.keep));
            for n in (1..self.keep).rev() {
                let from = self.rotated(n);
                if from.exists() {
                    fs::rename(&from, self.rotated(n + 1))?;
                }
            }
            fs::rename(&self.path, self.rotated(1))?;
        }
        state.file = open_append(&self.path)?;
        state.len = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // A non-empty file that this write would push past the cap rotates
        // first. An empty one takes the write whatever its size, so one huge
        // line cannot rotate forever.
        if state.len > 0 && state.len + buf.len() as u64 > self.max_bytes {
            // A failed rotation keeps writing to the old file rather than
            // losing the line; the cap is a target, the log is the point.
            let _ = self.rotate(&mut state);
        }
        let n = state.file.write(buf)?;
        state.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .file
            .flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("allele-log-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn lines_land_in_the_file() {
        let d = dir();
        let mut log = RotatingLog::open(&d.join("logs"), "a.log", 1024, 2).unwrap();
        log.write_all(b"hello\n").unwrap();
        log.flush().unwrap();
        assert_eq!(fs::read_to_string(d.join("logs/a.log")).unwrap(), "hello\n");
    }

    #[test]
    fn reopening_appends_rather_than_truncating() {
        let d = dir();
        RotatingLog::open(&d, "a.log", 1024, 2)
            .unwrap()
            .write_all(b"one\n")
            .unwrap();
        RotatingLog::open(&d, "a.log", 1024, 2)
            .unwrap()
            .write_all(b"two\n")
            .unwrap();
        assert_eq!(fs::read_to_string(d.join("a.log")).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn the_directory_is_capped_at_keep_plus_one_files() {
        let d = dir();
        let mut log = RotatingLog::open(&d, "a.log", 100, 2).unwrap();
        // 10 lines of 40 bytes: two fit per 100-byte file.
        for i in 0..10 {
            log.write_all(format!("{i:039}\n").as_bytes()).unwrap();
        }
        assert_eq!(files(&d), vec!["a.log", "a.log.1", "a.log.2"]);
        let total: u64 = files(&d)
            .iter()
            .map(|f| fs::metadata(d.join(f)).unwrap().len())
            .sum();
        assert!(total <= 3 * 100, "{total} bytes kept");
        // Newest in the live file, the one before it in .1.
        assert!(fs::read_to_string(d.join("a.log"))
            .unwrap()
            .ends_with(&format!("{:039}\n", 9)));
        assert!(fs::read_to_string(d.join("a.log.1"))
            .unwrap()
            .starts_with(&format!("{:039}\n", 6)));
    }

    #[test]
    fn an_oversized_line_is_written_not_rotated_forever() {
        let d = dir();
        let mut log = RotatingLog::open(&d, "a.log", 10, 1).unwrap();
        log.write_all(&[b'y'; 50]).unwrap();
        assert_eq!(fs::metadata(d.join("a.log")).unwrap().len(), 50);
        log.write_all(b"next\n").unwrap();
        assert_eq!(fs::read_to_string(d.join("a.log")).unwrap(), "next\n");
        assert_eq!(fs::metadata(d.join("a.log.1")).unwrap().len(), 50);
    }

    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = dir();
        let log = RotatingLog::open(&d, "a.log", 100, 1).unwrap();
        let mode = fs::metadata(log.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
