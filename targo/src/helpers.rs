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
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
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

/// The device and inode of a directory: the same whatever path leads to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirIdentity {
    dev: u64,
    ino: u64,
}

impl DirIdentity {
    pub(crate) fn new(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

/// Looks for the directory `identity` at `path` or above it, and returns a path to it.
/// If `path` doesn't exist, the search starts at the deepest directory above it that does.
pub(crate) fn find_dir_at_or_above(path: &Path, identity: DirIdentity) -> Result<Option<PathBuf>> {
    let (mut dir, mut dir_identity) = deepest_existing(path)?;
    loop {
        if dir_identity == identity {
            return Ok(Some(dir));
        }
        // Not `parent()`: the OS takes `..` from the directory itself, whatever path led there.
        let os_resolved_parent = dir.join("..");
        let parent_identity = read_identity(&os_resolved_parent)?;
        // Only the root is its own parent.
        let dir_is_root = parent_identity == dir_identity;
        if dir_is_root {
            return Ok(None);
        }
        (dir, dir_identity) = (os_resolved_parent, parent_identity);
    }
}

/// Looks for the directory `identity` at `root` or below it, and returns a path to it.
/// Symlinks are not followed: this sees the directories that removing `root` would remove.
pub(crate) fn find_dir_at_or_below(root: &Path, identity: DirIdentity) -> Result<Option<PathBuf>> {
    let Some(root_metadata) =
        unless_missing(fs::symlink_metadata(root)).wrap_err_with(|| metadata_error(root))?
    else {
        return Ok(None);
    };
    if !root_metadata.is_dir() {
        return Ok(None);
    }
    if DirIdentity::new(&root_metadata) == identity {
        return Ok(Some(root.to_owned()));
    }

    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        let read_error = || format!("failed to read directory `{}`", dir.display());
        let Some(entries) = unless_missing(fs::read_dir(&dir)).wrap_err_with(read_error)? else {
            continue;
        };
        for entry in entries {
            let Some(entry) = unless_missing(entry).wrap_err_with(read_error)? else {
                // The directory was removed while it was being read.
                break;
            };
            let path = entry.path();
            // Only directories are stat'ed: a target directory can hold millions of files.
            let Some(file_type) =
                unless_missing(entry.file_type()).wrap_err_with(|| metadata_error(&path))?
            else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Some(metadata) =
                unless_missing(entry.metadata()).wrap_err_with(|| metadata_error(&path))?
            else {
                continue;
            };
            if DirIdentity::new(&metadata) == identity {
                return Ok(Some(path));
            }
            pending.push(path);
        }
    }
    Ok(None)
}

/// Something removed during a search is not there to find.
fn unless_missing<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Returns the deepest of `path` and its ancestors that exists, with its identity.
pub(crate) fn deepest_existing(path: &Path) -> Result<(PathBuf, DirIdentity)> {
    for ancestor in path.ancestors() {
        match fs::metadata(ancestor) {
            Ok(metadata) => return Ok((ancestor.to_owned(), DirIdentity::new(&metadata))),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).wrap_err_with(|| metadata_error(ancestor)),
        }
    }
    // Only a relative path gets here: the root always exists.
    bail!("nothing exists at or above `{}`", path.display());
}

fn read_identity(path: &Path) -> Result<DirIdentity> {
    let metadata = fs::metadata(path).wrap_err_with(|| metadata_error(path))?;
    Ok(DirIdentity::new(&metadata))
}

