//! `gc-state.json` and `gc.log` for one store, in the user's state directory.

use crate::{
    gc::GcSummary,
    helpers::{try_lock_exclusive, TryLock},
    store::encode_workspace_path,
};
use atomicwrites::{AtomicFile, OverwriteBehavior};
use camino::Utf8Path;
use chrono::{DateTime, Utc};
use color_eyre::{
    eyre::{bail, eyre, WrapErr},
    Result,
};
use etcetera::{choose_base_strategy, BaseStrategy};
use serde::{Deserialize, Serialize};
use std::{
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// The version of `gc-state.json` that this targo reads and writes.
const STATE_VERSION: u32 = 1;

const STATE_FILE_NAME: &str = "gc-state.json";
const LOG_FILE_NAME: &str = "gc.log";
const OLD_LOG_FILE_NAME: &str = "gc.log.old";

/// A log larger than this is set aside as `gc.log.old` when the next run starts.
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// A background run, as `gc-state.json` records it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct LastRun {
    pub(super) started: DateTime<Utc>,
    /// `None` until the end of the run is recorded.
    pub(super) end: Option<RunEnd>,
}

/// How a background run ended, and whether the user has been told.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct RunEnd {
    pub(super) result: RunResult,
    pub(super) notice: NoticeState,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum RunResult {
    /// The run went through the store.
    Collected(GcSummary),
    /// The run left the store to a gc that was already running.
    AnotherGcRunning,
    /// The run left the store alone, since no entry in it was live.
    NoLiveEntry,
    /// The run could not be started, or gave up on an error.
    Failed { message: String },
    /// The run recorded no result, as when killed; wrap-cargo writes this one.
    Died,
}

/// Whether the user has been told of a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum NoticeState {
    Pending,
    Shown,
}

/// The contents of `gc-state.json`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct StateFile<R> {
    version: u32,
    last_run: R,
}

/// Just the version, which is read first: another version may have another shape.
#[derive(Debug, Deserialize)]
struct StateVersion {
    version: u32,
}

/// What reading `gc-state.json` came to.
#[derive(Debug)]
pub(super) enum StateRead {
    /// There is no file, so no background run has been started.
    Missing,
    Run(LastRun),
    /// The file can't be used. The next write replaces it.
    Unusable(Unusable),
}

/// Why a `gc-state.json` can't be used.
#[derive(Debug)]
pub(super) enum Unusable {
    Corrupt(serde_json::Error),
    UnknownVersion(u32),
}

impl fmt::Display for Unusable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt(error) => write!(f, "it could not be parsed ({error})"),
            Self::UnknownVersion(version) => write!(
                f,
                "it has version {version}, and this targo reads version {STATE_VERSION}"
            ),
        }
    }
}

fn parse_state(contents: &[u8]) -> StateRead {
    let version = match serde_json::from_slice::<StateVersion>(contents) {
        Ok(StateVersion { version }) => version,
        Err(error) => return StateRead::Unusable(Unusable::Corrupt(error)),
    };
    if version != STATE_VERSION {
        return StateRead::Unusable(Unusable::UnknownVersion(version));
    }
    match serde_json::from_slice::<StateFile<LastRun>>(contents) {
        Ok(file) => StateRead::Run(file.last_run),
        Err(error) => StateRead::Unusable(Unusable::Corrupt(error)),
    }
}

/// The directory that holds the state of automatic gc for one store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StateDir(PathBuf);

impl StateDir {
    /// Finds the directory for the store at `store_dir`, without creating it.
    pub(super) fn locate(store_dir: &Utf8Path) -> Result<Self> {
        let strategy =
            choose_base_strategy().wrap_err("failed to find the user's state directory")?;
        // The XDG strategy, which is the one for every Unix, always has one.
        let base = strategy
            .state_dir()
            .ok_or_else(|| eyre!("this platform has no state directory"))?;
        // A relative one would put the state wherever Cargo happens to be run.
        if !base.is_absolute() {
            bail!(
                "the user's state directory `{}` is not an absolute path",
                base.display()
            );
        }
        Ok(Self::in_base(&base, store_dir))
    }

    /// Each store has a directory of its own, named after the store's path.
    pub(super) fn in_base(base: &Path, store_dir: &Utf8Path) -> Self {
        Self(base.join("targo").join(encode_workspace_path(store_dir)))
    }

