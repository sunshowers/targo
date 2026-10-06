use clap::Parser;
use std::process::ExitCode;

fn main() -> color_eyre::Result<ExitCode> {
    color_eyre::install()?;
    let app = targo::TargoApp::parse();
    app.exec()
}
