//! Runs gc in the background from `wrap-cargo`, at most once a day.

mod state;

use self::state::{LastRun, NoticeState, RunEnd, RunResult, StateDir, StateRead, Unusable};
use crate::{
    dispatch::STORE_DIR_ENV,
    gc::{Entries, GcStatus, SizeBound},
    helpers::TryLock,
    store::LockedStore,
};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use color_eyre::{
    eyre::{bail, eyre, WrapErr},
    Result,
};
use std::{
    env,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{self, Write},
    os::{fd::RawFd, unix::process::CommandExt},
    path::Path,
    process::{Command, Stdio},
};

const AUTO_GC_ENV: &str = "TARGO_AUTO_GC";

const INTERVAL: TimeDelta = TimeDelta::hours(24);

/// A run that freed less than this, with no failure, gets no notice.
const NOTICE_MIN_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoGcSetting {
    On,
    Off,
}

impl AutoGcSetting {
    /// Only `0` and `1` are accepted: gc deletes, so another value is not guessed at.
    fn parse(value: Option<&OsStr>) -> Result<Self, BadAutoGcSetting> {
        match value {
            None => Ok(Self::On),
            Some(value) if value == "1" => Ok(Self::On),
            Some(value) if value == "0" => Ok(Self::Off),
            Some(value) => Err(BadAutoGcSetting(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BadAutoGcSetting(OsString);

impl fmt::Display for BadAutoGcSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{AUTO_GC_ENV}` must be `0` (off) or `1` (on), but is set to {:?}",
            self.0
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RunStatus {
    Never,
    Running { started: DateTime<Utc> },
    Ended { started: DateTime<Utc>, end: RunEnd },
}

impl RunStatus {
    /// For when no run has the log locked: one that recorded no result has died.
    fn settled(last_run: Option<LastRun>) -> Self {
        match last_run {
            None => Self::Never,
            Some(LastRun { started, end }) => Self::Ended {
                started,
                end: end.unwrap_or(RunEnd {
                    result: RunResult::Died,
                    notice: NoticeState::Pending,
                }),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartDecision {
    Start,
    NotDue,
    StillRunning,
}

fn decide_start(status: &RunStatus, now: DateTime<Utc>) -> StartDecision {
    let started = match status {
        RunStatus::Never => return StartDecision::Start,
        RunStatus::Running { .. } => return StartDecision::StillRunning,
        RunStatus::Ended { started, .. } => *started,
    };
    // Either direction, so a start time in the future can't put gc off for long.
    if now.signed_duration_since(started).abs() >= INTERVAL {
        StartDecision::Start
    } else {
        StartDecision::NotDue
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    started: DateTime<Utc>,
    result: RunResult,
}

impl Notice {
    fn shown(&self) -> LastRun {
        LastRun {
            started: self.started,
            end: Some(RunEnd {
                result: self.result.clone(),
                notice: NoticeState::Shown,
            }),
        }
    }

    fn line(&self, log_path: &Path) -> String {
        format!(
            "[targo] gc: the background run of {} {}; see `{}`",
            timestamp(self.started),
            self.result,
            log_path.display()
        )
    }
}

fn notice_for(status: &RunStatus) -> Option<Notice> {
    match status {
        RunStatus::Never | RunStatus::Running { .. } => None,
        RunStatus::Ended { started, end } => match end.notice {
            NoticeState::Shown => None,
            NoticeState::Pending => is_worth_a_notice(&end.result).then(|| Notice {
                started: *started,
                result: end.result.clone(),
            }),
        },
    }
}

fn is_worth_a_notice(result: &RunResult) -> bool {
    match result {
        RunResult::Collected(summary) => {
            let freed_enough = match summary.freed.bound {
                SizeBound::Exact => summary.freed.bytes >= NOTICE_MIN_BYTES,
                // A lower bound is not known to be small.
                SizeBound::AtLeast => true,
            };
            summary.failed > 0 || freed_enough
        }
        RunResult::AnotherGcRunning | RunResult::NoLiveEntry => false,
        RunResult::Failed { .. } | RunResult::Died => true,
    }
}

impl fmt::Display for RunResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Collected(summary) => {
                let (removed, emptied) = (Entries(summary.removed), Entries(summary.emptied));
                match (summary.removed, summary.emptied) {
                    (0, 0) => f.write_str("removed nothing")?,
                    (_, 0) => write!(f, "removed {removed} ({})", summary.freed)?,
                    (0, _) => write!(f, "emptied {emptied} ({})", summary.freed)?,
                    (_, _) => write!(
                        f,
                        "removed {removed} and emptied {emptied} ({})",
                        summary.freed
                    )?,
                }
                match summary.failed {
                    0 => Ok(()),
                    1 => f.write_str(", with 1 failure"),
                    failed => write!(f, ", with {failed} failures"),
                }
            }
            Self::AnotherGcRunning => f.write_str("did nothing, since another gc was running"),
            Self::NoLiveEntry => f.write_str("left the store alone, since no entry in it was live"),
            Self::Failed { message } => write!(f, "failed: {message}"),
            Self::Died => f.write_str("ended without recording a result"),
        }
    }
}

/// Rendered the same in the notice and in the log header, so that one can be matched to the other.
fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn last_run_status(state_dir: &StateDir) -> Result<(RunStatus, Option<Unusable>)> {
    let (last_run, unusable) = split_read(state_dir.read_state()?);
    let Some(LastRun { started, end: None }) = last_run else {
        return Ok((RunStatus::settled(last_run), unusable));
    };
    match state_dir.lock_log()? {
        TryLock::Busy => Ok((RunStatus::Running { started }, unusable)),
        // The run is gone, so what the state file says now is final; read again.
        TryLock::Acquired(_log) => {
            let (last_run, unusable) = split_read(state_dir.read_state()?);
            Ok((RunStatus::settled(last_run), unusable))
        }
    }
}

fn split_read(read: StateRead) -> (Option<LastRun>, Option<Unusable>) {
    match read {
        StateRead::Missing => (None, None),
        StateRead::Run(last_run) => (Some(last_run), None),
        StateRead::Unusable(unusable) => (None, Some(unusable)),
    }
}

#[derive(Debug)]
struct Prepared {
    notice: Option<Notice>,
    /// The locked log of a run that is to be started now.
    log: Option<fs::File>,
    /// Set only once the unusable state file has been replaced.
    replaced: Option<Unusable>,
}

/// State is written before anything is shown or started, so neither happens twice.
fn prepare(state_dir: &StateDir, now: DateTime<Utc>) -> Result<Prepared> {
    let (status, unusable) = last_run_status(state_dir)?;
    let notice = notice_for(&status);
    let log = match decide_start(&status, now) {
        StartDecision::Start => match state_dir.lock_log_for_run()? {
            TryLock::Acquired(log) => Some(log),
            TryLock::Busy => {
                tracing::debug!("no background gc started: one still has the log locked");
                None
            }
        },
        StartDecision::NotDue | StartDecision::StillRunning => None,
    };

    let next = match (&log, &notice) {
        (Some(_), _) => Some(LastRun {
            started: now,
            end: None,
        }),
        (None, Some(notice)) => Some(notice.shown()),
        (None, None) => None,
    };
    let replaced = match next {
        Some(next) => {
            state_dir.write_state(&next)?;
            unusable
        }
        None => None,
    };
    Ok(Prepared {
        notice,
        log,
        replaced,
    })
}

/// What `wrap-cargo` has left to do for automatic gc once the store is unlocked.
#[derive(Debug)]
#[must_use]
pub(crate) struct AfterUnlock {
    /// For stderr.
    lines: Vec<String>,
    run: Option<RunToStart>,
}

/// A run whose start is recorded, and whose log is locked.
#[derive(Debug)]
struct RunToStart {
    state_dir: StateDir,
    store_dir: Utf8PathBuf,
    log: fs::File,
    started: DateTime<Utc>,
}

/// Decides what to show and whether to start a run. Errors become lines to show.
/// Takes the locked store so that concurrent `wrap-cargo` runs can't each start a gc.
pub(crate) fn decide(store: &LockedStore) -> AfterUnlock {
    let skipped = |reason: String| AfterUnlock {
        lines: vec![format!("[targo] skipped automatic gc: {reason}")],
        run: None,
    };
    match AutoGcSetting::parse(env::var_os(AUTO_GC_ENV).as_deref()) {
        Ok(AutoGcSetting::On) => {}
        Ok(AutoGcSetting::Off) => {
            tracing::debug!("automatic gc is off: `{AUTO_GC_ENV}` is `0`");
            return AfterUnlock {
                lines: Vec::new(),
                run: None,
            };
        }
        Err(error) => return skipped(error.to_string()),
    }
    try_decide(store.path(), Utc::now()).unwrap_or_else(|error| skipped(turn_off_hint(&error)))
}

fn turn_off_hint(error: &color_eyre::Report) -> String {
    format!("{error:#} (set `{AUTO_GC_ENV}=0` to turn it off)")
}

fn try_decide(store_dir: &Utf8Path, now: DateTime<Utc>) -> Result<AfterUnlock> {
    let state_dir = StateDir::locate(store_dir)?;
    let prepared = prepare(&state_dir, now)?;
    let mut lines = Vec::new();
    if let Some(unusable) = prepared.replaced {
        lines.push(format!(
            "[targo] automatic gc replaced its state file `{}`: {unusable}",
            state_dir.state_path().display()
        ));
    }
    if let Some(notice) = prepared.notice {
        lines.push(notice.line(&state_dir.log_path()));
    }
    let run = prepared.log.map(|log| RunToStart {
        state_dir,
        store_dir: store_dir.to_owned(),
        log,
        started: now,
    });
    Ok(AfterUnlock { lines, run })
}

impl AfterUnlock {
    /// Not under the store lock: stderr can block, and starting a run waits for a process.
    pub(crate) fn finish(self, workspace_dir: &Utf8Path) {
        for line in &self.lines {
            tell(line);
        }
        if let Some(run) = self.run {
            if let Err(error) = run.start(workspace_dir) {
                tell(format_args!(
                    "[targo] skipped automatic gc: {}",
                    turn_off_hint(&error)
                ));
            }
        }
    }
}

/// Not `eprintln!`, which panics if stderr can't be written to: Cargo has to run all the same.
fn tell(line: impl fmt::Display) {
    let _ = writeln!(io::stderr(), "{line}");
}

impl RunToStart {
    fn start(self, workspace_dir: &Utf8Path) -> Result<()> {
        let Err(error) = self.spawn_starter(workspace_dir) else {
            return Ok(());
        };
        // Recorded as shown, since the caller reports it now.
        let failed = LastRun {
            started: self.started,
            end: Some(RunEnd {
                result: RunResult::Failed {
                    message: format!("{error:#}"),
                },
                notice: NoticeState::Shown,
            }),
        };
        match self.state_dir.write_state(&failed) {
            Ok(()) => Err(error),
            Err(write_error) => Err(eyre!("{error:#}, and then {write_error:#}")),
        }
    }

    /// Waits only for the starter, which exits as soon as the gc is on its own.
    fn spawn_starter(&self, workspace_dir: &Utf8Path) -> Result<()> {
        let mut log = &self.log;
        writeln!(
            log,
            "--- {}: gc started in the background by a Cargo command in `{workspace_dir}` ---",
            timestamp(self.started)
        )
        .wrap_err("failed to write to the gc log")?;

        let targo = env::current_exe().wrap_err("failed to find the targo executable")?;
        let clone_error = "failed to duplicate the handle of the gc log";
        let status = Command::new(&targo)
            .args(starter_args(self.started))
            // The store that this command used, however the gc would work one out for itself.
            .env(STORE_DIR_ENV, &self.store_dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone().wrap_err(clone_error)?)
            .stderr(log.try_clone().wrap_err(clone_error)?)
            // As for the gc, so that Ctrl-C can't stop the starter before the gc is on its own.
            .process_group(0)
            .status()
            .wrap_err_with(|| format!("failed to run `{}`", targo.display()))?;
        if !status.success() {
            bail!(
                "`{} {}` failed with {status}; see `{}`",
                targo.display(),
                starter_args(self.started).join(" "),
                self.state_dir.log_path().display()
            );
        }
        Ok(())
    }
}

pub(crate) fn starter_args(started: DateTime<Utc>) -> [String; 2] {
    ["spawn-auto-gc".to_owned(), started.to_rfc3339()]
}

pub(crate) fn background_gc_args(started: DateTime<Utc>) -> [String; 3] {
    ["gc".to_owned(), "--auto".to_owned(), started.to_rfc3339()]
}

/// Spawns the gc and doesn't wait: the starter then exits, so the gc is never a child of Cargo.
pub(crate) fn spawn_detached(started: DateTime<Utc>) -> Result<()> {
    let targo = env::current_exe().wrap_err("failed to find the targo executable")?;
    close_inherited_fds_on_exec()?;
    // Stdout and stderr are inherited, and are the locked gc log.
    Command::new(&targo)
        .args(background_gc_args(started))
        // Not the workspace, which could not be unmounted for as long as the gc runs.
        .current_dir("/")
        .stdin(Stdio::null())
        // Own process group, so Ctrl-C on the Cargo command doesn't reach the gc.
        .process_group(0)
        .spawn()
        .wrap_err_with(|| format!("failed to run `{}`", targo.display()))?;
    Ok(())
}

/// Keeps from the gc whatever else the Cargo command was given, such as a pipe that its caller
/// reads to the end.
fn close_inherited_fds_on_exec() -> Result<()> {
    const FD_DIR: &str = "/dev/fd";
    let read_error = || format!("failed to read `{FD_DIR}`");
    for dir_entry in fs::read_dir(FD_DIR).wrap_err_with(read_error)? {
        let name = dir_entry.wrap_err_with(read_error)?.file_name();
        let fd: RawFd = name
            .to_str()
            .and_then(|name| name.parse().ok())
            .ok_or_else(|| eyre!("`{FD_DIR}` has {name:?} in it, which is not a descriptor"))?;
        if fd <= libc::STDERR_FILENO {
            continue;
        }
        // SAFETY: `fcntl` with `F_SETFD` takes integers only, so it touches no memory of this
        // process, and it closes nothing; a descriptor that is not open is answered with `EBADF`.
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        if result == -1 {
            return Err(io::Error::last_os_error())
                .wrap_err_with(|| format!("failed to set close-on-exec on descriptor {fd}"));
        }
    }
    Ok(())
}

/// Records how a background run ended, for the next wrap-cargo to show.
pub(crate) fn record_end(
    store_dir: &Utf8Path,
    started: DateTime<Utc>,
    result: Result<GcStatus>,
) -> Result<GcStatus> {
    let run_result = match &result {
        Ok(GcStatus::Finished(summary)) => RunResult::Collected(*summary),
        Ok(GcStatus::AnotherGcRunning) => RunResult::AnotherGcRunning,
        Ok(GcStatus::NoLiveEntry) => RunResult::NoLiveEntry,
        Err(error) => RunResult::Failed {
            message: format!("{error:#}"),
        },
    };
    let last_run = LastRun {
        started,
        end: Some(RunEnd {
            result: run_result,
            notice: NoticeState::Pending,
        }),
    };
    let recorded = StateDir::locate(store_dir)
        .and_then(|state_dir| state_dir.write_state(&last_run))
        .wrap_err("failed to record how the background gc ended");
    match (result, recorded) {
        (result, Ok(())) => result,
        (Ok(_), Err(record_error)) => Err(record_error),
        // Only one error can be returned, so the other is printed here.
        (Err(error), Err(record_error)) => {
            eprintln!("{record_error:#}");
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        state::tests::{read_run, utc, TestStateDir},
        *,
    };
    use crate::gc::{GcSummary, ReportedSize};

    const MIB: u64 = 1024 * 1024;

    fn collected(removed: usize, emptied: usize, failed: usize, freed: ReportedSize) -> RunResult {
        RunResult::Collected(GcSummary {
            removed,
            emptied,
            failed,
            freed,
        })
    }

    fn exact(bytes: u64) -> ReportedSize {
        ReportedSize {
            bytes,
            bound: SizeBound::Exact,
        }
    }

    fn at_least(bytes: u64) -> ReportedSize {
        ReportedSize {
            bytes,
            bound: SizeBound::AtLeast,
        }
    }

    fn ended(started: DateTime<Utc>, result: RunResult, notice: NoticeState) -> RunStatus {
        RunStatus::Ended {
            started,
            end: RunEnd { result, notice },
        }
    }

    #[test]
    fn test_auto_gc_setting() {
        let bad = |value: &str| Err(BadAutoGcSetting(value.into()));
        let data = [
            (None, Ok(AutoGcSetting::On)),
            (Some("1"), Ok(AutoGcSetting::On)),
            (Some("0"), Ok(AutoGcSetting::Off)),
            (Some(""), bad("")),
            (Some("false"), bad("false")),
            (Some("true"), bad("true")),
            (Some("00"), bad("00")),
            (Some(" 0"), bad(" 0")),
        ];
        for (value, expected) in data {
            assert_eq!(
                AutoGcSetting::parse(value.map(OsStr::new)),
                expected,
                "for {value:?}"
            );
        }
    }

    #[test]
    fn test_decide_start() {
        let now = utc("2026-03-08T19:00:00Z");
        let day = TimeDelta::hours(24);
        let second = TimeDelta::seconds(1);
        let finished = |started| ended(started, RunResult::NoLiveEntry, NoticeState::Pending);

        let data = [
            (RunStatus::Never, StartDecision::Start),
            (finished(now), StartDecision::NotDue),
            (finished(now - day + second), StartDecision::NotDue),
            (finished(now - day), StartDecision::Start),
            (finished(now - day - second), StartDecision::Start),
            (finished(now - day * 400), StartDecision::Start),
            // A start in the future, from a clock that was wrong.
            (finished(now + second), StartDecision::NotDue),
            (finished(now + day - second), StartDecision::NotDue),
            (finished(now + day), StartDecision::Start),
            (finished(now + day * 400), StartDecision::Start),
            (
                RunStatus::Running { started: now },
                StartDecision::StillRunning,
            ),
            (
                RunStatus::Running {
                    started: now - day * 2,
                },
                StartDecision::StillRunning,
            ),
        ];
        for (status, expected) in data {
            assert_eq!(decide_start(&status, now), expected, "for {status:?}");
        }
    }

    #[test]
    fn test_notice_for() {
        let started = utc("2026-03-08T19:00:00Z");
        let pending = |result: RunResult| ended(started, result, NoticeState::Pending);
        let failed = RunResult::Failed {
            message: "no store".to_owned(),
        };

        let shown = [
            (pending(RunResult::Died), RunResult::Died),
            (pending(failed.clone()), failed.clone()),
            (
                pending(collected(1, 0, 0, exact(MIB))),
                collected(1, 0, 0, exact(MIB)),
            ),
            (
                pending(collected(0, 2, 0, exact(400 * MIB))),
                collected(0, 2, 0, exact(400 * MIB)),
            ),
            (
                pending(collected(0, 0, 1, exact(0))),
                collected(0, 0, 1, exact(0)),
            ),
            (
                pending(collected(1, 0, 0, at_least(4096))),
                collected(1, 0, 0, at_least(4096)),
            ),
        ];
        for (status, result) in shown {
            assert_eq!(
                notice_for(&status),
                Some(Notice { started, result }),
                "for {status:?}"
            );
        }

        let quiet = [
            RunStatus::Never,
            RunStatus::Running { started },
            pending(collected(0, 0, 0, exact(0))),
            // Below the size that is worth a notice.
            pending(collected(1, 1, 0, exact(MIB - 1))),
            pending(RunResult::NoLiveEntry),
            pending(RunResult::AnotherGcRunning),
            ended(
                started,
                collected(3, 0, 0, exact(400 * MIB)),
                NoticeState::Shown,
            ),
            ended(started, failed, NoticeState::Shown),
            ended(started, RunResult::Died, NoticeState::Shown),
        ];
        for status in quiet {
            assert_eq!(notice_for(&status), None, "for {status:?}");
        }
    }

    #[test]
    fn test_notice_line() {
        let started = utc("2026-03-08T19:00:00.123456789Z");
        let data = [
            (
                collected(3, 0, 0, exact(1536 * MIB)),
                "removed 3 entries (1.5 GiB)",
            ),
            (
                collected(0, 1, 0, exact(2 * MIB)),
                "emptied 1 entry (2.0 MiB)",
            ),
            (
                collected(1, 2, 0, at_least(2 * MIB)),
                "removed 1 entry and emptied 2 entries (at least 2.0 MiB)",
            ),
            (
                collected(1, 0, 1, exact(2 * MIB)),
                "removed 1 entry (2.0 MiB), with 1 failure",
            ),
            (
                collected(0, 0, 2, exact(0)),
                "removed nothing, with 2 failures",
            ),
            (
                RunResult::Failed {
                    message: "no store".to_owned(),
                },
                "failed: no store",
            ),
            (RunResult::Died, "ended without recording a result"),
        ];
        for (result, expected) in data {
            let notice = Notice { started, result };
            assert_eq!(
                notice.line(Path::new("/state/gc.log")),
                format!(
                    "[targo] gc: the background run of 2026-03-08T19:00:00Z {expected}; \
                     see `/state/gc.log`"
                )
            );
        }
    }

    fn prepare_at(test_dir: &TestStateDir, now: DateTime<Utc>) -> Prepared {
        prepare(&test_dir.state_dir, now).expect("prepared")
    }

    #[test]
    fn test_prepare_starts_a_run_at_most_once_a_day() {
        let test_dir = TestStateDir::new();
        let state_dir = &test_dir.state_dir;
        let first = utc("2026-03-08T19:00:00Z");
        let unfinished = |started| Some(LastRun { started, end: None });

        let prepared = prepare_at(&test_dir, first);
        assert!(prepared.log.is_some() && prepared.notice.is_none());
        assert_eq!(read_run(state_dir), unfinished(first));

        // While the first run has the log, nothing is started or shown, however late it is.
        for later in [TimeDelta::minutes(1), TimeDelta::hours(48)] {
            let during = prepare_at(&test_dir, first + later);
            assert!(during.log.is_none() && during.notice.is_none());
            assert_eq!(read_run(state_dir), unfinished(first));
        }

        // As the run does when it ends.
        let done = LastRun {
            started: first,
            end: Some(RunEnd {
                result: collected(1, 0, 0, exact(2 * MIB)),
                notice: NoticeState::Pending,
            }),
        };
        state_dir.write_state(&done).expect("wrote state");
        drop(prepared);

        let next = prepare_at(&test_dir, first + TimeDelta::hours(1));
        assert!(next.log.is_none(), "another run is not due");
        let notice = next.notice.expect("the result is shown");
        assert_eq!(notice.result, collected(1, 0, 0, exact(2 * MIB)));
        assert_eq!(read_run(state_dir), Some(notice.shown()));

        let again = prepare_at(&test_dir, first + TimeDelta::hours(2));
        assert!(again.log.is_none() && again.notice.is_none());
        assert_eq!(read_run(state_dir), Some(notice.shown()));

        let second = first + TimeDelta::hours(24);
        let due = prepare_at(&test_dir, second);
        assert!(due.log.is_some() && due.notice.is_none());
        assert_eq!(read_run(state_dir), unfinished(second));
    }

    #[test]
    fn test_prepare_shows_a_result_and_starts_the_next_run_together() {
        let test_dir = TestStateDir::new();
        let state_dir = &test_dir.state_dir;
        let first = utc("2026-03-08T19:00:00Z");
        let second = first + TimeDelta::hours(30);
        let done = LastRun {
            started: first,
            end: Some(RunEnd {
                result: collected(1, 0, 0, exact(2 * MIB)),
                notice: NoticeState::Pending,
            }),
        };
        state_dir.write_state(&done).expect("wrote state");

        let prepared = prepare_at(&test_dir, second);
        assert!(prepared.log.is_some());
        assert_eq!(
            prepared.notice.map(|notice| notice.started),
            Some(first),
            "the notice is of the run that ended"
        );
        assert_eq!(
            read_run(state_dir),
            Some(LastRun {
                started: second,
                end: None
            })
        );
    }

    #[test]
    fn test_prepare_sets_a_large_log_aside() {
        let test_dir = TestStateDir::new();
        let state_dir = &test_dir.state_dir;
        let log_len = || {
            fs::metadata(state_dir.log_path())
                .expect("read metadata")
                .len()
        };
        match state_dir.lock_log().expect("opened the log") {
            TryLock::Acquired(mut log) => log.write_all(&vec![b'x'; 2 * 1024 * 1024]),
            TryLock::Busy => panic!("nothing has the log locked"),
        }
        .expect("wrote to the log");
        assert_eq!(log_len(), 2 * MIB);

        let prepared = prepare_at(&test_dir, utc("2026-03-08T19:00:00Z"));
        assert!(prepared.log.is_some());
        assert_eq!(log_len(), 0, "the run gets a new log");
    }

    #[test]
    fn test_prepare_reports_a_run_that_died_once() {
        let test_dir = TestStateDir::new();
        let state_dir = &test_dir.state_dir;
        let started = utc("2026-03-08T19:00:00Z");
        // A start is recorded, and nothing has the log locked.
        state_dir
            .write_state(&LastRun { started, end: None })
            .expect("wrote state");

        let prepared = prepare_at(&test_dir, started + TimeDelta::minutes(1));
        assert!(prepared.log.is_none(), "another run is not due");
        let notice = prepared.notice.expect("the death is shown");
        assert_eq!(
            (notice.started, &notice.result),
            (started, &RunResult::Died)
        );
        assert_eq!(read_run(state_dir), Some(notice.shown()));

        let again = prepare_at(&test_dir, started + TimeDelta::minutes(2));
        assert!(again.log.is_none() && again.notice.is_none());
    }

    #[test]
    fn test_prepare_replaces_a_state_file_that_cannot_be_used() {
        let test_dir = TestStateDir::new();
        let state_dir = &test_dir.state_dir;
        let now = utc("2026-03-08T19:00:00Z");
        // Creates the directory.
        drop(state_dir.lock_log().expect("opened the log"));

        for contents in ["{", r#"{"version":2}"#] {
            fs::write(state_dir.state_path(), contents).expect("wrote state file");
            let prepared = prepare_at(&test_dir, now);
            assert!(prepared.log.is_some(), "for {contents:?}, a run is started");
            assert!(
                prepared.replaced.is_some() && prepared.notice.is_none(),
                "for {contents:?}"
            );
            assert_eq!(
                read_run(state_dir),
                Some(LastRun {
                    started: now,
                    end: None
                }),
                "for {contents:?}"
            );
        }

        // With the log locked no run starts, so the file is not replaced or reported.
        let held = state_dir.lock_log().expect("opened the log");
        fs::write(state_dir.state_path(), "{").expect("wrote state file");
        let prepared = prepare_at(&test_dir, now);
        assert!(prepared.log.is_none() && prepared.replaced.is_none());
        assert_eq!(
            fs::read_to_string(state_dir.state_path()).expect("read state file"),
            "{"
        );
        drop(held);
    }
}
