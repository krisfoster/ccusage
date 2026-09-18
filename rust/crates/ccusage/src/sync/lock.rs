//! A local advisory lock so one machine runs one sync at a time.
//!
//! Compare-and-swap in the bucket already makes two concurrent syncs safe —
//! neither can lose the other's writes — but two syncs on *one* machine are
//! never useful: they fold the same logs, upload the same shards over each
//! other, and race each other's rollup passes until one gives up with "the
//! bucket's rollups kept changing under this sync". That reads like a bucket
//! problem when it is really a double-click or a cron job overlapping itself.
//!
//! The lock is advisory and local. It cannot order syncs on *different*
//! machines, which is what the bucket's preconditions are for.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// A lock left behind by a process that died is broken after this long: the
/// alternative is a machine that never syncs again until the user finds a file
/// nobody told them about. Longer than any real sync, short enough that a
/// crashed run costs one skipped scheduled sync rather than a day of them.
const STALE_AFTER_MS: i64 = 60 * 60 * 1000;

/// Held for as long as a sync is running. Dropping it releases the lock,
/// including on the error paths, which is the whole reason it is a guard type
/// rather than a pair of functions.
#[derive(Debug)]
pub(crate) struct SyncLock {
    path: PathBuf,
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Takes the lock, or explains who has it.
///
/// `create_new` is the whole of the mutual exclusion: it is one atomic
/// filesystem operation, so two processes arriving together cannot both
/// believe they created the file.
pub(crate) fn acquire(path: &Path, now_ms: i64) -> Result<SyncLock, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    match create(path, now_ms) {
        Ok(()) => Ok(SyncLock {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if is_stale(path, now_ms) {
                // Whoever wrote it is long gone. Remove and retry once: if
                // another process wins the retry, it holds a fresh lock and
                // this one waits its turn like any other.
                let _ = fs::remove_file(path);
                return create(path, now_ms)
                    .map(|()| SyncLock {
                        path: path.to_path_buf(),
                    })
                    .map_err(|_| busy(path));
            }
            Err(busy(path))
        }
        Err(error) => Err(format!("could not lock {}: {error}", path.display())),
    }
}

fn create(path: &Path, now_ms: i64) -> std::io::Result<()> {
    let mut file: File = OpenOptions::new().write(true).create_new(true).open(path)?;
    write!(file, "{}\n{now_ms}", std::process::id())
}

fn busy(path: &Path) -> String {
    format!(
        "another 'ccusage sync' is already running on this machine. Wait for it to finish, or delete {} if no sync is running.",
        path.display()
    )
}

/// A lock file with no readable timestamp is treated as stale rather than
/// permanent: an unparseable lock is damage, and damage that blocks every
/// future sync is worse than one that is cleared.
fn is_stale(path: &Path, now_ms: i64) -> bool {
    let mut contents = String::new();
    let Ok(mut file) = File::open(path) else {
        return true;
    };
    if file.read_to_string(&mut contents).is_err() {
        return true;
    }
    match contents
        .lines()
        .nth(1)
        .and_then(|line| line.trim().parse::<i64>().ok())
    {
        Some(written_ms) => now_ms.saturating_sub(written_ms) > STALE_AFTER_MS,
        None => true,
    }
}

/// Where the lock lives: beside the state this machine already keeps, so it is
/// per-user and survives nothing being writable in the repo.
pub(crate) fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| ccusage_core::home::home_dir().map(|home| home.join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("ccusage").join("sync.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: i64 = 1_789_000_000_000;

    fn lock_path(dir: &assert_fs::TempDir) -> PathBuf {
        dir.path().join("state").join("sync.lock")
    }

    #[test]
    fn the_first_sync_takes_the_lock() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = lock_path(&dir);

        let _held = acquire(&path, NOW_MS).expect("the lock is free");

        assert!(path.exists());
    }

    #[test]
    fn a_second_sync_on_the_same_machine_is_refused_and_told_where_the_lock_is() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = lock_path(&dir);
        let _held = acquire(&path, NOW_MS).expect("the lock is free");

        let error = acquire(&path, NOW_MS).expect_err("the lock is held");

        assert!(error.contains("already running"), "{error}");
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

    #[test]
    fn releasing_the_lock_lets_the_next_sync_run() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = lock_path(&dir);

        drop(acquire(&path, NOW_MS).expect("the lock is free"));

        assert!(!path.exists(), "the lock file outlived the guard");
        acquire(&path, NOW_MS).expect("the lock is free again");
    }

    #[test]
    fn a_lock_left_by_a_crashed_sync_is_broken_rather_than_blocking_forever() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = lock_path(&dir);
        std::mem::forget(acquire(&path, NOW_MS).expect("the lock is free"));

        acquire(&path, NOW_MS + STALE_AFTER_MS + 1).expect("the stale lock is broken");
    }

    #[test]
    fn a_lock_from_a_sync_that_is_merely_slow_is_still_honored() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = lock_path(&dir);
        std::mem::forget(acquire(&path, NOW_MS).expect("the lock is free"));

        acquire(&path, NOW_MS + STALE_AFTER_MS - 1).expect_err("the lock is still held");
    }

    #[test]
    fn an_unreadable_lock_file_does_not_wedge_the_machine() {
        let dir = assert_fs::TempDir::new().expect("temp dir");
        let path = lock_path(&dir);
        fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
        fs::write(&path, b"not a lock").expect("write");

        acquire(&path, NOW_MS).expect("the damaged lock is replaced");
    }
}
