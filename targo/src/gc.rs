mod removal;

use self::removal::Remover;
use crate::{
    helpers::TryLock,
    metadata::TargetDirMetadata,
    store::{EntryName, StoreEntry, UnlockedStore, UnrecognizedDir},
};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std::fs::{Dir, DirEntry, Metadata, MetadataExt as _};
use chrono::{DateTime, Utc};
use color_eyre::{eyre::WrapErr, Report, Result};
use std::{
    cmp::Reverse,
    collections::HashSet,
    fmt, fs,
    io::{self, Write},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

/// When gc collects an entry.
#[derive(Debug)]
pub(crate) struct GcPolicy {
    /// How long an orphaned entry is kept after its last activity.
    pub(crate) orphan_grace: Duration,
    /// How long after its last activity a live entry is emptied. `None` means never.
    pub(crate) max_age: Option<Duration>,
}

/// Whether gc removes and empties what it decides to collect.
#[derive(Clone, Copy, Debug)]
pub(crate) enum GcMode {
    /// Report what would be removed or emptied, and change nothing.
    DryRun,
    Remove,
}

impl GcMode {
    fn wording(self) -> Wording {
        match self {
            Self::DryRun => Wording {
                remove: "would remove",
                empty: "would empty",
                keep: "would keep",
                skip: "would skip",
                and_empty: "empty",
                and_keep: "keep",
            },
            Self::Remove => Wording {
                remove: "removed",
                empty: "emptied",
                keep: "kept",
                skip: "skipped",
                and_empty: "emptied",
                and_keep: "kept",
            },
        }
    }
}

/// The verbs of the report, which differ between a dry run and a real one.
#[derive(Clone, Copy, Debug)]
struct Wording {
    remove: &'static str,
    empty: &'static str,
    keep: &'static str,
    skip: &'static str,
    /// The later verbs of the summary line.
    and_empty: &'static str,
    and_keep: &'static str,
}

/// How a gc run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub(crate) enum GcStatus {
    Completed,
    /// At least one entry could not be removed or emptied. Each failure has been reported.
    Failed,
    /// Another gc run holds `gc.lock`, so this one did nothing.
    AnotherGcRunning,
}

/// Gives the current time.
pub(crate) type Clock<'a> = &'a dyn Fn() -> DateTime<Utc>;

/// Collects the store at `store_dir`. The report goes to `out`, and failures go to `err`.
///
/// `clock` is read again before each entry is collected, which can come long after the run
/// started.
pub(crate) fn run(
    store_dir: Utf8PathBuf,
    mode: GcMode,
    policy: &GcPolicy,
    clock: Clock<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<GcStatus> {
    let mut output = Output {
        out,
        err,
        error: None,
    };
    let Some(store) = UnlockedStore::open(store_dir.clone())? else {
        output.line(format_args!(
            "nothing to collect: there is no targo store at `{store_dir}`"
        ));
        output.finish()?;
        return Ok(GcStatus::Completed);
    };

    let mut collector = match mode {
        GcMode::DryRun => Collector::DryRun,
        GcMode::Remove => match Remover::start(&store, clock())? {
            TryLock::Acquired(remover) => Collector::Remove(remover),
            TryLock::Busy => {
                output.failure(format_args!(
                    "another targo gc is running on the store at `{store_dir}`: \
                     nothing was removed"
                ));
                output.finish()?;
                return Ok(GcStatus::AnotherGcRunning);
            }
        },
    };
    let wording = mode.wording();
    let mut tally = Tally::default();
    collector.remove_leftovers(&mut tally, &mut output)?;

    let now = clock();
    let mut collections = Vec::new();
    let mut kept_lines = Vec::new();
    for entry in store.entries()? {
        match entry {
            StoreEntry::Recognized { name, metadata } => {
                let entry = examine(&store, name, metadata, now, policy);
                match decide(&entry, now, policy) {
                    Decision::RemoveOrphan { idle, signal } => {
                        collections.push(Collection::Remove(Removal {
                            entry,
                            idle,
                            signal,
                        }));
                    }
                    Decision::EmptyLive { idle, signal } => {
                        collections.push(Collection::Empty(Emptying {
                            entry,
                            idle,
                            signal,
                        }));
                    }
                    Decision::Keep(reason) => {
                        tally.kept.add(reason);
                        kept_lines.extend(kept_line(wording, &entry, reason));
                    }
                }
            }
            StoreEntry::Unrecognized(dir) => {
                tally.kept.unrecognized += 1;
                kept_lines.push(unrecognized_line(wording, &dir));
            }
        }
    }
    // Oldest first. The sort is stable, so entries that are as old stay in name order.
    collections.sort_by_key(|collection| Reverse(collection.idle()));

    for collection in collections {
        // Only between entries, so that a run never stops between a rename and its delete.
        if output.is_closed() {
            break;
        }
        // Slow, so each line is written as soon as its entry is dealt with.
        match collector.collect(&store, collection, policy, clock)? {
            Outcome::Removed { removal, usage } => {
                output.line(removal_line(wording, &removal, &usage));
                tally.removed.add(&usage);
            }
            Outcome::Emptied { emptying, usage } => {
                output.line(emptying_line(wording, &emptying, &usage));
                tally.emptied.add(&usage);
            }
            Outcome::InUse { name, reason } => {
                output.line(format_args!("{} `{name}`: in use, {reason}", wording.skip));
                tally.kept.in_use += 1;
            }
            Outcome::Kept { entry, reason } => {
                tally.kept.add(reason);
                kept_lines.extend(kept_line(wording, &entry, reason));
            }
            Outcome::Unrecognized(dir) => {
                tally.kept.unrecognized += 1;
                kept_lines.push(unrecognized_line(wording, &dir));
            }
            Outcome::RemoveFailed { name, error } => {
                // The alternate form puts the whole chain of causes on one line.
                output.failure(format_args!("failed to remove `{name}`: {error:#}"));
                tally.removed.failed += 1;
            }
            Outcome::EmptyFailed { name, error } => {
                output.failure(format_args!("failed to empty `{name}`: {error:#}"));
                tally.emptied.failed += 1;
            }
        }
    }
    for line in &kept_lines {
        output.line(line);
    }
    output.line(summary_line(wording, policy, &tally));
    output.finish()?;
    Ok(tally.status())
}

/// The report on `out` and the failures on `err`. Once a write fails, nothing more is written.
struct Output<'a> {
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    error: Option<io::Error>,
}

impl Output<'_> {
    fn line(&mut self, line: impl fmt::Display) {
        if self.error.is_none() {
            self.error = writeln!(self.out, "{line}").err();
        }
    }

    fn failure(&mut self, line: impl fmt::Display) {
        if self.error.is_none() {
            self.error = writeln!(self.err, "{line}").err();
        }
    }

    fn is_closed(&self) -> bool {
        self.error.is_some()
    }

    /// A reader that stops early, as with `targo gc | head`, is not a failure.
    fn finish(self) -> Result<()> {
        match self.error {
            None => Ok(()),
            Some(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            Some(error) => Err(error).wrap_err("failed to write the gc report"),
        }
    }
}

/// What a run did, for the summary line and the exit status.
#[derive(Debug, Default)]
struct Tally {
    removed: Collected,
    emptied: Collected,
    failed_leftovers: usize,
    kept: KeptCounts,
}

impl Tally {
    fn status(&self) -> GcStatus {
        if self.removed.failed + self.emptied.failed + self.failed_leftovers > 0 {
            GcStatus::Failed
        } else {
            GcStatus::Completed
        }
    }
}

/// The entries that a run removed, or the ones that it emptied.
#[derive(Debug, Default)]
struct Collected {
    entries: usize,
    freed: ReportedSize,
    failed: usize,
}

impl Collected {
    fn add(&mut self, usage: &DiskUsage) {
        self.entries += 1;
        self.freed.add(usage);
    }
}

/// An orphaned entry that is to be removed, with how long ago `signal` says it was active.
#[derive(Debug)]
struct Removal {
    entry: RecognizedEntry,
    idle: Duration,
    signal: ActivitySignal,
}

/// A live entry whose target directory is to be emptied. The fields are as in [`Removal`].
#[derive(Debug)]
struct Emptying {
    entry: RecognizedEntry,
    idle: Duration,
    signal: ActivitySignal,
}

/// What gc is to do with an entry that has been idle for too long.
#[derive(Debug)]
enum Collection {
    Remove(Removal),
    Empty(Emptying),
}

impl Collection {
    fn idle(&self) -> Duration {
        match self {
            Self::Remove(removal) => removal.idle,
            Self::Empty(emptying) => emptying.idle,
        }
    }
}

/// What came of an entry that was to be removed or emptied.
#[derive(Debug)]
enum Outcome {
    /// It was removed or, in a dry run, would be.
    Removed {
        removal: Removal,
        usage: DiskUsage,
    },
    /// Its target directory was emptied or, in a dry run, would be.
    Emptied {
        emptying: Emptying,
        usage: DiskUsage,
    },
    InUse {
        name: EntryName,
        reason: InUse,
    },
    /// A second look, under the store lock, found a reason to keep it.
    Kept {
        entry: RecognizedEntry,
        reason: KeepReason,
    },
    /// By the second look, its metadata was gone or unreadable.
    Unrecognized(UnrecognizedDir),
    RemoveFailed {
        name: EntryName,
        error: Report,
    },
    EmptyFailed {
        name: EntryName,
        error: Report,
    },
}

/// Why an entry counts as in use.
#[derive(Debug)]
enum InUse {
    /// Cargo is building in the entry.
    CargoLockHeld(PathBuf),
    /// This path vanished while the entry was measured, so something is changing the entry.
    ChangedWhileMeasured(PathBuf),
    /// Since it was listed, a workspace began to link to it, or the last one stopped.
    LinksChanged,
}

impl fmt::Display for InUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CargoLockHeld(path) => {
                write!(f, "Cargo holds the lock at `{}`", path.display())
            }
            Self::ChangedWhileMeasured(path) => write!(
                f,
                "`{}` vanished while the entry was being measured",
                path.display()
            ),
            Self::LinksChanged => f.write_str("its backlinks changed while gc was running"),
        }
    }
}

/// What a run does with the entries that it decides to remove or empty.
enum Collector<'a> {
    DryRun,
    Remove(Remover<'a>),
}

impl Collector<'_> {
    /// Deletes what an interrupted run left in the trash.
    fn remove_leftovers(&self, tally: &mut Tally, output: &mut Output<'_>) -> Result<()> {
        let remover = match self {
            Self::DryRun => return Ok(()),
            Self::Remove(remover) => remover,
        };
        for name in remover.leftovers()? {
            if output.is_closed() {
                break;
            }
            let shown = Path::new(&name).display();
            match remover.remove_leftover(&name) {
                Ok(Measured::Usage(usage)) => output.line(format_args!(
                    "removed leftover `{shown}` ({}) from an earlier run",
                    ReportedSize::of(&usage)
                )),
                Ok(Measured::Changed(_)) => output.line(format_args!(
                    "removed leftover `{shown}` from an earlier run"
                )),
                Err(error) => {
                    output.failure(format_args!(
                        "failed to remove leftover `{shown}`: {error:#}"
                    ));
                    tally.failed_leftovers += 1;
                }
            }
        }
        Ok(())
    }

    /// Measures what `collection` is to delete and then, unless this is a dry run, deletes it.
    fn collect(
        &mut self,
        store: &UnlockedStore,
        collection: Collection,
        policy: &GcPolicy,
        clock: Clock<'_>,
    ) -> Result<Outcome> {
        // Slow, so it is done before `targo.lock` is taken.
        let (name, measured) = match &collection {
            Collection::Remove(removal) => {
                let name = &removal.entry.name;
                (name.clone(), measure_entry(store, name))
            }
            Collection::Empty(emptying) => {
                let name = &emptying.entry.name;
                (name.clone(), measure_target_contents(store, name))
            }
        };
        let usage = match measured {
            Measured::Usage(usage) => usage,
            Measured::Changed(path) => {
                return Ok(Outcome::InUse {
                    name,
                    reason: InUse::ChangedWhileMeasured(path),
                });
            }
        };
        match (self, collection) {
            (Self::DryRun, Collection::Remove(removal)) => Ok(Outcome::Removed { removal, usage }),
            (Self::DryRun, Collection::Empty(emptying)) => Ok(Outcome::Emptied { emptying, usage }),
            (Self::Remove(remover), Collection::Remove(_)) => {
                remover.remove(&name, usage, policy, clock)
            }
            (Self::Remove(remover), Collection::Empty(_)) => {
                remover.empty(&name, usage, policy, clock)
            }
        }
    }
}

