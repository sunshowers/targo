use crate::support::{StoreLockState, TestEnv};
use camino::{Utf8Path, Utf8PathBuf};
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, BufRead, BufReader},
    iter,
    os::unix::{ffi::OsStrExt, fs::symlink},
    process::{Command, Output, Stdio},
};

#[test]
fn wrap_cargo_uses_store_dir_override() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");

    run_wrap_cargo(&mut env.targo(), &workspace_dir);

    assert_linked_into_store(&workspace_dir, &env.store_dir());
    assert!(
        !env.cargo_home().join("targo").exists(),
        "the default store is not used"
    );
}

#[test]
fn wrap_cargo_uses_default_store_without_override() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");

    run_wrap_cargo(env.targo().env_remove("TARGO_STORE_DIR"), &workspace_dir);

    assert_linked_into_store(&workspace_dir, &env.cargo_home().join("targo"));
}

#[test]
fn wrap_cargo_rejects_bad_store_dir_override() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");

    // Inside the temp dir, in case the value is ever accepted.
    let mut non_utf8 = env.store_dir().into_os_string();
    non_utf8.push(OsStr::from_bytes(b"\xff"));
    let data = [
        (
            OsString::new(),
            "`TARGO_STORE_DIR` is set to an empty value: \
             set it to an absolute path, or unset it to use the default store"
                .to_owned(),
        ),
        (
            OsString::from("relative/store"),
            "`TARGO_STORE_DIR` must be an absolute path, but is set to `relative/store`".to_owned(),
        ),
        (
            non_utf8.clone(),
            format!("`TARGO_STORE_DIR` must be valid UTF-8, but is set to {non_utf8:?}"),
        ),
    ];
    for (value, expected) in data {
        // Outside a workspace targo is disabled, and the override must still be checked.
        for current_dir in [workspace_dir.as_path(), env.root()] {
            let output = env
                .targo()
                .env("TARGO_STORE_DIR", &value)
                .current_dir(current_dir)
                .args(["wrap-cargo", "version"])
                .output()
                .expect("ran targo");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && stderr.contains(&expected),
                "for {value:?} in `{current_dir}`, stderr was:\n{stderr}"
            );
        }
    }

    assert!(
        !env.cargo_home().join("targo").exists(),
        "the default store is not used as a fallback"
    );
    assert!(
        workspace_dir.join("target").symlink_metadata().is_err(),
        "the workspace is left alone"
    );
}

#[test]
fn wrap_cargo_replaces_existing_target_dir() {
    // The second store is beside the target dir, and its name starts with the target dir's.
    for store_dir in ["store", "workspace/target-store"] {
        let env = TestEnv::new();
        let workspace_dir = env.create_workspace("workspace");
        let target_dir = workspace_dir.join("target");
        fs::create_dir_all(target_dir.join("debug")).expect("created target dir");
        fs::write(target_dir.join("debug/old-file"), "").expect("wrote old file");
        let store_dir = env.root().join(store_dir);

        run_wrap_cargo(
            env.targo().env("TARGO_STORE_DIR", &store_dir),
            &workspace_dir,
        );

        assert_linked_into_store(&workspace_dir, &store_dir);
        let entries = fs::read_dir(&target_dir).expect("read new target dir");
        assert_eq!(entries.count(), 0, "the old contents are gone");
    }
}

#[test]
fn wrap_cargo_keeps_managed_target_dir() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    // As when the store path is a symlink to another disk.
    let store_dir = env.root().join("store-alias");
    fs::create_dir(env.store_dir()).expect("created store dir");
    symlink(env.store_dir(), &store_dir).expect("created store symlink");
    let built_file = workspace_dir.join("target/built-file");

    run_wrap_cargo(
        env.targo().env("TARGO_STORE_DIR", &store_dir),
        &workspace_dir,
    );
    assert_linked_into_store(&workspace_dir, &store_dir);
    fs::write(&built_file, "").expect("wrote file through the symlink");

    run_wrap_cargo(
        env.targo().env("TARGO_STORE_DIR", &store_dir),
        &workspace_dir,
    );
    assert_linked_into_store(&workspace_dir, &store_dir);
    assert!(built_file.is_file(), "the second run keeps the target dir");
}

