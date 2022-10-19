use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::{eyre::Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{fs, io};

#[derive(Debug)]
pub(crate) struct UnlockedRoot<T> {
    file: fs::File,
    lock_path: Utf8PathBuf,
    pub(crate) ctx: T,
}

impl<T: AsRef<Utf8Path>> UnlockedRoot<T> {
    pub(crate) fn new(ctx: T) -> Result<Self> {
        let mut lock_path = ctx.as_ref().to_path_buf();
        lock_path.set_extension(LOCKFILE_EXT);
        let mut open_opts = fs::OpenOptions::new();
        // Create the file if it doesn't exist.
        let file = open_opts
            .write(true)
            .create(true)
            .open(&lock_path)
            .wrap_err_with(|| format!("failed to open lock at `{}`", lock_path))?;
        Ok(Self {
            file,
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

static LOCKFILE_EXT: &str = "lock";

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

pub(crate) fn read_metadata<T>(path: &Utf8Path) -> Result<Option<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let reader = match fs::File::open(&path) {
        Ok(reader) => io::BufReader::new(reader),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .wrap_err_with(|| format!("could not read targo metadata from file `{path}`"))
        }
    };
    Ok(Some(serde_json::from_reader(reader).wrap_err_with(
        || format!("failed to deserialize metadata from `{path}`"),
    )?))
}

pub(crate) fn write_metadata<T>(path: &Utf8Path, metadata: &T) -> Result<()>
where
    T: Serialize,
{
    let writer = io::BufWriter::new(
        fs::File::create(&path)
            .wrap_err_with(|| format!("failed to create targo metadata file `{path}`"))?,
    );
    serde_json::to_writer(writer, metadata)
        .wrap_err_with(|| format!("failed to serialize metadata to `{path}`"))?;
    Ok(())
}
