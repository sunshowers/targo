use camino::{Utf8Path, Utf8PathBuf};
use camino_tempfile::Utf8TempDir;
use std::{env, fs, process::Command};

/// Inherited variables with these prefixes could point targo or Cargo outside the temp dir.
const SCRUBBED_ENV_PREFIXES: [&str; 3] = ["CARGO_", "TARGO_", "XDG_"];

/// A temp dir that holds a targo store and throwaway workspaces.
pub(crate) struct TestEnv {
    // Held so that the directory is removed on drop.
    _temp_dir: Utf8TempDir,
    root: Utf8PathBuf,
}

impl TestEnv {
    pub(crate) fn new() -> Self {
        let temp_dir = camino_tempfile::tempdir().expect("created temp dir");
        // Cargo sees the canonical path, and a temp dir can be behind a symlink (as on macOS).
        let root = temp_dir
            .path()
            .canonicalize_utf8()
            .expect("canonicalized temp dir");
        // Commands run in the root by default, where Cargo must not find a workspace.
        for dir in root.ancestors() {
            assert!(
                !dir.join("Cargo.toml").exists(),
                "temp dir `{root}` is outside any Cargo workspace"
            );
        }
        Self {
            _temp_dir: temp_dir,
            root,
        }
    }

    /// The root of the temp dir, which is not itself a workspace.
    pub(crate) fn root(&self) -> &Utf8Path {
        &self.root
    }

    pub(crate) fn store_dir(&self) -> Utf8PathBuf {
        self.root.join("store")
    }

    pub(crate) fn cargo_home(&self) -> Utf8PathBuf {
        self.root.join("cargo-home")
    }

    /// Creates a minimal workspace and returns its directory.
    pub(crate) fn create_workspace(&self, name: &str) -> Utf8PathBuf {
        let workspace_dir = self.root.join(name);
        fs::create_dir_all(workspace_dir.join("src")).expect("created workspace dir");
        // The `[workspace]` table stops Cargo from looking for a root in parent directories.
        let manifest = format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
        );
        fs::write(workspace_dir.join("Cargo.toml"), manifest).expect("wrote manifest");
        fs::write(workspace_dir.join("src/lib.rs"), "").expect("wrote src/lib.rs");
        workspace_dir
    }

    /// Returns a command for the built `targo` binary, confined to this environment.
    pub(crate) fn targo(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_targo"));
        for (name, _) in env::vars_os() {
            let name_str = name.to_string_lossy();
            if SCRUBBED_ENV_PREFIXES
                .iter()
                .any(|prefix| name_str.starts_with(prefix))
            {
                command.env_remove(&name);
            }
        }
        command
            // Without this, a test that sets no directory would run targo on this repository.
            .current_dir(&self.root)
            .env("TARGO_STORE_DIR", self.store_dir())
            // If the override were ever ignored, the default store would still be in the temp dir.
            .env("CARGO_HOME", self.cargo_home())
            // Anything derived from the home directory stays in the temp dir.
            .env("HOME", self.root.join("home"))
            // A `cargo` found through `PATH` may be a wrapper that depends on `CARGO_HOME`.
            .env("CARGO", env!("CARGO"));
        command
    }
}
