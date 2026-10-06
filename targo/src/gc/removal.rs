//! Removing and emptying entries: the second look under `targo.lock`, the trash, and deletion.

use super::{
    classify, decide, find_build_dirs, BuildActivity, BuildDir, Clock, Decision, DiskUsage,
    Emptying, GcPolicy, InUse, Measured, Outcome, PathErrors, Removal, UsageWalk,
};
use crate::{
    helpers::{try_lock_exclusive, ExclusiveLock, TryLock},
    store::{EntryName, StoreEntry, StoreLock, UnlockedStore},
};
use camino::Utf8PathBuf;
use cap_std::fs::{Dir, DirEntry, Metadata, MetadataExt as _, OpenOptions, ReadDir};
use chrono::{DateTime, Utc};
use color_eyre::{
    eyre::{eyre, WrapErr},
    Result,
};
use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::PathBuf,
    process,
};

/// The directory in the store that things are moved into before they are deleted.
const TRASH_DIR_NAME: &str = ".targo-trash";

/// In a target directory: where tests keep files. Cargo makes it only when it compiles a test.
const TEST_TMP_DIR_NAME: &str = "tmp";

/// A gc run that removes and empties entries. It holds `gc.lock`, so there is only one at a time.
#[derive(Debug)]
pub(super) struct Remover<'a> {
    store: &'a UnlockedStore,
    trash: TrashDir,
    names: TrashNames,
    _gc_lock: ExclusiveLock,
}

impl<'a> Remover<'a> {
    /// `now` only goes into the names of what this run puts in the trash.
    pub(super) fn start(store: &'a UnlockedStore, now: DateTime<Utc>) -> Result<TryLock<Self>> {
        let gc_lock = match store.try_lock_gc()? {
            TryLock::Acquired(gc_lock) => gc_lock,
            TryLock::Busy => return Ok(TryLock::Busy),
        };
        let trash = TrashDir::open(store)?;
        Ok(TryLock::Acquired(Self {
            store,
            trash,
            names: TrashNames::new(now, process::id()),
            _gc_lock: gc_lock,
        }))
    }

    /// The names of what is in the trash, sorted. An interrupted run leaves things there.
    pub(super) fn leftovers(&self) -> Result<Vec<OsString>> {
        let read_error = || format!("failed to read `{}`", self.trash.path);
        let mut names = Vec::new();
        for dir_entry in self.trash.dir.entries().wrap_err_with(read_error)? {
            names.push(dir_entry.wrap_err_with(read_error)?.file_name());
        }
        names.sort();
        Ok(names)
    }

    /// Measures `name` in the trash, then deletes it.
    pub(super) fn remove_leftover(&self, name: &OsStr) -> Result<Measured> {
        let path = self.trash.path.as_std_path().join(name);
        let mut walk = UsageWalk::default();
        match self.trash.dir.open_dir(name) {
            Ok(dir) => walk.measure_tree(&dir, &path),
            Err(error) => walk.record_unmeasured(path, error),
        }
        self.trash.delete(name)?;
        Ok(walk.finish())
    }

    /// Removes the entry `name`, if a second look under `targo.lock` still finds it removable.
    ///
    /// An error is about the store as a whole. A failure to remove the entry is an outcome.
    pub(super) fn remove(
        &mut self,
        name: &EntryName,
        usage: DiskUsage,
        policy: &GcPolicy,
        clock: Clock<'_>,
    ) -> Result<Outcome> {
        let lock = self.store.lock()?;
        let moved = match recheck(&lock, name, policy, clock) {
            Ok(Recheck::Remove(permit)) => permit.move_to_trash(&self.trash, &mut self.names),
            // It is live now. The size that was measured is not what emptying it would free.
            Ok(Recheck::Empty(permit)) => {
                drop(permit);
                lock.unlock()?;
                return Ok(links_changed(name));
            }
            Ok(Recheck::Leave(outcome)) => {
                lock.unlock()?;
                return Ok(outcome);
            }
            Err(error) => Err(error),
        };
        // Released before the slow part, so that `wrap-cargo` never waits for a deletion.
        lock.unlock()?;

        let deleted = moved.and_then(|trashed| trashed.delete(&self.trash));
        Ok(match deleted {
            Ok(removal) => Outcome::Removed { removal, usage },
            Err(error) => Outcome::RemoveFailed {
                name: name.clone(),
                error,
            },
        })
    }

    /// Empties the target directory of the entry `name`, if a second look under `targo.lock`
    /// still finds it live and idle. Errors are as for [`Self::remove`].
    pub(super) fn empty(
        &mut self,
        name: &EntryName,
        usage: DiskUsage,
        policy: &GcPolicy,
        clock: Clock<'_>,
    ) -> Result<Outcome> {
        let lock = self.store.lock()?;
        let moved = match recheck(&lock, name, policy, clock) {
            Ok(Recheck::Empty(permit)) => permit.move_to_trash(&self.trash, &mut self.names),
            // It is an orphan now, which is not emptied. The next run removes it.
            Ok(Recheck::Remove(permit)) => {
                drop(permit);
                lock.unlock()?;
                return Ok(links_changed(name));
            }
            Ok(Recheck::Leave(outcome)) => {
                lock.unlock()?;
                return Ok(outcome);
            }
            Err(error) => Err(error),
        };
        lock.unlock()?;

        let deleted = moved.and_then(|trashed| trashed.delete(&self.trash));
        Ok(match deleted {
            Ok(emptying) => Outcome::Emptied { emptying, usage },
            Err(error) => Outcome::EmptyFailed {
                name: name.clone(),
                error,
            },
        })
    }
}

fn links_changed(name: &EntryName) -> Outcome {
    Outcome::InUse {
        name: name.clone(),
        reason: InUse::LinksChanged,
    }
}

/// What a second look at an entry, under `targo.lock`, came to.
#[derive(Debug)]
pub(super) enum Recheck<'a> {
    Remove(RemovalPermit<'a>),
    Empty(EmptyPermit<'a>),
    /// The entry stays as it is, for the reason that the outcome gives.
    Leave(Outcome),
}

/// Leave to remove an entry. Only [`recheck`] gives it, so it is never out of date.
#[derive(Debug)]
#[must_use]
pub(super) struct RemovalPermit<'a> {
    // Borrowed, so that the lock is held for as long as the permit exists.
    lock: &'a StoreLock<'a>,
    removal: Removal,
    // Held until the entry is in the trash, so that Cargo can't start a build in it.
    _cargo_locks: Vec<fs::File>,
}

/// Leave to empty an entry's target directory. Only [`recheck`] gives it, as for a removal.
#[derive(Debug)]
#[must_use]
pub(super) struct EmptyPermit<'a> {
    lock: &'a StoreLock<'a>,
    emptying: Emptying,
    // Held until the contents are in the trash.
    _cargo_locks: Vec<fs::File>,
}

