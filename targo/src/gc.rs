mod removal;

use self::removal::Remover;
use crate::{
    helpers::TryLock,
    metadata::TargetDirMetadata,
    store::{EntryName, StoreEntry, UnlockedStore, UnrecognizedDir},
};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std::fs::{Dir, Metadata, MetadataExt as _};
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
}

/// Whether gc removes what it decides to collect.
#[derive(Clone, Copy, Debug)]
pub(crate) enum GcMode {
    /// Report what would be removed, and change nothing.
    DryRun,
    Remove,
}

impl GcMode {
    fn wording(self) -> Wording {
        match self {
            Self::DryRun => Wording {
                remove: "would remove",
                keep: "would keep",
                skip: "would skip",
                and_keep: "keep",
            },
            Self::Remove => Wording {
                remove: "removed",
                keep: "kept",
                skip: "skipped",
                and_keep: "kept",
            },
        }
    }
}

/// The verbs of the report, which differ between a dry run and a real one.
#[derive(Clone, Copy, Debug)]
struct Wording {
    remove: &'static str,
    keep: &'static str,
    skip: &'static str,
    /// The second verb of the summary line.
    and_keep: &'static str,
}

/// How a gc run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub(crate) enum GcStatus {
    Completed,
    /// At least one removal failed. Each failure has been reported.
    Failed,
    /// Another gc run holds `gc.lock`, so this one did nothing.
    AnotherGcRunning,
}

/// Gives the current time.
pub(crate) type Clock<'a> = &'a dyn Fn() -> DateTime<Utc>;

