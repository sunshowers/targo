use atomicwrites::{AtomicFile, OverwriteBehavior};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use cap_std::fs_utf8::Dir;
use color_eyre::{
    eyre::{bail, Context},
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    fs::{self, TryLockError},
    io::{self, Write},
    path::PathBuf,
};

/// An exclusive lock on a lock file, held until it is unlocked or dropped.
#[derive(Debug)]
#[must_use]
pub(crate) struct ExclusiveLock {
    // Can't use cap_std::fs_utf8::File as it doesn't support locking, sadly.
    file: fs::File,
    lock_path: Utf8PathBuf,
}

impl ExclusiveLock {
    /// Creates `lock_name` in `dir` if it doesn't exist, and blocks until it is locked.
    pub(crate) fn acquire(dir: &DirWithPath, lock_name: &str) -> Result<Self> {
        let (file, lock_path) = Self::open(dir, lock_name)?;
        file.lock()
            .wrap_err_with(|| format!("failed to obtain exclusive lock at `{lock_path}`"))?;
        Ok(Self { file, lock_path })
    }

    /// Like [`Self::acquire`], but returns [`TryLock::Busy`] instead of blocking.
    pub(crate) fn try_acquire(dir: &DirWithPath, lock_name: &str) -> Result<TryLock<Self>> {
        let (file, lock_path) = Self::open(dir, lock_name)?;
        let attempt = try_lock_exclusive(file)
            .wrap_err_with(|| format!("failed to obtain exclusive lock at `{lock_path}`"))?;
        Ok(match attempt {
            TryLock::Acquired(file) => TryLock::Acquired(Self { file, lock_path }),
            TryLock::Busy => TryLock::Busy,
        })
    }

    fn open(dir: &DirWithPath, lock_name: &str) -> Result<(fs::File, Utf8PathBuf)> {
        let mut open_opts = cap_std::fs::OpenOptions::new();
        open_opts.write(true).create(true);
        let lock_path = dir.path().join(lock_name);

        // cap-std opens with `O_CLOEXEC`, so a process that targo execs never inherits the lock.
        let file = dir
            .dir()
            .open_with(lock_name, &open_opts)
            .wrap_err_with(|| format!("failed to open lock at `{lock_path}`"))?
            .into_std();
        Ok((file, lock_path))
    }

    /// Releases the lock. Dropping releases it too, but cannot report a failure.
    pub(crate) fn unlock(self) -> Result<()> {
        self.file
            .unlock()
            .wrap_err_with(|| format!("failed to release lock at `{}`", self.lock_path))
    }
}

/// What came of trying to take a lock without blocking.
#[derive(Debug)]
#[must_use]
pub(crate) enum TryLock<T> {
    Acquired(T),
    /// Something else holds the lock.
    Busy,
}

/// Tries to lock `file` exclusively without blocking. The lock is held until the file is dropped.
///
/// This is the call that Cargo locks its build directories with.
pub(crate) fn try_lock_exclusive(file: fs::File) -> io::Result<TryLock<fs::File>> {
    match file.try_lock() {
        Ok(()) => Ok(TryLock::Acquired(file)),
        Err(TryLockError::WouldBlock) => Ok(TryLock::Busy),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// A wrapper for `Dir` that also stores its path, for easier debuggability.
#[derive(Debug)]
pub(crate) struct DirWithPath {
    dir: Dir,
    path: Utf8PathBuf,
}

impl DirWithPath {
    pub(crate) fn new(dir: Dir, path: Utf8PathBuf) -> Self {
        Self { dir, path }
    }

    pub(crate) fn dir(&self) -> &Dir {
        &self.dir
    }

    pub(crate) fn path(&self) -> &Utf8Path {
        &self.path
    }

    pub(crate) fn read_metadata<T>(&self, file_name: &str) -> Result<Option<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        let reader = match self.dir.open(file_name) {
            Ok(reader) => io::BufReader::new(reader),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).wrap_err_with(|| {
                    format!(
                        "could not read targo metadata from file `{}`",
                        self.path.join(file_name)
                    )
                })
            }
        };
        Ok(Some(serde_json::from_reader(reader).wrap_err_with(
            || {
                format!(
                    "failed to deserialize metadata from `{}`",
                    self.path.join(file_name)
                )
            },
        )?))
    }

    pub(crate) fn write_metadata<T>(&self, file_name: &str, metadata: &T) -> Result<()>
    where
        T: Serialize + fmt::Debug,
    {
        // cap-std doesn't expose a way to write files atomically, so we use the path directly.
        let json = serde_json::to_string(metadata)
            .wrap_err_with(|| format!("failed to serialize metadata {metadata:?}",))?;

        let path = self.path.join(file_name);
        let file = AtomicFile::new(&path, OverwriteBehavior::AllowOverwrite);
        file.write(|f| f.write_all(json.as_bytes()))
            .wrap_err_with(|| format!("failed to write metadata to `{}`", path))?;

        Ok(())
    }
}