    pub(super) fn state_path(&self) -> PathBuf {
        self.0.join(STATE_FILE_NAME)
    }

    pub(super) fn log_path(&self) -> PathBuf {
        self.0.join(LOG_FILE_NAME)
    }

    fn create(&self) -> Result<()> {
        fs::create_dir_all(&self.0)
            .wrap_err_with(|| format!("failed to create the directory `{}`", self.0.display()))
    }

    pub(super) fn read_state(&self) -> Result<StateRead> {
        let path = self.state_path();
        match fs::read(&path) {
            Ok(contents) => Ok(parse_state(&contents)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(StateRead::Missing),
            Err(error) => {
                Err(error).wrap_err_with(|| format!("failed to read `{}`", path.display()))
            }
        }
    }

    /// Replaces `gc-state.json` in one step, so that a reader never sees part of it.
    pub(super) fn write_state(&self, last_run: &LastRun) -> Result<()> {
        self.create()?;
        let path = self.state_path();
        let file = StateFile {
            version: STATE_VERSION,
            last_run,
        };
        let mut json = serde_json::to_string(&file)
            .wrap_err_with(|| format!("failed to serialize {file:?}"))?;
        json.push('\n');
        AtomicFile::new(&path, OverwriteBehavior::AllowOverwrite)
            .write(|file| file.write_all(json.as_bytes()))
            .wrap_err_with(|| format!("failed to write `{}`", path.display()))
    }

    /// Opens `gc.log` and tries to lock it. A background run holds the lock until it exits.
    pub(super) fn lock_log(&self) -> Result<TryLock<fs::File>> {
        self.create()?;
        let path = self.log_path();
        let lock_error = || format!("failed to open and lock `{}`", path.display());
        let log = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .wrap_err_with(lock_error)?;
        try_lock_exclusive(log).wrap_err_with(lock_error)
    }

    /// As `lock_log`, but a log over the size limit is first renamed to `gc.log.old`.
    pub(super) fn lock_log_for_run(&self) -> Result<TryLock<fs::File>> {
        let log = match self.lock_log()? {
            TryLock::Acquired(log) => log,
            TryLock::Busy => return Ok(TryLock::Busy),
        };
        let path = self.log_path();
        let len = log
            .metadata()
            .wrap_err_with(|| format!("failed to read metadata for `{}`", path.display()))?
            .len();
        if len <= MAX_LOG_BYTES {
            return Ok(TryLock::Acquired(log));
        }
        // The lock is held here, so no run is writing to the log.
        let old_path = self.0.join(OLD_LOG_FILE_NAME);
        fs::rename(&path, &old_path).wrap_err_with(|| {
            format!(
                "failed to rename `{}` to `{}`",
                path.display(),
                old_path.display()
            )
        })?;
        self.lock_log()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        gc::{ReportedSize, SizeBound},
        helpers::tests::open_lock_file,
    };

    /// A state directory nested inside a temp dir.
    pub(in crate::auto_gc) struct TestStateDir {
        /// Held so that the directory is removed on drop.
        _temp_dir: camino_tempfile::Utf8TempDir,
        pub(in crate::auto_gc) state_dir: StateDir,
    }

    impl TestStateDir {
        pub(in crate::auto_gc) fn new() -> Self {
            let temp_dir = camino_tempfile::tempdir().expect("created temp dir");
            let base = temp_dir.path().join("a/b/c");
            Self {
                state_dir: StateDir::in_base(base.as_std_path(), Utf8Path::new("/store")),
                _temp_dir: temp_dir,
            }
        }
    }

    pub(in crate::auto_gc) fn utc(rfc3339: &str) -> DateTime<Utc> {
        rfc3339.parse().expect("parsed timestamp")
    }

    pub(in crate::auto_gc) fn read_run(state_dir: &StateDir) -> Option<LastRun> {
        match state_dir.read_state().expect("read state") {
            StateRead::Missing => None,
            StateRead::Run(last_run) => Some(last_run),
            StateRead::Unusable(unusable) => panic!("the state file is unusable: {unusable}"),
        }
    }

    #[test]
    fn test_state_dir_is_named_after_the_store() {
        let state_dir = StateDir::in_base(Path::new("/state"), Utf8Path::new("/opt/cargo/targo"));
        assert_eq!(
            (state_dir.state_path(), state_dir.log_path()),
            (
                PathBuf::from("/state/targo/_sopt_scargo_stargo/gc-state.json"),
                PathBuf::from("/state/targo/_sopt_scargo_stargo/gc.log"),
            )
        );
        assert_ne!(
            state_dir,
            StateDir::in_base(Path::new("/state"), Utf8Path::new("/opt/cargo_targo")),
            "stores don't share a directory"
        );
    }

    #[test]
    fn test_state_round_trips() {
        let test_dir = TestStateDir::new();
        let state_dir = &test_dir.state_dir;
        assert_eq!(read_run(state_dir), None, "there is no state at first");

        let started = utc("2026-03-08T19:00:00.123456789Z");
        let collected = RunResult::Collected(GcSummary {
            removed: 3,
            emptied: 1,
            failed: 2,
            freed: ReportedSize {
                bytes: 4096,
                bound: SizeBound::AtLeast,
            },
        });
        let ended = |result, notice| LastRun {
            started,
            end: Some(RunEnd { result, notice }),
        };
        let data = [
            (
                LastRun { started, end: None },
                r#"{"started":"2026-03-08T19:00:00.123456789Z","end":null}"#.to_owned(),
            ),
            (
                ended(collected, NoticeState::Pending),
                r#"{"started":"2026-03-08T19:00:00.123456789Z","end":{"result":{"collected":{"removed":3,"emptied":1,"failed":2,"freed":{"bytes":4096,"bound":"at-least"}}},"notice":"pending"}}"#
                    .to_owned(),
            ),
            (
                ended(RunResult::NoLiveEntry, NoticeState::Pending),
                r#"{"started":"2026-03-08T19:00:00.123456789Z","end":{"result":"no-live-entry","notice":"pending"}}"#
                    .to_owned(),
            ),
            (
                ended(RunResult::AnotherGcRunning, NoticeState::Pending),
                r#"{"started":"2026-03-08T19:00:00.123456789Z","end":{"result":"another-gc-running","notice":"pending"}}"#
                    .to_owned(),
            ),
            (
                ended(
                    RunResult::Failed {
                        message: "no \"store\"".to_owned(),
                    },
                    NoticeState::Shown,
                ),
                r#"{"started":"2026-03-08T19:00:00.123456789Z","end":{"result":{"failed":{"message":"no \"store\""}},"notice":"shown"}}"#
                    .to_owned(),
            ),
            (
                ended(RunResult::Died, NoticeState::Shown),
                r#"{"started":"2026-03-08T19:00:00.123456789Z","end":{"result":"died","notice":"shown"}}"#
                    .to_owned(),
            ),
        ];
        for (last_run, json) in data {
            state_dir.write_state(&last_run).expect("wrote state");
            assert_eq!(
                fs::read_to_string(state_dir.state_path()).expect("read state file"),
                format!("{{\"version\":1,\"last-run\":{json}}}\n")
            );
            assert_eq!(read_run(state_dir), Some(last_run));
        }
    }

    #[test]
    fn test_state_that_cannot_be_used() {
        let test_dir = TestStateDir::new();
        let state_dir = &test_dir.state_dir;
        fs::create_dir_all(&state_dir.0).expect("created state dir");
        let run = r#"{"started":"2026-03-08T19:00:00Z","end":null}"#;

        // A field that this targo doesn't know is passed over.
        let with_unknown_fields = r#"{"version":1,"last-run":{"started":"2026-03-08T19:00:00Z","end":null,"pid":7},"host":"x"}"#;
        fs::write(state_dir.state_path(), with_unknown_fields).expect("wrote state file");
        let expected = LastRun {
            started: utc("2026-03-08T19:00:00Z"),
            end: None,
        };
        assert_eq!(read_run(state_dir), Some(expected));

        let corrupt = [
            String::new(),
            "{".to_owned(),
            "[]".to_owned(),
            format!(r#"{{"last-run":{run}}}"#),
            r#"{"version":1}"#.to_owned(),
            r#"{"version":1,"last-run":null}"#.to_owned(),
            r#"{"version":1,"last-run":{"started":"soon","end":null}}"#.to_owned(),
            r#"{"version":1,"last-run":{"started":"2026-03-08T19:00:00Z","end":{"result":"lost","notice":"shown"}}}"#
                .to_owned(),
        ];
        for contents in corrupt {
            fs::write(state_dir.state_path(), &contents).expect("wrote state file");
            match state_dir.read_state().expect("read state") {
                StateRead::Unusable(Unusable::Corrupt(_)) => {}
                read => panic!("for {contents:?}, read {read:?}"),
            }
        }

        // Another version is not parsed any further.
        for contents in [
            format!(r#"{{"version":2,"last-run":{run}}}"#),
            r#"{"version":0,"runs":[]}"#.to_owned(),
        ] {
            fs::write(state_dir.state_path(), &contents).expect("wrote state file");
            match state_dir.read_state().expect("read state") {
                StateRead::Unusable(Unusable::UnknownVersion(0 | 2)) => {}
                read => panic!("for {contents:?}, read {read:?}"),
            }
        }

        // Whatever is there, a write replaces it.
        state_dir
            .write_state(&LastRun {
                started: utc("2026-03-09T19:00:00Z"),
                end: None,
            })
            .expect("wrote state");
        assert_eq!(
            read_run(state_dir).map(|last_run| last_run.started),
            Some(utc("2026-03-09T19:00:00Z"))
        );

        // An unreadable file is an error, not a missing file.
        fs::remove_file(state_dir.state_path()).expect("removed state file");
        fs::create_dir(state_dir.state_path()).expect("created dir");
        let error = state_dir.read_state().expect_err("a directory is no file");
        assert_eq!(
            error.to_string(),
            format!("failed to read `{}`", state_dir.state_path().display())
        );
    }

    #[test]
    fn test_log_lock_is_held_through_any_copy_of_the_log() {
        let test_dir = TestStateDir::new();
        let state_dir = &test_dir.state_dir;
        let lock = || state_dir.lock_log().expect("opened the log");

        let log = match lock() {
            TryLock::Acquired(log) => log,
            TryLock::Busy => panic!("nothing has the log locked"),
        };
        // As a background run inherits it.
        let inherited = log.try_clone().expect("cloned the log");
        drop(log);
        match lock() {
            TryLock::Acquired(_) => panic!("the copy keeps the log locked"),
            TryLock::Busy => {}
        }
        match state_dir.lock_log_for_run().expect("opened the log") {
            TryLock::Acquired(_) => panic!("the copy keeps the log locked"),
            TryLock::Busy => {}
        }

        drop(inherited);
        match lock() {
            TryLock::Acquired(_) => {}
            TryLock::Busy => panic!("the log is unlocked once every copy is closed"),
        }
    }

    #[test]
    fn test_large_log_is_set_aside_for_a_new_run() {
        let test_dir = TestStateDir::new();
        let state_dir = &test_dir.state_dir;
        let old_path = state_dir.0.join(OLD_LOG_FILE_NAME);
        let lock_for_run = || match state_dir.lock_log_for_run().expect("opened the log") {
            TryLock::Acquired(log) => log,
            TryLock::Busy => panic!("nothing has the log locked"),
        };
        let limit = usize::try_from(MAX_LOG_BYTES).expect("the limit fits in memory");

        let mut log = lock_for_run();
        log.write_all(&vec![b'x'; limit]).expect("wrote to the log");
        drop(log);
        let mut log = lock_for_run();
        assert!(!old_path.exists(), "a log at the limit is kept");
        log.write_all(b"y").expect("wrote to the log");
        drop(log);

        let mut log = lock_for_run();
        log.write_all(b"new").expect("wrote to the log");
        assert_eq!(
            fs::read(state_dir.log_path()).expect("read the log"),
            b"new"
        );
        assert_eq!(
            fs::metadata(&old_path).expect("read metadata").len(),
            MAX_LOG_BYTES + 1,
            "what was in the log is in the old log"
        );
        // The lock is on the new log, where the next `wrap-cargo` looks for it.
        let probe = open_lock_file(state_dir.log_path());
        match try_lock_exclusive(probe).expect("tried the log lock") {
            TryLock::Acquired(_) => panic!("the new log is locked"),
            TryLock::Busy => {}
        }
    }
}
