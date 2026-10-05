use crate::{
    cargo_cli::CargoCli,
    store::{remove_target_dir, LockedStore, TargetDirSetup},
};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand, ValueHint};
use color_eyre::{
    eyre::{bail, WrapErr},
    Result,
};
use std::{
    error,
    ffi::{OsStr, OsString},
    fmt,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
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
}

impl TargoApp {
    pub fn exec(self) -> Result<()> {
        let filter = EnvFilter::from_env("TARGO_LOG");
        tracing_subscriber::fmt().with_env_filter(filter).init();
        match self.command {
            TargoCommand::WrapCargo { args } => exec_wrap_cargo(args),
        }
    }
}

fn exec_wrap_cargo(args: Vec<OsString>) -> Result<()> {
    // Checked first so that a bad override fails even when targo is disabled.
    let store_dir_override = parse_store_dir_override(std::env::var_os(STORE_DIR_ENV))?;

    let parsed_args = match WrapCargoArgs::new(args)? {
        WrapCargoArgs::Enabled {
            parsed_args,
            workspace_dir,
            target_dir,
        } => {
            // Find the target directory destination.
            let store_dir = match store_dir_override {
                Some(store_dir) => store_dir,
                None => default_store_dir()?,
            };
            let store = set_up_target_dir(store_dir, &workspace_dir, &target_dir)?;

            // Cargo must not run under the store lock: a build can take a long time.
            store.unlock()?;

            parsed_args
        }
        WrapCargoArgs::Disabled { parsed_args } => parsed_args,
    };

    parsed_args.cargo_command().run_or_exec()?;

    Ok(())
}

/// Opens the store and points `target_dir` into it. The store is returned still locked.
fn set_up_target_dir(
    store_dir: Utf8PathBuf,
    workspace_dir: &Utf8Path,
    target_dir: &Utf8Path,
) -> Result<LockedStore> {
    let store = LockedStore::open(store_dir.clone())?;
    match store.set_up_target_dir(workspace_dir, target_dir)? {
        TargetDirSetup::Done(store) => return Ok(store),
        // The store is unlocked here, so a slow removal doesn't block other targo runs.
        TargetDirSetup::DirectoryInTheWay => remove_target_dir(target_dir)?,
    }

    let store = LockedStore::open(store_dir)?;
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

#[derive(Clone, Debug)]
enum WrapCargoArgs {
    Enabled {
        parsed_args: ParsedCargoArgs,
        workspace_dir: Utf8PathBuf,
        target_dir: Utf8PathBuf,
    },
    Disabled {
        parsed_args: ParsedCargoArgs,
    },
}

impl WrapCargoArgs {
    fn new(args: Vec<OsString>) -> Result<Self> {
        // TODO: intercept cargo clean -- it doesn't work right now, it should clean the symlink
        // target.

        let parsed_args =
            ParsedCargoArgs::new(args).with_context(|| "error parsing Cargo arguments")?;

        // Determine the workspace dir.
        let locate_project = parsed_args.locate_project_command();

        let output = match locate_project.stdout_output() {
            Ok(output) => output,
            Err(_) => {
                eprintln!("[targo] error running cargo locate-project, disabling");
                return Ok(Self::Disabled { parsed_args });
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
        cli.args(["locate-project", "--workspace", "--message-format=plain"]);
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
const STORE_DIR_ENV: &str = "TARGO_STORE_DIR";

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
    use std::os::unix::ffi::OsStringExt;

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
            ("build", "locate-project --workspace --message-format=plain"),
            (
                "build --manifest-path=-x/Cargo.toml",
                "locate-project --workspace --message-format=plain --manifest-path=-x/Cargo.toml",
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
            let TargoCommand::WrapCargo { args } = app.command;
            assert_eq!(args, input, "clap passes arguments through unchanged");
        }
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
