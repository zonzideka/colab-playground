//! The one-driver rule from the MCP design: the GUI and this server share one
//! data directory and both write the ledger, and core's state lock is
//! in-process only. The first driver to take the lock file in the data
//! directory serves; a second one fails fast instead of interleaving ledger
//! writes. The lock is an OS file lock, so it is released when the holder
//! exits, however it exits — the file itself only records the holder's pid
//! for the error message.

use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use fs2::FileExt;

#[derive(Debug)]
pub struct DriverLock {
    _file: File,
}

#[derive(Debug)]
pub enum DriverLockError {
    AlreadyRunning { pid: Option<u32>, path: PathBuf },
    Io(std::io::Error),
}

impl fmt::Display for DriverLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverLockError::AlreadyRunning { pid, path } => match pid {
                Some(pid) => write!(
                    f,
                    "driver_already_running: another driver (pid {pid}) holds {}",
                    path.display()
                ),
                None => write!(
                    f,
                    "driver_already_running: another driver holds {}",
                    path.display()
                ),
            },
            DriverLockError::Io(e) => write!(f, "driver lock: {e}"),
        }
    }
}

impl std::error::Error for DriverLockError {}

impl From<std::io::Error> for DriverLockError {
    fn from(e: std::io::Error) -> Self {
        DriverLockError::Io(e)
    }
}

impl DriverLock {
    pub fn acquire(data_dir: &Path) -> Result<DriverLock, DriverLockError> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join("driver.lock");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        if file.try_lock_exclusive().is_err() {
            let mut text = String::new();
            let _ = file.read_to_string(&mut text);
            return Err(DriverLockError::AlreadyRunning {
                pid: text.trim().parse().ok(),
                path,
            });
        }
        // Record our pid so the next contender's error can name the holder.
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        write!(file, "{}", std::process::id())?;
        file.flush()?;
        Ok(DriverLock { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_driver_is_refused_until_the_first_releases() {
        let dir = tempfile::tempdir().unwrap();
        let first = DriverLock::acquire(dir.path()).unwrap();
        let err = DriverLock::acquire(dir.path()).unwrap_err();
        assert!(
            matches!(err, DriverLockError::AlreadyRunning { pid: Some(p), .. } if p == std::process::id()),
            "{err}"
        );
        drop(first);
        let _second = DriverLock::acquire(dir.path()).unwrap();
    }
}
