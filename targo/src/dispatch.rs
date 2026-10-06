use crate::{
    auto_gc,
    cargo_cli::{CargoCli, CargoOutput},
    gc::{self, GcMode, GcPolicy, GcStatus},
    helpers::resolve_location,
    store::{remove_target_dir, LockedStore, TargetDirSetup},
};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueHint};
use color_eyre::{
    eyre::{bail, WrapErr},
    Result,
};
use std::{
    error,
    ffi::{OsStr, OsString},
    fmt, io, iter,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{ExitCode, ExitStatus},
    time::Duration,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version)]
pub struct TargoApp {
    // TODO: command
    #[command(subcommand)]
    command: TargoCommand,
}

#[derive(Debug, Subcommand)]
pub enum TargoCommand {
    /// Wrap Cargo and pass through commands.
    #[command(disable_help_flag = true)]
    WrapCargo {
        /// The arguments to pass through to Cargo.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_hint = ValueHint::CommandWithArguments,
        )]
        args: Vec<OsString>,
    },
    /// Remove the store entries that no workspace links to any more.
    Gc {
        /// Report what would be removed or emptied, and change nothing.
        #[arg(long)]
        dry_run: bool,

        /// How long an entry is kept after it was last used or built in, once no workspace
        /// links to it.
        #[arg(
            long,
            value_name = "DURATION",
            default_value = "7d",
            value_parser = humantime::parse_duration,
        )]
        orphan_grace: Duration,

        /// Also empty the target directory of an entry that a workspace still links to, once
        /// it has not been used or built in for this long. If unset, such entries are left alone.
        #[arg(
            long,
            value_name = "DURATION",
            env = MAX_AGE_ENV,
            value_parser = parse_max_age,
        )]
        max_age: Option<Duration>,

        #[arg(long, hide = true, value_name = "STARTED", conflicts_with = "dry_run")]
        auto: Option<DateTime<Utc>>,
    },
    #[command(hide = true)]
    SpawnAutoGc { started: DateTime<Utc> },
}

/// The environment variable that sets `--max-age` for `targo gc`.
const MAX_AGE_ENV: &str = "TARGO_GC_MAX_AGE";