#[test]
fn wrap_cargo_refuses_store_inside_target_dir() {
    for store_suffix in ["target", "target/nested/store"] {
        let env = TestEnv::new();
        let workspace_dir = env.create_workspace("workspace");
        let store_dir = workspace_dir.join(store_suffix);

        assert_store_refused(
            &env,
            env.targo().env("TARGO_STORE_DIR", &store_dir),
            &workspace_dir,
            &format!("`{store_dir}`"),
            &format!("`{}`", workspace_dir.join("target")),
        );
    }
}

#[test]
fn wrap_cargo_refuses_store_inside_target_dir_through_other_paths() {
    // Each case sets up the temp dir, then gives the store dir relative to the temp dir and
    // where that really is relative to the workspace.
    type Setup = fn(&Utf8Path, &Utf8Path);
    let data: [(Setup, &str, &str); 3] = [
        (
            // The target dir doesn't exist, and the store is named through a symlink above it.
            |root, workspace_dir| symlink(workspace_dir, root.join("alias")).expect("symlinked"),
            "alias/target/store",
            "target/store",
        ),
        (
            // The target dir is a real directory, and a symlink elsewhere leads into it.
            |root, workspace_dir| {
                let inner_dir = workspace_dir.join("target/inner");
                fs::create_dir_all(&inner_dir).expect("created target dir");
                symlink(inner_dir, root.join("alias")).expect("symlinked");
            },
            "alias/store",
            "target/inner/store",
        ),
        (|_, _| {}, "missing/../workspace/target/.", "target"),
    ];
    for (setup, store_dir, store_location) in data {
        let env = TestEnv::new();
        let workspace_dir = env.create_workspace("workspace");
        setup(env.root(), &workspace_dir);
        let store_dir = env.root().join(store_dir);
        let store_location = workspace_dir.join(store_location);

        assert_store_refused(
            &env,
            env.targo().env("TARGO_STORE_DIR", &store_dir),
            &workspace_dir,
            &format!("`{store_dir}` (which resolves to `{store_location}`)"),
            &format!("`{}`", workspace_dir.join("target")),
        );
    }
}

#[test]
fn wrap_cargo_refuses_default_store_inside_target_dir() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    let cargo_home = workspace_dir.join("target/cargo-home");

    assert_store_refused(
        &env,
        env.targo()
            .env_remove("TARGO_STORE_DIR")
            .env("CARGO_HOME", &cargo_home),
        &workspace_dir,
        &format!("`{}`", cargo_home.join("targo")),
        &format!("`{}`", workspace_dir.join("target")),
    );
}

#[test]
fn wrap_cargo_refuses_existing_store_inside_target_dir() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    let other_dir = env.create_workspace("other");
    let store_dir = workspace_dir.join("target/store");

    // A run in another workspace is fine, and leaves an entry in the store.
    run_wrap_cargo(env.targo().env("TARGO_STORE_DIR", &store_dir), &other_dir);
    assert_linked_into_store(&other_dir, &store_dir);
    let built_file = other_dir.join("target/built-file");
    fs::write(&built_file, "").expect("wrote file through the symlink");

    assert_store_refused(
        &env,
        env.targo().env("TARGO_STORE_DIR", &store_dir),
        &workspace_dir,
        &format!("`{store_dir}`"),
        &format!("`{}`", workspace_dir.join("target")),
    );
    assert!(built_file.is_file(), "the other workspace's entry survives");
}

#[test]
fn wrap_cargo_refuses_store_inside_linked_target_dir() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    run_wrap_cargo(&mut env.targo(), &workspace_dir);

    // Through the symlink that the first run made, this is inside the first store's entry.
    let target_dir = workspace_dir.join("target");
    let store_dir = target_dir.join("store");
    let target_location = target_dir.read_link_utf8().expect("target is a symlink");
    let store_location = target_location.join("store");

    assert_store_refused(
        &env,
        env.targo().env("TARGO_STORE_DIR", &store_dir),
        &workspace_dir,
        &format!("`{store_dir}` (which resolves to `{store_location}`)"),
        &format!("`{target_dir}` (which resolves to `{target_location}`)"),
    );
}

