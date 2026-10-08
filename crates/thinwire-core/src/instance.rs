//! One thinwire at a time.
//!
//! Two copies would open the same TDLib database and the same WhatsApp and
//! Signal stores. TDLib's own lock on `td.binlog` refuses the second client,
//! but only after that copy already started its other protocols. This lock
//! is taken at startup, before any protocol starts.
//!
//! It is an advisory exclusive lock on `instance.lock` in the thinwire data
//! folder (`File::try_lock`: `flock` on Unix, `LockFileEx` on Windows). The
//! OS drops it when the process ends, also after a crash or a kill, so a
//! stale lock file never blocks a start.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// File name of the lock in the thinwire data folder.
pub const LOCK_FILE_NAME: &str = "instance.lock";

/// Pause between two tries while another copy holds the lock.
pub const RETRY_STEP: Duration = Duration::from_millis(100);

/// The lock of this process. Keep it alive until the process ends.
#[derive(Debug)]
pub struct InstanceLock {
    /// The OS lock lives as long as this handle is open.
    _file: File,
    path: PathBuf,
}

/// Why the lock was not taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock: another thinwire runs or still closes.
    Held,
    /// The lock file could not be opened or locked, for example a read-only
    /// folder or a file system without locks.
    Io(io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held => f.write_str("another thinwire holds the lock"),
            Self::Io(error) => write!(f, "the lock file failed: {error}"),
        }
    }
}

impl std::error::Error for LockError {}

impl InstanceLock {
    /// `<data dir>/thinwire/instance.lock`, the folder of the mutes file.
    /// `None` when the platform has no data folder.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        dirs::data_dir().map(|root| root.join("thinwire").join(LOCK_FILE_NAME))
    }

    /// Take the lock once, without waiting.
    ///
    /// # Errors
    ///
    /// [`LockError::Held`] while another process holds it, or
    /// [`LockError::Io`] when the file cannot be opened or locked.
    pub fn try_acquire(path: &Path) -> Result<Self, LockError> {
        if let Some(dir) = path.parent() {
            create_private_dir_all(dir).map_err(LockError::Io)?;
        }
        // Never truncate: the file has no content, and the holder keeps it open.
        // Rust opens this with O_CLOEXEC (a non-inheritable handle on Windows),
        // so a helper child does not inherit the lock.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(LockError::Io)?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                _file: file,
                path: path.to_path_buf(),
            }),
            Err(TryLockError::WouldBlock) => Err(LockError::Held),
            Err(TryLockError::Error(error)) => Err(LockError::Io(error)),
        }
    }

    /// Try every [`RETRY_STEP`] while the lock is held, until `deadline`.
    /// An I/O error ends the wait at once: waiting does not fix it.
    ///
    /// # Errors
    ///
    /// [`LockError::Held`] when the lock is still held at `deadline`, or the
    /// first [`LockError::Io`].
    pub fn acquire_until(path: &Path, deadline: Instant) -> Result<Self, LockError> {
        loop {
            match Self::try_acquire(path) {
                Err(LockError::Held) if Instant::now() < deadline => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(RETRY_STEP.min(left));
                }
                result => return result,
            }
        }
    }

    /// Where the lock file is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Create the folder and its parents. The last folder is private to this
/// user on Unix, like the other thinwire data folders.
fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        std::env::temp_dir()
            .join(format!(
                "thinwire-instance-{name}-{}-{nanos}",
                std::process::id()
            ))
            .join("thinwire")
            .join(LOCK_FILE_NAME)
    }

    fn cleanup(path: &Path) {
        if let Some(root) = path.parent().and_then(Path::parent) {
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn a_second_acquire_fails_while_held_and_succeeds_after_release() {
        let path = scratch("second");
        let first = InstanceLock::try_acquire(&path).expect("first copy");
        assert_eq!(first.path(), path);
        assert!(
            matches!(InstanceLock::try_acquire(&path), Err(LockError::Held)),
            "a second copy must not start while the first holds the lock"
        );
        drop(first);
        let again = InstanceLock::try_acquire(&path).expect("free after release");
        drop(again);
        cleanup(&path);
    }

    #[test]
    fn a_lock_file_left_behind_does_not_block_a_start() {
        let path = scratch("stale");
        fs::create_dir_all(path.parent().expect("dir")).expect("dir");
        fs::write(&path, b"").expect("old file");
        InstanceLock::try_acquire(&path).expect("no process holds it");
        cleanup(&path);
    }

    #[test]
    fn the_wait_ends_when_the_holder_lets_go() {
        let path = scratch("wait");
        let first = InstanceLock::try_acquire(&path).expect("first copy");
        let release = std::thread::spawn(move || {
            std::thread::sleep(RETRY_STEP * 2);
            drop(first);
        });
        let start = Instant::now();
        let lock = InstanceLock::acquire_until(&path, start + Duration::from_secs(5))
            .expect("taken after the first copy closed");
        assert!(start.elapsed() < Duration::from_secs(5));
        release.join().expect("release thread");
        drop(lock);
        cleanup(&path);
    }

    #[test]
    fn the_wait_gives_up_at_the_deadline() {
        let path = scratch("deadline");
        let _first = InstanceLock::try_acquire(&path).expect("first copy");
        let start = Instant::now();
        let result = InstanceLock::acquire_until(&path, start + RETRY_STEP * 3);
        assert!(matches!(result, Err(LockError::Held)));
        assert!(start.elapsed() >= RETRY_STEP * 3);
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_folder_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch("private");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let mode = fs::metadata(path.parent().expect("dir"))
            .expect("dir")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        cleanup(&path);
    }
}
