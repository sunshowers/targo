use color_eyre::{
    eyre::{bail, Context},
    Result,
};
use std::{ffi::OsString, fmt, process::Command};

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

    pub(crate) fn stdout_output(&self) -> Result<Vec<u8>> {
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

    pub(crate) fn run_or_exec(&self) -> Result<()> {
        use std::os::unix::process::CommandExt;

        // TODO: Windows, can't exec there -- must run and propagate error etc
        let mut command = self.make_command();
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
