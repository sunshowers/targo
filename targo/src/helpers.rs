use camino::{Utf8Path, Utf8PathBuf};
use cap_std::fs_utf8::Dir;
use color_eyre::{eyre::Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{fs, io};

#[derive(Debug)]
pub(crate) struct UnlockedRoot<T> {
    // Can't use cap_std::fs_utf8::File as it doesn't support fs2 or locking, sadly.
    file: fs::File,
    lock_path: Utf8PathBuf,
    pub(crate) ctx: T,
}

impl<T: AsLockedCtx> UnlockedRoot<T> {
    pub(crate) fn new(ctx: T) -> Result<Self> {
        let (dir, lock_name) = ctx.dir_and_lock_name();
        let mut open_opts = cap_std::fs::OpenOptions::new();
        // Create the file if it doesn't exist.
        open_opts.write(true).create(true);
        let lock_path = dir.path().join(lock_name);

        let file = dir
            .dir()
            .open_with(lock_name, &open_opts)
            .wrap_err_with(|| format!("failed to open lock at `{lock_path}`"))?;
        Ok(Self {
            file: file.into_std(),
            lock_path,
            ctx,
        })
    }

    #[inline]
    pub(crate) fn lock_exclusive(self) -> Result<ExclusiveRoot<T>> {
        self.file
            .lock_exclusive()
            .wrap_err_with(|| format!("failed to obtain exclusive lock at `{}`", self.lock_path))?;
        Ok(ExclusiveRoot {
            file: self.file,
            ctx: self.ctx,
        })
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn lock_shared(self) -> Result<SharedRoot<T>> {
        self.file
            .lock_shared()
            .wrap_err_with(|| format!("failed to obtain shared lock at `{}`", self.lock_path))?;
        Ok(SharedRoot {
            file: self.file,
            ctx: self.ctx,
        })
    }
}

pub(crate) trait AsLockedCtx {
    fn dir_and_lock_name(&self) -> (&DirWithPath, &str);
}

/// Operations that can only be performed on a root where the shared lock has been acquired.
#[derive(Debug)]
#[must_use]
#[allow(dead_code)]
pub(crate) struct SharedRoot<T> {
    file: fs::File,
    pub(crate) ctx: T,
}

impl<T> SharedRoot<T> {
    /// Unlock this directory.
    #[allow(dead_code)]
    pub(crate) fn unlock(self) -> T {
        self.ctx
    }
}

/// Operations that can only be performed on a root where the exclusive lock has been acquired.
/// This forms a superset of the operations on the shared root.
#[derive(Debug)]
#[must_use]
#[allow(dead_code)]
pub(crate) struct ExclusiveRoot<T> {
    file: fs::File,
    pub(crate) ctx: T,
}

impl<T> ExclusiveRoot<T> {
    /// Unlock this directory.
    pub(crate) fn unlock(self) -> T {
        self.ctx
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
        T: Serialize,
    {
        let writer = io::BufWriter::new(self.dir.create(file_name).wrap_err_with(|| {
            format!(
                "failed to create targo metadata file `{}`",
                self.path.join(file_name)
            )
        })?);
        serde_json::to_writer(writer, metadata).wrap_err_with(|| {
            format!(
                "failed to serialize metadata to `{}`",
                self.path.join(file_name)
            )
        })?;
        Ok(())
    }
}