/// What is at the path of a backlink now.
#[derive(Debug)]
enum BacklinkState {
    /// The path leads to the entry's target directory.
    Live,
    /// Nothing is there.
    Missing,
    /// A symlink that dangles or resolves to something else.
    PointsElsewhere,
    /// A real file or directory.
    NotASymlink,
    /// The path could not be examined, so the backlink might be live.
    Unknown(io::Error),
}

impl fmt::Display for BacklinkState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Live => f.write_str("live"),
            Self::Missing => f.write_str("missing"),
            Self::PointsElsewhere => f.write_str("points elsewhere"),
            Self::NotASymlink => f.write_str("not a symlink"),
            Self::Unknown(error) => write!(f, "unknown: {error}"),
        }
    }
}

/// Where a backlink leads, when that is not the entry's target directory.
enum Leads {
    Elsewhere,
    Nowhere,
}

/// Examines `backlink`, a backlink of the entry whose target directory is `entry_target`.
fn inspect_backlink(backlink: &Utf8Path, entry_target: &Utf8Path) -> BacklinkState {
    // Targo only records absolute paths. A relative one would depend on the current directory.
    if !backlink.is_absolute() {
        return BacklinkState::Unknown(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path is not absolute",
        ));
    }

    // Checked first: a path that leads to the target directory is live, whatever is at it.
    let leads = match fs::metadata(backlink) {
        Ok(resolved) => match fs::metadata(entry_target) {
            // Not the path text: the same directory can be reached through several paths.
            Ok(target) if (resolved.dev(), resolved.ino()) == (target.dev(), target.ino()) => {
                return BacklinkState::Live;
            }
            Ok(_) => Leads::Elsewhere,
            // The entry has no target directory, so the path leads somewhere else.
            Err(error) if is_absent(&error) => Leads::Elsewhere,
            Err(error) => return BacklinkState::Unknown(error),
        },
        // Nothing is there, or a symlink dangles.
        Err(error) if is_absent(&error) => Leads::Nowhere,
        Err(error) => return BacklinkState::Unknown(error),
    };

    match fs::symlink_metadata(backlink) {
        Ok(metadata) if metadata.is_symlink() => match leads {
            Leads::Elsewhere => BacklinkState::PointsElsewhere,
            Leads::Nowhere => inspect_dangling_backlink(backlink, entry_target),
        },
        Ok(_) => BacklinkState::NotASymlink,
        Err(error) if is_absent(&error) => BacklinkState::Missing,
        Err(error) => BacklinkState::Unknown(error),
    }
}

/// Examines `backlink`, which is a symlink that dangles.
fn inspect_dangling_backlink(backlink: &Utf8Path, entry_target: &Utf8Path) -> BacklinkState {
    let link_text = match fs::read_link(backlink) {
        Ok(link_text) => link_text,
        Err(error) => return BacklinkState::Unknown(error),
    };
    if !names_target_of_entry(&link_text, entry_target) {
        return BacklinkState::PointsElsewhere;
    }
    match fs::metadata(entry_target) {
        // The store has another path where the link resolves, such as in a container.
        Ok(_) => BacklinkState::Unknown(io::Error::new(
            io::ErrorKind::NotFound,
            "the link names this entry by a path that does not resolve here",
        )),
        Err(error) if is_absent(&error) => BacklinkState::PointsElsewhere,
        Err(error) => BacklinkState::Unknown(error),
    }
}

/// Whether `link_text` ends as `entry_target` does, in the name of the entry and `target`.
fn names_target_of_entry(link_text: &Path, entry_target: &Utf8Path) -> bool {
    let entry_name = entry_target.parent().and_then(Utf8Path::file_name);
    match (entry_name, entry_target.file_name()) {
        (Some(entry_name), Some(target_name)) => {
            link_text.ends_with(Utf8Path::new(entry_name).join(target_name))
        }
        (None, _) | (_, None) => false,
    }
}

/// Whether `error` means that nothing is at a path. Any other error says nothing about it.
fn is_absent(error: &io::Error) -> bool {
    match error.kind() {
        // `ENOENT`, or `ENOTDIR` for a parent that is not a directory.
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => true,
        _ => false,
    }
}

/// A backlink recorded for an entry, with what is at its path now.
#[derive(Debug)]
struct Backlink {
    path: Utf8PathBuf,
    state: BacklinkState,
}

/// Whether any workspace still links to an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Liveness {
    /// At least one backlink is live.
    Live,
    /// No backlink is live or unknown. An entry with no backlinks is orphaned.
    Orphaned,
    /// No backlink is live, but at least one is unknown.
    Unknown,
}

impl Liveness {
    fn from_backlinks<'a>(states: impl IntoIterator<Item = &'a BacklinkState>) -> Self {
        let mut liveness = Self::Orphaned;
        for state in states {
            match state {
                BacklinkState::Live => return Self::Live,
                BacklinkState::Unknown(_) => liveness = Self::Unknown,
                BacklinkState::Missing
                | BacklinkState::PointsElsewhere
                | BacklinkState::NotASymlink => {}
            }
        }
        liveness
    }
}

/// The directory in an entry that a workspace's `target` symlink leads to.
const TARGET_DIR_NAME: &str = "target";

/// The lock file that Cargo holds while it builds in a directory.
const CARGO_LOCK_NAME: &str = ".cargo-lock";

/// Where rustc's outputs go in a build directory, up to Cargo 1.99.
const OLD_OUTPUT_DIR_NAME: &str = "deps";

/// Where each unit's fingerprint directory is, up to Cargo 1.99.
const OLD_FINGERPRINTS_DIR_NAME: &str = ".fingerprint";

/// Since Cargo 1.100, each unit has its own `build/<package>/<unit>` directory.
const UNITS_DIR_NAME: &str = "build";

/// In a unit's directory: rustc's outputs, and the unit's fingerprint.
const UNIT_DIR_NAMES: [&str; 2] = ["out", "fingerprint"];

/// A directory that Cargo builds in, which is one with a `.cargo-lock` in it.
#[derive(Debug)]
struct BuildDir {
    dir: Dir,
    path: PathBuf,
}

impl BuildDir {
    /// Returns `None` if `dir` has no `.cargo-lock`.
    fn new(dir: Dir, path: PathBuf) -> Result<Option<Self>, PathError> {
        match dir.symlink_metadata(CARGO_LOCK_NAME) {
            Ok(_) => Ok(Some(Self { dir, path })),
            Err(error) if is_absent(&error) => Ok(None),
            Err(error) => Err(PathError {
                path: path.join(CARGO_LOCK_NAME),
                error,
            }),
        }
    }

    /// Newest mtime of the directories that a compile changes. A failed compile changes only
    /// the unit's fingerprint directory, so that is read too.
    fn last_built(&self) -> Result<Option<DateTime<Utc>>, PathError> {
        let mut newest = dir_modified(&self.dir, &self.path, OLD_OUTPUT_DIR_NAME)?;
        if let Some((dir, path)) = open_subdir(&self.dir, &self.path, OLD_FINGERPRINTS_DIR_NAME)? {
            for_each_subdir_entry(&dir, &path, &mut |_, metadata, unit_path| {
                newest = newest.max(Some(modified(metadata, unit_path)?));
                Ok(())
            })?;
        }
        if let Some((dir, path)) = open_subdir(&self.dir, &self.path, UNITS_DIR_NAME)? {
            for_each_subdir(&dir, &path, &mut |package_dir, package_path| {
                for_each_subdir(&package_dir, &package_path, &mut |unit_dir, unit_path| {
                    for name in UNIT_DIR_NAMES {
                        newest = newest.max(dir_modified(&unit_dir, &unit_path, name)?);
                    }
                    Ok(())
                })
            })?;
        }
        Ok(newest)
    }
}

/// Opens the directory `name` in `dir`, if there is one. `dir_path` only names error paths.
fn open_subdir(
    dir: &Dir,
    dir_path: &Path,
    name: &str,
) -> Result<Option<(Dir, PathBuf)>, PathError> {
    let path = dir_path.join(name);
    match dir.open_dir(name) {
        Ok(subdir) => Ok(Some((subdir, path))),
        Err(error) if is_absent(&error) => Ok(None),
        Err(error) => Err(PathError { path, error }),
    }
}

/// Mtime of the directory `name` in `dir`; `dir_path` only names error paths.
fn dir_modified(
    dir: &Dir,
    dir_path: &Path,
    name: &str,
) -> Result<Option<DateTime<Utc>>, PathError> {
    match dir.symlink_metadata(name) {
        Ok(metadata) if metadata.is_dir() => modified(&metadata, dir_path.join(name)).map(Some),
        Ok(_) => Ok(None),
        Err(error) if is_absent(&error) => Ok(None),
        Err(error) => Err(PathError {
            path: dir_path.join(name),
            error,
        }),
    }
}

fn modified(metadata: &Metadata, path: PathBuf) -> Result<DateTime<Utc>, PathError> {
    match mtime_to_utc(metadata.mtime(), metadata.mtime_nsec()) {
        Some(modified) => Ok(modified),
        None => Err(PathError {
            path,
            error: io::Error::new(
                io::ErrorKind::InvalidData,
                "the modification time is out of range",
            ),
        }),
    }
}

/// `None` for a time that chrono can't represent, which a filesystem can hold.
fn mtime_to_utc(secs: i64, nanos: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(secs, u32::try_from(nanos).ok()?)
}

/// Finds an entry's build directories: `target/*`, `target/*/*`, and the same inside a
/// tool's own target directory under `target`. `entry_path` only names error paths.
fn find_build_dirs(entry_dir: &Dir, entry_path: &Path) -> Result<Vec<BuildDir>, PathError> {
    let Some((target_dir, target_path)) = open_subdir(entry_dir, entry_path, TARGET_DIR_NAME)?
    else {
        return Ok(Vec::new());
    };

    let mut build_dirs = Vec::new();
    for_each_subdir(&target_dir, &target_path, &mut |outer_dir, outer_path| {
        // With `--target`, the profile directories are one level down.
        let found_before = build_dirs.len();
        add_build_dirs_in(&outer_dir, &outer_path, &mut build_dirs)?;
        // This may be a tool's target directory, such as `target/rust-analyzer`, where
        // `--target` puts build directories another level down.
        if build_dirs.len() > found_before {
            for_each_subdir(&outer_dir, &outer_path, &mut |inner_dir, inner_path| {
                add_build_dirs_in(&inner_dir, &inner_path, &mut build_dirs)
            })?;
        }
        build_dirs.extend(BuildDir::new(outer_dir, outer_path)?);
        Ok(())
    })?;
    Ok(build_dirs)
}

/// Adds the build directories that are directly in `dir`.
fn add_build_dirs_in(
    dir: &Dir,
    path: &Path,
    build_dirs: &mut Vec<BuildDir>,
) -> Result<(), PathError> {
    for_each_subdir(dir, path, &mut |subdir, subdir_path| {
        build_dirs.extend(BuildDir::new(subdir, subdir_path)?);
        Ok(())
    })
}

/// Visits each real directory in `dir`, without opening it.
fn for_each_subdir_entry(
    dir: &Dir,
    path: &Path,
    visit: &mut dyn FnMut(&DirEntry, &Metadata, PathBuf) -> Result<(), PathError>,
) -> Result<(), PathError> {
    let path_error = |error| PathError {
        path: path.to_owned(),
        error,
    };
    for dir_entry in dir.entries().map_err(path_error)? {
        let dir_entry = dir_entry.map_err(path_error)?;
        let subdir_path = path.join(dir_entry.file_name());
        // Not followed, so a symlink to a directory is passed over.
        match dir_entry.metadata() {
            Ok(metadata) if metadata.is_dir() => visit(&dir_entry, &metadata, subdir_path)?,
            Ok(_) => {}
            Err(error) => {
                return Err(PathError {
                    path: subdir_path,
                    error,
                })
            }
        }
    }
    Ok(())
}

/// Opens each real directory in `dir` in turn. One at a time, since there can be thousands.
fn for_each_subdir(
    dir: &Dir,
    path: &Path,
    visit: &mut dyn FnMut(Dir, PathBuf) -> Result<(), PathError>,
) -> Result<(), PathError> {
    for_each_subdir_entry(
        dir,
        path,
        &mut |dir_entry, _, subdir_path| match dir_entry.open_dir() {
            Ok(subdir) => visit(subdir, subdir_path),
            Err(error) => Err(PathError {
                path: subdir_path,
                error,
            }),
        },
    )
}

/// When Cargo last built in an entry, going by the directories that a compile changes.
/// Unlike `last-used`, this also shows builds that didn't go through targo.
#[derive(Debug)]
enum BuildActivity {
    /// Not looked for, since the backlinks or `last-used` already keep the entry.
    NotNeeded,
    /// The target directory is empty or missing.
    EmptyTarget,
    /// No build directory shows a compile, but the target directory has something in it.
    None,
    Last(DateTime<Utc>),
    /// The entry could not be examined, so there may have been a build just now.
    Unknown(PathError),
}