/// Resolves symlinks and `..` in the absolute path `path`, which doesn't have to exist.
///
/// Components that don't exist are kept as they are: creating them makes real directories.
pub(crate) fn resolve_location(path: &Utf8Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("`{path}` is not an absolute path");
    }
    let mut location = PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::Prefix(_) | Utf8Component::RootDir => location.push(component.as_str()),
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                // `location` has no symlinks left in it, so its parent is the real parent.
                location.pop();
            }
            Utf8Component::Normal(name) => {
                location.push(name);
                match location.symlink_metadata() {
                    Ok(metadata) if metadata.is_symlink() => {
                        location = location.canonicalize().wrap_err_with(|| {
                            format!("failed to resolve symlink `{}`", location.display())
                        })?;
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(err).wrap_err_with(|| {
                            format!("failed to read metadata for `{}`", location.display())
                        });
                    }
                }
            }
        }
    }
    Ok(location)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{os::unix::fs::symlink, path::Path};

    /// Opens a lock file to probe or hold its lock.
    pub(crate) fn open_lock_file(path: impl AsRef<Path>) -> fs::File {
        // Read-write, as Cargo opens its lock files.
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("opened lock file")
    }

    #[test]
    fn test_resolve_location() {
        let temp_dir = camino_tempfile::tempdir().expect("created temp dir");
        // A temp dir can be behind a symlink (as on macOS).
        let root = temp_dir.path().canonicalize_utf8().expect("canonicalized");
        fs::create_dir_all(root.join("real/inner")).expect("created dirs");
        fs::write(root.join("file"), "").expect("wrote file");
        symlink(root.join("real/inner"), root.join("link")).expect("created symlink");
        symlink("real", root.join("relative-link")).expect("created symlink");
        symlink(root.join("nowhere"), root.join("dangling")).expect("created symlink");
        symlink("relative-link/inner/..", root.join("chain")).expect("created symlink");
        symlink("file", root.join("file-link")).expect("created symlink");

        let data = [
            ("real/inner", "real/inner"),
            ("real/./inner/", "real/inner"),
            ("file", "file"),
            ("missing/dir", "missing/dir"),
            ("link", "real/inner"),
            ("link/missing", "real/inner/missing"),
            ("relative-link/inner", "real/inner"),
            ("chain/inner", "real/inner"),
            ("file-link", "file"),
            // `..` after a symlink is the parent of where the symlink leads.
            ("link/..", "real"),
            ("link/../missing", "real/missing"),
            // `..` after a missing component is the directory that the component would be in.
            ("missing/../link", "real/inner"),
            ("real/missing/dir/../../../link/dir", "real/inner/dir"),
        ];
        for (input, expected) in data {
            let path = root.join(input);
            let location = resolve_location(&path).expect("resolved");
            assert_eq!(location, root.join(expected), "for {input:?}");
            if let Ok(canonical) = path.canonicalize() {
                assert_eq!(
                    location, canonical,
                    "for {input:?}, which the OS can resolve"
                );
            }
        }
        assert_eq!(
            resolve_location("/..".into()).expect("resolved"),
            PathBuf::from("/"),
            "the root is its own parent"
        );

        let error = resolve_location("real/inner".into()).expect_err("resolution failed");
        assert_eq!(error.to_string(), "`real/inner` is not an absolute path");

        let error_data = [
            (
                "dangling/dir",
                format!("failed to resolve symlink `{}`", root.join("dangling")),
            ),
            (
                "file/dir",
                format!("failed to read metadata for `{}`", root.join("file/dir")),
            ),
        ];
        for (input, expected) in error_data {
            let error = resolve_location(&root.join(input)).expect_err("resolution failed");
            assert_eq!(error.to_string(), expected, "for {input:?}");
        }
    }
}
