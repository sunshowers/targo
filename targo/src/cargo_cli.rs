use color_eyre::{eyre::Context, Result};
use std::{
    ffi::OsString,
    fmt,
    process::{Command, ExitStatus},
};

/// How a Cargo command that was run for its output ended.
#[derive(Clone, Debug)]
pub(crate) enum CargoOutput {
    Success { stdout: Vec<u8> },
    Failed { status: ExitStatus, stderr: String },
}

#[derive(Clone, Debug)]
pub(crate) struct CargoCli {
    cargo_bin: OsString,
    args: Vec<OsString>,
}

impl CargoCli {
    pub(crate) fn new() -> Self {
        let cargo_bin = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        Self {
            cargo_bin,
            args: Vec::new(),
        }
    }

    pub(crate) fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    pub(crate) fn args(
        &mut self,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> &mut Self {
        self.args.extend(args.into_iter().map(|arg| arg.into()));
        self
    }

    #[cfg(test)]
    pub(crate) fn get_args(&self) -> &[OsString] {
        &self.args
    }

    /// An error here means that Cargo couldn't be run at all.
    pub(crate) fn output(&self) -> Result<CargoOutput> {
        let mut command = self.make_command();
        let output = command.output().wrap_err_with(|| {
            format!(
                "failed to run `{self}` (Cargo is taken from the `CARGO` environment variable, \
                 or from `PATH` if that is unset)"
            )
        })?;
        if output.status.success() {
            Ok(CargoOutput::Success {
                stdout: output.stdout,
            })
        } else {
            Ok(CargoOutput::Failed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }

    pub(crate) fn run_or_exec(&self) -> Result<()> {
        use std::os::unix::process::CommandExt;

        // TODO: Windows, can't exec there -- must run and propagate error etc
        let mut command = self.make_command();
        tracing::debug!("running command: {self}");
        Err(command.exec().into())
    }

    fn make_command(&self) -> Command {
        let mut command = Command::new(&self.cargo_bin);
        command.args(&self.args);
        command
    }
}

impl fmt::Display for CargoCli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let iter = std::iter::once(self.cargo_bin.to_string_lossy())
            .chain(self.args.iter().map(|arg| arg.to_string_lossy()));
        f.write_str(&shell_words::join(iter))
    }
}
