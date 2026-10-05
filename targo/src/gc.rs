use crate::{
    metadata::TargetDirMetadata,
    store::{EntryName, StoreEntry, UnlockedStore, UnrecognizedDir},
};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std::fs::{Dir, Metadata, MetadataExt as _};
use chrono::{DateTime, Utc};
use color_eyre::{eyre::WrapErr, Result};
use std::{
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
    /// How long an orphaned entry is kept after it was last used.
    pub(crate) orphan_grace: Duration,
}

/// Reports to `out` what gc would collect from the store at `store_dir`, and changes nothing.
pub(crate) fn dry_run(
    store_dir: Utf8PathBuf,
    policy: &GcPolicy,
    now: DateTime<Utc>,
    out: &mut dyn Write,
) -> Result<()> {
    let Some(store) = UnlockedStore::open(store_dir.clone())? else {
        return report_result(writeln!(
            out,
            "nothing to collect: there is no targo store at `{store_dir}`"
        ));
    };

    let mut removals = Vec::new();
    let mut kept_counts = KeptCounts::default();
    let mut kept_lines = Vec::new();
    for entry in store.entries()? {
        match entry {
            StoreEntry::Recognized { name, metadata } => {
                let entry = classify(&store, name, metadata);
                match decide(&entry, now, policy) {
                    Decision::RemoveOrphan { idle } => removals.push((entry, idle)),
                    Decision::Keep(reason) => {
                        kept_counts.add(reason);
                        kept_lines.extend(kept_line(&entry, reason));
                    }
                }
            }
            StoreEntry::Unrecognized(dir) => {
                kept_counts.unrecognized += 1;
                kept_lines.push(unrecognized_line(&dir));
            }
        }
    }
    // Oldest first. The sort is stable, so entries used at the same time stay in name order.
    removals.sort_by_key(|(entry, _)| entry.last_used);

    report_result(write_report(
        &store,
        &removals,
        &kept_lines,
        &kept_counts,
        out,
    ))
}

fn write_report(
    store: &UnlockedStore,
    removals: &[(RecognizedEntry, Duration)],
    kept_lines: &[String],
    kept_counts: &KeptCounts,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut total_size = ReportedSize::default();
    for (entry, idle) in removals {
        // Slow, so each line is written as soon as its entry is measured.
        let usage = measure_entry(store, &entry.name);
        writeln!(out, "{}", removal_line(entry, *idle, &usage))?;
        total_size.add(&usage);
    }
    for line in kept_lines {
        writeln!(out, "{line}")?;
    }
    writeln!(
        out,
        "would remove {} ({total_size}) and keep {kept_counts}",
        Entries(removals.len()),
    )
}

/// A reader that stops early, as with `targo gc --dry-run | head`, is not a failure.
fn report_result(result: io::Result<()>) -> Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result.wrap_err("failed to write the gc report"),
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
    match fs::metadata(backlink) {
        Ok(resolved) => match fs::metadata(entry_target) {
            // Not the path text: the same directory can be reached through several paths.
            Ok(target) if (resolved.dev(), resolved.ino()) == (target.dev(), target.ino()) => {
                return BacklinkState::Live;
            }
            Ok(_) => {}
            // The entry has no target directory, so the path leads somewhere else.
            Err(error) if is_absent(&error) => {}
            Err(error) => return BacklinkState::Unknown(error),
        },
        // Nothing is there, or a symlink dangles.
        Err(error) if is_absent(&error) => {}
        Err(error) => return BacklinkState::Unknown(error),
    }

    match fs::symlink_metadata(backlink) {
        Ok(metadata) if metadata.is_symlink() => BacklinkState::PointsElsewhere,
        Ok(_) => BacklinkState::NotASymlink,
        Err(error) if is_absent(&error) => BacklinkState::Missing,
        Err(error) => BacklinkState::Unknown(error),
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

/// An entry with readable metadata, and the state of each of its backlinks.
#[derive(Debug)]
struct RecognizedEntry {
    name: EntryName,
    last_used: DateTime<Utc>,
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
        backlinks,
    }
}