#[test]
fn wrap_cargo_refuses_store_through_symlink_in_replaced_target_dir() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    let target_dir = workspace_dir.join("target");
    let elsewhere_dir = env.root().join("elsewhere");
    fs::create_dir_all(&elsewhere_dir).expect("created dir");
    fs::create_dir(&target_dir).expect("created target dir");
    symlink(&elsewhere_dir, target_dir.join("link")).expect("created symlink");
    let store_dir = target_dir.join("link/store");

    assert_refusal(
        env.targo().env("TARGO_STORE_DIR", &store_dir),
        &workspace_dir,
        &format!("`{store_dir}`"),
        &format!("`{target_dir}`"),
    );
    assert!(
        target_dir.symlink_metadata().is_err(),
        "once the target dir and its symlink are removed, no store is created in its place"
    );
}

#[test]
fn wrap_cargo_refuses_store_inside_target_dir_named_in_another_case() {
    assert_store_refused_through(TargetDirAlias::OtherCase);
}

#[test]
fn wrap_cargo_refuses_store_inside_target_dir_of_bind_mounted_workspace() {
    assert_store_refused_through(TargetDirAlias::BindMount);
}

/// A second path to a workspace's `target` that path text can't connect to the first.
#[derive(Clone, Copy, Debug)]
enum TargetDirAlias {
    /// The name in another case, where the filesystem ignores case.
    OtherCase,
    /// A bind mount of the workspace, in a mount namespace that only targo is in.
    BindMount,
}

impl TargetDirAlias {
    /// Makes a workspace that has the alias. Returns `None`, and says why, where it can't.
    fn create_workspace(self, env: &TestEnv) -> Option<Utf8PathBuf> {
        let workspace_dir = env.create_workspace("workspace");
        match self {
            Self::OtherCase => {
                let probe = env.root().join("case-probe");
                fs::write(&probe, "").expect("wrote probe");
                let ignores_case = env.root().join("CASE-PROBE").exists();
                fs::remove_file(&probe).expect("removed probe");
                if !ignores_case {
                    eprintln!("skipped: the temp dir is on a case-sensitive filesystem");
                    return None;
                }
            }
            Self::BindMount => {
                let mount_point = Self::mount_point(env);
                fs::create_dir(&mount_point).expect("created mount point");
                if !env.can_bind_mount_in_namespace(&workspace_dir, &mount_point) {
                    return None;
                }
            }
        }
        Some(workspace_dir)
    }

    fn target_dir(self, env: &TestEnv, workspace_dir: &Utf8Path) -> Utf8PathBuf {
        match self {
            Self::OtherCase => workspace_dir.join("TARGET"),
            Self::BindMount => Self::mount_point(env).join("target"),
        }
    }

    fn targo(self, env: &TestEnv, workspace_dir: &Utf8Path) -> Command {
        self.command(env, workspace_dir, env!("CARGO_BIN_EXE_targo"))
    }

    fn command(self, env: &TestEnv, workspace_dir: &Utf8Path, program: &str) -> Command {
        match self {
            Self::OtherCase => env.confined_command(program),
            Self::BindMount => env.command_in_namespace_with_bind_mount(
                workspace_dir,
                &Self::mount_point(env),
                program,
            ),
        }
    }

    fn mount_point(env: &TestEnv) -> Utf8PathBuf {
        env.root().join("mounted-workspace")
    }
}

fn assert_store_refused_through(alias: TargetDirAlias) {
    let scenarios: [fn(TargetDirAlias, &TestEnv, &Utf8Path); 3] = [
        assert_no_store_is_created_in_a_real_target_dir,
        assert_store_of_another_workspace_is_kept,
        assert_store_that_made_the_target_dir_is_kept,
    ];
    for scenario in scenarios {
        let env = TestEnv::new();
        let Some(workspace_dir) = alias.create_workspace(&env) else {
            return;
        };
        scenario(alias, &env, &workspace_dir);
    }
}

fn assert_no_store_is_created_in_a_real_target_dir(
    alias: TargetDirAlias,
    env: &TestEnv,
    workspace_dir: &Utf8Path,
) {
    let target_dir = workspace_dir.join("target");
    fs::create_dir(&target_dir).expect("created target dir");
    fs::write(target_dir.join("old-file"), "").expect("wrote old file");
    let target_alias = alias.target_dir(env, workspace_dir);
    let store_dir = target_alias.join("store");

    assert_store_refused(
        env,
        alias
            .targo(env, workspace_dir)
            .env("TARGO_STORE_DIR", &store_dir),
        workspace_dir,
        &format!("`{store_dir}`"),
        &format!("`{target_dir}` (the same directory as `{target_alias}`)"),
    );
}

