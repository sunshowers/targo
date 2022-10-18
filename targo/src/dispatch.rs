use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use color_eyre::{
    eyre::{bail, WrapErr},
    Result,
};
use lexopt::prelude::*;
use std::{ffi::OsString, fmt, path::PathBuf, process::Command};

#[derive(Debug, Parser)]
pub struct TargoApp {
    // TODO: command
    #[command(subcommand)]
    command: TargoCommand,
}

#[derive(Debug, Subcommand)]
pub enum TargoCommand {
    /// Wrap cargo and pass through commands.
    WrapCargo {
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
    let args_with_data = CargoArgsWithData::from_parser(parser)?;

    // Find the target directory destination.
    let targo_base = find_targo_dir_base()?;

    // TODO: read --target-dir/build.target-dir from cargo.

    let target_dir = args_with_data.target_dir();
    let (exists, should_create) = match target_dir.symlink_metadata() {
        Ok(metadata) => (true, !metadata.is_symlink()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (false, true),
        Err(err) => {
            return Err::<(), _>(err)
                .wrap_err_with(|| format!("failed to read metadata for target dir `{target_dir}`"))
        }
    };

    // Is the target directory already a symlink? If so, don't touch it.
    if should_create {
        // Create a symlink to the destination directory.
        let dest = targo_base
            .join(args_with_data.hash_workspace_dir())
            .join("target");
        std::fs::create_dir_all(&dest)
            .wrap_err_with(|| format!("failed to create target dir {dest}"))?;

        if exists {
            // TODO: do something better than rm -rf target/ here!
            match std::fs::remove_dir_all(&target_dir) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    // The directory doesn't exist. Skip this.
                }
                Err(err) => {
                    Err::<(), _>(err).wrap_err_with(|| {
                        format!("failed to remove old target dir `{target_dir}`")
                    })?;
                }
            }
        }

        // Create the symlink now.
        std::os::unix::fs::symlink(&dest, &target_dir).wrap_err_with(|| {
            format!("failed to create symlink from `{target_dir}` to `{dest}`")
        })?;
    }

    args_with_data.cargo_command().run_or_exec()?;

    Ok(())
}

#[derive(Clone, Debug)]
struct CargoArgsWithData {
    cli_args: Vec<OsString>,
    workspace_dir: Utf8PathBuf,
}

impl CargoArgsWithData {
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
                    cli_args.extend(["--manifest-path".into(), new_manifest_path.into()]);
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
        if locate_project_output.chars().last() != Some('\n') {
            bail!("`{locate_project}` produced output not terminated with a newline: {locate_project_output}");
        }
        locate_project_output.pop();
        let mut workspace_dir = Utf8PathBuf::from(locate_project_output);
        // The filename of workspace dir should be Cargo.toml.
        if workspace_dir.file_name() != Some("Cargo.toml") {
            bail!("cargo locate-project output `{workspace_dir}` doesn't end with Cargo.toml");
        }
        workspace_dir.pop();

        Ok(Self {
            cli_args,
            workspace_dir,
        })
    }

    fn target_dir(&self) -> Utf8PathBuf {
        // TODO: read --target-dir/build.target-dir from cargo.
        self.workspace_dir.join("target")
    }

    fn cargo_command(&self) -> CargoCli {
        let mut cli = CargoCli::new();
        cli.args(&self.cli_args);
        cli
    }

    fn hash_workspace_dir(&self) -> String {
        let mut hasher = blake3::Hasher::new_keyed(TARGO_HASHER_KEY);
        hasher.update(self.workspace_dir.as_str().as_bytes());
        bs58::encode(&hasher.finalize().as_bytes()[..20]).into_string()
    }
}

static TARGO_HASHER_KEY: &[u8; 32] = b"targo\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

fn find_targo_dir_base() -> Result<Utf8PathBuf> {
    let dir = home::cargo_home().wrap_err("unable to determine cargo home dir")?;
    let mut utf8_dir: Utf8PathBuf = dir
        .clone()
        .try_into()
        .wrap_err_with(|| format!("cargo home `{}` is invalid UTF-8", dir.display()))?;
    utf8_dir.push("targo");
    Ok(utf8_dir)
}

#[derive(Clone, Debug)]
struct CargoCli {
    cargo_bin: OsString,
    args: Vec<OsString>,
}

impl CargoCli {
    fn new() -> Self {
        let cargo_bin = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        Self {
            cargo_bin,
            args: Vec::new(),
        }
    }

    fn args(&mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> &mut Self {
        self.args.extend(args.into_iter().map(|arg| arg.into()));
        self
    }

    fn make_command(&self) -> Command {
        let mut command = Command::new(&self.cargo_bin);
        command.args(&self.args);
        command
    }

    fn stdout_output(&self) -> Result<Vec<u8>> {
        let mut command = self.make_command();
        let output = command
            .output()
            .wrap_err_with(|| format!("failed to run `{self}`"))?;
        if !output.status.success() {
            let mut message = format!("command `{self}` failed");
            if let Some(code) = output.status.code() {
                message.push_str(&format!(" with exit code {code}"));
            }
            message.push_str("\n\n--- stdout ---\n");
            message.push_str(&String::from_utf8_lossy(&output.stdout));
            message.push_str("\n\n--- stderr ---\n");
            message.push_str(&String::from_utf8_lossy(&output.stderr));

            bail!(message);
        }

        Ok(output.stdout)
    }

    fn run_or_exec(&self) -> Result<()> {
        use std::os::unix::process::CommandExt;

        // TODO: Windows, can't exec there -- must run and propagate error etc
        let mut command = self.make_command();
        Err(command.exec().into())
    }
}

impl fmt::Display for CargoCli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let iter = std::iter::once(self.cargo_bin.to_string_lossy())
            .chain(self.args.iter().map(|arg| arg.to_string_lossy()));
        f.write_str(&shell_words::join(iter))
    }
}
