use atomicwrites::{AtomicFile, OverwriteBehavior};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std::fs_utf8::Dir;
use color_eyre::{eyre::Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fmt, fs,
    io::{self, Write},
};

/// An exclusive lock on a lock file, held until it is unlocked or dropped.
#[derive(Debug)]
#[must_use]
pub(crate) struct ExclusiveLock {
    // Can't use cap_std::fs_utf8::File as it doesn't support fs2 or locking, sadly.
    file: fs::File,
    lock_path: Utf8PathBuf,
}

impl ExclusiveLock {
    /// Creates `lock_name` in `dir` if it doesn't exist, and blocks until it is locked.
    pub(crate) fn acquire(dir: &DirWithPath, lock_name: &str) -> Result<Self> {
        let mut open_opts = cap_std::fs::OpenOptions::new();
        open_opts.write(true).create(true);
        let lock_path = dir.path().join(lock_name);

        // cap-std opens with `O_CLOEXEC`, so a process that targo execs never inherits the lock.
        let file = dir
            .dir()
            .open_with(lock_name, &open_opts)
            .wrap_err_with(|| format!("failed to open lock at `{lock_path}`"))?
            .into_std();
        file.lock_exclusive()
            .wrap_err_with(|| format!("failed to obtain exclusive lock at `{lock_path}`"))?;
        Ok(Self { file, lock_path })
    }

    /// Releases the lock. Dropping releases it too, but cannot report a failure.
    pub(crate) fn unlock(self) -> Result<()> {
        FileExt::unlock(&self.file)
            .wrap_err_with(|| format!("failed to release lock at `{}`", self.lock_path))
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