fn assert_store_of_another_workspace_is_kept(
    alias: TargetDirAlias,
    env: &TestEnv,
    workspace_dir: &Utf8Path,
) {
    let other_dir = env.create_workspace("other");
    let target_dir = workspace_dir.join("target");
    let store_location = target_dir.join("store");
    run_wrap_cargo(
        env.targo().env("TARGO_STORE_DIR", &store_location),
        &other_dir,
    );
    assert_linked_into_store(&other_dir, &store_location);
    let built_file = other_dir.join("target/built-file");
    fs::write(&built_file, "").expect("wrote file through the symlink");
    let store_dir = alias.target_dir(env, workspace_dir).join("store");

    assert_store_refused(
        env,
        alias
            .targo(env, workspace_dir)
            .env("TARGO_STORE_DIR", &store_dir),
        workspace_dir,
        &format!("`{store_dir}`"),
        &format!("`{target_dir}` (the same directory as `{store_dir}/..`)"),
    );
    assert!(built_file.is_file(), "the other workspace's entry survives");
}

fn assert_store_that_made_the_target_dir_is_kept(
    alias: TargetDirAlias,
    env: &TestEnv,
    workspace_dir: &Utf8Path,
) {
    let target_dir = workspace_dir.join("target");
    let store_dir = alias.target_dir(env, workspace_dir).join("store");

    assert_refusal(
        alias
            .targo(env, workspace_dir)
            .env("TARGO_STORE_DIR", &store_dir),
        workspace_dir,
        &format!("`{store_dir}`"),
        &format!("`{target_dir}` (the same directory as `{store_dir}/..`)"),
    );
    assert!(
        target_dir.join("store/targo-metadata.json").is_file(),
        "the store that the run created is not removed along with the target dir"
    );
}

#[test]
fn wrap_cargo_refuses_store_under_bind_mount_of_directory_inside_target_dir() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    let target_dir = workspace_dir.join("target");
    let inner_dir = target_dir.join("inner");
    let old_file = target_dir.join("old-file");
    let mount_point = env.root().join("mounted-inner");
    fs::create_dir_all(&inner_dir).expect("created target dir");
    fs::write(&old_file, "").expect("wrote old file");
    fs::create_dir(&mount_point).expect("created mount point");
    if !env.can_bind_mount_in_namespace(&inner_dir, &mount_point) {
        return;
    }
    // Walking up from here leaves the mount at its root, without reaching the target dir.
    let store_dir = mount_point.join("store");
    let store_location = inner_dir.join("store");

    assert_refusal(
        env.command_in_namespace_with_bind_mount(
            &inner_dir,
            &mount_point,
            env!("CARGO_BIN_EXE_targo"),
        )
        .env("TARGO_STORE_DIR", &store_dir),
        &workspace_dir,
        &format!("`{store_dir}`"),
        &format!("`{target_dir}` (`{store_dir}` is the same directory as `{store_location}`)"),
    );
    assert!(
        store_location.join("targo-metadata.json").is_file() && old_file.is_file(),
        "neither the store that the run created nor anything else in the target dir is removed"
    );
}

#[test]
fn wrap_cargo_leaves_unresolvable_target_symlink() {
    let looping = "target";
    let dangling = "missing/target";
    for link_dest in [looping, dangling] {
        let env = TestEnv::new();
        let workspace_dir = env.create_workspace("workspace");
        let target_dir = workspace_dir.join("target");
        symlink(link_dest, &target_dir).expect("created symlink");

        run_wrap_cargo(&mut env.targo(), &workspace_dir);

        assert_eq!(
            target_dir.read_link_utf8().expect("target is a symlink"),
            link_dest,
            "the symlink is left alone"
        );
    }
}

#[test]
fn wrap_cargo_records_every_backlink_to_an_entry() {
    let env = TestEnv::new();
    let first_dir = env.create_workspace("first");
    let second_dir = env.create_workspace("second");
    run_wrap_cargo(&mut env.targo(), &first_dir);

    // As when a workspace is copied along with its `target` symlink.
    let first_link = first_dir.join("target");
    let second_link = second_dir.join("target");
    let link_dest = first_link.read_link_utf8().expect("target is a symlink");
    symlink(&link_dest, &second_link).expect("created second symlink");
    run_wrap_cargo(&mut env.targo(), &second_dir);

    let entry_dir = link_dest.parent().expect("link destination has a parent");
    assert_eq!(
        read_backlinks(entry_dir),
        serde_json::json!([first_link, second_link])
    );
}