/// Parses a maximum age. A bare `0` is refused: it reads as "off", and would empty everything.
fn parse_max_age(value: &str) -> Result<Duration, String> {
    if value == "0" {
        return Err(format!(
            "`0` needs a time unit: `0s` empties every entry that is not being built in; \
             to empty none, leave `--max-age` and `{MAX_AGE_ENV}` unset"
        ));
    }
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

impl TargoApp {
    pub fn exec(self) -> Result<ExitCode> {
        let filter = EnvFilter::from_env("TARGO_LOG");
        tracing_subscriber::fmt().with_env_filter(filter).init();
        match self.command {
            TargoCommand::WrapCargo { args } => exec_wrap_cargo(args),
            TargoCommand::Gc {
                dry_run,
                orphan_grace,
                max_age,
                auto,
            } => {
                let policy = GcPolicy {
                    orphan_grace,
                    max_age,
                };
                match (auto, dry_run) {
                    // clap refuses `--auto` with `--dry-run`.
                    (Some(started), _) => exec_background_gc(started, policy),
                    (None, true) => exec_gc(GcMode::DryRun, policy),
                    (None, false) => exec_gc(GcMode::Remove, policy),
                }
            }
            TargoCommand::SpawnAutoGc { started } => {
                auto_gc::spawn_detached(started)?;
                Ok(ExitCode::SUCCESS)
            }
        }
    }
}

fn exec_wrap_cargo(args: Vec<OsString>) -> Result<ExitCode> {
    // Checked first so that a bad override fails even when targo is disabled.
    let store_dir_override = store_dir_override_from_env()?;

    let parsed_args = match WrapCargoArgs::new(args)? {
        WrapCargoArgs::Enabled {
            parsed_args,
            workspace_dir,
            target_dir,
        } => {
            // Find the target directory destination.
            let store_dir = choose_store_dir(store_dir_override)?;
            let store = set_up_target_dir(&store_dir, &workspace_dir, &target_dir)?;
            let auto_gc = auto_gc::decide(&store);

            // Cargo must not run under the store lock: a build can take a long time.
            store.unlock()?;
            auto_gc.finish(&workspace_dir);

            parsed_args
        }
        WrapCargoArgs::Disabled {
            parsed_args,
            reason,
        } => {
            reason.report();
            parsed_args
        }
    };

    parsed_args.cargo_command().run_or_exec()?;

    Ok(ExitCode::SUCCESS)
}

/// The exit code of a gc that did nothing because another one was running.
///
/// This is `EX_TEMPFAIL`: not success, since nothing was collected, and not the failure of 1.
const GC_BUSY_EXIT_CODE: u8 = 75;

fn exec_gc(mode: GcMode, policy: GcPolicy) -> Result<ExitCode> {
    let store_dir = choose_store_dir(store_dir_override_from_env()?)?;
    run_gc(store_dir, mode, &policy).map(gc_exit_code)
}

/// The run that wrap-cargo started at `started`; its output is the gc log.
fn exec_background_gc(started: DateTime<Utc>, policy: GcPolicy) -> Result<ExitCode> {
    let store_dir = choose_store_dir(store_dir_override_from_env()?)?;
    let result = run_gc(store_dir.clone(), GcMode::Background, &policy);
    auto_gc::record_end(&store_dir, started, result).map(gc_exit_code)
}

fn run_gc(store_dir: Utf8PathBuf, mode: GcMode, policy: &GcPolicy) -> Result<GcStatus> {
    gc::run(
        store_dir,
        mode,
        policy,
        &Utc::now,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

fn gc_exit_code(status: GcStatus) -> ExitCode {
    match status {
        GcStatus::Finished(summary) => match summary.failed {
            0 => ExitCode::SUCCESS,
            _ => ExitCode::FAILURE,
        },
        GcStatus::AnotherGcRunning => ExitCode::from(GC_BUSY_EXIT_CODE),
        GcStatus::NoLiveEntry => ExitCode::SUCCESS,
    }
}

/// Opens the store and points `target_dir` into it. The store is returned still locked.
fn set_up_target_dir(
    store_dir: &Utf8Path,
    workspace_dir: &Utf8Path,
    target_dir: &Utf8Path,
) -> Result<LockedStore> {
    let store = open_store_outside_target_dir(store_dir, target_dir)?;
    match store.set_up_target_dir(workspace_dir, target_dir)? {
        TargetDirSetup::Done(store) => return Ok(store),
        // The store is unlocked here, so a slow removal doesn't block other targo runs.
        TargetDirSetup::DirectoryInTheWay => remove_target_dir(target_dir)?,
    }

    let store = open_store_outside_target_dir(store_dir, target_dir)?;
    match store.set_up_target_dir(workspace_dir, target_dir)? {
        TargetDirSetup::Done(store) => Ok(store),
        TargetDirSetup::DirectoryInTheWay => {
            bail!(
                "target dir `{target_dir}` was recreated while targo was moving it into the \
                 store: make sure nothing else is building in this workspace, then try again"
            );
        }
    }
}

/// Opens the store after checking that it is outside the target dir.
fn open_store_outside_target_dir(
    store_dir: &Utf8Path,
    target_dir: &Utf8Path,
) -> Result<LockedStore> {
    // Before the store is opened, because opening creates it.
    ensure_store_outside_target_dir(store_dir, target_dir)?;
    LockedStore::open(store_dir.to_owned())
}

/// Fails if the store would be at or inside the target dir, where it would be deleted.
fn ensure_store_outside_target_dir(store_dir: &Utf8Path, target_dir: &Utf8Path) -> Result<()> {
    let store_location = resolve_location(store_dir)
        .wrap_err_with(|| format!("failed to resolve targo store directory `{store_dir}`"))?;

    // The path itself, which setup replaces if a real directory is there.
    let (Some(target_parent), Some(target_name)) = (target_dir.parent(), target_dir.file_name())
    else {
        bail!("target dir `{target_dir}` has no file name");
    };
    let replaced_location = resolve_location(target_parent)
        .wrap_err_with(|| format!("failed to resolve target dir `{target_dir}`"))?
        .join(target_name);
    // Where the path leads if it is a symlink, which is where build output goes.
    // Best effort: nothing is deleted through the link, so a broken one is not an error.
    let linked_location = target_dir
        .canonicalize()
        .inspect_err(|err| {
            tracing::debug!(
                "target dir `{target_dir}` can't be resolved, so only the path itself is \
                 checked: {err}"
            );
        })
        .ok();

    for target_location in iter::once(replaced_location).chain(linked_location) {
        if store_location.starts_with(&target_location) {
            bail!(
                "targo store directory {} must be outside target dir {}, where it would be \
                 deleted along with build output: set `{STORE_DIR_ENV}` to a directory \
                 somewhere else",
                display_with_location(store_dir, &store_location),
                display_with_location(target_dir, &target_location),
            );
        }
    }
    Ok(())
}

/// Formats `path`, along with where it really is if that differs.
fn display_with_location(path: &Utf8Path, location: &Path) -> String {
    if location == path.as_std_path() {
        format!("`{path}`")
    } else {
        format!("`{path}` (which resolves to `{}`)", location.display())
    }
}

#[derive(Clone, Debug)]
enum WrapCargoArgs {
    Enabled {
        parsed_args: ParsedCargoArgs,
        workspace_dir: Utf8PathBuf,
        target_dir: Utf8PathBuf,
    },
    Disabled {
        parsed_args: ParsedCargoArgs,
        reason: DisabledReason,
    },
}

/// Why targo leaves a Cargo command unmanaged.
#[derive(Clone, Debug)]
enum DisabledReason {
    /// Cargo found no manifest, as for cargo version outside any workspace.
    NoManifest,
    /// Locating failed for another reason, such as a broken manifest.
    LocateProjectFailed {
        locate_project: CargoCli,
        status: ExitStatus,
        stderr: String,
    },
}

impl DisabledReason {
    fn report(&self) {
        match self {
            // Quiet, since Cargo complains itself if the command needs a manifest.
            Self::NoManifest => {
                tracing::debug!("disabled for this command: Cargo found no manifest");
            }
            Self::LocateProjectFailed {
                locate_project,
                status,
                stderr,
            } => {
                let mut message = format!(
                    "[targo] disabled for this command: `{locate_project}` failed with {status}"
                );
                // Indented to tell it apart from what the command prints next.
                for line in stderr.lines() {
                    message.push_str("\n    ");
                    message.push_str(line);
                }
                eprintln!("{message}");
            }
        }
    }
}

/// The message is the only signal, since Cargo's exit code is the same as for other errors.
fn is_manifest_not_found(stderr: &str) -> bool {
    // Warnings can come first, and the directory that follows can be anything.
    stderr
        .lines()
        .any(|line| line.starts_with("error: could not find `Cargo.toml` in `"))
}

impl WrapCargoArgs {
    fn new(args: Vec<OsString>) -> Result<Self> {
        // TODO: intercept cargo clean -- it doesn't work right now, it should clean the symlink
        // target.

        let parsed_args =
            ParsedCargoArgs::new(args).with_context(|| "error parsing Cargo arguments")?;

        // Determine the workspace dir.
        let locate_project = parsed_args.locate_project_command();

        // An error, since a Cargo that can't be started is unlikely to run the command.
        let output = match locate_project.output()? {
            CargoOutput::Success { stdout } => stdout,
            CargoOutput::Failed { status, stderr } => {
                tracing::debug!("`{locate_project}` failed with {status}:\n{stderr}");
                let reason = if is_manifest_not_found(&stderr) {
                    DisabledReason::NoManifest
                } else {
                    DisabledReason::LocateProjectFailed {
                        locate_project,
                        status,
                        stderr,
                    }
                };
                return Ok(Self::Disabled {
                    parsed_args,
                    reason,
                });
            }
        };

        let mut locate_project_output = String::from_utf8(output)
            .wrap_err_with(|| format!("`{locate_project}` produced invalid UTF-8 output"))?;
        // Last character of workspace_dir_str must be a newline.
        if !locate_project_output.ends_with('\n') {
            bail!("`{locate_project}` produced output not terminated with a newline: {locate_project_output}");
        }
        locate_project_output.pop();
        let mut workspace_dir = Utf8PathBuf::from(locate_project_output);
        // The filename of workspace dir should be Cargo.toml.
        if workspace_dir.file_name() != Some("Cargo.toml") {
            bail!("cargo locate-project output `{workspace_dir}` doesn't end with Cargo.toml");
        }
        workspace_dir.pop();

        // TODO: read --target-dir/build.target-dir from cargo.
        let target_dir = workspace_dir.join("target");

        Ok(Self::Enabled {
            parsed_args,
            workspace_dir,
            target_dir,
        })
    }
}

#[derive(Clone, Debug)]
struct ParsedCargoArgs {
    /// The arguments exactly as given, passed through to Cargo unchanged.
    args: Vec<OsString>,
    manifest_path: Option<PathBuf>,
}

impl ParsedCargoArgs {
    fn new(args: Vec<OsString>) -> Result<Self, ManifestPathError> {
        let manifest_path = find_manifest_path(&args)?.map(PathBuf::from);
        tracing::debug!("manifest-path: {manifest_path:?}");
        Ok(Self {
            args,
            manifest_path,
        })
    }

    fn cargo_command(&self) -> CargoCli {
        let mut cli = CargoCli::new();
        cli.args(&self.args);
        cli
    }

    fn locate_project_command(&self) -> CargoCli {
        let mut cli = CargoCli::new();
        // Options and values are separate words so that the command is shown unquoted.
        cli.args(["locate-project", "--workspace", "--message-format", "plain"]);
        // CARGO_TERM_COLOR could otherwise color the error that targo matches.
        cli.args(["--color", "never"]);
        if let Some(manifest_path) = &self.manifest_path {
            // Cargo only accepts a path starting with `-` in the `=` form.
            let mut arg = OsString::from("--manifest-path=");
            arg.push(manifest_path);
            cli.arg(arg);
        }
        cli
    }
}

/// Finds the value of `--manifest-path` among arguments meant for Cargo.
///
/// Targo doesn't know which of Cargo's options take values, so no other argument is parsed.
fn find_manifest_path(args: &[OsString]) -> Result<Option<&OsStr>, ManifestPathError> {
    let mut manifest_path = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            // The rest is for whatever Cargo runs, not for Cargo.
            break;
        }
        let value = if arg == "--manifest-path" {
            // Cargo itself rejects an option-like value, so take whatever is next.
            args.next()
                .ok_or(ManifestPathError::MissingValue)?
                .as_os_str()
        } else if let Some(value) = arg.as_bytes().strip_prefix(b"--manifest-path=") {
            OsStr::from_bytes(value)
        } else {
            continue;
        };
        if manifest_path.replace(value).is_some() {
            return Err(ManifestPathError::Duplicate);
        }
    }
    Ok(manifest_path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManifestPathError {
    MissingValue,
    Duplicate,
}

impl fmt::Display for ManifestPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue => f.write_str(
                "a value is required for `--manifest-path <PATH>`, but none was supplied",
            ),
            Self::Duplicate => f.write_str(
                "the argument `--manifest-path <PATH>` was provided more than once, \
                 but cannot be used multiple times",
            ),
        }
    }
}