/// Looks at the entry `name` afresh and decides again, now that `wrap-cargo` is locked out.
pub(super) fn recheck<'a>(
    lock: &'a StoreLock<'a>,
    name: &EntryName,
    policy: &GcPolicy,
    clock: Clock<'_>,
) -> Result<Recheck<'a>> {
    let store = lock.store();
    let metadata = match store.reread_entry(name.clone()) {
        StoreEntry::Recognized { metadata, .. } => metadata,
        StoreEntry::Unrecognized(dir) => return Ok(Recheck::Leave(Outcome::Unrecognized(dir))),
    };
    let entry_dir = store
        .open_entry_dir(name)
        .wrap_err_with(|| format!("failed to open `{}`", store.entry_path(name)))?;

    let mut cargo_locks = Vec::new();
    let (entry_dir, entry_path) = (entry_dir.dir().as_cap_std(), entry_dir.path().as_ref());
    let build_activity = match find_build_dirs(entry_dir, entry_path) {
        Ok(build_dirs) => {
            for build_dir in &build_dirs {
                for &lock_name in &build_dir.lock_names {
                    match build_dir.try_lock(lock_name)? {
                        TryLock::Acquired(cargo_lock) => cargo_locks.push(cargo_lock),
                        TryLock::Busy => {
                            return Ok(Recheck::Leave(Outcome::InUse {
                                name: name.clone(),
                                reason: InUse::CargoLockHeld(build_dir.lock_path(lock_name)),
                            }));
                        }
                    }
                }
            }
            // Read with Cargo's locks held, so that a build that has just ended is seen.
            BuildActivity::of(entry_dir, entry_path, &build_dirs)
        }
        Err(error) => BuildActivity::Unknown(error),
    };

    let entry = classify(store, name.clone(), metadata, build_activity);
    // The time is read here: measuring the entry may have taken minutes.
    match decide(&entry, clock(), policy) {
        Decision::Keep(reason) => Ok(Recheck::Leave(Outcome::Kept { entry, reason })),
        Decision::RemoveOrphan { idle, signal } => {
            // The backlinks were compared with what is at the store's path.
            store.ensure_at_path()?;
            Ok(Recheck::Remove(RemovalPermit {
                lock,
                removal: Removal {
                    entry,
                    idle,
                    signal,
                },
                _cargo_locks: cargo_locks,
            }))
        }
        Decision::EmptyLive { idle, signal } => {
            store.ensure_at_path()?;
            Ok(Recheck::Empty(EmptyPermit {
                lock,
                emptying: Emptying {
                    entry,
                    idle,
                    signal,
                },
                _cargo_locks: cargo_locks,
            }))
        }
    }
}

impl BuildDir {
    fn lock_path(&self, lock_name: &str) -> PathBuf {
        self.path.join(lock_name)
    }

    /// Tries to take one of the locks that Cargo holds while it builds in this directory.
    /// Cargo holds some of its locks shared, so the try is for an exclusive one.
    fn try_lock(&self, lock_name: &str) -> Result<TryLock<fs::File>> {
        // Opened as Cargo opens it, but not created: a lock that isn't there isn't held.
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        let lock_error = || format!("failed to lock `{}`", self.lock_path(lock_name).display());
        let file = self
            .dir
            .open_with(lock_name, &options)
            .wrap_err_with(lock_error)?;
        try_lock_exclusive(file.into_std()).wrap_err_with(lock_error)
    }
}

impl RemovalPermit<'_> {
    /// Moves the entry into the trash, which takes it out of the store in one step.
    fn move_to_trash(self, trash: &TrashDir, names: &mut TrashNames) -> Result<TrashedEntry> {
        let store = self.lock.store();
        let name = &self.removal.entry.name;
        let move_error = || {
            format!(
                "failed to move `{}` into `{}`",
                store.entry_path(name),
                trash.path
            )
        };
        let trash_name = names.unused(&trash.dir).wrap_err_with(move_error)?;
        // By name in the open store directory, so no path is resolved and no symlink followed.
        store
            .dir()
            .dir()
            .as_cap_std()
            .rename(name.as_str(), &trash.dir, &trash_name)
            .wrap_err_with(move_error)?;
        Ok(TrashedEntry {
            trash_name,
            removal: self.removal,
        })
    }
}

impl EmptyPermit<'_> {
    /// Moves what is in the entry's target directory into a new directory in the trash. The
    /// directory itself stays, so a workspace's `target` symlink never dangles.
    fn move_to_trash(self, trash: &TrashDir, names: &mut TrashNames) -> Result<TrashedContents> {
        let store = self.lock.store();
        let name = &self.emptying.entry.name;
        let target_path = store.entry_target_path(name);
        let move_error = || {
            format!(
                "failed to move the contents of `{target_path}` into `{}`",
                trash.path
            )
        };
        let target_dir = store
            .open_entry_target_dir(name)
            .wrap_err_with(move_error)?;
        let target_dir = target_dir.dir().as_cap_std();
        // Listed before anything is moved: a rename changes the directory that is being read.
        let mut child_names = Vec::new();
        for dir_entry in target_dir.entries().wrap_err_with(move_error)? {
            child_names.push(dir_entry.wrap_err_with(move_error)?.file_name());
        }
        child_names.sort();
        // Last: a test that is already built fails without it, so it must outlast the builds.
        child_names.sort_by_key(|child_name| child_name == TEST_TMP_DIR_NAME);

        let trash_name = names.unused(&trash.dir).wrap_err_with(move_error)?;
        trash
            .dir
            .create_dir(&trash_name)
            .wrap_err_with(move_error)?;
        let trash_subdir = trash.dir.open_dir(&trash_name).wrap_err_with(move_error)?;
        let (mut moved, mut left_behind) = (0_u64, None);
        for child_name in child_names {
            // Each is moved in one step, and a failure doesn't stop the rest from going.
            let result = if child_name == TEST_TMP_DIR_NAME && left_behind.is_some() {
                Err(io::Error::other("a build that was left behind may need it"))
            } else {
                target_dir.rename(&child_name, &trash_subdir, &child_name)
            };
            match result {
                Ok(()) => moved += 1,
                // Something else removed it first.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    let child_path = target_path.as_std_path().join(child_name);
                    tracing::debug!("could not move `{}`: {error}", child_path.display());
                    PathErrors::record(&mut left_behind, child_path, error);
                }
            }
        }
        Ok(TrashedContents {
            trash_name,
            emptying: self.emptying,
            moved,
            left_behind,
        })
    }
}

/// What was in an entry's target directory, now in a directory in the trash.
#[derive(Debug)]
#[must_use]
struct TrashedContents {
    trash_name: String,
    emptying: Emptying,
    /// How many children of the target directory were moved.
    moved: u64,
    /// What could not be moved, and so is still in the target directory.
    left_behind: Option<PathErrors>,
}

impl TrashedContents {
    /// Deletes the contents, which can take minutes and doesn't need `targo.lock`.
    fn delete(self, trash: &TrashDir) -> Result<Emptying> {
        let deleted = trash.delete(OsStr::new(&self.trash_name));
        match (deleted, self.left_behind) {
            (Ok(()), None) => Ok(self.emptying),
            (Ok(()), Some(left_behind)) => Err(match self.moved {
                0 => eyre!("could not move {left_behind}; nothing was deleted"),
                _ => eyre!(
                    "could not move {left_behind}; the rest of the target directory was deleted"
                ),
            }),
            (Err(error), None) => Err(error),
            (Err(error), Some(left_behind)) => Err(error.wrap_err(format!(
                "could not move {left_behind}, and could not delete what was moved"
            ))),
        }
    }
}

/// An entry that is in the trash, and so no longer in the store.
#[derive(Debug)]
#[must_use]
struct TrashedEntry {
    trash_name: String,
    removal: Removal,
}

impl TrashedEntry {
    /// Deletes the entry, which can take minutes and doesn't need `targo.lock`.
    fn delete(self, trash: &TrashDir) -> Result<Removal> {
        trash.delete(OsStr::new(&self.trash_name))?;
        Ok(self.removal)
    }
}

/// Makes names for what a run puts in the trash, which no other run makes.
#[derive(Debug)]
struct TrashNames {
    run: String,
    next_index: u64,
}

impl TrashNames {
    fn new(started: DateTime<Utc>, pid: u32) -> Self {
        Self {
            run: format!("{:x}-{pid}", started.timestamp_micros()),
            next_index: 0,
        }
    }