#[test]
fn wrap_cargo_releases_store_lock_before_cargo_runs() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");

    let mut child = env
        .targo()
        .env("CARGO", env.create_waiting_cargo())
        .current_dir(&workspace_dir)
        .args(["wrap-cargo", "build"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawned targo");

    // The stand-in cargo is the process targo exec'ed, and stays alive until stdin is closed.
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("read from stand-in cargo");
    assert_eq!(line, "ready\n", "targo ran the stand-in cargo");

    assert_linked_into_store(&workspace_dir, &env.store_dir());
    assert_eq!(
        env.store_lock_state(),
        StoreLockState::Free,
        "the store lock is not held while cargo runs"
    );

    drop(child.stdin.take());
    let status = child.wait().expect("waited for stand-in cargo");
    assert!(status.success(), "stand-in cargo exited with {status}");
}

#[test]
fn wrap_cargo_is_quiet_outside_a_workspace() {
    let env = TestEnv::new();
    let before = env.snapshot();

    // Forced color must not hide Cargo's "no manifest" error from targo.
    for color in ["auto", "always"] {
        let output = run_wrap_cargo(env.targo().env("CARGO_TERM_COLOR", color), env.root());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            (stdout.lines().count(), &*stderr),
            (1, ""),
            "with color {color:?}, only Cargo's version is printed, stdout was:\n{stdout}"
        );
    }

    let output = run_wrap_cargo(env.targo().env("TARGO_LOG", "debug"), env.root());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            " DEBUG targo::dispatch: disabled for this command: Cargo found no manifest\n"
        ),
        "the reason is in the debug log, which was:\n{stderr}"
    );

    assert_eq!(env.snapshot(), before, "a disabled run changes nothing");
}

#[test]
fn wrap_cargo_logs_to_stderr() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");

    let quiet = run_wrap_cargo(&mut env.targo(), &workspace_dir);
    // Without `NO_COLOR`, only the check for a terminal keeps color off.
    let logged = run_wrap_cargo(
        env.targo().env("TARGO_LOG", "debug").env_remove("NO_COLOR"),
        &workspace_dir,
    );

    assert_eq!(
        String::from_utf8_lossy(&logged.stdout),
        String::from_utf8_lossy(&quiet.stdout),
        "the log is kept out of Cargo's output"
    );
    assert_eq!(String::from_utf8_lossy(&quiet.stderr), "");
    let stderr = String::from_utf8_lossy(&logged.stderr);
    let running = format!(
        " DEBUG targo::cargo_cli: running command: {} version\n",
        shell_words::quote(env!("CARGO"))
    );
    assert!(
        stderr.ends_with(&running) && !stderr.contains('\x1b'),
        "the log is on stderr, uncolored as stderr is not a terminal, and was:\n{stderr}"
    );
}

#[test]
fn wrap_cargo_runs_cargo_when_the_log_cannot_be_written() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    // Closed before targo starts, so every write to stderr fails.
    let (reader, writer) = io::pipe().expect("created pipe");
    drop(reader);

    // The helper checks that Cargo ran.
    run_wrap_cargo(
        env.targo().env("TARGO_LOG", "debug").stderr(writer),
        &workspace_dir,
    );
}

#[test]
fn wrap_cargo_shows_why_cargo_cannot_locate_the_workspace() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    fs::write(workspace_dir.join("Cargo.toml"), "not a manifest [").expect("wrote manifest");

    // Cargo's wording varies by version, so ask the real Cargo for it.
    let locate_project_args = [
        "locate-project",
        "--workspace",
        "--message-format",
        "plain",
        "--color",
        "never",
    ];
    let cargo_output = env
        .cargo()
        .current_dir(&workspace_dir)
        .args(locate_project_args)
        .output()
        .expect("ran cargo");
    let cargo_stderr = String::from_utf8(cargo_output.stderr).expect("Cargo's stderr is UTF-8");
    assert!(
        !cargo_output.status.success() && !cargo_stderr.is_empty(),
        "Cargo rejects the manifest, with stderr:\n{cargo_stderr}"
    );

    let locate_project = shell_words::join(iter::once(env!("CARGO")).chain(locate_project_args));
    let mut expected = format!(
        "[targo] disabled for this command: `{locate_project}` failed with {}\n",
        cargo_output.status
    );
    for line in cargo_stderr.lines() {
        expected.push_str("    ");
        expected.push_str(line);
        expected.push('\n');
    }

    let before = env.snapshot();
    let output = run_wrap_cargo(&mut env.targo(), &workspace_dir);
    assert_eq!(String::from_utf8_lossy(&output.stderr), expected);
    assert_eq!(env.snapshot(), before, "a disabled run changes nothing");
}

