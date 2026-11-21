//! One driver per data directory.
//!
//! The real backend keeps its state (accounts, the session ledger, plugin
//! data) as whole files under the local data directory, loaded at start-up
//! and saved back wholesale. Two processes driving the same directory —
//! the GUI and an MCP server, or two servers — would load independent
//! snapshots and overwrite each other's saves, losing session tracking.
//! So constructing a real backend takes an exclusive OS file lock on
//! `driver.lock` in the data directory, and a second process fails fast
//! with [`ErrorKind::DriverAlreadyRunning`] instead of joining the fight.
//!
//! Within one process the lock is shared through a registry: the GUI
//! rebuilds its backend in place (host settings), and a rebuild must not
//! fight the backend it is about to replace. The OS lock is released when
//! the last backend for the directory drops — and by the OS itself when a
//! process exits, however it exits, so a crash never wedges the directory.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use fs2::FileExt;

use crate::Result;
use crate::error::{Error, ErrorKind};

/// A held driver lock. Dropping the last clone for a directory releases
/// the OS lock.
pub struct DriverGuard {
    _file: File,
}

static HELD: OnceLock<Mutex<HashMap<PathBuf, Weak<DriverGuard>>>> = OnceLock::new();

/// Takes (or shares) the driver lock for `local_dir`.
pub fn acquire_driver_lock(local_dir: &Path) -> Result<Arc<DriverGuard>> {
    std::fs::create_dir_all(local_dir)?;
    // One canonical key per directory, so different spellings of the same
    // path share the lock instead of fighting over it.
    let key = std::fs::canonicalize(local_dir).unwrap_or_else(|_| local_dir.to_path_buf());
    let registry = HELD.get_or_init(|| Mutex::new(HashMap::new()));
    let mut held = registry.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(guard) = held.get(&key).and_then(Weak::upgrade) {
        return Ok(guard);
    }
    let path = key.join("driver.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    if file.try_lock_exclusive().is_err() {
        // Name the holder when it recorded itself; the pid is a courtesy
        // for the error message, never a correctness input.
        let mut text = String::new();
        let _ = file.seek(SeekFrom::Start(0));
        let _ = file.read_to_string(&mut text);
        let mut error = Error::new(ErrorKind::DriverAlreadyRunning)
            .with("path", path.display().to_string())
            .detail("another Colab Playground driver is already using this data directory");
        if let Ok(pid) = text.trim().parse::<u32>() {
            error = error.with("pid", pid.to_string());
        }
        return Err(error);
    }
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = writeln!(file, "{}", std::process::id());
    let _ = file.flush();
    let guard = Arc::new(DriverGuard { _file: file });
    held.insert(key, Arc::downgrade(&guard));
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_is_shared_within_the_process_and_locked_outside_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_driver_lock(dir.path()).unwrap();
        // A second acquire in this process shares the same lock.
        let second = acquire_driver_lock(dir.path()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        // An independent open of the lock file cannot take it.
        let outsider = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join("driver.lock"))
            .unwrap();
        assert!(outsider.try_lock_exclusive().is_err());

        // Once every guard is gone, the directory is free again.
        drop(first);
        drop(second);
        assert!(outsider.try_lock_exclusive().is_ok());
    }

    #[test]
    fn different_directories_lock_independently() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _ga = acquire_driver_lock(a.path()).unwrap();
        let _gb = acquire_driver_lock(b.path()).unwrap();
    }
}