/// What gc does with an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    /// Remove an orphaned entry, last used `idle` ago.
    RemoveOrphan {
        idle: Duration,
    },
    Keep(KeepReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeepReason {
    Live,
    OrphanWithinGrace,
    UnknownBacklinks,
    /// The clock went backwards or is wrong, so the age of the entry is unknown.
    LastUsedInFuture {
        ahead: Duration,
    },
}

/// Decides what to do with `entry` at the time `now`.
fn decide(entry: &RecognizedEntry, now: DateTime<Utc>, policy: &GcPolicy) -> Decision {
    let reason = match entry.liveness() {
        Liveness::Live => KeepReason::Live,
        Liveness::Unknown => KeepReason::UnknownBacklinks,
        Liveness::Orphaned => {
            let since_last_used = now.signed_duration_since(entry.last_used);
            // Only a negative duration fails to convert.
            match since_last_used.to_std() {
                Ok(idle) if idle >= policy.orphan_grace => {
                    return Decision::RemoveOrphan { idle };
                }
                Ok(_) => KeepReason::OrphanWithinGrace,
                Err(_) => KeepReason::LastUsedInFuture {
                    ahead: since_last_used
                        .abs()
                        .to_std()
                        .expect("an absolute value is not negative"),
                },
            }
        }
    };
    Decision::Keep(reason)
}

/// The disk usage of a tree.
#[derive(Debug)]
struct DiskUsage {
    bytes: u64,
    /// Set if part of the tree could not be measured, which makes `bytes` a lower bound.
    unmeasured: Option<UnmeasuredPaths>,
}

#[derive(Debug)]
struct UnmeasuredPaths {
    first_path: PathBuf,
    first_error: io::Error,
    other_paths: u64,
}

/// Measures the disk usage of the entry `name`, which can take minutes.
fn measure_entry(store: &UnlockedStore, name: &EntryName) -> DiskUsage {
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
    unmeasured: Option<UnmeasuredPaths>,
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
        match &mut self.unmeasured {
            Some(unmeasured) => unmeasured.other_paths += 1,
            None => {
                self.unmeasured = Some(UnmeasuredPaths {
                    first_path: path,
                    first_error: error,
                    other_paths: 0,
                });
            }
        }
    }

    fn finish(self) -> DiskUsage {
        DiskUsage {
            bytes: self.bytes,
            unmeasured: self.unmeasured,
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

fn removal_line(entry: &RecognizedEntry, idle: Duration, usage: &DiskUsage) -> String {
    let mut size = ReportedSize::default();
    size.add(usage);
    let mut line = format!(
        "would remove `{}` ({size}): orphaned, last used {} ago; {}",
        entry.name,
        Age(idle),
        BacklinkList(&entry.backlinks),
    );
    if let Some(unmeasured) = &usage.unmeasured {
        line.push_str(&format!(
            "; could not measure `{}`: {}",
            unmeasured.first_path.display(),
            unmeasured.first_error,
        ));
        match unmeasured.other_paths {
            0 => {}
            1 => line.push_str(" (and 1 other path)"),
            others => line.push_str(&format!(" (and {others} other paths)")),
        }
    }
    line
}

/// The line for an entry that is kept, if the reason is worth a line of its own.
fn kept_line(entry: &RecognizedEntry, reason: KeepReason) -> Option<String> {
    let reason = match reason {
        KeepReason::Live | KeepReason::OrphanWithinGrace => return None,
        KeepReason::UnknownBacklinks => "backlink state unknown".to_owned(),
        KeepReason::LastUsedInFuture { ahead } => {
            format!("last used {} in the future", Age(ahead))
        }
    };
    Some(format!(
        "would keep `{}`: {reason}; {}",
        entry.name,
        BacklinkList(&entry.backlinks),
    ))
}

fn unrecognized_line(dir: &UnrecognizedDir) -> String {
    format!(
        "would keep `{}`: unrecognized; {}",
        Path::new(&dir.name).display(),
        dir.reason,
    )
}

/// How many entries are kept, by reason.
#[derive(Debug, Default)]
struct KeptCounts {
    live: usize,
    orphans_within_grace: usize,
    unknown_backlinks: usize,
    unrecognized: usize,
    last_used_in_future: usize,
}

impl KeptCounts {
    fn add(&mut self, reason: KeepReason) {
        let count = match reason {
            KeepReason::Live => &mut self.live,
            KeepReason::OrphanWithinGrace => &mut self.orphans_within_grace,
            KeepReason::UnknownBacklinks => &mut self.unknown_backlinks,
            KeepReason::LastUsedInFuture { .. } => &mut self.last_used_in_future,
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
            (self.unrecognized, "unrecognized"),
            (self.last_used_in_future, "last used in the future"),
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
    use cap_std::ambient_authority;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// A directory nested inside a temp dir, so that a path with `..` in it stays inside.
    struct TestRoot {
        // Held so that the directory is removed on drop.
        _temp_dir: camino_tempfile::Utf8TempDir,
        root: Utf8PathBuf,
    }

    impl TestRoot {
        fn new() -> Self {
            let temp_dir = camino_tempfile::tempdir().expect("created temp dir");
            let root = temp_dir.path().join("a/b/c");
            fs::create_dir_all(&root).expect("created root");
            Self {
                _temp_dir: temp_dir,
                root,
            }
        }

        fn path(&self, relative: &str) -> Utf8PathBuf {
            self.root.join(relative)
        }

        fn create_dir(&self, relative: &str) -> Utf8PathBuf {
            let path = self.path(relative);
            fs::create_dir_all(&path).expect("created dir");
            path
        }

        fn write_file(&self, relative: &str, len: usize) -> Utf8PathBuf {
            let path = self.path(relative);
            fs::create_dir_all(path.parent().expect("path has a parent")).expect("created dir");
            let mut file = fs::File::create(&path).expect("created file");
            file.write_all(&vec![b'x'; len]).expect("wrote file");
            // So that the size the filesystem reports for the file is settled.
            file.sync_all().expect("synced file");
            path
        }

        /// Creates a symlink at `relative` with `dest` as its contents.
        fn symlink(&self, dest: impl AsRef<Utf8Path>, relative: &str) -> Utf8PathBuf {
            let path = self.path(relative);
            fs::create_dir_all(path.parent().expect("path has a parent")).expect("created dir");
            symlink(dest.as_ref(), &path).expect("created symlink");
            path
        }
    }

    /// A directory that nothing can be reached through, until this is dropped.
    struct LockedDir(Utf8PathBuf);

    impl LockedDir {
        /// Returns `None` if permissions are not enforced, as when running as root.
        fn new(path: Utf8PathBuf) -> Option<Self> {
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
            backlinks,
        }
    }

    fn utc(rfc3339: &str) -> DateTime<Utc> {
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
        let remove = |idle_secs: u64| Decision::RemoveOrphan {
            idle: Duration::from_secs(idle_secs),
        };
        let orphan = |last_used| recognized(vec![BacklinkState::Missing], last_used);

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
                Decision::Keep(KeepReason::LastUsedInFuture {
                    ahead: Duration::from_secs(1),
                }),
            ),
            (
                DAY,
                orphan(ahead(3 * DAY)),
                Decision::Keep(KeepReason::LastUsedInFuture {
                    ahead: Duration::from_secs(3 * DAY),
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
        walk.finish()
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
                .contains(&Utf8PathBuf::try_from(unmeasured.first_path).expect("path is UTF-8")),
            "the first path is one of the unreadable directories"
        );
        assert_eq!(
            (unmeasured.first_error.kind(), unmeasured.other_paths),
            (io::ErrorKind::PermissionDenied, 1)
        );
    }

    #[test]
    fn test_removal_line_with_unmeasured_paths() {
        let entry = recognized(
            vec![BacklinkState::Missing, BacklinkState::NotASymlink],
            utc("2026-03-08T19:00:00Z"),
        );
        let idle = Duration::from_secs(3 * 24 * 60 * 60);

        let data = [
            (0, ""),
            (1, " (and 1 other path)"),
            (2, " (and 2 other paths)"),
        ];
        for (other_paths, others) in data {
            let lower_bound = DiskUsage {
                bytes: 1536,
                unmeasured: Some(UnmeasuredPaths {
                    first_path: "/store/entry/target/locked".into(),
                    first_error: io::Error::other("no access"),
                    other_paths,
                }),
            };
            assert_eq!(
                removal_line(&entry, idle, &lower_bound),
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
            unmeasured: Some(UnmeasuredPaths {
                first_path: "/store/entry".into(),
                first_error: io::Error::other("no access"),
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