#[test]
fn wrap_cargo_fails_when_cargo_cannot_be_run() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    let not_executable = env.root().join("not-executable");
    fs::write(&not_executable, "").expect("wrote file");

    let data = [
        (
            env.root().join("missing/cargo"),
            "No such file or directory (os error 2)",
        ),
        (not_executable, "Permission denied (os error 13)"),
    ];
    let before = env.snapshot();
    for (cargo, os_error) in data {
        let output = env
            .targo()
            .env("CARGO", &cargo)
            .current_dir(&workspace_dir)
            .args(["wrap-cargo", "version"])
            .output()
            .expect("ran targo");

        let expected = format!(
            "failed to run `{} locate-project ",
            shell_words::quote(cargo.as_str())
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains(&expected) && stderr.contains(os_error),
            "for `{cargo}`, expected failure with {expected:?} and {os_error:?}, stderr was:\n{stderr}"
        );
        assert_eq!(
            stderr.matches("os error").count(),
            1,
            "for `{cargo}`, there is one error, not a second from running the command:\n{stderr}"
        );
    }

    assert_eq!(env.snapshot(), before, "a failed run changes nothing");
}

/// Runs `targo wrap-cargo version` in `workspace_dir`, which must succeed.
pub(crate) fn run_wrap_cargo(command: &mut Command, workspace_dir: &Utf8Path) -> Output {
    let output = command
        .current_dir(workspace_dir)
        .args(["wrap-cargo", "version"])
        .output()
        .expect("ran targo");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.lines().any(|line| line.starts_with("cargo ")),
        "targo exited with {}, stdout was:\n{stdout}\nstderr was:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Runs `targo wrap-cargo version` in `workspace_dir`, which must fail and change nothing.
///
/// `store` and `target` are how the error names the store directory and the target dir.
fn assert_store_refused(
    env: &TestEnv,
    command: &mut Command,
    workspace_dir: &Utf8Path,
    store: &str,
    target: &str,
) {
    let before = env.snapshot();
    assert_refusal(command, workspace_dir, store, target);
    assert_eq!(env.snapshot(), before, "a refused run changes nothing");
}

fn assert_refusal(command: &mut Command, workspace_dir: &Utf8Path, store: &str, target: &str) {
    let output = command
        .current_dir(workspace_dir)
        .args(["wrap-cargo", "version"])
        .output()
        .expect("ran targo");

    let expected = format!(
        "targo store directory {store} must be outside target dir {target}, where it would be \
         deleted along with build output: set `TARGO_STORE_DIR` to a directory somewhere else"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(&expected),
        "expected failure with {expected:?}, stderr was:\n{stderr}"
    );
}

/// Checks that `target` in the workspace links to an entry of the store at `store_dir`.
fn assert_linked_into_store(workspace_dir: &Utf8Path, store_dir: &Utf8Path) {
    assert!(store_dir.join("targo-metadata.json").is_file());

    let link = workspace_dir.join("target");
    let link_dest = link.read_link_utf8().expect("target is a symlink");
    let entry_dir = link_dest.parent().expect("link destination has a parent");
    assert_eq!(
        (entry_dir.parent(), link_dest.file_name()),
        (Some(store_dir), Some("target")),
        "`{link_dest}` is the target dir of an entry in the store"
    );
    assert!(link_dest.is_dir());

    assert_eq!(read_backlinks(entry_dir), serde_json::json!([link]));
}

/// Returns the backlinks recorded in the metadata of the store entry at `entry_dir`.
fn read_backlinks(entry_dir: &Utf8Path) -> serde_json::Value {
    let metadata = fs::read_to_string(entry_dir.join("target-dir-metadata.json"))
        .expect("read entry metadata");
    let mut metadata: serde_json::Value = serde_json::from_str(&metadata).expect("parsed JSON");
    metadata["backlinks"].take()
}