    fn next_name(&mut self) -> String {
        let name = format!("{}-{}", self.run, self.next_index);
        self.next_index += 1;
        name
    }

    /// The next name that nothing in `trash` has. A leftover can, if the clock was set back.
    fn unused(&mut self, trash: &Dir) -> io::Result<String> {
        loop {
            let name = self.next_name();
            match trash.symlink_metadata(&name) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(name),
                Err(error) => return Err(error),
            }
        }
    }
}

/// The trash directory of a store.
#[derive(Debug)]
struct TrashDir {
    dir: Dir,
    path: Utf8PathBuf,
    /// The device that the trash is on, which is the store's.
    dev: u64,
}

impl TrashDir {
    /// Opens the trash directory of `store`, after creating it if it isn't there.
    fn open(store: &UnlockedStore) -> Result<Self> {
        let store_dir = store.dir().dir().as_cap_std();
        let path = store.dir().path().join(TRASH_DIR_NAME);
        let open_error = || format!("failed to open the trash directory `{path}`");
        let unusable = || {
            eyre!(
                "`{path}` must be a directory on the store's filesystem, since gc deletes \
                 everything in it: move it out of the way, then run gc again"
            )
        };

        match store_dir.create_dir(TRASH_DIR_NAME) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).wrap_err_with(open_error),
        }
        let found = store_dir
            .symlink_metadata(TRASH_DIR_NAME)
            .wrap_err_with(open_error)?;
        if !found.is_dir() {
            return Err(unusable());
        }
        let dir = store_dir
            .open_dir(TRASH_DIR_NAME)
            .wrap_err_with(open_error)?;
        let opened = dir.dir_metadata().wrap_err_with(open_error)?;
        let store_dev = store_dir.dir_metadata().wrap_err_with(open_error)?.dev();
        // A symlink swapped in could lead to an entry, which would then be deleted.
        if !is_same_dir(&found, &opened) || opened.dev() != store_dev {
            return Err(unusable());
        }
        Ok(Self {
            dir,
            path,
            dev: store_dev,
        })
    }

    /// Deletes `name` in the trash, with everything under it.
    fn delete(&self, name: &OsStr) -> Result<()> {
        let path = self.path.as_std_path().join(name);
        let mut walk = DeleteWalk::new(self.dev);
        walk.delete(&self.dir, name, path.clone());
        match walk.failures {
            None => Ok(()),
            Some(failures) => Err(eyre!(
                "could not delete {failures}; what is left is in `{}`, where the next gc run \
                 tries again",
                path.display()
            )),
        }
    }
}

fn is_same_dir(a: &Metadata, b: &Metadata) -> bool {
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

/// Deletes a tree, without following symlinks and without leaving its filesystem.
#[derive(Debug)]
struct DeleteWalk {
    /// The device of the tree. A directory on another device is a mount point.
    dev: u64,
    failures: Option<PathErrors>,
}

/// A name in a directory: where a walk starts, or a directory entry that it came to.
enum Node<'a> {
    Root { parent: &'a Dir, name: &'a OsStr },
    Entry(DirEntry),
}

impl Node<'_> {
    /// The metadata of the name itself, and not of what a symlink leads to.
    fn metadata(&self) -> io::Result<Metadata> {
        match self {
            Self::Root { parent, name } => parent.symlink_metadata(name),
            Self::Entry(dir_entry) => dir_entry.metadata(),
        }
    }

    fn open_dir(&self) -> io::Result<Dir> {
        match self {
            Self::Root { parent, name } => parent.open_dir(name),
            Self::Entry(dir_entry) => dir_entry.open_dir(),
        }
    }

    fn remove_file(&self) -> io::Result<()> {
        match self {
            Self::Root { parent, name } => parent.remove_file(name),
            Self::Entry(dir_entry) => dir_entry.remove_file(),
        }
    }

    fn remove_dir(&self) -> io::Result<()> {
        match self {
            Self::Root { parent, name } => parent.remove_dir(name),
            Self::Entry(dir_entry) => dir_entry.remove_dir(),
        }
    }
}

/// A directory that a walk is emptying.
struct Frame<'a> {
    node: Node<'a>,
    entries: ReadDir,
    path: PathBuf,
    /// How many failures the walk had when it entered the directory.
    failures_before: u64,
}

impl DeleteWalk {
    fn new(dev: u64) -> Self {
        Self {
            dev,
            failures: None,
        }
    }

    /// Deletes `name` in `parent`, with everything under it. A failure doesn't stop the walk.
    ///
    /// `path` is only for naming paths in errors.
    fn delete(&mut self, parent: &Dir, name: &OsStr, path: PathBuf) {
        // A stack rather than recursion, so that a deep tree can't overflow the call stack.
        let mut pending = Vec::new();
        pending.extend(self.delete_or_enter(Node::Root { parent, name }, path));
        while let Some(frame) = pending.last_mut() {
            match frame.entries.next() {
                Some(Ok(dir_entry)) => {
                    let path = frame.path.join(dir_entry.file_name());
                    let entered = self.delete_or_enter(Node::Entry(dir_entry), path);
                    pending.extend(entered);
                }
                Some(Err(error)) => {
                    // The rest of the directory is given up on: the error could repeat forever.
                    let path = frame.path.clone();
                    pending.pop();
                    self.record_failure(path, error);
                }
                None => {
                    // If something in it failed, the directory is not empty.
                    if self.failure_count() == frame.failures_before {
                        if let Err(error) = ignore_not_found(frame.node.remove_dir()) {
                            let path = frame.path.clone();
                            self.record_failure(path, error);
                        }
                    }
                    pending.pop();
                }
            }
        }
    }

    /// Deletes `node`, unless it is a directory with something in it, which comes first.
    fn delete_or_enter<'a>(&mut self, node: Node<'a>, path: PathBuf) -> Option<Frame<'a>> {
        match self.try_delete_or_enter(&node) {
            Ok(Some(entries)) => Some(Frame {
                node,
                entries,
                path,
                failures_before: self.failure_count(),
            }),
            Ok(None) => None,
            Err(error) => {
                self.record_failure(path, error);
                None
            }
        }
    }

    /// Returns the entries of `node` if it is a directory that has to be emptied first.
    fn try_delete_or_enter(&self, node: &Node<'_>) -> io::Result<Option<ReadDir>> {
        let metadata = match node.metadata() {
            Ok(metadata) => metadata,
            // Something else removed it first.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() {
            // This removes a symlink itself, and not what it leads to.
            return ignore_not_found(node.remove_file()).map(|()| None);
        }
        if metadata.dev() != self.dev {
            return Err(io::Error::new(
                io::ErrorKind::CrossesDevices,
                "it is on another filesystem, which gc deletes nothing from",
            ));
        }
        // Before looking inside: a mount point can't be removed, whatever device it is on.
        match node.remove_dir() {
            Ok(()) => return Ok(None),
            // POSIX allows either error for a directory that is not empty.
            Err(error)
                if error.kind() == io::ErrorKind::DirectoryNotEmpty
                    || error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return ignore_not_found(Err(error)).map(|()| None),
        }

        let dir = node.open_dir()?;
        // Opening follows a symlink, which could have replaced the directory.
        if !is_same_dir(&metadata, &dir.dir_metadata()?) {
            return Err(io::Error::other("it was replaced while gc was deleting it"));
        }
        dir.entries().map(Some)
    }

    fn failure_count(&self) -> u64 {
        match &self.failures {
            Some(failures) => failures.other_paths.saturating_add(1),
            None => 0,
        }
    }

    fn record_failure(&mut self, path: PathBuf, error: io::Error) {
        tracing::debug!("could not delete `{}`: {error}", path.display());
        PathErrors::record(&mut self.failures, path, error);
    }
}