impl error::Error for ManifestPathError {}

/// The environment variable that overrides the store directory.
pub(crate) const STORE_DIR_ENV: &str = "TARGO_STORE_DIR";

fn store_dir_override_from_env() -> Result<Option<Utf8PathBuf>, StoreDirEnvError> {
    parse_store_dir_override(std::env::var_os(STORE_DIR_ENV))
}

/// The store directory: the override if there is one, otherwise `$CARGO_HOME/targo`.
fn choose_store_dir(store_dir_override: Option<Utf8PathBuf>) -> Result<Utf8PathBuf> {
    match store_dir_override {
        Some(store_dir) => Ok(store_dir),
        None => default_store_dir(),
    }
}

/// Returns the store directory named by the value of `TARGO_STORE_DIR`, if the variable is set.
fn parse_store_dir_override(
    value: Option<OsString>,
) -> Result<Option<Utf8PathBuf>, StoreDirEnvError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(StoreDirEnvError::Empty);
    }
    let store_dir = value
        .into_string()
        .map(Utf8PathBuf::from)
        .map_err(StoreDirEnvError::NotUtf8)?;
    if !store_dir.is_absolute() {
        return Err(StoreDirEnvError::NotAbsolute(store_dir));
    }
    Ok(Some(store_dir))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum StoreDirEnvError {
    Empty,
    NotUtf8(OsString),
    NotAbsolute(Utf8PathBuf),
}

