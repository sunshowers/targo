use crate::{cargo_cli::CargoCli, store::TargoStore};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand, ValueHint};
use color_eyre::{
    eyre::{bail, WrapErr},
    Result,
};
use lexopt::prelude::*;
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
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
    let parser = lexopt::Parser::from_args(args);
    let args = WrapCargoArgs::new(parser)?;

    // Find the target directory destination.
    let store_dir = find_targo_store_dir()?;
    let store = TargoStore::new(store_dir)?;

    let kind = store.determine_target_dir(&args.workspace_dir, &args.target_dir)?;
    store.actualize_kind(kind)?;

    args.parsed_args.cargo_command().run_or_exec()?;

    Ok(())
}

#[derive(Clone, Debug)]
struct WrapCargoArgs {
    parsed_args: ParsedCargoArgs,
    workspace_dir: Utf8PathBuf,
    target_dir: Utf8PathBuf,
}

impl WrapCargoArgs {
    fn new(parser: lexopt::Parser) -> Result<Self> {
        // TODO: intercept cargo clean -- it doesn't work right now, it should clean the symlink
        // target.

        let parsed_args = ParsedCargoArgs::from_parser(parser)
            .with_context(|| "error parsing Cargo arguments")?;

        // Determine the workspace dir.
        let mut locate_project = CargoCli::new();
        locate_project.args(["locate-project", "--workspace", "--message-format=plain"]);
        if let Some(manifest_path) = &parsed_args.manifest_path {
            locate_project.arg("--manifest-path");
            locate_project.arg(manifest_path);
        }

        let workspace_dir = locate_project.stdout_output()?;
        let mut locate_project_output = String::from_utf8(workspace_dir)
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

        Ok(Self {
            parsed_args,
            workspace_dir,
            target_dir,
        })
    }
}

#[derive(Clone, Debug)]
struct ParsedCargoArgs {
    cli_args: Vec<OsString>,
    post_double_hyphen: Vec<OsString>,
    manifest_path: Option<PathBuf>,
}

impl ParsedCargoArgs {
    fn from_parser(mut parser: lexopt::Parser) -> Result<Self> {
        let mut seen_double_hyphen = false;
        let mut cli_args = Vec::new();
        let mut post_double_hyphen = Vec::new();
        let mut manifest_path = None;
        while let Some(arg) = parser.next()? {
            match arg {
                Long("manifest-path") => {
                    // manifest-path can't be specified multiple times
                    let new_manifest_path = match &manifest_path {
                        None => parser.value()?,
                        Some(_) => {
                            return Err(lexopt::Error::Custom(
                                "error: The argument '--manifest-path <PATH>' was provided \
                                 more than once, but cannot be used multiple times"
                                    .into(),
                            )
                            .into());
                        }
                    };
                    manifest_path = Some(PathBuf::from(new_manifest_path.clone()));
                    tracing::debug!(
                        "setting manifest-path to {}",
                        Path::new(&new_manifest_path).display()
                    );

                    // Also pass through the manifest path to the underlying cargo command.
                    cli_args.extend(["--manifest-path".into(), new_manifest_path]);
                }
                Long(other) => {
                    let other = other.to_owned();
                    if let Some(val) = parser.optional_value() {
                        tracing::debug!("long arg: {other} with optional value: {val:?}");
                        let mut arg = OsString::from(format!("--{other}="));
                        arg.push(&val);
                        cli_args.push(arg);
                    } else {
                        tracing::debug!("long arg: {other} without optional value");
                        cli_args.push(format!("--{other}").into());
                    }
                }
                Short(arg) => {
                    if let Some(val) = parser.optional_value() {
                        tracing::debug!("short arg: {arg} with optional value: {val:?}");
                        let mut arg = OsString::from(format!("-{arg}="));
                        arg.push(&val);
                        cli_args.push(arg);
                    } else {
                        tracing::debug!("short arg: {arg} without optional value");
                        cli_args.push(format!("-{arg}").into());
                    }
                }
                Value(value) => {
                    if seen_double_hyphen {
                        tracing::debug!(
                            "argument {value:?}, post-double-hyphen so treating literally"
                        );
                        post_double_hyphen.push(value);
                    } else {
                        tracing::debug!("argument {value:?}");
                        cli_args.push(value);
                    }
                }
            }
            if parser.raw_args()?.peek() == Some(OsStr::new("--")) {
                seen_double_hyphen = true;
            }
        }

        Ok(Self {
            cli_args,
            post_double_hyphen,
            manifest_path,
        })
    }

    fn cargo_command(&self) -> CargoCli {
        let mut cli = CargoCli::new();
        cli.args(&self.cli_args);
        if !self.post_double_hyphen.is_empty() {
            cli.arg("--");
            cli.args(&self.post_double_hyphen);
        }
        cli
    }
}

fn find_targo_store_dir() -> Result<Utf8PathBuf> {
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

    #[test]
    fn test_parse_wrap_cargo_args() -> Result<()> {
        let data = [
            "build -p foo",
            "test",
            "clippy --package baz --manifest-path test",
            "check --all-targets -- -Dwarnings",
            "run --package baz -- -- arg1 arg2",
        ];
        for input in data {
            let input_args = shell_words::split(input)?;
            let parser = lexopt::Parser::from_args(input_args);
            let args = ParsedCargoArgs::from_parser(parser)?;

            let cargo_command = args.cargo_command();
            let output = cargo_command
                .get_args()
                .iter()
                .map(|s| s.to_str().expect("inputs were valid strings"));
            let output = shell_words::join(output);

            assert_eq!(input, &output, "input matches output");
        }

        Ok(())
    }
}
