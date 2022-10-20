use crate::{cargo_cli::CargoCli, store::TargoStore};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use color_eyre::{
    eyre::{bail, WrapErr},
    Result,
};
use lexopt::prelude::*;
use std::{ffi::OsString, path::PathBuf};

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
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

impl TargoApp {
    pub fn exec(self) -> Result<()> {
        match self.command {
            TargoCommand::WrapCargo { args } => exec_wrap_cargo(args),
        }
    }
}

fn exec_wrap_cargo(args: Vec<OsString>) -> Result<()> {
    let parser = lexopt::Parser::from_args(args);
    let args = WrapCargoArgs::from_parser(parser)?;

    // Find the target directory destination.
    let store_dir = find_targo_store_dir()?;
    let store = TargoStore::new(store_dir)?;

    let kind = store.determine_target_dir(&args.workspace_dir, &args.target_dir)?;
    store.actualize_kind(kind)?;

    args.cargo_command().run_or_exec()?;

    Ok(())
}

#[derive(Clone, Debug)]
struct WrapCargoArgs {
    cli_args: Vec<OsString>,
    workspace_dir: Utf8PathBuf,
    target_dir: Utf8PathBuf,
}

impl WrapCargoArgs {
    fn from_parser(mut parser: lexopt::Parser) -> Result<Self> {
        // TODO: intercept cargo clean -- it doesn't work right now, it should clean the symlink
        // target.

        let mut cli_args = Vec::new();
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

                    // Also pass through the manifest path to the underlying cargo command.
                    cli_args.extend(["--manifest-path".into(), new_manifest_path]);
                }
                Long(other) => {
                    cli_args.push(format!("--{other}").into());
                    if let Some(val) = parser.optional_value() {
                        cli_args.push(val);
                    }
                }
                Short(arg) => {
                    cli_args.push(format!("-{arg}").into());
                    if let Some(val) = parser.optional_value() {
                        cli_args.push(val);
                    }
                }
                Value(value) => {
                    cli_args.push(value);
                }
            }
        }

        // Determine the workspace dir.
        let mut locate_project = CargoCli::new();
        locate_project.args(["locate-project", "--workspace", "--message-format=plain"]);

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
            cli_args,
            workspace_dir,
            target_dir,
        })
    }

    fn cargo_command(&self) -> CargoCli {
        let mut cli = CargoCli::new();
        cli.args(&self.cli_args);
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