/// Collects the store at `store_dir`. The report goes to `out`, and failures go to `err`.
///
/// `clock` is read again before each removal, which can come long after the run started.
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
    let mut removals = Vec::new();
    let mut kept_lines = Vec::new();
    for entry in store.entries()? {
        match entry {
            StoreEntry::Recognized { name, metadata } => {
                let build_activity = BuildActivity::read(&store, &name);
                let entry = classify(&store, name, metadata, build_activity);
                match decide(&entry, now, policy) {
                    Decision::RemoveOrphan { idle, signal } => removals.push(Removal {
                        entry,
                        idle,
                        signal,
                    }),
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
    removals.sort_by_key(|removal| Reverse(removal.idle));

    for removal in removals {
        // Only between entries, so that a run never stops between a rename and its delete.
        if output.is_closed() {
            break;
        }
        // Slow, so each line is written as soon as its entry is dealt with.
        match collector.collect(&store, removal, policy, clock)? {
            Outcome::Removed { removal, usage } => {
                output.line(removal_line(wording, &removal, &usage));
                tally.removed += 1;
                tally.freed.add(&usage);
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
            Outcome::Failed { name, error } => {
                // The alternate form puts the whole chain of causes on one line.
                output.failure(format_args!("failed to remove `{name}`: {error:#}"));
                tally.failed += 1;
            }
        }
    }
    for line in &kept_lines {
        output.line(line);
    }
    output.line(summary_line(wording, &tally));
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
    removed: usize,
    freed: ReportedSize,
    failed: usize,
    failed_leftovers: usize,
    kept: KeptCounts,
}

impl Tally {
    fn status(&self) -> GcStatus {
        if self.failed + self.failed_leftovers > 0 {
            GcStatus::Failed
        } else {
            GcStatus::Completed
        }
    }
}

/// An orphaned entry that is to be removed, with how long ago `signal` says it was active.
#[derive(Debug)]
struct Removal {
    entry: RecognizedEntry,
    idle: Duration,
    signal: ActivitySignal,
}

/// What came of an entry that was to be removed.
#[derive(Debug)]
enum Outcome {
    /// It was removed or, in a dry run, would be.
    Removed {
        removal: Removal,
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
    Failed {
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
        }
    }
}

/// What a run does with the entries that it decides to remove.
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

    /// Measures the entry of `removal` and then, unless this is a dry run, removes it.
    fn collect(
        &mut self,
        store: &UnlockedStore,
        removal: Removal,
        policy: &GcPolicy,
        clock: Clock<'_>,
    ) -> Result<Outcome> {
        let name = removal.entry.name.clone();
        // Slow, so it is done before `targo.lock` is taken.
        let usage = match measure_entry(store, &name) {
            Measured::Usage(usage) => usage,
            Measured::Changed(path) => {
                return Ok(Outcome::InUse {
                    name,
                    reason: InUse::ChangedWhileMeasured(path),
                });
            }
        };
        match self {
            Self::DryRun => Ok(Outcome::Removed { removal, usage }),
            Self::Remove(remover) => remover.remove(&name, usage, policy, clock),
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

/// The lock file that Cargo holds while it builds in a directory.
const CARGO_LOCK_NAME: &str = ".cargo-lock";

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

    /// When the `deps` directory last changed, which it does whenever anything is compiled.
    fn deps_modified(&self) -> Result<Option<DateTime<Utc>>, PathError> {
        let path_error = |error| PathError {
            path: self.path.join("deps"),
            error,
        };
        match self.dir.symlink_metadata("deps") {
            Ok(metadata) if metadata.is_dir() => {
                let modified = metadata.modified().map_err(path_error)?;
                Ok(Some(modified.into_std().into()))
            }
            Ok(_) => Ok(None),
            Err(error) if is_absent(&error) => Ok(None),
            Err(error) => Err(path_error(error)),
        }
    }
}

/// Finds the build directories of an entry, which are at `target/*` and `target/*/*`.
///
/// `entry_path` is only for naming paths in errors.
fn find_build_dirs(entry_dir: &Dir, entry_path: &Path) -> Result<Vec<BuildDir>, PathError> {
    let target_path = entry_path.join("target");
    let target_dir = match entry_dir.open_dir("target") {
        Ok(target_dir) => target_dir,
        Err(error) if is_absent(&error) => return Ok(Vec::new()),
        Err(error) => {
            return Err(PathError {
                path: target_path,
                error,
            })
        }
    };

    let mut build_dirs = Vec::new();
    for_each_subdir(&target_dir, &target_path, &mut |outer_dir, outer_path| {
        // With `--target`, the profile directories are one level down.
        for_each_subdir(&outer_dir, &outer_path, &mut |inner_dir, inner_path| {
            build_dirs.extend(BuildDir::new(inner_dir, inner_path)?);
            Ok(())
        })?;
        build_dirs.extend(BuildDir::new(outer_dir, outer_path)?);
        Ok(())
    })?;
    Ok(build_dirs)
}

/// Opens each real directory in `dir` in turn. One at a time, since there can be thousands.
fn for_each_subdir(
    dir: &Dir,
    path: &Path,
    visit: &mut dyn FnMut(Dir, PathBuf) -> Result<(), PathError>,
) -> Result<(), PathError> {
    let path_error = |error| PathError {
        path: path.to_owned(),
        error,
    };
    for dir_entry in dir.entries().map_err(path_error)? {
        let dir_entry = dir_entry.map_err(path_error)?;
        let subdir_path = path.join(dir_entry.file_name());
        // Not followed, so a symlink to a directory is passed over.
        let subdir = match dir_entry.metadata() {
            Ok(metadata) if metadata.is_dir() => dir_entry.open_dir(),
            Ok(_) => continue,
            Err(error) => Err(error),
        };
        match subdir {
            Ok(subdir) => visit(subdir, subdir_path)?,
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

/// When Cargo last built in an entry, going by its `deps` directories.
///
/// Unlike `last-used`, this also shows builds that didn't go through targo.
#[derive(Debug)]
enum BuildActivity {
    /// No build directory has a `deps` directory.
    None,
    Last(DateTime<Utc>),
    /// The entry could not be examined, so there may have been a build just now.
    Unknown(PathError),
}

impl BuildActivity {
    fn read(store: &UnlockedStore, name: &EntryName) -> Self {
        let build_dirs = match store.open_entry_dir(name) {
            Ok(entry_dir) => {
                find_build_dirs(entry_dir.dir().as_cap_std(), entry_dir.path().as_ref())
            }
            Err(error) => Err(PathError {
                path: store.entry_path(name).into(),
                error,
            }),
        };
        match build_dirs {
            Ok(build_dirs) => Self::of(&build_dirs),
            Err(error) => Self::Unknown(error),
        }
    }

    fn of(build_dirs: &[BuildDir]) -> Self {
        let mut newest = None;
        for build_dir in build_dirs {
            match build_dir.deps_modified() {
                // `None` is less than any time.
                Ok(modified) => newest = newest.max(modified),
                Err(error) => return Self::Unknown(error),
            }
        }
        match newest {
            Some(built) => Self::Last(built),
            None => Self::None,
        }
    }
}

impl fmt::Display for BuildActivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

/// Which of an entry's timestamps is its last activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivitySignal {
    /// `last-used` in the metadata, which only targo updates.
    LastUsed,
    /// The newest change to a `deps` directory.
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
    Keep(KeepReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeepReason {
    Live,
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

/// Decides what to do with `entry` at the time `now`.
fn decide(entry: &RecognizedEntry, now: DateTime<Utc>, policy: &GcPolicy) -> Decision {
    let reason = match entry.liveness() {
        Liveness::Live => KeepReason::Live,
        Liveness::Unknown => KeepReason::UnknownBacklinks,
        Liveness::Orphaned => {
            let last_built = match &entry.build_activity {
                BuildActivity::Unknown(_) => {
                    return Decision::Keep(KeepReason::UnknownBuildActivity)
                }
                BuildActivity::None => None,
                BuildActivity::Last(last_built) => Some(*last_built),
            };
            let (last_activity, signal) = match last_built {
                Some(last_built) if last_built > entry.last_used => {
                    (last_built, ActivitySignal::LastBuilt)
                }
                Some(_) | None => (entry.last_used, ActivitySignal::LastUsed),
            };
            match time_since(now, last_activity) {
                Ok(idle) if idle >= policy.orphan_grace => {
                    return Decision::RemoveOrphan { idle, signal };
                }
                Ok(idle) => match (signal, time_since(now, entry.last_used)) {
                    // Without the build, the entry would be removed.
                    (ActivitySignal::LastBuilt, Ok(unused)) if unused >= policy.orphan_grace => {
                        KeepReason::OrphanBuiltWithinGrace { idle }
                    }
                    (ActivitySignal::LastBuilt | ActivitySignal::LastUsed, Ok(_) | Err(_)) => {
                        KeepReason::OrphanWithinGrace
                    }
                },
                Err(ahead) => KeepReason::ActivityInFuture { ahead, signal },
            }
        }
    };
    Decision::Keep(reason)
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
    let mut line = format!(
        "{} `{}` ({}): orphaned, {} {} ago; {}",
        wording.remove,
        removal.entry.name,
        ReportedSize::of(usage),
        removal.signal,
        Age(removal.idle),
        BacklinkList(&removal.entry.backlinks),
    );
    if let Some(unmeasured) = &usage.unmeasured {
        line.push_str(&format!("; could not measure {unmeasured}"));
    }
    line
}

/// The line for an entry that is kept, if the reason is worth a line of its own.
fn kept_line(wording: Wording, entry: &RecognizedEntry, reason: KeepReason) -> Option<String> {
    let reason = match reason {
        KeepReason::Live | KeepReason::OrphanWithinGrace => return None,
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

fn summary_line(wording: Wording, tally: &Tally) -> String {
    let mut line = format!(
        "{} {} ({})",
        wording.remove,
        Entries(tally.removed),
        tally.freed
    );
    if tally.failed > 0 {
        line.push_str(&format!(", failed to remove {},", Entries(tally.failed)));
    }
    line.push_str(&format!(" and {} {}", wording.and_keep, tally.kept));
    line
}

/// How many entries are kept, by reason.
#[derive(Debug, Default)]
struct KeptCounts {
    live: usize,
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
        ];
        for (grace_secs, entry, expected) in data {
            let policy = GcPolicy {
                orphan_grace: Duration::from_secs(grace_secs),
            };
            assert_eq!(
                decide(&entry, now, &policy),
                expected,
                "for {entry:?} with a grace of {grace_secs}s"
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

    #[test]
    fn test_build_activity() {
        let root = TestRoot::new();
        let entry = root.create_dir("store/entry");
        let read = || {
            let entry_dir = Dir::open_ambient_dir(&entry, ambient_authority()).expect("opened");
            let build_dirs = find_build_dirs(&entry_dir, entry.as_std_path());
            let build_dirs = build_dirs.expect("found build dirs");
            let mut paths: Vec<_> = build_dirs.iter().map(|dir| dir.path.clone()).collect();
            paths.sort();
            (paths, BuildActivity::of(&build_dirs))
        };
        let assert_activity = |expected: Option<DateTime<Utc>>, when: &str| match read().1 {
            BuildActivity::None => assert_eq!(None, expected, "{when}"),
            BuildActivity::Last(built) => assert_eq!(Some(built), expected, "{when}"),
            BuildActivity::Unknown(error) => panic!("{when}, the activity is unknown: {error}"),
        };
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

        // None of these is a `deps` directory next to a `.cargo-lock` at depth 1 or 2.
        set_modified(&root.create_dir("store/entry/target/doc/deps"), newer);
        set_modified(&root.create_dir("store/entry/target/deps"), newer);
        set_modified(&root.create_dir("store/entry/target/a/b/c/deps"), newer);
        root.write_file("store/entry/target/a/b/c/.cargo-lock", 0);
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
        let build_dir_paths: Vec<_> = [
            "target/debug",
            "target/file-deps",
            "target/no-deps",
            "target/x86_64-unknown-linux-gnu/release",
        ]
        .map(|relative| entry.join(relative).into_std_path_buf())
        .into();
        assert_eq!(read().0, build_dir_paths);

        // The newest counts, at either depth.
        set_debug(old);
        set_triple_release(older);
        assert_activity(Some(old), "with the newest at depth 1");
        set_triple_release(new);
        assert_activity(Some(new), "with the newest at depth 2");
    }

    #[test]
    fn test_build_activity_unknown() {
        let root = TestRoot::new();
        root.write_file("store/entry/target/debug/.cargo-lock", 0);
        root.create_dir("store/entry/target/debug/deps");
        let store = test_store(&root);
        let locked = root.path("store/entry/target/debug");
        let Some(_locked) = LockedDir::new(locked.clone()) else {
            return;
        };

        match BuildActivity::read(&store, &EntryName::new("entry")) {
            BuildActivity::Unknown(error) => assert_eq!(
                (error.path, error.error.kind()),
                (locked.into(), io::ErrorKind::PermissionDenied)
            ),
            activity => panic!("the activity is {activity:?}"),
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
        tally.removed += 1;

        let kept = "kept 6 entries: 1 live, 2 orphaned within grace, \
                    1 with unknown build activity, 1 last active in the future, 1 in use";
        assert_eq!(
            summary_line(wording, &tally),
            format!("removed 1 entry (0 B) and {kept}")
        );
        assert_eq!(tally.status(), GcStatus::Completed);

        tally.failed += 2;
        assert_eq!(
            summary_line(wording, &tally),
            format!("removed 1 entry (0 B), failed to remove 2 entries, and {kept}")
        );
        assert_eq!(tally.status(), GcStatus::Failed);

        let leftover_failed = Tally {
            failed_leftovers: 1,
            ..Tally::default()
        };
        assert_eq!(leftover_failed.status(), GcStatus::Failed);
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
