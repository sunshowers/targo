use camino::{Utf8Path, Utf8PathBuf};
use camino_tempfile::Utf8TempDir;
use fs2::FileExt;
use std::{
    collections::BTreeMap, env, fs, os::unix::fs::PermissionsExt, process::Command,
    time::SystemTime,
};

/// Inherited variables with these prefixes could point targo or Cargo outside the temp dir.
const SCRUBBED_ENV_PREFIXES: [&str; 3] = ["CARGO_", "TARGO_", "XDG_"];

/// Whether some process holds the store lock.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StoreLockState {
    Held,
    Free,
}

/// What is at a path, with its modification time.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Dir(SystemTime),
    File(SystemTime, String),
    Symlink(SystemTime, Utf8PathBuf),
}

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

    /// Records everything in the temp dir, to check that a run leaves it untouched.
    pub(crate) fn snapshot(&self) -> BTreeMap<Utf8PathBuf, Entry> {
        let mut entries = BTreeMap::new();
        record_entries(&self.root, &mut entries);
        entries
    }

    /// Reports the state of the store lock by trying to take it without blocking.
    pub(crate) fn store_lock_state(&self) -> StoreLockState {
        let lock_path = self.store_dir().join("targo.lock");
        let probe = fs::File::open(&lock_path).expect("opened store lock file");
        match probe.try_lock_exclusive() {
            Ok(()) => StoreLockState::Free,
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                StoreLockState::Held
            }
            Err(error) => panic!("failed to probe the lock at `{lock_path}`: {error}"),
        }
    }

    /// Writes a stand-in for `cargo` that says `ready` when targo runs it, then waits for stdin.
    ///
    /// `locate-project`, which targo runs before the real command, goes to the real Cargo.
    pub(crate) fn create_waiting_cargo(&self) -> Utf8PathBuf {
        let real_cargo = shell_words::quote(env!("CARGO"));
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$1\" = locate-project ]; then exec {real_cargo} \"$@\"; fi\n\
             echo ready\n\
             read -r _\n\
             exit 0\n"
        );
        let path = self.root.join("waiting-cargo");
        fs::write(&path, script).expect("wrote stand-in cargo");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("made stand-in cargo executable");
        path
    }

    /// Returns a command for the built `targo` binary, confined to this environment.
    pub(crate) fn targo(&self) -> Command {
        self.confined_command(env!("CARGO_BIN_EXE_targo"))
    }

    /// The real Cargo, confined like targo so that it behaves as targo's Cargo does.
    pub(crate) fn cargo(&self) -> Command {
        self.confined_command(env!("CARGO"))
    }

    fn confined_command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
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

fn record_entries(dir: &Utf8Path, entries: &mut BTreeMap<Utf8PathBuf, Entry>) {
    for entry in dir.read_dir_utf8().expect("read dir") {
        let path = entry.expect("read dir entry").into_path();
        let metadata = path.symlink_metadata().expect("read metadata");
        let modified = metadata.modified().expect("read modification time");
        let entry = if metadata.is_symlink() {
            Entry::Symlink(modified, path.read_link_utf8().expect("read symlink"))
        } else if metadata.is_dir() {
            record_entries(&path, entries);
            Entry::Dir(modified)
        } else {
            Entry::File(modified, fs::read_to_string(&path).expect("read file"))
        };
        entries.insert(path, entry);
    }
}
