//! Small filesystem helpers shared across applets

use std::io::Write;

use anyhow::{Context, Result};
use camino::Utf8Path;
use tempfile::NamedTempFile;

/// Write `contents` to `path` only if it does not already exist (idempotent
/// scaffolding for `em setup` / `em crossdev`, which must not clobber a file the
/// user or a previous run wrote).
pub(crate) fn write_if_absent(path: &Utf8Path, contents: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::write(path, contents).with_context(|| format!("writing {path}"))
}

/// Atomically replace `path` with `contents`
///
/// Creates a temporary file in the same directory as `path` (so rename stays
/// on one filesystem), writes `contents`, then [`NamedTempFile::persist`]s it
/// over the destination. A crash mid-write cannot leave a truncated final
/// file; a failed attempt cleans up the temp on drop.
///
/// Creates the parent directory if it does not exist.
pub(crate) fn write_atomic(path: &Utf8Path, contents: impl AsRef<[u8]>) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_str().is_empty())
        .unwrap_or_else(|| Utf8Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {parent}"))?;
    let mut tmp = NamedTempFile::new_in(parent.as_std_path())
        .with_context(|| format!("tempfile in {parent}"))?;
    tmp.write_all(contents.as_ref())
        .with_context(|| format!("writing temp for {path}"))?;
    tmp.persist(path.as_std_path())
        .map_err(|e| e.error)
        .with_context(|| format!("persisting {path}"))?;
    Ok(())
}

/// Lock the state file `path` itself, as portage's `lockfile` does for its config memory and
/// preserved-libs registry, so em and emerge exclude each other while one rewrites it
///
/// A missing file is created empty, mode 0660 in group portage. Takes an exclusive `flock` and a
/// POSIX record lock, and retries when a writer renamed a new file over the one locked. Blocks
/// until held; released on drop. `None` when the file cannot be opened (a read-only root).
pub(crate) fn lock_state_file(path: &Utf8Path) -> Option<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir.as_std_path());
    }
    loop {
        let created = std::fs::symlink_metadata(path.as_std_path()).is_err();
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o660)
            .open(path.as_std_path())
            .ok()?;
        if created {
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o660));
            let gid =
                portage_repo::userdb::id_in_file(std::path::Path::new("/etc/group"), "portage");
            let _ = std::os::unix::fs::fchown(&f, None, gid);
        }
        rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive).ok()?;
        while rustix::fs::fcntl_lock(&f, rustix::fs::FlockOperation::LockExclusive)
            == Err(rustix::io::Errno::INTR)
        {}
        let held = rustix::fs::fstat(&f).ok()?;
        if let Ok(now) = rustix::fs::stat(path.as_std_path())
            && (now.st_dev, now.st_ino) == (held.st_dev, held.st_ino)
        {
            return Some(f);
        }
    }
}

/// Read the state file `path` through its `lock` when held
///
/// Opening `path` again would drop the record lock when that fd closes, as POSIX record locks are per process.
pub(crate) fn read_state_file(
    path: &Utf8Path,
    lock: Option<&std::fs::File>,
) -> std::io::Result<String> {
    use std::io::{Read, Seek};
    let Some(mut f) = lock else {
        return std::fs::read_to_string(path.as_std_path());
    };
    let mut text = String::new();
    f.rewind()?;
    f.read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_state_file_creates_the_file_and_follows_a_replacement() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let path = Utf8Path::from_path(tmp.path())
            .unwrap()
            .join("var/lib/portage/config");
        let first = lock_state_file(&path).unwrap();
        let meta = std::fs::metadata(path.as_std_path()).unwrap();
        assert_eq!((meta.len(), meta.permissions().mode() & 0o777), (0, 0o660));

        let waiter = std::thread::spawn({
            let path = path.clone();
            move || lock_state_file(&path).map(|f| f.metadata().unwrap().ino())
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_atomic(&path, "replaced\n").unwrap();
        let replaced = std::fs::metadata(path.as_std_path()).unwrap().ino();
        drop(first);
        assert_eq!(waiter.join().unwrap(), Some(replaced));
    }
}