/// Something that is already gone doesn't have to be removed.
fn ignore_not_found(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            examine,
            tests::{set_modified, test_store, utc, TestRoot},
            ActivitySignal, KeepReason, CARGO_LOCK_NAMES,
        },
        *,
    };
    use crate::{helpers::tests::open_lock_file, metadata::TargetDirMetadata};
    use camino::Utf8Path;
    use cap_std::ambient_authority;
    use std::{
        collections::HashSet,
        os::unix::fs::{MetadataExt as _, PermissionsExt},
        path::{Component, Path},
        slice,
        time::Duration,
    };

    const HOUR: u64 = 60 * 60;
    const DAY: u64 = 24 * HOUR;

    fn now() -> DateTime<Utc> {
        utc("2026-03-08T19:00:00Z")
    }

    fn policy() -> GcPolicy {
        GcPolicy {
            orphan_grace: Duration::from_secs(7 * DAY),
            max_age: None,
        }
    }

    /// A policy that also empties a live entry, once it has been idle for 30 days.
    fn max_age_policy() -> GcPolicy {
        GcPolicy {
            max_age: Some(Duration::from_secs(30 * DAY)),
            ..policy()
        }
    }

    /// Writes the metadata of the entry `name` in the store of `root`.
    fn write_entry(root: &TestRoot, name: &str, backlinks: &[&Utf8Path], last_used: DateTime<Utc>) {
        root.create_dir(&format!("store/{name}/target"));
        let metadata = TargetDirMetadata {
            backlinks: backlinks.iter().map(|path| path.to_path_buf()).collect(),
            last_used: last_used.into(),
        };
        let metadata = serde_json::to_string(&metadata).expect("serialized metadata");
        fs::write(metadata_path(root, name), metadata).expect("wrote metadata");
    }

    fn metadata_path(root: &TestRoot, name: &str) -> Utf8PathBuf {
        root.path(&format!("store/{name}/target-dir-metadata.json"))
    }

    /// Creates an entry that no workspace links to, last used 30 days ago.
    fn write_orphan(root: &TestRoot, name: &str) {
        let backlink = root.path(&format!("workspaces/{name}/target"));
        write_entry(
            root,
            name,
            &[&backlink],
            now() - Duration::from_secs(30 * DAY),
        );
    }

    /// Creates an entry that a workspace links to, last used 60 days ago. Returns the link.
    fn write_live(root: &TestRoot, name: &str) -> Utf8PathBuf {
        let target = root.path(&format!("store/{name}/target"));
        let backlink = root.symlink(&target, &format!("workspaces/{name}/target"));
        write_entry(
            root,
            name,
            &[&backlink],
            now() - Duration::from_secs(60 * DAY),
        );
        backlink
    }

    /// The entries that are to be removed going by a look at the store without any lock.
    fn list_removals(store: &UnlockedStore) -> Vec<EntryName> {
        list_collections(store, &policy()).0
    }

    /// The entries that are to be removed, and those that are to be emptied.
    fn list_collections(
        store: &UnlockedStore,
        policy: &GcPolicy,
    ) -> (Vec<EntryName>, Vec<EntryName>) {
        let (mut removals, mut emptyings) = (Vec::new(), Vec::new());
        for entry in store.entries().expect("listed entries") {
            let StoreEntry::Recognized { name, metadata } = entry else {
                continue;
            };
            let entry = examine(store, name, metadata, now(), policy);
            match decide(&entry, now(), policy) {
                Decision::RemoveOrphan { .. } => removals.push(entry.name),
                Decision::EmptyLive { .. } => emptyings.push(entry.name),
                Decision::Keep(_) => {}
            }
        }
        (removals, emptyings)
    }

    /// Why a second look left an entry in the store.
    #[derive(Debug, PartialEq, Eq)]
    enum Left {
        Kept(KeepReason),
        Unrecognized,
        InUse(PathBuf),
    }

    /// Takes a second look at `name`, which must leave it in the store.
    fn recheck_left(store: &UnlockedStore, name: &EntryName) -> Left {
        recheck_left_with(store, name, &policy())
    }

    /// Takes a second look at `name` under `policy`, which must leave it as it is.
    fn recheck_left_with(store: &UnlockedStore, name: &EntryName, policy: &GcPolicy) -> Left {
        let lock = store.lock().expect("locked store");
        match recheck(&lock, name, policy, &now).expect("rechecked") {
            Recheck::Leave(Outcome::Kept { reason, .. }) => Left::Kept(reason),
            Recheck::Leave(Outcome::Unrecognized(_)) => Left::Unrecognized,
            Recheck::Leave(Outcome::InUse {
                reason: InUse::CargoLockHeld(path),
                ..
            }) => Left::InUse(path),
            recheck => panic!("the second look came to {recheck:?}"),
        }
    }

    fn open_dir(path: &Utf8Path) -> Dir {
        Dir::open_ambient_dir(path, ambient_authority()).expect("opened directory")
    }

    fn names_in(dir: &Utf8Path) -> Vec<String> {
        let mut names: Vec<_> = dir
            .read_dir_utf8()
            .expect("read directory")
            .map(|entry| entry.expect("read entry").file_name().to_owned())
            .collect();
        names.sort();
        names
    }

    fn is_held(lock_path: &Utf8Path) -> bool {
        let probe = open_lock_file(lock_path);
        match try_lock_exclusive(probe).expect("tried Cargo's lock") {
            TryLock::Acquired(_) => false,
            TryLock::Busy => true,
        }
    }

    #[test]
    fn test_recheck_leaves_an_entry_that_changed_after_it_was_listed() {
        // Each case is a change to an entry made after it was listed as one to remove.
        const BACKLINK: &str = "workspaces/entry/target";
        type Change = fn(&TestRoot);
        let data: [(&str, Change, Left); 8] = [
            (
                "a workspace links to it again",
                |root| {
                    root.symlink(root.path("store/entry/target"), BACKLINK);
                },
                Left::Kept(KeepReason::Live),
            ),
            (
                "it was used again",
                |root| write_entry(root, "entry", &[&root.path(BACKLINK)], now()),
                Left::Kept(KeepReason::OrphanWithinGrace),
            ),
            (
                "something was built in it",
                |root| {
                    root.write_file("store/entry/target/debug/.cargo-lock", 0);
                    let deps = root.create_dir("store/entry/target/debug/deps");
                    set_modified(&deps, now() - Duration::from_secs(HOUR));
                },
                Left::Kept(KeepReason::OrphanBuiltWithinGrace {
                    idle: Duration::from_secs(HOUR),
                }),
            ),
            (
                "its backlink can no longer be examined",
                |root| {
                    root.symlink("target", BACKLINK);
                },
                Left::Kept(KeepReason::UnknownBacklinks),
            ),
            (
                "its backlink names it through another path to the store",
                |root| {
                    root.symlink(root.path("other-view/store/entry/target"), BACKLINK);
                },
                Left::Kept(KeepReason::UnknownBacklinks),
            ),
            (
                "its metadata is corrupt",
                |root| fs::write(metadata_path(root, "entry"), "{").expect("wrote metadata"),
                Left::Unrecognized,
            ),
            (
                "its metadata is gone",
                |root| fs::remove_file(metadata_path(root, "entry")).expect("removed metadata"),
                Left::Unrecognized,
            ),
            (
                "it was replaced by a directory that targo didn't make",
                |root| {
                    fs::remove_dir_all(root.path("store/entry")).expect("removed entry");
                    root.write_file("store/entry/precious", 1);
                },
                Left::Unrecognized,
            ),
        ];
        for (change_name, change, expected) in data {
            let root = TestRoot::new();
            write_orphan(&root, "entry");
            let store = test_store(&root);
            let removals = list_removals(&store);
            assert_eq!(removals, [EntryName::new("entry")], "before {change_name}");

            change(&root);

            assert_eq!(
                recheck_left(&store, &removals[0]),
                expected,
                "{change_name}"
            );
            assert!(root.path("store/entry").is_dir(), "{change_name}");
            assert!(
                !names_in(&root.path("store")).contains(&TRASH_DIR_NAME.to_owned()),
                "{change_name}, and nothing was moved"
            );
        }
    }

    #[test]
    fn test_recheck_leaves_an_entry_that_cargo_builds_in() {
        let build_dirs = [
            "target/debug",
            "target/x86_64-unknown-linux-gnu/release",
            "target/tool/x86_64-unknown-linux-gnu/debug",
        ];
        for build_dir in build_dirs {
            let root = TestRoot::new();
            write_orphan(&root, "entry");
            let free_lock_path = root.write_file("store/entry/target/release/.cargo-lock", 0);
            root.write_file("store/entry/target/tool/debug/.cargo-lock", 0);
            let lock_path = root.write_file(&format!("store/entry/{build_dir}/.cargo-lock"), 0);
            let store = test_store(&root);
            let name = EntryName::new("entry");

            // As Cargo holds it for the length of a build.
            let cargo_lock = open_lock_file(&lock_path);
            cargo_lock.lock().expect("locked as Cargo does");
            assert_eq!(
                recheck_left(&store, &name),
                Left::InUse(lock_path.clone().into()),
                "for `{build_dir}`"
            );
            assert!(
                !is_held(&free_lock_path),
                "the locks that were taken are let go"
            );

            drop(cargo_lock);
            let lock = store.lock().expect("locked store");
            let permit = match recheck(&lock, &name, &policy(), &now).expect("rechecked") {
                Recheck::Remove(permit) => permit,
                recheck => panic!("for `{build_dir}`, the second look came to {recheck:?}"),
            };
            assert!(
                is_held(&lock_path) && is_held(&free_lock_path),
                "for `{build_dir}`, Cargo is kept out while the entry is moved"
            );
            drop(permit);
            assert!(!is_held(&lock_path) && !is_held(&free_lock_path));
        }
    }

    #[test]
    fn test_recheck_leaves_an_entry_with_any_of_cargos_locks_held() {
        type Lock = fn(&fs::File) -> io::Result<()>;
        let (shared, exclusive): (Lock, Lock) = (fs::File::lock_shared, fs::File::lock);
        let all: &[&str] = &CARGO_LOCK_NAMES;
        // Each case: a build directory's lock files, the one that Cargo holds, and how.
        let data: [(&[&str], &str, Lock); 7] = [
            (&[".cargo-lock"], ".cargo-lock", exclusive),
            (&[".cargo-build-lock"], ".cargo-build-lock", exclusive),
            (&[".cargo-artifact-lock"], ".cargo-artifact-lock", exclusive),
            (all, ".cargo-lock", shared),
            (all, ".cargo-build-lock", exclusive),
            (all, ".cargo-build-lock", shared),
            (all, ".cargo-artifact-lock", exclusive),
        ];
        for (lock_names, held_name, lock_as_cargo) in data {
            let case = format!("with `{held_name}` of {lock_names:?} held");
            let root = TestRoot::new();
            write_orphan(&root, "entry");
            let build_dir = root.create_dir("store/entry/target/debug");
            for lock_name in lock_names {
                root.write_file(&format!("store/entry/target/debug/{lock_name}"), 0);
            }
            let before = names_in(&build_dir);
            let store = test_store(&root);
            let name = EntryName::new("entry");
            let held = || -> Vec<&str> {
                let names = lock_names.iter().copied();
                names
                    .filter(|lock_name| is_held(&build_dir.join(lock_name)))
                    .collect()
            };

            let held_path = build_dir.join(held_name);
            let cargo_lock = open_lock_file(&held_path);
            lock_as_cargo(&cargo_lock).expect("locked as Cargo does");
            assert_eq!(
                recheck_left(&store, &name),
                Left::InUse(held_path.into()),
                "{case}"
            );
            assert_eq!(held(), [held_name], "{case}, the others are let go");

            drop(cargo_lock);
            let lock = store.lock().expect("locked store");
            let permit = match recheck(&lock, &name, &policy(), &now).expect("rechecked") {
                Recheck::Remove(permit) => permit,
                recheck => panic!("{case}, the second look came to {recheck:?}"),
            };
            assert_eq!(held(), lock_names, "{case}, Cargo is kept out of each");
            drop(permit);
            assert_eq!(held(), [""; 0], "{case}");
            assert_eq!(names_in(&build_dir), before, "{case}, none was created");
        }
    }

    #[test]
    fn test_recheck_leaves_a_live_entry_that_changed_after_it_was_listed() {
        // Each case is a change to a live entry made after it was listed as one to empty,
        // and the grace period in days.
        const BACKLINK: &str = "workspaces/entry/target";
        const BUILT_FILE: &str = "store/entry/target/built-file";
        type Change = fn(&TestRoot);
        let data: [(&str, Change, u64, Left); 7] = [
            (
                "it was used again",
                |root| write_entry(root, "entry", &[&root.path(BACKLINK)], now()),
                7,
                Left::Kept(KeepReason::Live),
            ),
            (
                "something was built in it",
                |root| {
                    root.write_file("store/entry/target/debug/.cargo-lock", 0);
                    let deps = root.create_dir("store/entry/target/debug/deps");
                    set_modified(&deps, now() - Duration::from_secs(HOUR));
                },
                7,
                Left::Kept(KeepReason::Live),
            ),
            (
                "something else emptied its target directory",
                |root| fs::remove_file(root.path(BUILT_FILE)).expect("removed file"),
                7,
                Left::Kept(KeepReason::LiveAlreadyEmpty),
            ),
            (
                "its workspace is gone, and the grace period is longer than the maximum age",
                |root| fs::remove_file(root.path(BACKLINK)).expect("removed link"),
                90,
                Left::Kept(KeepReason::OrphanWithinGrace),
            ),
            (
                "its backlink can no longer be examined",
                |root| {
                    fs::remove_file(root.path(BACKLINK)).expect("removed link");
                    root.symlink("target", BACKLINK);
                },
                7,
                Left::Kept(KeepReason::UnknownBacklinks),
            ),
            (
                "its target directory can no longer be examined",
                |root| {
                    root.write_file("store/entry/target/debug/.cargo-lock", 0);
                    root.symlink("build", "store/entry/target/debug/build");
                },
                7,
                Left::Kept(KeepReason::UnknownBuildActivity),
            ),
            (
                "its metadata is corrupt",
                |root| fs::write(metadata_path(root, "entry"), "{").expect("wrote metadata"),
                7,
                Left::Unrecognized,
            ),
        ];
        for (change_name, change, grace_days, expected) in data {
            let root = TestRoot::new();
            write_live(&root, "entry");
            root.write_file(BUILT_FILE, 1);
            let store = test_store(&root);
            let policy = GcPolicy {
                orphan_grace: Duration::from_secs(grace_days * DAY),
                ..max_age_policy()
            };
            let (removals, emptyings) = list_collections(&store, &policy);
            assert_eq!(
                (removals.as_slice(), emptyings.as_slice()),
                (&[][..], &[EntryName::new("entry")][..]),
                "before {change_name}"
            );

            change(&root);

            let after_change = names_in(&root.path("store/entry/target"));
            assert_eq!(
                recheck_left_with(&store, &emptyings[0], &policy),
                expected,
                "{change_name}"
            );
            assert_eq!(
                names_in(&root.path("store/entry/target")),
                after_change,
                "{change_name}, and nothing was moved"
            );
            assert!(
                !names_in(&root.path("store")).contains(&TRASH_DIR_NAME.to_owned()),
                "{change_name}, and nothing was moved"
            );
        }
    }

    #[test]
    fn test_recheck_leaves_a_live_entry_that_cargo_builds_in() {
        let root = TestRoot::new();
        write_live(&root, "entry");
        let lock_path = root.write_file("store/entry/target/debug/.cargo-lock", 0);
        let store = test_store(&root);
        let name = EntryName::new("entry");

        // As Cargo holds it for the length of a build.
        let cargo_lock = open_lock_file(&lock_path);
        cargo_lock.lock().expect("locked as Cargo does");
        assert_eq!(
            recheck_left_with(&store, &name, &max_age_policy()),
            Left::InUse(lock_path.clone().into())
        );

        drop(cargo_lock);
        let lock = store.lock().expect("locked store");
        let permit = match recheck(&lock, &name, &max_age_policy(), &now).expect("rechecked") {
            Recheck::Empty(permit) => permit,
            recheck => panic!("the second look came to {recheck:?}"),
        };
        assert!(
            is_held(&lock_path),
            "Cargo is kept out while the contents are moved"
        );
        drop(permit);
        assert!(!is_held(&lock_path));
    }

    #[test]
    fn test_recheck_fails_once_the_store_is_replaced() {
        type Setup = fn(&TestRoot);
        // Each case is an entry that would otherwise be removed, or be emptied.
        let data: [(Setup, GcPolicy); 2] = [
            (|root| write_orphan(root, "entry"), policy()),
            (
                |root| {
                    write_live(root, "entry");
                },
                max_age_policy(),
            ),
        ];
        for (setup, policy) in data {
            let root = TestRoot::new();
            setup(&root);
            root.write_file("store/entry/target/built-file", 1);
            let store = test_store(&root);
            let lock = store.lock().expect("locked store");

            // As when the path is pointed at another disk after the lock was taken. A link
            // to the entry then leads into the new store.
            fs::rename(root.path("store"), root.path("moved-store")).expect("moved store");
            root.create_dir("store/entry/target");
            let error = recheck(&lock, &EntryName::new("entry"), &policy, &now)
                .expect_err("another directory is at the store's path");
            let expected = format!(
                "targo store directory `{}` was replaced by another directory",
                root.path("store")
            );
            assert!(
                error.to_string().starts_with(&expected),
                "error was: {error}"
            );
            assert!(root.path("moved-store/entry/target/built-file").is_file());
        }
    }

    #[test]
    fn test_remover_removes_only_what_a_second_look_permits() {
        let root = TestRoot::new();
        write_orphan(&root, "adopted");
        write_orphan(&root, "orphan");
        root.write_file("store/orphan/target/debug/deps/built-file", 1);
        root.write_file("store/orphan/target/debug/.cargo-lock", 0);
        set_modified(
            &root.path("store/orphan/target/debug/deps"),
            now() - Duration::from_secs(10 * DAY),
        );
        let store = test_store(&root);
        let names = list_removals(&store);
        assert_eq!(names, ["adopted", "orphan"].map(EntryName::new));
        // What `wrap-cargo` does when the workspace comes back.
        let backlink = root.symlink(
            root.path("store/adopted/target"),
            "workspaces/adopted/target",
        );
        write_entry(&root, "adopted", &[&backlink], now());

        let mut remover = match Remover::start(&store, now()).expect("started") {
            TryLock::Acquired(remover) => remover,
            TryLock::Busy => panic!("no other gc is running"),
        };
        match store.try_lock_gc().expect("tried the gc lock") {
            TryLock::Acquired(_) => panic!("the gc lock is held for the whole run"),
            TryLock::Busy => {}
        }
        let mut remove = |name| {
            let usage = DiskUsage {
                bytes: 0,
                unmeasured: None,
            };
            remover
                .remove(name, usage, &policy(), &now)
                .expect("the store is fine")
        };

        match remove(&names[0]) {
            Outcome::Kept { entry, reason } => {
                assert_eq!((entry.name, reason), (names[0].clone(), KeepReason::Live));
            }
            outcome => panic!("the adopted entry came to {outcome:?}"),
        }
        match remove(&names[1]) {
            Outcome::Removed { removal, .. } => assert_eq!(
                (removal.entry.name, removal.idle, removal.signal),
                (
                    names[1].clone(),
                    Duration::from_secs(10 * DAY),
                    ActivitySignal::LastBuilt
                )
            ),
            outcome => panic!("the orphan came to {outcome:?}"),
        }

        assert_eq!(
            names_in(&root.path("store")),
            [
                TRASH_DIR_NAME,
                "adopted",
                "gc.lock",
                "targo-metadata.json",
                "targo.lock"
            ]
        );
        assert_eq!(names_in(&root.path("store").join(TRASH_DIR_NAME)), [""; 0]);
        // Free again, or this would block.
        store
            .lock()
            .expect("locked store")
            .unlock()
            .expect("unlocked");
    }

    #[test]
    fn test_permit_moves_the_entry_into_the_trash() {
        let root = TestRoot::new();
        write_orphan(&root, "entry");
        root.write_file("store/entry/target/built-file", 1);
        let store = test_store(&root);
        let name = EntryName::new("entry");
        let trash = TrashDir::open(&store).expect("opened trash");
        let mut names = TrashNames::new(now(), 4242);
        let taken_name = TrashNames::new(now(), 4242).next_name();
        root.create_dir(&format!("store/{TRASH_DIR_NAME}/{taken_name}/leftover"));

        let lock = store.lock().expect("locked store");
        let permit = match recheck(&lock, &name, &policy(), &now).expect("rechecked") {
            Recheck::Remove(permit) => permit,
            recheck => panic!("the second look came to {recheck:?}"),
        };
        let trashed = permit
            .move_to_trash(&trash, &mut names)
            .expect("moved the entry");
        lock.unlock().expect("unlocked");

        assert!(!root.path("store/entry").exists());
        let trash_names = names_in(&trash.path);
        assert_eq!(
            trash_names,
            [taken_name.clone(), trashed.trash_name.clone()]
        );
        assert!(trash
            .path
            .join(&trashed.trash_name)
            .join("target/built-file")
            .is_file());

        let removal = trashed.delete(&trash).expect("deleted the entry");
        assert_eq!(removal.entry.name, name);
        assert_eq!(
            names_in(&trash.path),
            [taken_name],
            "the leftover is not touched"
        );
    }

    fn start_remover(store: &UnlockedStore) -> Remover<'_> {
        match Remover::start(store, now()).expect("started") {
            TryLock::Acquired(remover) => remover,
            TryLock::Busy => panic!("no other gc is running"),
        }
    }

    fn no_usage() -> DiskUsage {
        DiskUsage {
            bytes: 0,
            unmeasured: None,
        }
    }

    fn inode_of(path: &Utf8Path) -> u64 {
        fs::symlink_metadata(path).expect("read metadata").ino()
    }

    #[test]
    fn test_remover_empties_only_what_a_second_look_permits() {
        let root = TestRoot::new();
        let stale_link = write_live(&root, "stale");
        let orphaned_link = write_live(&root, "orphaned");
        write_live(&root, "used");
        write_orphan(&root, "adopted");
        let outside_file = root.write_file("outside/file", 1);
        // Everything in the target directory goes, whatever made it.
        for name in ["stale", "orphaned", "used", "adopted"] {
            root.write_file(&format!("store/{name}/target/CACHEDIR.TAG"), 1);
            root.write_file(&format!("store/{name}/target/.rustc_info.json"), 1);
            root.write_file(&format!("store/{name}/target/debug/.cargo-lock"), 0);
            root.write_file(&format!("store/{name}/target/debug/deps/built-file"), 1);
            root.write_file(
                &format!("store/{name}/target/rust-analyzer/flycheck0/file"),
                1,
            );
            root.symlink(&outside_file, &format!("store/{name}/target/link"));
            set_modified(
                &root.path(&format!("store/{name}/target/debug/deps")),
                now() - Duration::from_secs(45 * DAY),
            );
        }
        let store = test_store(&root);
        let policy = max_age_policy();
        let (removals, emptyings) = list_collections(&store, &policy);
        assert_eq!(removals, [EntryName::new("adopted")]);
        assert_eq!(emptyings, ["orphaned", "stale", "used"].map(EntryName::new));
        let [orphaned, stale, used] = &emptyings[..] else {
            panic!("three entries are to be emptied");
        };
        let contents = names_in(&root.path("store/stale/target"));
        let stale_target_inode = inode_of(&root.path("store/stale/target"));

        // What changed since the entries were listed.
        fs::remove_file(&orphaned_link).expect("removed link");
        write_entry(
            &root,
            "used",
            &[&root.path("workspaces/used/target")],
            now(),
        );
        let adopted_link = root.symlink(
            root.path("store/adopted/target"),
            "workspaces/adopted/target",
        );

        let mut remover = start_remover(&store);
        match remover
            .empty(stale, no_usage(), &policy, &now)
            .expect("the store is fine")
        {
            Outcome::Emptied { emptying, .. } => assert_eq!(
                (emptying.entry.name, emptying.idle, emptying.signal),
                (
                    stale.clone(),
                    Duration::from_secs(45 * DAY),
                    ActivitySignal::LastBuilt
                )
            ),
            outcome => panic!("the stale entry came to {outcome:?}"),
        }
        match remover
            .empty(used, no_usage(), &policy, &now)
            .expect("the store is fine")
        {
            Outcome::Kept { reason, .. } => assert_eq!(reason, KeepReason::Live),
            outcome => panic!("the entry that was used again came to {outcome:?}"),
        }
        // Neither is what it was listed as, so each is left for the next run.
        let orphaned_outcome = remover.empty(orphaned, no_usage(), &policy, &now);
        let adopted_outcome = remover.remove(&removals[0], no_usage(), &policy, &now);
        for outcome in [orphaned_outcome, adopted_outcome] {
            match outcome.expect("the store is fine") {
                Outcome::InUse {
                    reason: InUse::LinksChanged,
                    ..
                } => {}
                outcome => panic!("an entry whose links changed came to {outcome:?}"),
            }
        }

        assert_eq!(
            names_in(&root.path("store/stale")),
            ["target", "target-dir-metadata.json"]
        );
        assert_eq!(names_in(&root.path("store/stale/target")), [""; 0]);
        assert_eq!(
            inode_of(&root.path("store/stale/target")),
            stale_target_inode,
            "the target directory itself is never moved"
        );
        assert!(stale_link.is_dir() && adopted_link.is_dir());
        assert!(outside_file.is_file(), "a symlink is not followed");
        for name in ["orphaned", "used", "adopted"] {
            assert_eq!(
                names_in(&root.path(&format!("store/{name}/target"))),
                contents,
                "`{name}` is untouched"
            );
        }
        assert_eq!(names_in(&root.path("store").join(TRASH_DIR_NAME)), [""; 0]);
        // Free again, or this would block.
        store
            .lock()
            .expect("locked store")
            .unlock()
            .expect("unlocked");
    }

    #[test]
    fn test_permit_moves_the_contents_into_the_trash() {
        let root = TestRoot::new();
        let backlink = write_live(&root, "entry");
        root.write_file("store/entry/target/debug/deps/built-file", 1);
        root.write_file("store/entry/target/.rustc_info.json", 1);
        let store = test_store(&root);
        let name = EntryName::new("entry");
        let policy = max_age_policy();
        let trash = TrashDir::open(&store).expect("opened trash");
        let mut names = TrashNames::new(now(), 4242);

        let lock = store.lock().expect("locked store");
        let permit = match recheck(&lock, &name, &policy, &now).expect("rechecked") {
            Recheck::Empty(permit) => permit,
            recheck => panic!("the second look came to {recheck:?}"),
        };
        let trashed = permit
            .move_to_trash(&trash, &mut names)
            .expect("moved the contents");
        lock.unlock().expect("unlocked");

        // A run that dies here leaves this: the target directory is empty, and the link works.
        assert!(trashed.left_behind.is_none(), "{trashed:?}");
        assert_eq!(
            names_in(&root.path("store/entry")),
            ["target", "target-dir-metadata.json"]
        );
        assert_eq!(names_in(&root.path("store/entry/target")), [""; 0]);
        assert!(backlink.is_dir());
        assert_eq!(names_in(&trash.path), slice::from_ref(&trashed.trash_name));
        assert_eq!(
            names_in(&trash.path.join(&trashed.trash_name)),
            [".rustc_info.json", "debug"]
        );
        assert_eq!(
            recheck_left_with(&store, &name, &policy),
            Left::Kept(KeepReason::LiveAlreadyEmpty),
            "the next run has nothing to empty"
        );

        let emptying = trashed.delete(&trash).expect("deleted the contents");
        assert_eq!(emptying.entry.name, name);
        assert_eq!(names_in(&trash.path), [""; 0]);
    }

    #[test]
    fn test_emptying_leaves_what_it_cannot_move() {
        let root = TestRoot::new();
        let backlink = write_live(&root, "entry");
        root.write_file("store/entry/target/a-file", 1);
        root.write_file("store/entry/target/tmp/file", 1);
        root.write_file("store/entry/target/z-dir/file", 1);
        let stuck_file = root.write_file("store/entry/target/zz-read-only/file", 1);
        let store = test_store(&root);
        let name = EntryName::new("entry");
        let policy = max_age_policy();
        let mut remover = start_remover(&store);
        // Nothing can be moved out of a directory without write permission on it.
        let target = root.path("store/entry/target");
        let Some(read_only_target) = ReadOnlyDir::new(target.clone()) else {
            return;
        };
        let outcome = remover.empty(&name, no_usage(), &policy, &now);
        match outcome.expect("the store is fine") {
            Outcome::EmptyFailed { error, .. } => assert_eq!(
                error.to_string(),
                format!(
                    "could not move `{target}/a-file`: Permission denied (os error 13) \
                     (and 3 other paths); nothing was deleted"
                )
            ),
            outcome => panic!("the entry came to {outcome:?}"),
        }
        assert_eq!(
            names_in(&target),
            ["a-file", "tmp", "z-dir", "zz-read-only"]
        );
        drop(read_only_target);

        // A directory can't be moved to another parent without write permission on it.
        let stuck_dir = root.path("store/entry/target/zz-read-only");
        let Some(read_only) = ReadOnlyDir::new(stuck_dir.clone()) else {
            return;
        };

        let outcome = remover.empty(&name, no_usage(), &policy, &now);
        match outcome.expect("the store is fine") {
            Outcome::EmptyFailed { error, .. } => assert_eq!(
                error.to_string(),
                format!(
                    "could not move `{stuck_dir}`: Permission denied (os error 13) \
                     (and 1 other path); the rest of the target directory was deleted"
                )
            ),
            outcome => panic!("the entry came to {outcome:?}"),
        }
        // `tmp` stays too: a test built in what was left behind would fail without it.
        assert_eq!(names_in(&target), ["tmp", "zz-read-only"]);
        assert!(stuck_file.is_file() && backlink.is_dir());
        assert_eq!(names_in(&root.path("store").join(TRASH_DIR_NAME)), [""; 0]);

        // The next run finishes the job, once whatever was in the way is gone.
        drop(read_only);
        match remover
            .empty(&name, no_usage(), &policy, &now)
            .expect("the store is fine")
        {
            Outcome::Emptied { .. } => {}
            outcome => panic!("the entry came to {outcome:?}"),
        }
        assert_eq!(names_in(&root.path("store/entry/target")), [""; 0]);
        assert!(backlink.is_dir());
    }

    #[test]
    fn test_trash_dir_must_be_a_real_directory() {
        type Setup = fn(&TestRoot);
        let data: [Setup; 2] = [
            // Everything in the trash is deleted, so this would delete the entry.
            |root| {
                root.symlink("entry", &format!("store/{TRASH_DIR_NAME}"));
            },
            |root| {
                root.write_file(&format!("store/{TRASH_DIR_NAME}"), 1);
            },
        ];
        for setup in data {
            let root = TestRoot::new();
            write_orphan(&root, "entry");
            let built_file = root.write_file("store/entry/target/built-file", 1);
            let store = test_store(&root);
            setup(&root);

            let error = Remover::start(&store, now()).expect_err("the trash is refused");
            let expected = format!(
                "`{}` must be a directory on the store's filesystem",
                root.path("store").join(TRASH_DIR_NAME)
            );
            assert!(
                error.to_string().starts_with(&expected),
                "error was: {error}"
            );
            assert!(built_file.is_file());
        }
    }

    #[test]
    fn test_trash_names() {
        let root = TestRoot::new();
        let trash = open_dir(&root.create_dir("trash"));
        let started = utc("2026-03-08T19:00:00Z");

        let runs = [
            (started, 4242),
            (started, 4243),
            (started + Duration::from_micros(1), 4242),
            (DateTime::<Utc>::MIN_UTC, 0),
            (DateTime::<Utc>::MAX_UTC, u32::MAX),
        ];
        let mut seen = HashSet::new();
        for (started, pid) in runs {
            let mut names = TrashNames::new(started, pid);
            for _ in 0..3 {
                let name = names.unused(&trash).expect("made a name");
                let mut components = Path::new(&name).components();
                match (components.next(), components.next()) {
                    (Some(Component::Normal(_)), None) => {}
                    _ => panic!("`{name}` is not a single path component"),
                }
                assert!(
                    name.len() <= 48 && name.bytes().all(|byte| byte.is_ascii_graphic()),
                    "`{name}` is short and plain"
                );
                assert!(seen.insert(name.clone()), "`{name}` was made twice");
            }
        }
    }

    fn delete(trash: &Utf8Path, name: &str, dev: u64) -> Option<PathErrors> {
        let mut walk = DeleteWalk::new(dev);
        walk.delete(&open_dir(trash), OsStr::new(name), trash.join(name).into());
        walk.failures
    }

    fn dev_of(path: &Utf8Path) -> u64 {
        fs::metadata(path).expect("read metadata").dev()
    }

    #[test]
    fn test_delete_walk() {
        let root = TestRoot::new();
        let trash = root.create_dir("trash");
        let dev = dev_of(&trash);
        let outside_file = root.write_file("outside/file", 1);
        let linked_file = root.write_file("outside/linked-file", 1);
        let sibling_file = root.write_file("trash/sibling/file", 1);

        root.write_file("trash/item/file", 1);
        root.write_file("trash/item/dir/nested/file", 1);
        root.create_dir("trash/item/dir/empty");
        let unreadable_empty = root.create_dir("trash/item/dir/unreadable-empty");
        fs::set_permissions(&unreadable_empty, fs::Permissions::from_mode(0o000))
            .expect("removed permissions");
        fs::hard_link(&linked_file, root.path("trash/item/dir/hard-link")).expect("linked");
        // A symlink is removed, and what it leads to is left alone.
        root.symlink(root.path("outside"), "trash/item/dir-link");
        root.symlink(&outside_file, "trash/item/dir/file-link");
        root.symlink(root.path("trash/sibling"), "trash/item/sibling-link");
        root.symlink("nowhere", "trash/item/dangling");
        root.symlink(root.path("outside"), "trash/link");
        root.write_file("trash/plain-file", 1);

        for name in ["item", "link", "plain-file", "missing"] {
            let failures = delete(&trash, name, dev);
            assert!(failures.is_none(), "for `{name}`: {failures:?}");
        }

        assert_eq!(names_in(&trash), ["sibling"]);
        assert!(outside_file.is_file() && linked_file.is_file() && sibling_file.is_file());
    }

    #[test]
    fn test_delete_walk_leaves_a_directory_on_another_device() {
        let root = TestRoot::new();
        let trash = root.create_dir("trash");
        let file = root.write_file("trash/item/file", 1);
        root.create_dir("trash/empty");
        // No directory is on this device, so each one looks like a mount point.
        let other_dev = dev_of(&trash) + 1;

        for name in ["item", "empty"] {
            let failures = delete(&trash, name, other_dev).expect("the directory is refused");
            assert_eq!(
                (
                    failures.first.path,
                    failures.first.error.kind(),
                    failures.other_paths
                ),
                (trash.join(name).into(), io::ErrorKind::CrossesDevices, 0)
            );
        }
        assert_eq!(names_in(&trash), ["empty", "item"]);
        assert!(file.is_file());
    }

    /// A directory that nothing can be removed from, until this is dropped.
    struct ReadOnlyDir(Utf8PathBuf);

    impl ReadOnlyDir {
        /// Returns `None` if permissions are not enforced, as when running as root.
        fn new(path: Utf8PathBuf) -> Option<Self> {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o555))
                .expect("removed write permission");
            let probe = path.join("probe");
            let read_only = Self(path);
            match fs::write(&probe, "") {
                Ok(()) => {
                    fs::remove_file(&probe).expect("removed probe");
                    eprintln!("skipped: permissions are not enforced for this user");
                    None
                }
                Err(_) => Some(read_only),
            }
        }
    }

    impl Drop for ReadOnlyDir {
        fn drop(&mut self) {
            // Otherwise the temp dir can't be removed.
            fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755))
                .expect("restored permissions");
        }
    }

    #[test]
    fn test_delete_walk_leaves_what_it_cannot_delete() {
        let root = TestRoot::new();
        let trash = root.create_dir("trash");
        let dev = dev_of(&trash);
        root.write_file("trash/item/a-file", 1);
        root.write_file("trash/item/z-dir/file", 1);
        let stuck_files = [
            root.write_file("trash/item/read-only/file-1", 1),
            root.write_file("trash/item/read-only/file-2", 1),
        ];
        let Some(read_only) = ReadOnlyDir::new(root.path("trash/item/read-only")) else {
            return;
        };

        let failures = delete(&trash, "item", dev).expect("the files can't be deleted");
        let first_path = Utf8PathBuf::try_from(failures.first.path).expect("path is UTF-8");
        assert!(
            stuck_files.contains(&first_path),
            "first path: {first_path}"
        );
        // The directories above the files are not failures of their own.
        assert_eq!(
            (failures.first.error.kind(), failures.other_paths),
            (io::ErrorKind::PermissionDenied, 1)
        );
        assert_eq!(names_in(&root.path("trash/item")), ["read-only"]);
        assert_eq!(
            names_in(&root.path("trash/item/read-only")),
            ["file-1", "file-2"]
        );

        // The next run finishes the job, once whatever was in the way is gone.
        drop(read_only);
        let failures = delete(&trash, "item", dev);
        assert!(failures.is_none(), "failures: {failures:?}");
        assert_eq!(names_in(&trash), [""; 0]);
    }
}