impl fmt::Display for StoreDirEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(
                f,
                "`{STORE_DIR_ENV}` is set to an empty value: \
                 set it to an absolute path, or unset it to use the default store"
            ),
            Self::NotUtf8(value) => write!(
                f,
                "`{STORE_DIR_ENV}` must be valid UTF-8, but is set to {value:?}"
            ),
            Self::NotAbsolute(store_dir) => write!(
                f,
                "`{STORE_DIR_ENV}` must be an absolute path, but is set to `{store_dir}`"
            ),
        }
    }
}

impl error::Error for StoreDirEnvError {}

fn default_store_dir() -> Result<Utf8PathBuf> {
    let dir = home::cargo_home().wrap_err("unable to determine cargo home dir")?;
    let mut utf8_dir: Utf8PathBuf = dir
        .clone()
        .try_into()
        .wrap_err_with(|| format!("cargo home `{}` is invalid UTF-8", dir.display()))?;
    utf8_dir.push("targo");
    Ok(utf8_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;
    use std::{
        fs,
        os::unix::{ffi::OsStringExt, fs::symlink},
    };

    #[test]
    fn test_parse_wrap_cargo_args() {
        let data: &[(&str, Result<Option<&str>, ManifestPathError>)] = &[
            ("", Ok(None)),
            ("-vv version", Ok(None)),
            ("build -pfoo -j8", Ok(None)),
            ("build --foo= - ''", Ok(None)),
            ("run --", Ok(None)),
            ("-- version", Ok(None)),
            (
                "clippy --package baz --manifest-path test",
                Ok(Some("test")),
            ),
            (
                "build --manifest-path=a/Cargo.toml -v",
                Ok(Some("a/Cargo.toml")),
            ),
            ("build --manifest-path=a=b", Ok(Some("a=b"))),
            ("build --manifest-path=", Ok(Some(""))),
            ("build --manifest-path -- a", Ok(Some("--"))),
            (
                "build --manifest-path --manifest-path=a",
                Ok(Some("--manifest-path=a")),
            ),
            ("--manifest-path a run -- --manifest-path b", Ok(Some("a"))),
            ("build --manifest-paths a --manifest a", Ok(None)),
            (
                "build --manifest-path",
                Err(ManifestPathError::MissingValue),
            ),
            (
                "build --manifest-path=a --manifest-path a",
                Err(ManifestPathError::Duplicate),
            ),
        ];
        for (input, expected) in data {
            assert_parsed(
                shell_args(input),
                expected.map(|path| path.map(PathBuf::from)),
            );
        }
    }

    #[test]
    fn test_parse_wrap_cargo_args_non_utf8() {
        assert_parsed_bytes(
            &[b"build", b"-\xff\xfe", b"--f\xffoo", b"--f\xffoo=b\xffar"],
            None,
        );
        assert_parsed_bytes(&[b"build", b"--manifest-path", b"a\xff"], Some(b"a\xff"));
        assert_parsed_bytes(&[b"build", b"--manifest-path=\xff=a"], Some(b"\xff=a"));
    }

    fn assert_parsed_bytes(input: &[&[u8]], expected: Option<&[u8]>) {
        let args = input.iter().map(|arg| os_string(arg)).collect();
        assert_parsed(
            args,
            Ok(expected.map(|path| PathBuf::from(os_string(path)))),
        );
    }

    #[test]
    fn test_locate_project_command() {
        let data = [
            (
                "build",
                "locate-project --workspace --message-format plain --color never",
            ),
            (
                "build --manifest-path=-x/Cargo.toml",
                "locate-project --workspace --message-format plain --color never \
                 --manifest-path=-x/Cargo.toml",
            ),
        ];
        for (input, expected) in data {
            let parsed = ParsedCargoArgs::new(shell_args(input)).expect("input is valid");
            assert_eq!(
                parsed.locate_project_command().get_args(),
                shell_args(expected),
                "for {input:?}"
            );
        }
    }

    #[test]
    fn test_is_manifest_not_found() {
        // These are Cargo 1.99's outputs, with shortened paths.
        let not_found = [
            "error: could not find `Cargo.toml` in `/dir` or any parent directory\n",
            "error: could not find `Cargo.toml` in `/dir` or any parent directory, \
             but found cargo.toml please try to rename it to Cargo.toml\n",
            "warning: `/dir/.cargo/config` is deprecated in favor of `config.toml`\n  \
             |\n  = help: if you need to support cargo 1.38 or earlier, you can symlink \
             `config` to `config.toml`\n\
             error: could not find `Cargo.toml` in `/dir` or any parent directory\n",
        ];
        for stderr in not_found {
            assert!(is_manifest_not_found(stderr), "for {stderr:?}");
        }

        let other = [
            "",
            "error: key with no value, expected `=`\n --> Cargo.toml:1:6\n",
            "error: manifest path `nope/Cargo.toml` does not exist\n",
            "error: toolchain 'nonexistent' is not installed\n",
            // Synthetic case, the message as the cause of another error.
            "error: failed to load manifest\n\nCaused by:\n  \
             could not find `Cargo.toml` in `/dir` or any parent directory\n",
            // Output under CARGO_TERM_COLOR=always without `--color never`.
            "\x1b[1m\x1b[91merror\x1b[0m: could not find `Cargo.toml` in `/dir` \
             or any parent directory\n",
        ];
        for stderr in other {
            assert!(!is_manifest_not_found(stderr), "for {stderr:?}");
        }
    }

    #[test]
    fn test_wrap_cargo_args_from_clap() {
        // A leading `--` is not covered: clap drops it.
        let data = [
            "",
            "-vv version",
            "--help",
            "--version",
            "run -- -- arg1 --",
        ];
        let inputs = data
            .into_iter()
            .map(shell_args)
            .chain([vec![os_string(b"-\xff"), os_string(b"--f\xffoo=\xff")]]);
        for input in inputs {
            let app_args = ["targo", "wrap-cargo"]
                .into_iter()
                .map(OsString::from)
                .chain(input.iter().cloned());
            let app = TargoApp::try_parse_from(app_args)
                .unwrap_or_else(|error| panic!("for {input:?}, clap failed: {error}"));
            match app.command {
                TargoCommand::WrapCargo { args } => {
                    assert_eq!(args, input, "clap passes arguments through unchanged");
                }
                TargoCommand::Gc { .. } | TargoCommand::SpawnAutoGc { .. } => {
                    panic!("for {input:?}, clap parsed another command")
                }
            }
        }
    }

    #[test]
    fn test_gc_args_from_clap() {
        let parse = |input: &str| {
            let app_args = ["targo", "gc"].into_iter().map(OsString::from);
            TargoApp::try_parse_from(app_args.chain(shell_args(input)))
        };

        const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);
        // Each case is the arguments, whether they ask for a dry run, and the grace period.
        let data = [
            ("", false, WEEK),
            ("--dry-run", true, WEEK),
            ("--orphan-grace 90m", false, Duration::from_secs(90 * 60)),
            ("--dry-run --orphan-grace=0s", true, Duration::ZERO),
        ];
        for (input, expected_dry_run, expected_grace) in data {
            let app = parse(input).unwrap_or_else(|error| panic!("for {input:?}: {error}"));
            match app.command {
                TargoCommand::Gc {
                    dry_run,
                    orphan_grace,
                    // Not given here, so it is whatever the environment of the test sets.
                    max_age: _,
                    auto,
                } => {
                    assert_eq!(auto, None, "for {input:?}");
                    assert_eq!(
                        (dry_run, orphan_grace),
                        (expected_dry_run, expected_grace),
                        "for {input:?}"
                    );
                }
                TargoCommand::WrapCargo { .. } | TargoCommand::SpawnAutoGc { .. } => {
                    panic!("for {input:?}, clap parsed another command")
                }
            }
        }

        let max_age_data = [
            ("--max-age 30d", 30 * 24 * 60 * 60),
            ("--dry-run --max-age=90m", 90 * 60),
            ("--max-age 0s --orphan-grace 1d", 0),
        ];
        for (input, expected_secs) in max_age_data {
            let app = parse(input).unwrap_or_else(|error| panic!("for {input:?}: {error}"));
            match app.command {
                TargoCommand::Gc { max_age, .. } => {
                    assert_eq!(
                        max_age,
                        Some(Duration::from_secs(expected_secs)),
                        "for {input:?}"
                    );
                }
                TargoCommand::WrapCargo { .. } | TargoCommand::SpawnAutoGc { .. } => {
                    panic!("for {input:?}, clap parsed another command")
                }
            }
        }

        let error_data = [
            ("--orphan-grace soon", ErrorKind::ValueValidation),
            ("--max-age soon", ErrorKind::ValueValidation),
            ("--max-age 30", ErrorKind::ValueValidation),
            // `humantime` takes a bare `0` as zero, which would empty every entry.
            ("--max-age 0", ErrorKind::ValueValidation),
            ("--max-age", ErrorKind::InvalidValue),
            // A number needs a unit.
            ("--dry-run --orphan-grace 1", ErrorKind::ValueValidation),
            ("--dry-run=yes", ErrorKind::TooManyValues),
        ];
        for (input, expected) in error_data {
            let error = parse(input).expect_err("clap rejects the arguments");
            assert_eq!(error.kind(), expected, "for {input:?}: {error}");
        }
    }

    #[test]
    fn test_background_gc_args_from_clap() {
        let started: DateTime<Utc> = "2026-03-08T19:00:00.123456789Z"
            .parse()
            .expect("parsed timestamp");
        let parse = |args: &[String]| {
            let app_args = iter::once("targo").chain(args.iter().map(String::as_str));
            TargoApp::try_parse_from(app_args)
        };

        // The arguments that wrap-cargo and the starter build are the ones clap takes.
        match parse(&auto_gc::starter_args(started))
            .expect("parsed")
            .command
        {
            TargoCommand::SpawnAutoGc { started: parsed } => assert_eq!(parsed, started),
            TargoCommand::WrapCargo { .. } | TargoCommand::Gc { .. } => {
                panic!("clap parsed another command")
            }
        }
        match parse(&auto_gc::background_gc_args(started))
            .expect("parsed")
            .command
        {
            TargoCommand::Gc { dry_run, auto, .. } => {
                assert_eq!((dry_run, auto), (false, Some(started)));
            }
            TargoCommand::WrapCargo { .. } | TargoCommand::SpawnAutoGc { .. } => {
                panic!("clap parsed another command")
            }
        }

        let mut with_dry_run = auto_gc::background_gc_args(started).to_vec();
        with_dry_run.push("--dry-run".to_owned());
        let error = parse(&with_dry_run).expect_err("clap rejects the arguments");
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);

        let not_a_time = ["gc".to_owned(), "--auto".to_owned(), "now".to_owned()];
        let error = parse(&not_a_time).expect_err("clap rejects the arguments");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn test_parse_store_dir_override() {
        let not_absolute = |dir: &str| Err(StoreDirEnvError::NotAbsolute(dir.into()));
        let data = [
            ("/store", Ok(Some("/store".into()))),
            ("/my store/", Ok(Some("/my store/".into()))),
            ("", Err(StoreDirEnvError::Empty)),
            ("store", not_absolute("store")),
            ("~/store", not_absolute("~/store")),
        ];
        for (input, expected) in data {
            assert_eq!(
                parse_store_dir_override(Some(input.into())),
                expected,
                "for {input:?}"
            );
        }

        assert_eq!(parse_store_dir_override(None), Ok(None));

        let non_utf8 = os_string(b"/store\xff");
        assert_eq!(
            parse_store_dir_override(Some(non_utf8.clone())),
            Err(StoreDirEnvError::NotUtf8(non_utf8))
        );
    }

    #[test]
    fn test_store_refused_inside_target_dir_of_symlinked_workspace() {
        let temp_dir = camino_tempfile::tempdir().expect("created temp dir");
        let root = temp_dir.path().canonicalize_utf8().expect("canonicalized");
        fs::create_dir(root.join("workspace")).expect("created workspace dir");
        symlink("workspace", root.join("alias")).expect("created symlink");

        let error = ensure_store_outside_target_dir(
            &root.join("workspace/target/store"),
            &root.join("alias/target"),
        )
        .expect_err("the store is refused");
        let expected = format!(
            "must be outside target dir `{root}/alias/target` \
             (which resolves to `{root}/workspace/target`)"
        );
        assert!(error.to_string().contains(&expected), "error was: {error}");
    }

    fn shell_args(input: &str) -> Vec<OsString> {
        shell_words::split(input)
            .expect("input is valid shell syntax")
            .into_iter()
            .map(OsString::from)
            .collect()
    }

    fn os_string(bytes: &[u8]) -> OsString {
        OsString::from_vec(bytes.to_vec())
    }

    /// Checks the manifest path found in `input`, and that Cargo is given exactly `input`.
    fn assert_parsed(input: Vec<OsString>, expected: Result<Option<PathBuf>, ManifestPathError>) {
        let actual = ParsedCargoArgs::new(input.clone()).map(|parsed| {
            let cargo_args = parsed.cargo_command().get_args().to_vec();
            (parsed.manifest_path, cargo_args)
        });
        let expected = expected.map(|manifest_path| (manifest_path, input.clone()));
        assert_eq!(actual, expected, "for {input:?}");
    }
}