fn metadata_error(path: &Path) -> String {
    format!("failed to read metadata for `{}`", path.display())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

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

    fn identity_of(path: &Path) -> DirIdentity {
        DirIdentity::new(&fs::metadata(path).expect("read metadata"))
    }

    #[test]
    fn test_find_dir_at_or_above() {
        let temp_dir = camino_tempfile::tempdir().expect("created temp dir");
        let root = temp_dir.path().canonicalize().expect("canonicalized");
        let root = root.join("a/b/c");
        fs::create_dir_all(root.join("dir/one/two")).expect("created dirs");
        fs::create_dir_all(root.join("beside/dir")).expect("created dirs");
        fs::write(root.join("file"), "").expect("wrote file");
        symlink(root.join("dir/one/two"), root.join("beside/link")).expect("created symlink");

        let dir_identity = identity_of(&root.join("dir"));
        let data = [
            ("dir", Some("dir")),
            ("dir/one/two", Some("dir/one/two/../..")),
            (
                "dir/one/two/../../../dir/one",
                Some("dir/one/two/../../../dir/one/.."),
            ),
            ("dir/one/missing/store", Some("dir/one/..")),
            ("beside", None),
            ("beside/dir", None),
            ("beside/missing/store", None),
            ("dir/one/two/../../../beside", None),
            ("missing", None),
            ("beside/link", Some("beside/link/../..")),
            ("beside/link/missing/store", Some("beside/link/../..")),
        ];
        for (path, expected) in data {
            let found = find_dir_at_or_above(&root.join(path), dir_identity).expect("searched");
            assert_eq!(found, expected.map(|path| root.join(path)), "for {path:?}");
        }

        let found = find_dir_at_or_above(&root.join("dir/missing/../../beside"), dir_identity)
            .expect("searched");
        assert_eq!(
            found,
            Some(root.join("dir")),
            "a `..` after a missing component is not followed"
        );

        let found =
            find_dir_at_or_above(&root.join("beside/link"), identity_of(&root)).expect("searched");
        assert_eq!(
            found,
            Some(root.join("beside/link/../../..")),
            "`..` after a symlink is the parent of where the symlink leads"
        );
        let found =
            find_dir_at_or_above(&root.join("beside/link"), identity_of(&root.join("beside")))
                .expect("searched");
        assert_eq!(
            found, None,
            "the directory that holds a symlink is not above where it leads"
        );

        let depth = root.components().count() - 1;
        for (levels, what) in [(depth, "the root"), (depth - 1, "a child of the root")] {
            let ancestor = root
                .ancestors()
                .nth(levels)
                .expect("the temp dir is this deep");
            let found = find_dir_at_or_above(&root, identity_of(ancestor))
                .expect("searched")
                .unwrap_or_else(|| panic!("{what}, `{}`, is found", ancestor.display()));
            assert!(
                found.starts_with(&root) && identity_of(&found) == identity_of(ancestor),
                "for {what}, `{}` was found at `{}`",
                ancestor.display(),
                found.display()
            );
        }

        let error_data = [
            (
                root.join("file/store"),
                format!(
                    "failed to read metadata for `{}`",
                    root.join("file/store").display()
                ),
            ),
            (
                root.join("file"),
                format!(
                    "failed to read metadata for `{}`",
                    root.join("file/..").display()
                ),
            ),
            (
                PathBuf::from("missing/relative"),
                "nothing exists at or above `missing/relative`".to_owned(),
            ),
        ];
        for (path, expected) in error_data {
            let error = find_dir_at_or_above(&path, dir_identity).expect_err("the search failed");
            assert_eq!(error.to_string(), expected, "for `{}`", path.display());
        }
    }

    #[test]
    fn test_find_dir_at_or_below() {
        let temp_dir = camino_tempfile::tempdir().expect("created temp dir");
        let root = temp_dir.path().canonicalize().expect("canonicalized");
        let root = root.join("a/b/c");
        fs::create_dir_all(root.join("dir/one/two")).expect("created dirs");
        fs::create_dir_all(root.join("dir/other")).expect("created dirs");
        fs::create_dir_all(root.join("beside/dir")).expect("created dirs");
        fs::write(root.join("dir/one/file"), "").expect("wrote file");
        symlink(root.join("dir/one/two"), root.join("beside/link")).expect("created symlink");
        symlink(root.join("beside"), root.join("dir/link")).expect("created symlink");

        let data = [
            ("dir", "dir", Some("dir")),
            ("dir", "dir/one/two", Some("dir/one/two")),
            ("dir", "dir/other", Some("dir/other")),
            ("dir/one", "dir", None),
            ("dir", "beside", None),
            ("dir", "beside/dir", None),
            ("beside", "dir/one/two", None),
            ("beside/link", "dir/one/two", None),
            ("dir/one/file", "dir", None),
            ("missing", "dir", None),
        ];
        for (start, sought, expected) in data {
            let found = find_dir_at_or_below(&root.join(start), identity_of(&root.join(sought)))
                .expect("searched");
            assert_eq!(
                found,
                expected.map(|path| root.join(path)),
                "for {sought:?} at or below {start:?}"
            );
        }
    }

    #[test]
    fn test_dir_identity_includes_the_device() {
        let temp_dir = camino_tempfile::tempdir().expect("created temp dir");
        let root = temp_dir.path().canonicalize().expect("canonicalized");
        let root = root.join("a/b/c");
        fs::create_dir_all(root.join("dir/one")).expect("created dirs");

        let metadata = fs::metadata(root.join("dir")).expect("read metadata");
        let on_this_device = DirIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        let on_another_device = DirIdentity {
            dev: metadata.dev().wrapping_add(1),
            ino: metadata.ino(),
        };
        let data = [
            (on_this_device, Some("dir/one/.."), Some("dir")),
            (on_another_device, None, None),
        ];
        for (identity, expected_above, expected_below) in data {
            let above = find_dir_at_or_above(&root.join("dir/one"), identity).expect("searched");
            let below = find_dir_at_or_below(&root, identity).expect("searched");
            assert_eq!(
                (above, below),
                (
                    expected_above.map(|path| root.join(path)),
                    expected_below.map(|path| root.join(path)),
                ),
                "for {identity:?}"
            );
        }
    }
}