impl BuildActivity {
    fn read(store: &UnlockedStore, name: &EntryName) -> Self {
        let entry_dir = match store.open_entry_dir(name) {
            Ok(entry_dir) => entry_dir,
            Err(error) => {
                return Self::Unknown(PathError {
                    path: store.entry_path(name).into(),
                    error,
                });
            }
        };
        let (entry_dir, entry_path) = (entry_dir.dir().as_cap_std(), entry_dir.path().as_ref());
        match find_build_dirs(entry_dir, entry_path) {
            Ok(build_dirs) => Self::of(entry_dir, entry_path, &build_dirs),
            Err(error) => Self::Unknown(error),
        }
    }

    /// The activity that `build_dirs`, which are those of the entry at `entry_dir`, show.
    fn of(entry_dir: &Dir, entry_path: &Path, build_dirs: &[BuildDir]) -> Self {
        let mut newest = None;
        for build_dir in build_dirs {
            match build_dir.last_built() {
                // `None` is less than any time.
                Ok(built) => newest = newest.max(built),
                Err(error) => return Self::Unknown(error),
            }
        }
        match newest {
            Some(built) => Self::Last(built),
            None => Self::without_builds(entry_dir, entry_path),
        }
    }

    /// For an entry that shows no compile: whether its target directory has anything in it.
    fn without_builds(entry_dir: &Dir, entry_path: &Path) -> Self {
        let (target_dir, target_path) = match open_subdir(entry_dir, entry_path, TARGET_DIR_NAME) {
            Ok(Some(target)) => target,
            Ok(None) => return Self::EmptyTarget,
            Err(error) => return Self::Unknown(error),
        };
        let first = target_dir
            .entries()
            .and_then(|mut dir_entries| dir_entries.next().transpose());
        match first {
            Ok(Some(_)) => Self::None,
            Ok(None) => Self::EmptyTarget,
            Err(error) => Self::Unknown(PathError {
                path: target_path,
                error,
            }),
        }
    }
}

impl fmt::Display for BuildActivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotNeeded => f.write_str("the build directories were not examined"),
            Self::EmptyTarget => f.write_str("the target directory is empty"),
            Self::None => f.write_str("nothing was built"),
            Self::Last(built) => write!(f, "last built at {built}"),
            Self::Unknown(error) => write!(f, "could not examine {error}"),
        }
    }
}

/// An entry with readable metadata, and what was found out about it.
#[derive(Debug)]
struct RecognizedEntry {
    name: EntryName,
    last_used: DateTime<Utc>,
    build_activity: BuildActivity,
    backlinks: Vec<Backlink>,
}

impl RecognizedEntry {
    fn liveness(&self) -> Liveness {
        Liveness::from_backlinks(self.backlinks.iter().map(|backlink| &backlink.state))
    }
}

fn classify(
    store: &UnlockedStore,
    name: EntryName,
    metadata: TargetDirMetadata,
    build_activity: BuildActivity,
) -> RecognizedEntry {
    let entry_target = store.entry_target_path(&name);
    let backlinks = metadata
        .backlinks
        .into_iter()
        .map(|path| {
            let state = inspect_backlink(&path, &entry_target);
            Backlink { path, state }
        })
        .collect();
    RecognizedEntry {
        name,
        // An instant, so the local offset in effect when it was recorded can't matter.
        last_used: metadata.last_used.with_timezone(&Utc),
        build_activity,
        backlinks,
    }
}

/// Classifies an entry, looking at its builds only if the decision depends on them.
fn examine(
    store: &UnlockedStore,
    name: EntryName,
    metadata: TargetDirMetadata,
    now: DateTime<Utc>,
    policy: &GcPolicy,
) -> RecognizedEntry {
    let mut entry = classify(store, name, metadata, BuildActivity::NotNeeded);
    match decide_without_builds(&entry, now, policy) {
        Preliminary::Keep(_) => {}
        // Slow for a large entry, so only done for an entry that would otherwise be collected.
        Preliminary::Unused { .. } => {
            entry.build_activity = BuildActivity::read(store, &entry.name);
        }
    }
    entry
}

/// Which of an entry's timestamps is its last activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivitySignal {
    /// `last-used` in the metadata, which only targo updates.
    LastUsed,
    /// The newest change that a compile made in a build directory.
    LastBuilt,
}

impl fmt::Display for ActivitySignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LastUsed => f.write_str("last used"),
            Self::LastBuilt => f.write_str("last built"),
        }
    }
}

