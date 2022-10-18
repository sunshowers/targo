use clap::Parser;

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let app = targo::TargoApp::parse();
    app.exec()
}