/// What gc does with an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    /// Remove an orphaned entry, whose last activity was `idle` ago.
    RemoveOrphan {
        idle: Duration,
        signal: ActivitySignal,
    },
    /// Empty the target directory of a live entry, whose last activity was `idle` ago.
    EmptyLive {
        idle: Duration,
        signal: ActivitySignal,
    },
    Keep(KeepReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeepReason {
    /// Live, and either active within the maximum age or there is no maximum age.
    Live,
    /// Live and past the maximum age, with nothing in its target directory to delete.
    LiveAlreadyEmpty,
    OrphanWithinGrace,
    /// Last used longer ago than the grace period, but built in since.
    OrphanBuiltWithinGrace {
        idle: Duration,
    },
    UnknownBacklinks,
    UnknownBuildActivity,
    /// The clock went backwards or is wrong, so the age of the entry is unknown.
    ActivityInFuture {
        ahead: Duration,
        signal: ActivitySignal,
    },
}

/// How long before `now` the time `at` was, or how far it is ahead of `now`.
fn time_since(now: DateTime<Utc>, at: DateTime<Utc>) -> Result<Duration, Duration> {
    let since = now.signed_duration_since(at);
    // Only a negative duration fails to convert.
    since.to_std().map_err(|_| {
        since
            .abs()
            .to_std()
            .expect("an absolute value is not negative")
    })
}

/// A kind of entry that gc collects once it has been idle for long enough.
#[derive(Clone, Copy, Debug)]
enum Collectable {
    /// Removed once idle for the grace period.
    Orphan,
    /// Emptied once idle for the maximum age.
    Live,
}

impl Collectable {
    fn collect(self, idle: Duration, signal: ActivitySignal) -> Decision {
        match self {
            Self::Orphan => Decision::RemoveOrphan { idle, signal },
            Self::Live => Decision::EmptyLive { idle, signal },
        }
    }

    /// Why an entry last used within the limit is kept.
    fn used_within_limit(self) -> KeepReason {
        match self {
            Self::Orphan => KeepReason::OrphanWithinGrace,
            Self::Live => KeepReason::Live,
        }
    }

    /// Why an entry last built in `idle` ago, which is within the limit, is kept.
    fn built_within_limit(self, idle: Duration) -> KeepReason {
        match self {
            Self::Orphan => KeepReason::OrphanBuiltWithinGrace { idle },
            Self::Live => KeepReason::Live,
        }
    }
}

/// What an entry's backlinks and `last-used` come to, before its builds are looked at.
#[derive(Debug)]
enum Preliminary {
    Keep(KeepReason),
    /// Not used for `unused`, which is `limit` or longer, so its builds decide.
    Unused {
        collectable: Collectable,
        unused: Duration,
        limit: Duration,
    },
}

fn decide_without_builds(
    entry: &RecognizedEntry,
    now: DateTime<Utc>,
    policy: &GcPolicy,
) -> Preliminary {
    let (collectable, limit) = match (entry.liveness(), policy.max_age) {
        (Liveness::Unknown, _) => return Preliminary::Keep(KeepReason::UnknownBacklinks),
        (Liveness::Live, None) => return Preliminary::Keep(KeepReason::Live),
        (Liveness::Live, Some(max_age)) => (Collectable::Live, max_age),
        (Liveness::Orphaned, _) => (Collectable::Orphan, policy.orphan_grace),
    };
    match time_since(now, entry.last_used) {
        Ok(unused) if unused >= limit => Preliminary::Unused {
            collectable,
            unused,
            limit,
        },
        Ok(_) => Preliminary::Keep(collectable.used_within_limit()),
        Err(ahead) => Preliminary::Keep(KeepReason::ActivityInFuture {
            ahead,
            signal: ActivitySignal::LastUsed,
        }),
    }
}

/// Decides what to do with `entry` at the time `now`.
fn decide(entry: &RecognizedEntry, now: DateTime<Utc>, policy: &GcPolicy) -> Decision {
    let (collectable, unused, limit) = match decide_without_builds(entry, now, policy) {
        Preliminary::Keep(reason) => return Decision::Keep(reason),
        Preliminary::Unused {
            collectable,
            unused,
            limit,
        } => (collectable, unused, limit),
    };
    let last_built = match &entry.build_activity {
        // Without a look at the builds, there may have been one just now.
        BuildActivity::NotNeeded | BuildActivity::Unknown(_) => {
            return Decision::Keep(KeepReason::UnknownBuildActivity);
        }
        BuildActivity::EmptyTarget => match collectable {
            Collectable::Orphan => None,
            // Otherwise every later run would empty it again.
            Collectable::Live => return Decision::Keep(KeepReason::LiveAlreadyEmpty),
        },
        BuildActivity::None => None,
        BuildActivity::Last(last_built) => Some(*last_built),
    };
    let last_built = match last_built {
        Some(last_built) if last_built > entry.last_used => last_built,
        Some(_) | None => return collectable.collect(unused, ActivitySignal::LastUsed),
    };
    let signal = ActivitySignal::LastBuilt;
    match time_since(now, last_built) {
        Ok(idle) if idle >= limit => collectable.collect(idle, signal),
        Ok(idle) => Decision::Keep(collectable.built_within_limit(idle)),
        Err(ahead) => Decision::Keep(KeepReason::ActivityInFuture { ahead, signal }),
    }
}

/// An I/O error, and the path that it happened at.
#[derive(Debug)]
struct PathError {
    path: PathBuf,
    error: io::Error,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`: {}", self.path.display(), self.error)
    }
}

/// The first error that a walk of a tree ran into, and how many more paths had one.
#[derive(Debug)]
struct PathErrors {
    first: PathError,
    other_paths: u64,
}

impl PathErrors {
    fn record(errors: &mut Option<Self>, path: PathBuf, error: io::Error) {
        match errors {
            Some(errors) => errors.other_paths += 1,
            None => {
                *errors = Some(Self {
                    first: PathError { path, error },
                    other_paths: 0,
                });
            }
        }
    }
}

impl fmt::Display for PathErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.first)?;
        match self.other_paths {
            0 => Ok(()),
            1 => f.write_str(" (and 1 other path)"),
            others => write!(f, " (and {others} other paths)"),
        }
    }
}

/// The disk usage of a tree.
#[derive(Debug)]
struct DiskUsage {
    bytes: u64,
    /// Set if part of the tree could not be measured, which makes `bytes` a lower bound.
    unmeasured: Option<PathErrors>,
}

/// What came of measuring a tree.
#[derive(Debug)]
enum Measured {
    Usage(DiskUsage),
    /// This path vanished during the walk, so something is changing the tree.
    Changed(PathBuf),
}

/// Measures the disk usage of the entry `name`, which can take minutes.
fn measure_entry(store: &UnlockedStore, name: &EntryName) -> Measured {
    let mut walk = UsageWalk::default();
    match store.open_entry_dir(name) {
        Ok(entry_dir) => walk.measure_tree(entry_dir.dir().as_cap_std(), entry_dir.path().as_ref()),
        Err(error) => walk.record_unmeasured(store.entry_path(name).into(), error),
    }
    walk.finish()
}

/// Measures what is in the target directory of the entry `name`, which is what emptying frees.
fn measure_target_contents(store: &UnlockedStore, name: &EntryName) -> Measured {
    let mut walk = UsageWalk::default();
    let target_path = store.entry_target_path(name).into_std_path_buf();
    match store.open_entry_target_dir(name) {
        Ok(target_dir) => walk.measure_contents(target_dir.dir().as_cap_std(), &target_path),
        Err(error) => walk.record_unmeasured(target_path, error),
    }
    walk.finish()
}

/// Adds up `st_blocks` over a tree, counting a file with several hard links once.
#[derive(Debug, Default)]
struct UsageWalk {
    bytes: u64,
    /// Device and inode of each file counted so far that has more than one link.
    counted_hard_links: HashSet<(u64, u64)>,
    unmeasured: Option<PathErrors>,
    /// The first path that was gone by the time the walk got to it.
    vanished: Option<PathBuf>,
}

impl UsageWalk {
    /// Measures `root` and everything under it, without following symlinks.
    ///
    /// `root_path` is only for naming paths in errors, which don't stop the walk.
    fn measure_tree(&mut self, root: &Dir, root_path: &Path) {
        match root.dir_metadata() {
            Ok(metadata) => self.count(&metadata),
            Err(error) => self.record_unmeasured(root_path.to_owned(), error),
        }
        self.measure_contents(root, root_path);
    }

    /// Like [`Self::measure_tree`], but without `root` itself.
    fn measure_contents(&mut self, root: &Dir, root_path: &Path) {
        // A stack rather than recursion, so that a deep tree can't overflow the call stack.
        let mut pending = Vec::new();
        match root.entries() {
            Ok(dir_entries) => pending.push((dir_entries, root_path.to_owned())),
            Err(error) => self.record_unmeasured(root_path.to_owned(), error),
        }
        while let Some((dir_entries, dir_path)) = pending.last_mut() {
            let dir_entry = match dir_entries.next() {
                Some(Ok(dir_entry)) => dir_entry,
                Some(Err(error)) => {
                    // The rest of the directory is given up on: the error could repeat forever.
                    self.record_unmeasured(dir_path.clone(), error);
                    pending.pop();
                    continue;
                }
                None => {
                    pending.pop();
                    continue;
                }
            };

            // Like `lstat`: this is the entry itself, not what a symlink leads to.
            let metadata = match dir_entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.record_unmeasured(dir_path.join(dir_entry.file_name()), error);
                    continue;
                }
            };
            self.count(&metadata);

            if metadata.is_dir() {
                let path = dir_path.join(dir_entry.file_name());
                // cap-std opens beneath the parent, so a symlink swapped in can't lead outside.
                match dir_entry.open_dir().and_then(|dir| dir.entries()) {
                    Ok(dir_entries) => pending.push((dir_entries, path)),
                    Err(error) => self.record_unmeasured(path, error),
                }
            }
        }
    }

    fn count(&mut self, metadata: &Metadata) {
        // A directory's link count is about its subdirectories, not about hard links.
        if !metadata.is_dir()
            && metadata.nlink() > 1
            && !self
                .counted_hard_links
                .insert((metadata.dev(), metadata.ino()))
        {
            return;
        }
        // `st_blocks` is in units of 512 bytes, whatever the filesystem's block size.
        let bytes = metadata.blocks().saturating_mul(512);
        self.bytes = self.bytes.saturating_add(bytes);
    }

    fn record_unmeasured(&mut self, path: PathBuf, error: io::Error) {
        tracing::debug!("could not measure `{}`: {error}", path.display());
        match error.kind() {
            // The path was listed a moment ago, so it has just been removed.
            io::ErrorKind::NotFound => {
                self.vanished.get_or_insert(path);
            }
            _ => PathErrors::record(&mut self.unmeasured, path, error),
        }
    }

    fn finish(self) -> Measured {
        match self.vanished {
            Some(path) => Measured::Changed(path),
            None => Measured::Usage(DiskUsage {
                bytes: self.bytes,
                unmeasured: self.unmeasured,
            }),
        }
    }
}

/// Whether a size is exact or a lower bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SizeBound {
    #[default]
    Exact,
    AtLeast,
}

/// A size to report, which is a lower bound if part of it could not be measured.
#[derive(Debug, Default)]
struct ReportedSize {
    bytes: u64,
    bound: SizeBound,
}

impl ReportedSize {
    fn of(usage: &DiskUsage) -> Self {
        let mut size = Self::default();
        size.add(usage);
        size
    }

    fn add(&mut self, usage: &DiskUsage) {
        self.bytes = self.bytes.saturating_add(usage.bytes);
        if usage.unmeasured.is_some() {
            self.bound = SizeBound::AtLeast;
        }
    }
}

impl fmt::Display for ReportedSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.bound {
            SizeBound::Exact => write!(f, "{}", HumanBytes(self.bytes)),
            SizeBound::AtLeast => write!(f, "at least {}", HumanBytes(self.bytes)),
        }
    }
}

fn removal_line(wording: Wording, removal: &Removal, usage: &DiskUsage) -> String {
    let Removal { entry, idle, .. } = removal;
    collected_line(
        wording.remove,
        "orphaned",
        entry,
        removal.signal,
        *idle,
        usage,
    )
}

fn emptying_line(wording: Wording, emptying: &Emptying, usage: &DiskUsage) -> String {
    let Emptying { entry, idle, .. } = emptying;
    collected_line(wording.empty, "live", entry, emptying.signal, *idle, usage)
}

/// The line for an entry that was removed or emptied, of which `usage` is the part deleted.
fn collected_line(
    verb: &str,
    liveness: &str,
    entry: &RecognizedEntry,
    signal: ActivitySignal,
    idle: Duration,
    usage: &DiskUsage,
) -> String {
    let mut line = format!(
        "{verb} `{}` ({}): {liveness}, {signal} {} ago; {}",
        entry.name,
        ReportedSize::of(usage),
        Age(idle),
        BacklinkList(&entry.backlinks),
    );
    if let Some(unmeasured) = &usage.unmeasured {
        line.push_str(&format!("; could not measure {unmeasured}"));
    }
    line
}

/// The line for an entry that is kept, if the reason is worth a line of its own.
fn kept_line(wording: Wording, entry: &RecognizedEntry, reason: KeepReason) -> Option<String> {
    let reason = match reason {
        KeepReason::Live | KeepReason::LiveAlreadyEmpty | KeepReason::OrphanWithinGrace => {
            return None;
        }
        KeepReason::OrphanBuiltWithinGrace { idle } => {
            format!("orphaned, {} {} ago", ActivitySignal::LastBuilt, Age(idle))
        }
        KeepReason::UnknownBacklinks => "backlink state unknown".to_owned(),
        KeepReason::UnknownBuildActivity => {
            format!("build activity unknown, {}", entry.build_activity)
        }
        KeepReason::ActivityInFuture { ahead, signal } => {
            format!("{signal} {} in the future", Age(ahead))
        }
    };
    Some(format!(
        "{} `{}`: {reason}; {}",
        wording.keep,
        entry.name,
        BacklinkList(&entry.backlinks),
    ))
}

fn unrecognized_line(wording: Wording, dir: &UnrecognizedDir) -> String {
    format!(
        "{} `{}`: unrecognized; {}",
        wording.keep,
        Path::new(&dir.name).display(),
        dir.reason,
    )
}

fn summary_line(wording: Wording, policy: &GcPolicy, tally: &Tally) -> String {
    let (removed, emptied) = (&tally.removed, &tally.emptied);
    let mut clauses = vec![format!(
        "{} {} ({})",
        wording.remove,
        Entries(removed.entries),
        removed.freed
    )];
    if removed.failed > 0 {
        clauses.push(format!("failed to remove {}", Entries(removed.failed)));
    }
    // Without a maximum age no entry is emptied, so the line says nothing of it.
    if policy.max_age.is_some() {
        clauses.push(format!(
            "{} {} ({})",
            wording.and_empty,
            Entries(emptied.entries),
            emptied.freed
        ));
        if emptied.failed > 0 {
            clauses.push(format!("failed to empty {}", Entries(emptied.failed)));
        }
    }
    let kept = format!("{} {}", wording.and_keep, tally.kept);
    match clauses.as_slice() {
        [only] => format!("{only} and {kept}"),
        _ => format!("{}, and {kept}", clauses.join(", ")),
    }
}

/// How many entries are kept, by reason.
#[derive(Debug, Default)]
struct KeptCounts {
    live: usize,
    live_already_empty: usize,
    orphans_within_grace: usize,
    unknown_backlinks: usize,
    unknown_build_activity: usize,
    unrecognized: usize,
    activity_in_future: usize,
    in_use: usize,
}

impl KeptCounts {
    fn add(&mut self, reason: KeepReason) {
        let count = match reason {
            KeepReason::Live => &mut self.live,
            KeepReason::LiveAlreadyEmpty => &mut self.live_already_empty,
            KeepReason::OrphanWithinGrace | KeepReason::OrphanBuiltWithinGrace { .. } => {
                &mut self.orphans_within_grace
            }
            KeepReason::UnknownBacklinks => &mut self.unknown_backlinks,
            KeepReason::UnknownBuildActivity => &mut self.unknown_build_activity,
            KeepReason::ActivityInFuture { .. } => &mut self.activity_in_future,
        };
        *count += 1;
    }
}

impl fmt::Display for KeptCounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let counts = [
            (self.live, "live"),
            (self.live_already_empty, "already empty"),
            (self.orphans_within_grace, "orphaned within grace"),
            (self.unknown_backlinks, "with unknown backlinks"),
            (self.unknown_build_activity, "with unknown build activity"),
            (self.unrecognized, "unrecognized"),
            (self.activity_in_future, "last active in the future"),
            (self.in_use, "in use"),
        ];
        let total = counts.iter().map(|(count, _)| count).sum();
        write!(f, "{}", Entries(total))?;
        // Zero counts are left out, to keep the line short.
        let mut separator = ": ";
        for (count, label) in counts {
            if count > 0 {
                write!(f, "{separator}{count} {label}")?;
                separator = ", ";
            }
        }
        Ok(())
    }
}

struct BacklinkList<'a>(&'a [Backlink]);

impl fmt::Display for BacklinkList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("no backlinks");
        }
        f.write_str("backlinks: ")?;
        for (index, backlink) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "`{}` ({})", backlink.path, backlink.state)?;
        }
        Ok(())
    }
}

/// A count of entries, such as `1 entry`.
struct Entries(usize);

impl fmt::Display for Entries {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            1 => f.write_str("1 entry"),
            count => write!(f, "{count} entries"),
        }
    }
}

/// A duration rounded down to its largest whole unit, such as `3d`.
struct Age(Duration);

impl fmt::Display for Age {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MINUTE: u64 = 60;
        const HOUR: u64 = 60 * MINUTE;
        const DAY: u64 = 24 * HOUR;

        let secs = self.0.as_secs();
        if secs >= DAY {
            write!(f, "{}d", secs / DAY)
        } else if secs >= HOUR {
            write!(f, "{}h", secs / HOUR)
        } else if secs >= MINUTE {
            write!(f, "{}m", secs / MINUTE)
        } else {
            write!(f, "{secs}s")
        }
    }
}

/// A size in binary units to one decimal place, such as `1.5 GiB`.
struct HumanBytes(u64);

impl HumanBytes {
    /// The size in tenths of a unit of `unit_size` bytes, rounded to the nearest.
    fn tenths(&self, unit_size: u128) -> u128 {
        (u128::from(self.0) * 10 + unit_size / 2) / unit_size
    }
}

impl fmt::Display for HumanBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 1024 {
            return write!(f, "{} B", self.0);
        }

        let mut unit = "KiB";
        let mut unit_size = 1024;
        for larger_unit in ["MiB", "GiB", "TiB", "PiB", "EiB"] {
            // Checked after rounding, so that 1023.96 KiB is 1.0 MiB and not 1024.0 KiB.
            if self.tenths(unit_size) < 10240 {
                break;
            }
            unit = larger_unit;
            unit_size *= 1024;
        }
        let tenths = self.tenths(unit_size);
        write!(f, "{}.{} {unit}", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::TargoStoreMetadata;
    use cap_std::ambient_authority;
    use std::{
        os::unix::fs::{symlink, PermissionsExt},
        time::SystemTime,
    };

    /// A directory nested inside a temp dir, so that a path with `..` in it stays inside.
    pub(super) struct TestRoot {
        // Held so that the directory is removed on drop.
        _temp_dir: camino_tempfile::Utf8TempDir,
        root: Utf8PathBuf,
    }

    impl TestRoot {
        pub(super) fn new() -> Self {
            let temp_dir = camino_tempfile::tempdir().expect("created temp dir");
            let root = temp_dir.path().join("a/b/c");
            fs::create_dir_all(&root).expect("created root");
            Self {
                _temp_dir: temp_dir,
                root,
            }
        }

        pub(super) fn path(&self, relative: &str) -> Utf8PathBuf {
            self.root.join(relative)
        }

        pub(super) fn create_dir(&self, relative: &str) -> Utf8PathBuf {
            let path = self.path(relative);
            fs::create_dir_all(&path).expect("created dir");
            path
        }

        pub(super) fn write_file(&self, relative: &str, len: usize) -> Utf8PathBuf {
            let path = self.path(relative);
            fs::create_dir_all(path.parent().expect("path has a parent")).expect("created dir");
            let mut file = fs::File::create(&path).expect("created file");
            file.write_all(&vec![b'x'; len]).expect("wrote file");
            // So that the size the filesystem reports for the file is settled.
            file.sync_all().expect("synced file");
            path
        }

        /// Creates a symlink at `relative` with `dest` as its contents.
        pub(super) fn symlink(&self, dest: impl AsRef<Utf8Path>, relative: &str) -> Utf8PathBuf {
            let path = self.path(relative);
            fs::create_dir_all(path.parent().expect("path has a parent")).expect("created dir");
            symlink(dest.as_ref(), &path).expect("created symlink");
            path
        }
    }

    /// A directory that nothing can be reached through, until this is dropped.
    pub(super) struct LockedDir(Utf8PathBuf);

    impl LockedDir {
        /// Returns `None` if permissions are not enforced, as when running as root.
        pub(super) fn new(path: Utf8PathBuf) -> Option<Self> {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
                .expect("removed permissions");
            let locked = Self(path);
            match fs::read_dir(&locked.0) {
                Ok(_) => {
                    eprintln!("skipped: permissions are not enforced for this user");
                    None
                }
                Err(_) => Some(locked),
            }
        }
    }

    impl Drop for LockedDir {
        fn drop(&mut self) {
            // Otherwise the temp dir can't be removed.
            fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755))
                .expect("restored permissions");
        }
    }

    /// Opens the store at `store` in `root`, after giving it the metadata that makes it one.
    pub(super) fn test_store(root: &TestRoot) -> UnlockedStore {
        let store_dir = root.create_dir("store");
        let metadata = serde_json::to_string(&TargoStoreMetadata::new()).expect("serialized");
        fs::write(
            store_dir.join(TargoStoreMetadata::METADATA_FILE_NAME),
            metadata,
        )
        .expect("wrote store metadata");
        UnlockedStore::open(store_dir)
            .expect("opened store")
            .expect("the store exists")
    }

    /// A backlink state that can be compared.
    #[derive(Debug, PartialEq, Eq)]
    enum StateKind {
        Live,
        Missing,
        PointsElsewhere,
        NotASymlink,
        Unknown(io::ErrorKind),
    }

    impl StateKind {
        fn new(state: &BacklinkState) -> Self {
            match state {
                BacklinkState::Live => Self::Live,
                BacklinkState::Missing => Self::Missing,
                BacklinkState::PointsElsewhere => Self::PointsElsewhere,
                BacklinkState::NotASymlink => Self::NotASymlink,
                BacklinkState::Unknown(error) => Self::Unknown(error.kind()),
            }
        }
    }

    fn unknown() -> BacklinkState {
        BacklinkState::Unknown(io::Error::other("unknown"))
    }

    fn recognized(states: Vec<BacklinkState>, last_used: DateTime<Utc>) -> RecognizedEntry {
        recognized_built(states, last_used, BuildActivity::None)
    }

    fn recognized_built(
        states: Vec<BacklinkState>,
        last_used: DateTime<Utc>,
        build_activity: BuildActivity,
    ) -> RecognizedEntry {
        let backlinks = states
            .into_iter()
            .enumerate()
            .map(|(index, state)| Backlink {
                path: format!("/workspace-{index}/target").into(),
                state,
            })
            .collect();
        RecognizedEntry {
            name: EntryName::new("entry"),
            last_used,
            build_activity,
            backlinks,
        }
    }

    /// Sets when the directory at `path` was last modified.
    pub(super) fn set_modified(path: &Utf8Path, modified: DateTime<Utc>) {
        let dir = fs::File::open(path).expect("opened directory");
        dir.set_modified(SystemTime::from(modified))
            .expect("set modification time");
    }

    pub(super) fn utc(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("parsed timestamp")
            .with_timezone(&Utc)
    }

    #[test]
    fn test_liveness_from_backlinks() {
        let data = [
            (vec![], Liveness::Orphaned),
            (vec![BacklinkState::Live], Liveness::Live),
            (vec![BacklinkState::Missing], Liveness::Orphaned),
            (vec![BacklinkState::PointsElsewhere], Liveness::Orphaned),
            (vec![BacklinkState::NotASymlink], Liveness::Orphaned),
            (vec![unknown()], Liveness::Unknown),
            (
                vec![
                    BacklinkState::Missing,
                    BacklinkState::PointsElsewhere,
                    BacklinkState::NotASymlink,
                ],
                Liveness::Orphaned,
            ),
            (vec![BacklinkState::Missing, unknown()], Liveness::Unknown),
            (vec![unknown(), BacklinkState::Missing], Liveness::Unknown),
            (vec![unknown(), BacklinkState::Live], Liveness::Live),
            (vec![BacklinkState::Live, unknown()], Liveness::Live),
            (
                vec![
                    BacklinkState::Missing,
                    BacklinkState::Live,
                    BacklinkState::PointsElsewhere,
                ],
                Liveness::Live,
            ),
        ];
        for (states, expected) in data {
            assert_eq!(
                Liveness::from_backlinks(&states),
                expected,
                "for {states:?}"
            );
        }
    }

    #[test]
    fn test_decide() {
        const DAY: u64 = 24 * 60 * 60;
        let now = utc("2026-03-08T19:00:00Z");
        let ago = |secs: u64| now - Duration::from_secs(secs);
        let ahead = |secs: u64| now + Duration::from_secs(secs);
        let remove_by = |idle_secs: u64, signal| Decision::RemoveOrphan {
            idle: Duration::from_secs(idle_secs),
            signal,
        };
        let remove = |idle_secs: u64| remove_by(idle_secs, ActivitySignal::LastUsed);
        let orphan = |last_used| recognized(vec![BacklinkState::Missing], last_used);
        let built_orphan = |last_used, last_built| {
            recognized_built(
                vec![BacklinkState::Missing],
                last_used,
                BuildActivity::Last(last_built),
            )
        };
        let unknown_activity = || {
            BuildActivity::Unknown(PathError {
                path: "/store/entry/target".into(),
                error: io::Error::other("no access"),
            })
        };

        // Each case is the grace period in seconds, the entry, and the expected outcome.
        let data = [
            (DAY, orphan(ago(DAY + 1)), remove(DAY + 1)),
            (DAY, orphan(ago(DAY)), remove(DAY)),
            (
                DAY,
                orphan(ago(DAY - 1)),
                Decision::Keep(KeepReason::OrphanWithinGrace),
            ),
            (
                DAY,
                orphan(now),
                Decision::Keep(KeepReason::OrphanWithinGrace),
            ),
            (0, orphan(now), remove(0)),
            (
                0,
                orphan(ahead(1)),
                Decision::Keep(KeepReason::ActivityInFuture {
                    ahead: Duration::from_secs(1),
                    signal: ActivitySignal::LastUsed,
                }),
            ),
            (
                DAY,
                orphan(ahead(3 * DAY)),
                Decision::Keep(KeepReason::ActivityInFuture {
                    ahead: Duration::from_secs(3 * DAY),
                    signal: ActivitySignal::LastUsed,
                }),
            ),
            // With no backlinks at all, nothing links to the entry.
            (DAY, recognized(vec![], ago(DAY)), remove(DAY)),
            (
                DAY,
                recognized(
                    vec![BacklinkState::PointsElsewhere, BacklinkState::NotASymlink],
                    ago(DAY),
                ),
                remove(DAY),
            ),
            (
                DAY,
                recognized(
                    vec![BacklinkState::Missing, BacklinkState::Live],
                    ago(400 * DAY),
                ),
                Decision::Keep(KeepReason::Live),
            ),
            (
                DAY,
                recognized(vec![BacklinkState::Live], ahead(DAY)),
                Decision::Keep(KeepReason::Live),
            ),
            (
                DAY,
                recognized(vec![BacklinkState::Missing, unknown()], ago(400 * DAY)),
                Decision::Keep(KeepReason::UnknownBacklinks),
            ),
            (
                DAY,
                recognized(vec![unknown()], ahead(DAY)),
                Decision::Keep(KeepReason::UnknownBacklinks),
            ),
            // The later of the two times counts, whichever it is.
            (
                DAY,
                built_orphan(ago(3 * DAY), ago(2 * DAY)),
                remove_by(2 * DAY, ActivitySignal::LastBuilt),
            ),
            (
                DAY,
                built_orphan(ago(2 * DAY), ago(3 * DAY)),
                remove(2 * DAY),
            ),
            (DAY, built_orphan(ago(DAY), ago(DAY)), remove(DAY)),
            (
                DAY,
                built_orphan(ago(3 * DAY), ago(DAY)),
                remove_by(DAY, ActivitySignal::LastBuilt),
            ),
            (
                DAY,
                built_orphan(ago(3 * DAY), ago(DAY - 1)),
                Decision::Keep(KeepReason::OrphanBuiltWithinGrace {
                    idle: Duration::from_secs(DAY - 1),
                }),
            ),
            (
                DAY,
                built_orphan(ago(400 * DAY), now),
                Decision::Keep(KeepReason::OrphanBuiltWithinGrace {
                    idle: Duration::ZERO,
                }),
            ),
            // The last use alone keeps these, so the build is not the reason.
            (
                DAY,
                built_orphan(ago(3600), ago(60)),
                Decision::Keep(KeepReason::OrphanWithinGrace),
            ),
            (
                DAY,
                built_orphan(ago(60), ago(3 * DAY)),
                Decision::Keep(KeepReason::OrphanWithinGrace),
            ),
            (
                DAY,
                built_orphan(ago(3 * DAY), ahead(60)),
                Decision::Keep(KeepReason::ActivityInFuture {
                    ahead: Duration::from_secs(60),
                    signal: ActivitySignal::LastBuilt,
                }),
            ),
            (
                DAY,
                built_orphan(ahead(60), ago(3 * DAY)),
                Decision::Keep(KeepReason::ActivityInFuture {
                    ahead: Duration::from_secs(60),
                    signal: ActivitySignal::LastUsed,
                }),
            ),
            // An entry that can't be examined may have been built in a moment ago.
            (
                0,
                recognized_built(
                    vec![BacklinkState::Missing],
                    ago(400 * DAY),
                    unknown_activity(),
                ),
                Decision::Keep(KeepReason::UnknownBuildActivity),
            ),
            (
                DAY,
                recognized_built(vec![BacklinkState::Live], ago(DAY), unknown_activity()),
                Decision::Keep(KeepReason::Live),
            ),
            (
                DAY,
                recognized_built(vec![unknown()], ago(DAY), unknown_activity()),
                Decision::Keep(KeepReason::UnknownBacklinks),
            ),
            // Where the last use alone keeps the entry, its builds don't matter.
            (
                DAY,
                recognized_built(
                    vec![BacklinkState::Missing],
                    ago(DAY - 1),
                    unknown_activity(),
                ),
                Decision::Keep(KeepReason::OrphanWithinGrace),
            ),
            (
                DAY,
                built_orphan(ago(60), ahead(60)),
                Decision::Keep(KeepReason::OrphanWithinGrace),
            ),
            (
                DAY,
                built_orphan(ahead(60), ahead(120)),
                Decision::Keep(KeepReason::ActivityInFuture {
                    ahead: Duration::from_secs(60),
                    signal: ActivitySignal::LastUsed,
                }),
            ),
            (
                DAY,
                recognized_built(vec![BacklinkState::Missing], ahead(60), unknown_activity()),
                Decision::Keep(KeepReason::ActivityInFuture {
                    ahead: Duration::from_secs(60),
                    signal: ActivitySignal::LastUsed,
                }),
            ),
            (
                DAY,
                recognized_built(
                    vec![BacklinkState::Missing],
                    ago(DAY - 1),
                    BuildActivity::NotNeeded,
                ),
                Decision::Keep(KeepReason::OrphanWithinGrace),
            ),
            (
                DAY,
                recognized_built(
                    vec![BacklinkState::Live],
                    ago(DAY),
                    BuildActivity::NotNeeded,
                ),
                Decision::Keep(KeepReason::Live),
            ),
            // An entry is never removed without a look at its builds.
            (
                DAY,
                recognized_built(
                    vec![BacklinkState::Missing],
                    ago(DAY),
                    BuildActivity::NotNeeded,
                ),
                Decision::Keep(KeepReason::UnknownBuildActivity),
            ),
        ];
        for (grace_secs, entry, expected) in data {
            let policy = GcPolicy {
                orphan_grace: Duration::from_secs(grace_secs),
                max_age: None,
            };
            assert_eq!(
                decide(&entry, now, &policy),
                expected,
                "for {entry:?} with a grace of {grace_secs}s"
            );
        }
    }

    #[test]
    fn test_decide_with_max_age() {
        const DAY: u64 = 24 * 60 * 60;
        const GRACE: u64 = 7 * DAY;
        let now = utc("2026-03-08T19:00:00Z");
        let ago = |secs: u64| now - Duration::from_secs(secs);
        let ahead = |secs: u64| now + Duration::from_secs(secs);
        let empty_by = |idle_secs: u64, signal| Decision::EmptyLive {
            idle: Duration::from_secs(idle_secs),
            signal,
        };
        let empty = |idle_secs: u64| empty_by(idle_secs, ActivitySignal::LastUsed);
        let live = |last_used, build_activity| {
            recognized_built(
                vec![BacklinkState::Missing, BacklinkState::Live],
                last_used,
                build_activity,
            )
        };
        let built_live = |last_used, last_built| live(last_used, BuildActivity::Last(last_built));
        let unknown_activity = || {
            BuildActivity::Unknown(PathError {
                path: "/store/entry/target".into(),
                error: io::Error::other("no access"),
            })
        };
        let keep_live = Decision::Keep(KeepReason::Live);

        // Each case is the maximum age in seconds, the entry, and the expected outcome.
        let data = [
            (
                Some(30 * DAY),
                live(ago(30 * DAY + 1), BuildActivity::None),
                empty(30 * DAY + 1),
            ),
            (
                Some(30 * DAY),
                live(ago(30 * DAY), BuildActivity::None),
                empty(30 * DAY),
            ),
            (
                Some(30 * DAY),
                live(ago(30 * DAY - 1), BuildActivity::None),
                keep_live,
            ),
            (Some(30 * DAY), live(now, BuildActivity::None), keep_live),
            (Some(0), live(now, BuildActivity::None), empty(0)),
            // Without a maximum age, a live entry is never emptied.
            (None, live(ago(400 * DAY), BuildActivity::None), keep_live),
            (None, built_live(ago(400 * DAY), ago(400 * DAY)), keep_live),
            // The later of the two times counts, whichever it is.
            (
                Some(30 * DAY),
                built_live(ago(60 * DAY), ago(45 * DAY)),
                empty_by(45 * DAY, ActivitySignal::LastBuilt),
            ),
            (
                Some(30 * DAY),
                built_live(ago(45 * DAY), ago(60 * DAY)),
                empty(45 * DAY),
            ),
            (
                Some(30 * DAY),
                built_live(ago(60 * DAY), ago(30 * DAY)),
                empty_by(30 * DAY, ActivitySignal::LastBuilt),
            ),
            (
                Some(30 * DAY),
                built_live(ago(60 * DAY), ago(30 * DAY - 1)),
                keep_live,
            ),
            (Some(30 * DAY), built_live(ago(400 * DAY), now), keep_live),
            (
                Some(30 * DAY),
                built_live(ago(DAY), ago(400 * DAY)),
                keep_live,
            ),
            // A maximum age shorter than the grace period is still the limit for a live entry.
            (
                Some(DAY),
                live(ago(2 * DAY), BuildActivity::None),
                empty(2 * DAY),
            ),
            (
                Some(30 * DAY),
                live(ago(60 * DAY), BuildActivity::EmptyTarget),
                Decision::Keep(KeepReason::LiveAlreadyEmpty),
            ),
            (
                Some(30 * DAY),
                live(ago(DAY), BuildActivity::EmptyTarget),
                keep_live,
            ),
            // An entry is never emptied without a look at its builds.
            (
                Some(30 * DAY),
                live(ago(60 * DAY), BuildActivity::NotNeeded),
                Decision::Keep(KeepReason::UnknownBuildActivity),
            ),
            (
                Some(0),
                live(ago(60 * DAY), unknown_activity()),
                Decision::Keep(KeepReason::UnknownBuildActivity),
            ),
            (
                Some(30 * DAY),
                live(ago(DAY), unknown_activity()),
                keep_live,
            ),
            (
                Some(30 * DAY),
                live(ahead(60), BuildActivity::None),
                Decision::Keep(KeepReason::ActivityInFuture {
                    ahead: Duration::from_secs(60),
                    signal: ActivitySignal::LastUsed,
                }),
            ),
            (
                Some(30 * DAY),
                built_live(ago(60 * DAY), ahead(60)),
                Decision::Keep(KeepReason::ActivityInFuture {
                    ahead: Duration::from_secs(60),
                    signal: ActivitySignal::LastBuilt,
                }),
            ),
            // An orphan is removed or kept by the grace period alone, and never emptied.
            (
                Some(DAY),
                recognized(vec![BacklinkState::Missing], ago(GRACE)),
                Decision::RemoveOrphan {
                    idle: Duration::from_secs(GRACE),
                    signal: ActivitySignal::LastUsed,
                },
            ),
            (
                Some(DAY),
                recognized(vec![BacklinkState::Missing], ago(GRACE - 1)),
                Decision::Keep(KeepReason::OrphanWithinGrace),
            ),
            (
                Some(0),
                recognized_built(vec![], ago(400 * DAY), BuildActivity::Last(ago(GRACE - 1))),
                Decision::Keep(KeepReason::OrphanBuiltWithinGrace {
                    idle: Duration::from_secs(GRACE - 1),
                }),
            ),
            (
                Some(400 * DAY),
                recognized_built(vec![], ago(GRACE), BuildActivity::EmptyTarget),
                Decision::RemoveOrphan {
                    idle: Duration::from_secs(GRACE),
                    signal: ActivitySignal::LastUsed,
                },
            ),
            (
                Some(0),
                recognized(vec![BacklinkState::Missing, unknown()], ago(400 * DAY)),
                Decision::Keep(KeepReason::UnknownBacklinks),
            ),
        ];
        for (max_age_secs, entry, expected) in data {
            let policy = GcPolicy {
                orphan_grace: Duration::from_secs(GRACE),
                max_age: max_age_secs.map(Duration::from_secs),
            };
            assert_eq!(
                decide(&entry, now, &policy),
                expected,
                "for {entry:?} with a maximum age of {max_age_secs:?}s"
            );
        }
    }

    #[test]
    fn test_inspect_backlink() {
        let root = TestRoot::new();
        let entry_target = root.create_dir("store/entry/target");
        // As when the store path is a symlink to another disk.
        let store_alias = root.symlink(root.path("store"), "store-alias");
        let alias_target = store_alias.join("entry/target");
        let other_dir = root.create_dir("other-dir");
        let file = root.write_file("file", 1);

        let looping = root.symlink("target", "looping/target");
        let loop_error = fs::metadata(&looping).expect_err("a looping symlink doesn't resolve");

        let data = [
            (root.symlink(&entry_target, "live/target"), StateKind::Live),
            (
                root.symlink(&alias_target, "live-through-alias/target"),
                StateKind::Live,
            ),
            (
                root.symlink("../store/entry/../entry/target", "live-relative/target"),
                StateKind::Live,
            ),
            (
                root.symlink("../live/target", "live-through-link/target"),
                StateKind::Live,
            ),
            // The OS follows the symlink in these, so the path itself is no symlink.
            (root.path("live/target/"), StateKind::Live),
            (root.path("live/target/."), StateKind::Live),
            (
                root.create_dir("missing").join("target"),
                StateKind::Missing,
            ),
            (root.path("missing-parent/target"), StateKind::Missing),
            (file.join("target"), StateKind::Missing),
            (
                root.symlink(root.path("nowhere"), "dangling/target"),
                StateKind::PointsElsewhere,
            ),
            (
                root.symlink(file.join("target"), "dangling-through-file/target"),
                StateKind::PointsElsewhere,
            ),
            (
                root.symlink(&other_dir, "elsewhere/target"),
                StateKind::PointsElsewhere,
            ),
            (
                root.symlink(root.path("store/entry"), "entry-dir/target"),
                StateKind::PointsElsewhere,
            ),
            (
                root.symlink(&file, "to-file/target"),
                StateKind::PointsElsewhere,
            ),
            // The entry is named, but through a path to the store that doesn't resolve here.
            (
                root.symlink(
                    root.path("other-view/entry/target"),
                    "other-view-link/target",
                ),
                StateKind::Unknown(io::ErrorKind::NotFound),
            ),
            (
                root.symlink("../gone/store/entry/target/", "other-view-relative/target"),
                StateKind::Unknown(io::ErrorKind::NotFound),
            ),
            (
                root.symlink(
                    root.path("other-view/not-entry/target"),
                    "other-entry/target",
                ),
                StateKind::PointsElsewhere,
            ),
            (
                root.symlink(root.path("other-view/entry/target/debug"), "inside/target"),
                StateKind::PointsElsewhere,
            ),
            (root.create_dir("real-dir/target"), StateKind::NotASymlink),
            (
                root.write_file("real-file/target", 1),
                StateKind::NotASymlink,
            ),
            (looping, StateKind::Unknown(loop_error.kind())),
            (
                Utf8PathBuf::from("live/target"),
                StateKind::Unknown(io::ErrorKind::InvalidInput),
            ),
            (
                Utf8PathBuf::new(),
                StateKind::Unknown(io::ErrorKind::InvalidInput),
            ),
            (
                root.path("nul\0/target"),
                StateKind::Unknown(io::ErrorKind::InvalidInput),
            ),
        ];
        for (backlink, expected) in &data {
            // The entry's target directory is the same directory by either path.
            for entry_target in [&entry_target, &alias_target] {
                let state = inspect_backlink(backlink, entry_target);
                assert_eq!(
                    StateKind::new(&state),
                    *expected,
                    "for `{backlink}` with target `{entry_target}`"
                );
            }
        }

        // An entry without a target directory has no live backlinks.
        let no_target = root.path("store/no-target-entry/target");
        let data = [
            (
                root.symlink(&no_target, "to-no-target/target"),
                StateKind::PointsElsewhere,
            ),
            (root.path("elsewhere/target"), StateKind::PointsElsewhere),
            (root.path("missing/target"), StateKind::Missing),
        ];
        for (backlink, expected) in data {
            let state = inspect_backlink(&backlink, &no_target);
            assert_eq!(StateKind::new(&state), expected, "for `{backlink}`");
        }

        // A target directory that can't be examined might be where a path leads.
        let state = inspect_backlink(&root.path("live/target"), &root.path("looping/target"));
        assert_eq!(
            StateKind::new(&state),
            StateKind::Unknown(loop_error.kind())
        );

        // A target directory that is itself a symlink is followed.
        let linked_target = root.symlink(&other_dir, "store/linked-entry/target");
        let backlink = root.symlink(&linked_target, "to-linked/target");
        let state = inspect_backlink(&backlink, &linked_target);
        assert_eq!(StateKind::new(&state), StateKind::Live);
    }

    #[test]
    fn test_inspect_backlink_permission_denied() {
        let root = TestRoot::new();
        let entry_target = root.create_dir("store/entry/target");
        let live = root.symlink(&entry_target, "live/target");
        let behind_locked = root.create_dir("locked/inner");
        let into_locked = root.symlink(&behind_locked, "into-locked/target");
        let Some(_locked) = LockedDir::new(root.path("locked")) else {
            return;
        };

        let denied = StateKind::Unknown(io::ErrorKind::PermissionDenied);
        let data = [
            // The backlink can't be looked at.
            (root.path("locked/target"), &entry_target),
            // The backlink is a symlink, but where it leads can't be looked at.
            (into_locked, &entry_target),
            // The backlink resolves, but the entry's target directory can't be looked at.
            (live, &behind_locked),
        ];
        for (backlink, entry_target) in data {
            let state = inspect_backlink(&backlink, entry_target);
            assert_eq!(
                StateKind::new(&state),
                denied,
                "for `{backlink}` with target `{entry_target}`"
            );
        }
    }

    fn measure(tree: &Utf8Path) -> DiskUsage {
        let dir = Dir::open_ambient_dir(tree, ambient_authority()).expect("opened tree");
        let mut walk = UsageWalk::default();
        walk.measure_tree(&dir, tree.as_std_path());
        match walk.finish() {
            Measured::Usage(usage) => usage,
            Measured::Changed(path) => panic!("`{}` vanished", path.display()),
        }
    }

    /// The disk usage of each of `paths` itself, without following symlinks.
    fn usage_of(paths: &[&Utf8Path]) -> u64 {
        paths
            .iter()
            .map(|path| {
                let metadata = fs::symlink_metadata(path).expect("read metadata");
                metadata.blocks() * 512
            })
            .sum()
    }

    #[test]
    fn test_measure_tree() {
        const FILE_LEN: usize = 64 * 1024;
        let root = TestRoot::new();
        let entry = root.create_dir("entry");
        let file = root.write_file("entry/file", FILE_LEN);
        fs::hard_link(&file, root.path("entry/hard-link")).expect("created hard link");
        let nested_dir = root.create_dir("entry/dir");
        let nested_file = root.write_file("entry/dir/file", FILE_LEN);
        // A second link from outside the tree: the file is still counted, once.
        fs::hard_link(&nested_file, root.path("outside-hard-link")).expect("created hard link");
        let outside_file = root.write_file("outside/file", 16 * FILE_LEN);
        let dir_link = root.symlink(root.path("outside"), "entry/dir-link");
        let file_link = root.symlink(&outside_file, "entry/file-link");

        let usage = measure(&entry);

        assert!(
            usage_of(&[&file]) > 0,
            "the filesystem reports the blocks of a file as soon as it is written"
        );
        assert_eq!(
            usage.bytes,
            usage_of(&[
                &entry,
                &file,
                &nested_dir,
                &nested_file,
                &dir_link,
                &file_link
            ]),
        );
        assert!(usage.unmeasured.is_none(), "unmeasured: {usage:?}");
    }

    #[test]
    fn test_measure_tree_with_unreadable_dirs() {
        let root = TestRoot::new();
        let entry = root.create_dir("entry");
        let file = root.write_file("entry/file", 64 * 1024);
        root.write_file("entry/locked-1/file", 64 * 1024);
        root.write_file("entry/locked-2/file", 64 * 1024);
        let locked_dirs = [root.path("entry/locked-1"), root.path("entry/locked-2")];
        let [Some(_locked_1), Some(_locked_2)] = locked_dirs.clone().map(LockedDir::new) else {
            return;
        };

        let usage = measure(&entry);

        // Everything that can be read is still counted, including the directories themselves.
        assert_eq!(
            usage.bytes,
            usage_of(&[&entry, &file, &locked_dirs[0], &locked_dirs[1]])
        );
        let unmeasured = usage.unmeasured.expect("the size is a lower bound");
        assert!(
            locked_dirs
                .contains(&Utf8PathBuf::try_from(unmeasured.first.path).expect("path is UTF-8")),
            "the first path is one of the unreadable directories"
        );
        assert_eq!(
            (unmeasured.first.error.kind(), unmeasured.other_paths),
            (io::ErrorKind::PermissionDenied, 1)
        );
    }

    #[test]
    fn test_usage_walk_reports_a_path_that_vanished() {
        let root = TestRoot::new();
        let entry = root.create_dir("entry");
        let dir = Dir::open_ambient_dir(&entry, ambient_authority()).expect("opened tree");
        let not_found = || io::Error::from(io::ErrorKind::NotFound);

        let mut walk = UsageWalk::default();
        walk.measure_tree(&dir, entry.as_std_path());
        walk.record_unmeasured("/store/entry/locked".into(), io::Error::other("no access"));
        walk.record_unmeasured("/store/entry/gone".into(), not_found());
        walk.record_unmeasured("/store/entry/gone-later".into(), not_found());
        match walk.finish() {
            Measured::Changed(path) => assert_eq!(path, Path::new("/store/entry/gone")),
            Measured::Usage(usage) => panic!("the walk reported a size: {usage:?}"),
        }
    }

    fn read_build_activity(entry: &Utf8Path) -> (Vec<PathBuf>, BuildActivity) {
        let entry_dir = Dir::open_ambient_dir(entry, ambient_authority()).expect("opened");
        let build_dirs = find_build_dirs(&entry_dir, entry.as_std_path());
        let build_dirs = build_dirs.expect("found build dirs");
        let mut paths: Vec<_> = build_dirs.iter().map(|dir| dir.path.clone()).collect();
        paths.sort();
        let activity = BuildActivity::of(&entry_dir, entry.as_std_path(), &build_dirs);
        (paths, activity)
    }

    fn assert_last_built(entry: &Utf8Path, expected: Option<DateTime<Utc>>, when: &str) {
        match read_build_activity(entry).1 {
            BuildActivity::EmptyTarget | BuildActivity::None => {
                assert_eq!(None, expected, "{when}");
            }
            BuildActivity::Last(built) => assert_eq!(Some(built), expected, "{when}"),
            BuildActivity::Unknown(error) => panic!("{when}, the activity is unknown: {error}"),
            BuildActivity::NotNeeded => panic!("{when}, the activity was not looked for"),
        }
    }

    #[test]
    fn test_build_activity() {
        let root = TestRoot::new();
        let entry = root.create_dir("store/entry");
        let read = || read_build_activity(&entry);
        let assert_activity = |expected, when: &str| assert_last_built(&entry, expected, when);
        let build_dir = |relative: &str| {
            root.write_file(&format!("store/entry/{relative}/.cargo-lock"), 0);
            let deps = root.create_dir(&format!("store/entry/{relative}/deps"));
            move |modified| set_modified(&deps, modified)
        };
        let old = utc("2026-03-01T12:00:00Z");
        let older = utc("2026-02-01T12:00:00Z");
        let new = utc("2026-03-08T12:00:00Z");
        let newer = utc("2026-03-08T19:00:00Z");

        assert_activity(None, "without a target directory");
        root.create_dir("store/entry/target");
        assert_activity(None, "with an empty target directory");

        // None of these is a `deps` directory in a build directory that gc finds.
        set_modified(&root.create_dir("store/entry/target/doc/deps"), newer);
        set_modified(&root.create_dir("store/entry/target/deps"), newer);
        set_modified(&root.create_dir("store/entry/target/a/b/c/deps"), newer);
        root.write_file("store/entry/target/a/b/c/.cargo-lock", 0);
        set_modified(
            &root.create_dir("store/entry/target/tool/a/b/c/deps"),
            newer,
        );
        root.write_file("store/entry/target/tool/a/b/c/.cargo-lock", 0);
        root.write_file("store/entry/target/.cargo-lock", 0);
        root.write_file("store/entry/target/no-deps/.cargo-lock", 0);
        root.write_file("store/entry/target/file-deps/.cargo-lock", 0);
        root.write_file("store/entry/target/file-deps/deps", 0);
        root.symlink(
            root.path("store/entry/target/doc"),
            "store/entry/target/link",
        );
        assert_activity(None, "without a deps directory in a build directory");

        let set_debug = build_dir("target/debug");
        let set_triple_release = build_dir("target/x86_64-unknown-linux-gnu/release");
        let set_tool_debug = build_dir("target/tool/debug");
        let set_tool_triple_debug = build_dir("target/tool/x86_64-unknown-linux-gnu/debug");
        let build_dir_paths: Vec<_> = [
            "target/debug",
            "target/file-deps",
            "target/no-deps",
            "target/tool/debug",
            "target/tool/x86_64-unknown-linux-gnu/debug",
            "target/x86_64-unknown-linux-gnu/release",
        ]
        .map(|relative| entry.join(relative).into_std_path_buf())
        .into();
        assert_eq!(read().0, build_dir_paths);

        // The newest counts, at any depth.
        set_debug(old);
        set_triple_release(older);
        set_tool_debug(older);
        set_tool_triple_debug(older);
        assert_activity(Some(old), "with the newest at depth 1");
        set_triple_release(new);
        assert_activity(Some(new), "with the newest at depth 2");
        set_tool_triple_debug(newer);
        assert_activity(Some(newer), "with the newest at depth 3");
    }

    #[test]
    fn test_build_activity_of_units() {
        let root = TestRoot::new();
        let entry = root.create_dir("store/entry");
        let assert_activity = |expected, when: &str| assert_last_built(&entry, expected, when);
        let dir = |relative: &str| root.create_dir(&format!("store/entry/target/{relative}"));
        let file = |relative: &str| root.write_file(&format!("store/entry/target/{relative}"), 0);
        let old = utc("2026-03-01T12:00:00Z");
        let new = utc("2026-03-08T12:00:00Z");
        let newest = utc("2026-04-01T12:00:00Z");

        file("debug/.cargo-lock");
        file("thumbv7em-none-eabi/release/.cargo-lock");
        assert_activity(None, "with empty build directories");

        // gc dates a build by none of these; `serde-0f0f/out` is as Cargo 1.99 has it.
        let not_outputs = [
            "debug",
            "debug/.fingerprint",
            "debug/build",
            "debug/build/serde",
            "debug/build/serde/0f0f",
            "debug/build/serde/0f0f/run",
            "debug/build/serde-0f0f/out",
            "debug/build/no-output/a1a1",
            "debug/build/link-output/b2b2",
            "debug/incremental/serde-c3c3",
            "doc/build/serde/0f0f/out",
            "doc/.fingerprint/serde-0f0f",
        ]
        .map(dir);
        file("debug/.fingerprint/file");
        file("debug/build/file");
        file("debug/build/serde/file");
        file("debug/build/file-output/d4d4/out");
        file("debug/build/file-output/d4d4/fingerprint");
        for link in ["out", "fingerprint"] {
            root.symlink(
                root.path("store/entry/target/debug/build/serde/0f0f/run"),
                &format!("store/entry/target/debug/build/link-output/b2b2/{link}"),
            );
        }
        root.symlink(
            root.path("store/entry/target/doc/build/serde"),
            "store/entry/target/debug/build/link-package",
        );
        root.symlink(
            root.path("store/entry/target/doc/.fingerprint/serde-0f0f"),
            "store/entry/target/debug/.fingerprint/link-unit",
        );
        let set_not_outputs = || {
            for not_output in &not_outputs {
                set_modified(not_output, newest);
            }
        };
        set_not_outputs();
        assert_activity(None, "without an output directory");

        let outputs = [
            ("debug/deps", "in the output directory of Cargo 1.99"),
            (
                "debug/.fingerprint/serde-0f0f",
                "in a fingerprint directory of Cargo 1.99",
            ),
            ("debug/build/serde/0f0f/out", "in a unit"),
            (
                "debug/build/serde/0f0f/fingerprint",
                "in the fingerprint directory of a unit",
            ),
            (
                "debug/build/serde/e5e5/out",
                "in another unit of the package",
            ),
            ("debug/build/syn/f6f6/out", "in a unit of another package"),
            (
                "thumbv7em-none-eabi/release/build/serde/0f0f/out",
                "in a unit at depth 2",
            ),
        ]
        .map(|(relative, place)| (dir(relative), place));
        set_modified(&dir("debug/build/serde/0f0f/out/nested"), newest);
        set_not_outputs();
        for (output, _) in &outputs {
            set_modified(output, old);
        }
        assert_activity(
            Some(old),
            "with every output directory as old as the others",
        );

        for (output, place) in &outputs {
            set_modified(output, new);
            assert_activity(Some(new), &format!("with the newest {place}"));
            set_modified(output, old);
        }
    }

    #[test]
    fn test_build_activity_tells_an_empty_target_apart() {
        type Setup = fn(&TestRoot);
        // Each case puts one thing in the target directory, none of which shows a compile.
        let not_empty: [(&str, Setup); 4] = [
            ("a file", |root| {
                root.write_file("store/entry/target/.rustc_info.json", 0);
            }),
            ("a directory", |root| {
                root.create_dir("store/entry/target/doc");
            }),
            ("a dangling symlink", |root| {
                root.symlink("nowhere", "store/entry/target/link");
            }),
            ("a build directory that nothing was built in", |root| {
                root.write_file("store/entry/target/debug/.cargo-lock", 0);
            }),
        ];
        for (contents, setup) in not_empty {
            let root = TestRoot::new();
            let entry = root.create_dir("store/entry");
            let activity = || read_build_activity(&entry).1;
            match activity() {
                BuildActivity::EmptyTarget => {}
                activity => panic!("without a target directory, the activity is {activity:?}"),
            }
            root.create_dir("store/entry/target");
            match activity() {
                BuildActivity::EmptyTarget => {}
                activity => panic!("with an empty target directory, it is {activity:?}"),
            }

            setup(&root);
            match activity() {
                BuildActivity::None => {}
                activity => panic!("with {contents} in the target directory, it is {activity:?}"),
            }
        }
    }

    #[test]
    fn test_build_activity_unknown() {
        let read_error = |root: &TestRoot, when: &str| {
            let store = test_store(root);
            match BuildActivity::read(&store, &EntryName::new("entry")) {
                BuildActivity::Unknown(error) => error,
                activity @ (BuildActivity::NotNeeded
                | BuildActivity::EmptyTarget
                | BuildActivity::None
                | BuildActivity::Last(_)) => panic!("{when}, the activity is {activity:?}"),
            }
        };

        for name in ["build", ".fingerprint"] {
            let root = TestRoot::new();
            root.write_file("store/entry/target/debug/.cargo-lock", 0);
            let looping = root.symlink(name, &format!("store/entry/target/debug/{name}"));
            let error = read_error(&root, &format!("with a `{name}` that is a looping symlink"));
            assert_eq!(error.path, looping.into_std_path_buf());
        }

        let data = [
            "target",
            "target/debug",
            "target/debug/build",
            "target/debug/build/serde",
            "target/debug/build/serde/0f0f",
        ];
        for locked in data {
            let root = TestRoot::new();
            root.write_file("store/entry/target/debug/.cargo-lock", 0);
            root.create_dir("store/entry/target/debug/deps");
            root.create_dir("store/entry/target/debug/build/serde/0f0f/out");
            let locked_path = root.path(&format!("store/entry/{locked}"));
            let Some(_locked) = LockedDir::new(locked_path.clone()) else {
                return;
            };

            let when = format!("with `{locked}` locked");
            let error = read_error(&root, &when);
            // The directory itself or a name in it, depending on the platform.
            assert!(
                error.path.starts_with(&locked_path),
                "{when}, the error is at `{}`",
                error.path.display()
            );
            assert_eq!(
                error.error.kind(),
                io::ErrorKind::PermissionDenied,
                "{when}"
            );
        }

        // With no build directory, the target directory is looked into.
        let error_without_builds = |entry: &Utf8Path, when: &str| {
            let entry_dir = Dir::open_ambient_dir(entry, ambient_authority()).expect("opened");
            match BuildActivity::of(&entry_dir, entry.as_std_path(), &[]) {
                BuildActivity::Unknown(error) => error,
                activity @ (BuildActivity::NotNeeded
                | BuildActivity::EmptyTarget
                | BuildActivity::None
                | BuildActivity::Last(_)) => panic!("{when}, the activity is {activity:?}"),
            }
        };
        let root = TestRoot::new();
        let entry = root.create_dir("store/entry");
        let looping = root.symlink("target", "store/entry/target");
        let error = error_without_builds(&entry, "with a `target` that is a looping symlink");
        assert_eq!(error.path, looping.into_std_path_buf());

        let root = TestRoot::new();
        let entry = root.create_dir("store/entry");
        let target = root.create_dir("store/entry/target");
        let Some(_locked) = LockedDir::new(target.clone()) else {
            return;
        };
        let error = error_without_builds(&entry, "with `target` locked");
        assert_eq!(error.path, target.into_std_path_buf());
    }

    #[test]
    fn test_mtime_to_utc() {
        assert_eq!(
            mtime_to_utc(1_772_996_400, 500),
            Some(utc("2026-03-08T19:00:00.0000005Z"))
        );
        assert_eq!(
            mtime_to_utc(-1, 0),
            Some(utc("1969-12-31T23:59:59Z")),
            "before the epoch"
        );
        for (secs, nanos) in [
            (9_999_999_999_999, 0),
            (i64::MAX, 0),
            (i64::MIN, 0),
            (0, -1),
        ] {
            assert_eq!(mtime_to_utc(secs, nanos), None, "for {secs}s and {nanos}ns");
        }
    }

    #[test]
    fn test_examine_looks_at_builds_only_where_they_decide() {
        const DAY: u64 = 24 * 60 * 60;
        let now = utc("2026-03-08T19:00:00Z");
        let ago = |secs: u64| now - Duration::from_secs(secs);
        let policy = |max_age_days: Option<u64>| GcPolicy {
            orphan_grace: Duration::from_secs(7 * DAY),
            max_age: max_age_days.map(|days| Duration::from_secs(days * DAY)),
        };
        let built = ago(3600);
        let root = TestRoot::new();
        let store = test_store(&root);
        let live_link = |name: &str| {
            root.symlink(
                root.path(&format!("store/{name}/target")),
                &format!("workspaces/{name}/target"),
            )
        };
        let gone_link = root.path("workspaces/gone/target");

        // Each case is the entry, its backlink, its last use, the maximum age in days, and
        // the build that is seen if the builds are looked at.
        let data = [
            ("live", live_link("live"), ago(30 * DAY), None, None),
            ("recent", gone_link.clone(), ago(7 * DAY - 1), None, None),
            (
                "future",
                gone_link.clone(),
                now + Duration::from_secs(DAY),
                None,
                None,
            ),
            ("unused", gone_link, ago(7 * DAY), None, Some(built)),
            (
                "live-recent",
                live_link("live-recent"),
                ago(30 * DAY - 1),
                Some(30),
                None,
            ),
            (
                "live-unused",
                live_link("live-unused"),
                ago(30 * DAY),
                Some(30),
                Some(built),
            ),
        ];
        for (name, backlink, last_used, max_age_days, expected) in data {
            root.write_file(&format!("store/{name}/target/debug/.cargo-lock"), 0);
            let deps = root.create_dir(&format!("store/{name}/target/debug/deps"));
            set_modified(&deps, built);
            let metadata = TargetDirMetadata {
                backlinks: [backlink].into(),
                last_used: last_used.into(),
            };

            let policy = policy(max_age_days);
            let entry = examine(&store, EntryName::new(name), metadata, now, &policy);
            let looked_at = match entry.build_activity {
                BuildActivity::NotNeeded => None,
                BuildActivity::Last(built) => Some(built),
                activity @ (BuildActivity::EmptyTarget
                | BuildActivity::None
                | BuildActivity::Unknown(_)) => {
                    panic!("for `{name}`, the activity is {activity:?}")
                }
            };
            assert_eq!(looked_at, expected, "for `{name}`");
        }
    }

    #[test]
    fn test_kept_and_summary_lines() {
        let wording = GcMode::Remove.wording();
        let last_used = utc("2026-03-08T19:00:00Z");
        let unknown_activity = BuildActivity::Unknown(PathError {
            path: "/store/entry/target".into(),
            error: io::Error::other("no access"),
        });
        let entry = recognized_built(vec![], last_used, unknown_activity);
        let hour = Duration::from_secs(60 * 60);

        let data = [
            (KeepReason::Live, None),
            (KeepReason::LiveAlreadyEmpty, None),
            (KeepReason::OrphanWithinGrace, None),
            (
                KeepReason::OrphanBuiltWithinGrace { idle: 3 * hour },
                Some("kept `entry`: orphaned, last built 3h ago; no backlinks"),
            ),
            (
                KeepReason::UnknownBuildActivity,
                Some(
                    "kept `entry`: build activity unknown, could not examine \
                     `/store/entry/target`: no access; no backlinks",
                ),
            ),
            (
                KeepReason::ActivityInFuture {
                    ahead: 2 * hour,
                    signal: ActivitySignal::LastBuilt,
                },
                Some("kept `entry`: last built 2h in the future; no backlinks"),
            ),
        ];
        let mut tally = Tally::default();
        for (reason, expected) in data {
            assert_eq!(
                kept_line(wording, &entry, reason).as_deref(),
                expected,
                "for {reason:?}"
            );
            tally.kept.add(reason);
        }
        tally.kept.in_use += 1;
        let usage = |bytes| DiskUsage {
            bytes,
            unmeasured: None,
        };
        tally.removed.add(&usage(1024));
        tally.emptied.add(&usage(2048));
        tally.emptied.add(&usage(1024));

        let kept = "7 entries: 1 live, 1 already empty, 2 orphaned within grace, \
                    1 with unknown build activity, 1 last active in the future, 1 in use";
        let policy = |max_age| GcPolicy {
            orphan_grace: Duration::ZERO,
            max_age,
        };
        let (orphans_only, with_max_age) = (policy(None), policy(Some(Duration::ZERO)));
        let dry_wording = GcMode::DryRun.wording();
        assert_eq!(
            summary_line(wording, &orphans_only, &tally),
            format!("removed 1 entry (1.0 KiB) and kept {kept}")
        );
        assert_eq!(
            summary_line(wording, &with_max_age, &tally),
            format!("removed 1 entry (1.0 KiB), emptied 2 entries (3.0 KiB), and kept {kept}")
        );
        assert_eq!(
            summary_line(dry_wording, &with_max_age, &tally),
            format!("would remove 1 entry (1.0 KiB), empty 2 entries (3.0 KiB), and keep {kept}")
        );
        assert_eq!(tally.status(), GcStatus::Completed);

        tally.removed.failed += 2;
        assert_eq!(
            summary_line(wording, &orphans_only, &tally),
            format!("removed 1 entry (1.0 KiB), failed to remove 2 entries, and kept {kept}")
        );
        assert_eq!(tally.status(), GcStatus::Failed);

        tally.removed.failed = 0;
        tally.emptied.failed += 1;
        assert_eq!(
            summary_line(wording, &with_max_age, &tally),
            format!(
                "removed 1 entry (1.0 KiB), emptied 2 entries (3.0 KiB), \
                 failed to empty 1 entry, and kept {kept}"
            )
        );
        assert_eq!(tally.status(), GcStatus::Failed);

        let leftover_failed = Tally {
            failed_leftovers: 1,
            ..Tally::default()
        };
        assert_eq!(leftover_failed.status(), GcStatus::Failed);
    }

    #[test]
    fn test_emptying_line() {
        let emptying = Emptying {
            entry: recognized(
                vec![BacklinkState::Live, BacklinkState::Missing],
                utc("2026-03-08T19:00:00Z"),
            ),
            idle: Duration::from_secs(45 * 24 * 60 * 60),
            signal: ActivitySignal::LastBuilt,
        };
        let usage = DiskUsage {
            bytes: 1536,
            unmeasured: None,
        };
        let reason = "(1.5 KiB): live, last built 45d ago; backlinks: \
                      `/workspace-0/target` (live), `/workspace-1/target` (missing)";
        let data = [(GcMode::Remove, "emptied"), (GcMode::DryRun, "would empty")];
        for (mode, verb) in data {
            assert_eq!(
                emptying_line(mode.wording(), &emptying, &usage),
                format!("{verb} `entry` {reason}"),
            );
        }
    }

    #[test]
    fn test_removal_line_with_unmeasured_paths() {
        let removal = Removal {
            entry: recognized(
                vec![BacklinkState::Missing, BacklinkState::NotASymlink],
                utc("2026-03-08T19:00:00Z"),
            ),
            idle: Duration::from_secs(3 * 24 * 60 * 60),
            signal: ActivitySignal::LastUsed,
        };

        let data = [
            (0, ""),
            (1, " (and 1 other path)"),
            (2, " (and 2 other paths)"),
        ];
        for (other_paths, others) in data {
            let lower_bound = DiskUsage {
                bytes: 1536,
                unmeasured: Some(PathErrors {
                    first: PathError {
                        path: "/store/entry/target/locked".into(),
                        error: io::Error::other("no access"),
                    },
                    other_paths,
                }),
            };
            assert_eq!(
                removal_line(GcMode::DryRun.wording(), &removal, &lower_bound),
                format!(
                    "would remove `entry` (at least 1.5 KiB): orphaned, last used 3d ago; \
                     backlinks: `/workspace-0/target` (missing), `/workspace-1/target` \
                     (not a symlink); could not measure `/store/entry/target/locked`: \
                     no access{others}"
                ),
            );
        }
    }

    #[test]
    fn test_reported_size() {
        let mut total = ReportedSize::default();
        assert_eq!(total.to_string(), "0 B");

        total.add(&DiskUsage {
            bytes: 1024,
            unmeasured: None,
        });
        assert_eq!(total.to_string(), "1.0 KiB");

        total.add(&DiskUsage {
            bytes: 512,
            unmeasured: Some(PathErrors {
                first: PathError {
                    path: "/store/entry".into(),
                    error: io::Error::other("no access"),
                },
                other_paths: 0,
            }),
        });
        total.add(&DiskUsage {
            bytes: 512,
            unmeasured: None,
        });
        assert_eq!(total.to_string(), "at least 2.0 KiB");
    }

    #[test]
    fn test_human_bytes() {
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * KIB;
        const GIB: u64 = 1024 * MIB;
        let data = [
            (0, "0 B"),
            (512, "512 B"),
            (1023, "1023 B"),
            (KIB, "1.0 KiB"),
            (KIB + 512, "1.5 KiB"),
            // Rounded to the nearest tenth, with a half rounded up.
            (KIB + 51, "1.0 KiB"),
            (KIB + 52, "1.1 KiB"),
            (MIB - 52, "1023.9 KiB"),
            (MIB - 51, "1.0 MiB"),
            (MIB, "1.0 MiB"),
            (GIB - 1, "1.0 GiB"),
            (340 * GIB + GIB / 4, "340.3 GiB"),
            (1024 * GIB, "1.0 TiB"),
            (u64::MAX, "16.0 EiB"),
        ];
        for (bytes, expected) in data {
            assert_eq!(HumanBytes(bytes).to_string(), expected, "for {bytes}");
        }
    }

    #[test]
    fn test_age() {
        let data = [
            (0, "0s"),
            (59, "59s"),
            (60, "1m"),
            (3599, "59m"),
            (3600, "1h"),
            (86399, "23h"),
            (86400, "1d"),
            (3 * 86400 + 86399, "3d"),
            (400 * 86400, "400d"),
        ];
        for (secs, expected) in data {
            let age = Age(Duration::from_secs(secs));
            assert_eq!(age.to_string(), expected, "for {secs}s");
        }
        assert_eq!(Age(Duration::from_millis(999)).to_string(), "0s");
    }
}
