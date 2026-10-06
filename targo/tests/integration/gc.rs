use crate::support::{open_lock_file, Entry, TestEnv};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, FixedOffset, TimeDelta, Utc};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    os::unix::{
        fs::{symlink, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::SystemTime,
};

pub(crate) const HOUR: i64 = 60 * 60;
pub(crate) const DAY: i64 = 24 * HOUR;

/// The exit code of a gc that did nothing because another one was running.
const GC_BUSY_EXIT_CODE: i32 = 75;

/// A store with an entry of every kind, and what gc makes of it.
struct Scenario {
    store: TestStore,
    /// The entries that gc removes, in order, each with the reason that gc gives.
    removals: Vec<(Utf8PathBuf, String)>,
    /// The lines for entries that are kept, after the verb.
    kept_lines: Vec<String>,
    /// The summary line, after the verb for the entries that are kept.
    kept_summary: &'static str,
}

impl Scenario {
    fn new(env: &TestEnv) -> Self {
        let store = TestStore::new(env);
        let workspaces = env.root().join("workspaces");

        let live_link = workspaces.join("live/target");
        let live_target = store
            .create_entry("live", &[&live_link], -400 * DAY)
            .join("target");
        create_symlink(&live_target, &live_link);

        let gone_link = workspaces.join("gone/target");
        let orphan = store.create_entry("orphan", &[&gone_link], -(8 * DAY + 12 * HOUR));
        write_synced(&orphan.join("target/built-file"), &[b'x'; 64 * 1024]);

        // The oldest, so it is removed first even though its name sorts last.
        let relinked_link = workspaces.join("relinked/target");
        let replaced_link = workspaces.join("replaced/target");
        let stale = store.create_entry(
            "stale",
            &[&relinked_link, &replaced_link],
            -(40 * DAY + 12 * HOUR),
        );
        create_symlink(&live_target, &relinked_link);
        fs::create_dir_all(&replaced_link).expect("created dir");

        // The builds are newer than the last use, at either depth, so they give the age.
        let built = store.create_entry("built-long-ago", &[&gone_link], -(60 * DAY + 12 * HOUR));
        store.create_build_dir(
            &built,
            "target/debug",
            OutputLayout::Deps,
            -(30 * DAY + 12 * HOUR),
        );
        store.create_build_dir(
            &built,
            "target/x86_64-unknown-linux-gnu/release",
            OutputLayout::Units,
            -(20 * DAY + 12 * HOUR),
        );

        let no_backlinks = store.create_entry("no-backlinks", &[], -(7 * DAY + 12 * HOUR));
        store.create_entry("recent-orphan", &[&gone_link], -HOUR);
        let recently_built = store.create_entry("recently-built", &[&gone_link], -400 * DAY);
        store.create_build_dir(
            &recently_built,
            "target/debug",
            OutputLayout::Deps,
            -(HOUR + HOUR / 2),
        );
        store.create_entry("future", &[&gone_link], 2 * DAY + 12 * HOUR);

        let looping_link = workspaces.join("looping/target");
        store.create_entry("unknown", &[&gone_link, &looping_link], -400 * DAY);
        create_symlink(Utf8Path::new("target"), &looping_link);
        let loop_error =
            fs::metadata(&looping_link).expect_err("a looping symlink doesn't resolve");

        // As when the workspace is used in a container that has the store at another path.
        let other_view_link = workspaces.join("other-view/target");
        store.create_entry("other-view", &[&other_view_link], -400 * DAY);
        create_symlink(
            &env.root().join("container/store/other-view/target"),
            &other_view_link,
        );

        let corrupt_metadata = "{";
        let json_error = serde_json::from_str::<serde_json::Value>(corrupt_metadata)
            .expect_err("the metadata is not JSON");
        let corrupt = store.dir.join("corrupt");
        fs::create_dir_all(corrupt.join("target")).expect("created dir");
        let corrupt_metadata_path = corrupt.join("target-dir-metadata.json");
        fs::write(&corrupt_metadata_path, corrupt_metadata).expect("wrote metadata");

        fs::create_dir(store.dir.join("empty")).expect("created dir");

        // None of these is an entry: a dot-directory, a symlink to a directory, and a file.
        let dot_dir = store.create_entry(".stray", &[], -400 * DAY);
        create_symlink(&dot_dir, &store.dir.join("link-to-dir"));
        fs::copy(
            dot_dir.join("target-dir-metadata.json"),
            store.dir.join("file"),
        )
        .expect("copied file");

        let removals = vec![
            (
                stale,
                format!(
                    "orphaned, last used 40d ago; backlinks: `{relinked_link}` \
                     (points elsewhere), `{replaced_link}` (not a symlink)"
                ),
            ),
            (
                built,
                format!("orphaned, last built 20d ago; backlinks: `{gone_link}` (missing)"),
            ),
            (
                orphan,
                format!("orphaned, last used 8d ago; backlinks: `{gone_link}` (missing)"),
            ),
            (
                no_backlinks,
                "orphaned, last used 7d ago; no backlinks".to_owned(),
            ),
        ];
        let kept_lines = vec![
            format!(
                "`corrupt`: unrecognized; failed to deserialize metadata from \
                 `{corrupt_metadata_path}`: {json_error}"
            ),
            "`empty`: unrecognized; it has no `target-dir-metadata.json`".to_owned(),
            format!("`future`: last used 2d in the future; backlinks: `{gone_link}` (missing)"),
            format!(
                "`other-view`: backlink state unknown; backlinks: `{other_view_link}` \
                 (unknown: the link names this entry by a path that does not resolve here)"
            ),
            format!(
                "`recently-built`: orphaned, last built 1h ago; \
                 backlinks: `{gone_link}` (missing)"
            ),
            format!(
                "`unknown`: backlink state unknown; backlinks: `{gone_link}` (missing), \
                 `{looping_link}` (unknown: {loop_error})"
            ),
        ];
        Self {
            store,
            removals,
            kept_lines,
            kept_summary: "8 entries: 1 live, 2 orphaned within grace, \
                           2 with unknown backlinks, 2 unrecognized, 1 last active in the future",
        }
    }

    /// The report, with the verbs of a dry run or of a real one.
    fn report(&self, remove: &str, keep: &str, and_keep: &str) -> String {
        let mut report = String::new();
        let mut total_size = 0;
        for (dir, reason) in &self.removals {
            let name = dir.file_name().expect("entry has a name");
            let size = disk_usage(dir);
            total_size += size;
            report.push_str(&format!(
                "{remove} `{name}` ({}): {reason}\n",
                human_size(size)
            ));
        }
        for line in &self.kept_lines {
            report.push_str(&format!("{keep} {line}\n"));
        }
        report.push_str(&format!(
            "{remove} 4 entries ({}) and {and_keep} {}\n",
            human_size(total_size),
            self.kept_summary
        ));
        report
    }
}

#[test]
fn gc_dry_run_reports_and_changes_nothing() {
    let env = TestEnv::new();
    let scenario = Scenario::new(&env);
    let expected = scenario.report("would remove", "would keep", "keep");

    let before = env.snapshot();
    let output = run_dry_gc(&env, &[]);
    assert_eq!(stdout_of_success(&output), expected);
    assert_eq!(env.snapshot(), before, "a dry run changes nothing");
}

#[test]
fn gc_removes_orphans_and_touches_nothing_else() {
    let env = TestEnv::new();
    let scenario = Scenario::new(&env);
    let store_dir = &scenario.store.dir;
    let expected = scenario.report("removed", "kept", "kept");

    let before = env.snapshot();
    let output = run_gc(&env, &[]);
    assert_eq!(stdout_of_success(&output), expected);

    let mut expected = before;
    expected.retain(|path, _| {
        !scenario
            .removals
            .iter()
            .any(|(dir, _)| path.starts_with(dir))
    });
    let mut after = env.snapshot();
    // Removing entries from the store changes when the store directory was modified.
    for snapshot in [&mut expected, &mut after] {
        match snapshot.remove(store_dir) {
            Some(Entry::Dir(_)) => {}
            entry => panic!("the store is {entry:?}"),
        }
    }
    match after.remove(&store_dir.join("gc.lock")) {
        Some(Entry::File(_, contents)) => assert_eq!(contents, ""),
        entry => panic!("the gc lock is {entry:?}"),
    }
    match after.remove(&store_dir.join(".targo-trash")) {
        Some(Entry::Dir(_)) => {}
        entry => panic!("the trash is {entry:?}"),
    }
    assert_eq!(
        after, expected,
        "only the orphans are gone, and the trash is empty"
    );

    // With the orphans gone, a second run finds nothing more.
    let output = run_gc(&env, &[]);
    assert_eq!(
        stdout_of_success(&output),
        format!(
            "{}removed 0 entries (0 B) and kept {}\n",
            scenario
                .kept_lines
                .iter()
                .map(|line| format!("kept {line}\n"))
                .collect::<String>(),
            scenario.kept_summary
        )
    );
}

#[test]
fn gc_skips_an_entry_that_cargo_is_building_in() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let free = store.create_entry("free", &[], -(30 * DAY + 12 * HOUR));
    let locked = store.create_entry("locked", &[], -(20 * DAY + 12 * HOUR));
    let locked_triple = store.create_entry("locked-triple", &[], -(10 * DAY + 12 * HOUR));
    create_cargo_lock(&free, "target/debug");
    let free_size = human_size(disk_usage(&free));
    let lock_path = create_cargo_lock(&locked, "target/debug");
    create_cargo_lock(&locked_triple, "target/release");
    let triple_lock_path = create_cargo_lock(&locked_triple, "target/aarch64-apple-darwin/debug");

    // As Cargo holds them for the length of a build.
    let _cargo_locks = [&lock_path, &triple_lock_path].map(|path| {
        let cargo_lock = open_lock_file(path);
        cargo_lock.lock().expect("locked as Cargo does");
        cargo_lock
    });
    let before = env.snapshot();
    let output = run_gc(&env, &[]);
    assert_eq!(
        stdout_of_success(&output),
        format!(
            "removed `free` ({free_size}): orphaned, last used 30d ago; no backlinks\n\
             skipped `locked`: in use, Cargo holds the lock at `{lock_path}`\n\
             skipped `locked-triple`: in use, Cargo holds the lock at `{triple_lock_path}`\n\
             removed 1 entry ({free_size}) and kept 2 entries: 2 in use\n"
        )
    );
    assert!(!free.exists());
    let after = env.snapshot();
    for dir in [&locked, &locked_triple] {
        let (before, after) = (subtree(&before, dir), subtree(&after, dir));
        assert!(
            !before.is_empty() && after == before,
            "`{dir}` is untouched"
        );
    }
}

/// What the test of a real build hears of first.
enum BuildEvent {
    /// The build script is running, and waits until this is dropped.
    Running(UnixStream),
    Exited(Output),
}

#[test]
fn gc_skips_an_entry_that_a_real_cargo_build_is_in() {
    assert_gc_skips_an_entry_while_cargo_runs("build");
}

#[test]
fn gc_skips_an_entry_that_a_real_cargo_check_is_in() {
    assert_gc_skips_an_entry_while_cargo_runs("check");
}

fn assert_gc_skips_an_entry_while_cargo_runs(cargo_subcommand: &str) {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let entry = store.create_entry("entry", &[], -400 * DAY);
    let workspace_dir = env.create_workspace("workspace");
    // The build script holds the build up for as long as the test keeps the socket open.
    let build_script = "use std::{env, io::Read, os::unix::net::UnixStream};\n\
         fn main() {\n    \
             let socket = env::var_os(\"GC_TEST_SOCKET\").expect(\"the socket path is set\");\n    \
             let mut stream = UnixStream::connect(socket).expect(\"connected to the test\");\n    \
             stream.read_to_end(&mut Vec::new()).expect(\"waited for the test\");\n\
         }\n";
    fs::write(workspace_dir.join("build.rs"), build_script).expect("wrote build script");
    let socket_path = env.root().join("socket");
    let listener = UnixListener::bind(&socket_path).expect("bound socket");

    let cargo = env
        .cargo()
        .current_dir(&workspace_dir)
        .env("CARGO_TARGET_DIR", entry.join("target"))
        .env("GC_TEST_SOCKET", &socket_path)
        .args([cargo_subcommand, "--offline"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawned cargo");
    // Whichever comes first is heard of first, so a build that fails doesn't hang the test.
    let (sender, events) = mpsc::channel();
    let exit_sender = sender.clone();
    thread::spawn(move || {
        let output = cargo.wait_with_output().expect("waited for cargo");
        exit_sender.send(BuildEvent::Exited(output))
    });
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accepted the build script");
        sender.send(BuildEvent::Running(stream))
    });
    let build_script = match events.recv().expect("heard of the build") {
        BuildEvent::Running(stream) => stream,
        BuildEvent::Exited(output) => panic!(
            "cargo exited with {} before the build script ran, stderr was:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    };

    // With no grace, only Cargo's own lock keeps the entry.
    let no_grace = ["--orphan-grace", "0s"];
    assert_eq!(
        stdout_of_success(&run_gc(&env, &no_grace)),
        format!(
            "skipped `entry`: in use, Cargo holds the lock at `{entry}/target/debug/.cargo-lock`\n\
             removed 0 entries (0 B) and kept 1 entry: 1 in use\n"
        )
    );
    assert!(entry.join("target/debug/.cargo-lock").is_file());

    drop(build_script);
    match events.recv().expect("heard of the build") {
        BuildEvent::Exited(output) => assert!(
            output.status.success(),
            "cargo exited with {}, stderr was:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
        BuildEvent::Running(_) => panic!("the build script ran twice"),
    }
    let stdout = stdout_of_success(&run_gc(&env, &no_grace));
    assert!(
        stdout.starts_with("removed `entry` ("),
        "stdout was:\n{stdout}"
    );
    assert!(!entry.exists());
}

/// Which Cargo a test builds with.
#[derive(Clone, Copy, Debug)]
enum TestCargo {
    /// The Cargo that built the tests.
    Own,
    /// The Cargo of a rustup toolchain, which may not be installed.
    Rustup(&'static str),
}

impl TestCargo {
    fn command(self, env: &TestEnv) -> Command {
        match self {
            Self::Own => env.cargo(),
            Self::Rustup(toolchain) => {
                let mut command = env.confined_command("rustup");
                // `CARGO` names the tests' own Cargo, not this toolchain's.
                command
                    .env_remove("CARGO")
                    .args(["run", toolchain, "cargo"]);
                command
            }
        }
    }
}

/// Whether the source compiles when a test checks it again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Compile {
    Succeeds,
    Fails,
}

impl Compile {
    fn source(self) -> &'static str {
        match self {
            Self::Succeeds => "pub fn changed() {}\n",
            Self::Fails => "pub fn broken() -> u32 { \"no\" }\n",
        }
    }
}

#[test]
fn gc_dates_an_entry_by_a_real_cargo_check() {
    assert_gc_dates_an_entry_by_a_cargo_check(TestCargo::Own, Compile::Succeeds);
}

#[test]
fn gc_dates_an_entry_by_a_real_cargo_check_that_fails() {
    assert_gc_dates_an_entry_by_a_cargo_check(TestCargo::Own, Compile::Fails);
}

#[test]
#[ignore = "needs the stable toolchain of rustup"]
fn gc_dates_an_entry_by_checks_with_stable_cargo() {
    for second in [Compile::Succeeds, Compile::Fails] {
        assert_gc_dates_an_entry_by_a_cargo_check(TestCargo::Rustup("stable"), second);
    }
}

#[test]
#[ignore = "needs the beta toolchain of rustup"]
fn gc_dates_an_entry_by_checks_with_beta_cargo() {
    for second in [Compile::Succeeds, Compile::Fails] {
        assert_gc_dates_an_entry_by_a_cargo_check(TestCargo::Rustup("beta"), second);
    }
}

fn assert_gc_dates_an_entry_by_a_cargo_check(cargo: TestCargo, second: Compile) {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let gone_link = env.root().join("workspaces/gone/target");
    let entry = store.create_entry("entry", &[&gone_link], -400 * DAY);
    let workspace_dir = env.create_workspace("workspace");
    let check = |expected: Compile| {
        let output = cargo
            .command(&env)
            .current_dir(&workspace_dir)
            .env("CARGO_TARGET_DIR", entry.join("target"))
            .args(["check", "--offline"])
            .output()
            .expect("ran cargo");
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Any failure other than the source's type error is neither.
        let compile = if output.status.success() {
            Some(Compile::Succeeds)
        } else if stderr.contains("error[E0308]") {
            Some(Compile::Fails)
        } else {
            None
        };
        assert_eq!(
            compile,
            Some(expected),
            "for {cargo:?}, cargo exited with {}, stderr was:\n{stderr}",
            output.status
        );
    };
    let kept = |age: &str| {
        format!(
            "would keep `entry`: orphaned, last built {age} ago; \
             backlinks: `{gone_link}` (missing)\n\
             would remove 0 entries (0 B) and keep 1 entry: 1 orphaned within grace\n"
        )
    };

    check(Compile::Succeeds);
    // Backdated, so that only what the next check changes is recent.
    let long_ago = store.now - TimeDelta::seconds(30 * DAY + 12 * HOUR);
    set_dir_times(&entry.join("target"), SystemTime::from(long_ago));
    assert_eq!(
        stdout_of_success(&run_dry_gc(&env, &["--orphan-grace", "40d"])),
        kept("30d")
    );

    let mut source =
        fs::File::create(workspace_dir.join("src/lib.rs")).expect("opened source file");
    source
        .write_all(second.source().as_bytes())
        .expect("wrote source file");
    // Newer than the first check, whatever the filesystem's timestamp granularity.
    source
        .set_modified(SystemTime::from(store.now + TimeDelta::seconds(HOUR)))
        .expect("set modification time");
    check(second);
    // The age shown depends on timing, so only the text around it is compared.
    let stdout = stdout_of_success(&run_dry_gc(&env, &["--orphan-grace", "1h"]));
    let kept_recently = kept("AGE");
    let (before_age, after_age) = kept_recently
        .split_once("AGE")
        .expect("the line has an age");
    assert!(
        stdout.starts_with(before_age) && stdout.ends_with(after_age),
        "stdout was:\n{stdout}"
    );
}

/// Sets the modification time of `dir` and of every directory under it.
fn set_dir_times(dir: &Utf8Path, modified: SystemTime) {
    for entry in dir.read_dir_utf8().expect("read dir") {
        let path = entry.expect("read dir entry").into_path();
        if path.symlink_metadata().expect("read metadata").is_dir() {
            set_dir_times(&path, modified);
        }
    }
    let dir = fs::File::open(dir).expect("opened dir");
    dir.set_modified(modified).expect("set modification time");
}

#[test]
fn gc_does_nothing_while_another_gc_runs() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = store.create_entry("orphan", &[], -400 * DAY);
    fs::create_dir_all(store.dir.join(".targo-trash/leftover")).expect("created leftover");
    let gc_lock_path = store.dir.join("gc.lock");
    fs::write(&gc_lock_path, "").expect("created the gc lock");

    // As a running gc holds it.
    let gc_lock = open_lock_file(&gc_lock_path);
    gc_lock.lock().expect("locked as gc does");
    let before = env.snapshot();
    let output = run_gc(&env, &[]);
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).as_ref(),
            String::from_utf8_lossy(&output.stderr).as_ref()
        ),
        (
            Some(GC_BUSY_EXIT_CODE),
            "",
            format!(
                "another targo gc is running on the store at `{}`: nothing was removed\n",
                store.dir
            )
            .as_str()
        )
    );
    assert_eq!(env.snapshot(), before, "nothing is changed");

    // A dry run changes nothing, so it doesn't have to wait its turn.
    let stdout = stdout_of_success(&run_dry_gc(&env, &[]));
    assert!(
        stdout.starts_with("would remove `orphan` ("),
        "stdout was:\n{stdout}"
    );
    assert_eq!(env.snapshot(), before, "nothing is changed");

    drop(gc_lock);
    let stdout = stdout_of_success(&run_gc(&env, &[]));
    assert!(
        stdout.starts_with("removed leftover `leftover` (")
            && stdout.contains("\nremoved `orphan` ("),
        "stdout was:\n{stdout}"
    );
    assert!(!orphan.exists());
}

#[test]
fn gc_removes_what_an_interrupted_run_left_in_the_trash() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let trash = store.dir.join(".targo-trash");
    // An entry that was moved, and one that was partly deleted as well.
    let moved = trash.join("moved");
    fs::create_dir_all(moved.join("target/debug")).expect("created leftover");
    write_synced(&moved.join("target-dir-metadata.json"), b"{}");
    write_synced(&moved.join("target/debug/built-file"), &[b'x'; 64 * 1024]);
    let partly_deleted = trash.join("partly-deleted");
    fs::create_dir_all(partly_deleted.join("target")).expect("created leftover");
    let (moved_size, partly_deleted_size) = (disk_usage(&moved), disk_usage(&partly_deleted));
    let entry_named_like_leftover = store.create_entry("moved", &[], -HOUR);
    let before = env.snapshot();

    let output = run_gc(&env, &[]);
    assert_eq!(
        stdout_of_success(&output),
        format!(
            "removed leftover `moved` ({}) from an earlier run\n\
             removed leftover `partly-deleted` ({}) from an earlier run\n\
             removed 0 entries (0 B) and kept 1 entry: 1 orphaned within grace\n",
            human_size(moved_size),
            human_size(partly_deleted_size),
        )
    );
    assert_eq!(names_in(&trash), [""; 0]);
    let after = env.snapshot();
    let (before, after) = (
        subtree(&before, &entry_named_like_leftover),
        subtree(&after, &entry_named_like_leftover),
    );
    assert!(
        !before.is_empty() && after == before,
        "only the trash is emptied"
    );
}

#[test]
fn gc_reports_an_entry_that_it_cannot_delete_and_carries_on() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let trash = store.dir.join(".targo-trash");
    // The older one, so it comes first.
    let stuck = store.create_entry("stuck", &[], -(30 * DAY + 12 * HOUR));
    let fine = store.create_entry("fine", &[], -(20 * DAY + 12 * HOUR));
    let fine_size = human_size(disk_usage(&fine));
    fs::create_dir(stuck.join("target/read-only")).expect("created dir");
    write_synced(&stuck.join("target/read-only/file"), b"");
    // Nothing can be removed from a directory without write permission.
    let Some(read_only) = ReadOnlyDirs::new(&env, &stuck.join("target/read-only")) else {
        return;
    };

    let output = run_gc(&env, &[]);
    let trash_names = names_in(&trash);
    let [trash_name] = trash_names.as_slice() else {
        panic!("the trash has {trash_names:?} in it");
    };
    let left_dir = trash.join(trash_name);
    let cannot_delete = format!(
        "could not delete `{left_dir}/target/read-only/file`: Permission denied (os error 13); \
         what is left is in `{left_dir}`, where the next gc run tries again\n"
    );
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).as_ref(),
            String::from_utf8_lossy(&output.stderr).as_ref()
        ),
        (
            Some(1),
            format!(
                "removed `fine` ({fine_size}): orphaned, last used 20d ago; no backlinks\n\
                 removed 1 entry ({fine_size}), failed to remove 1 entry, and kept 0 entries\n"
            )
            .as_str(),
            format!("failed to remove `stuck`: {cannot_delete}").as_str()
        )
    );
    assert!(!stuck.exists() && !fine.exists(), "neither is in the store");
    assert_eq!(names_in(&left_dir), ["target"]);
    assert_eq!(names_in(&left_dir.join("target")), ["read-only"]);

    // While it can't be deleted, each run fails.
    let output = run_gc(&env, &[]);
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).as_ref(),
            String::from_utf8_lossy(&output.stderr).as_ref()
        ),
        (
            Some(1),
            "removed 0 entries (0 B) and kept 0 entries\n",
            format!("failed to remove leftover `{trash_name}`: {cannot_delete}").as_str()
        )
    );

    drop(read_only);
    let stdout = stdout_of_success(&run_gc(&env, &[]));
    assert!(
        stdout.starts_with(&format!("removed leftover `{trash_name}` ("))
            && stdout
                .ends_with(") from an earlier run\nremoved 0 entries (0 B) and kept 0 entries\n"),
        "stdout was:\n{stdout}"
    );
    assert_eq!(names_in(&trash), [""; 0]);
}

#[test]
fn gc_deletes_nothing_under_a_mount_point_on_the_store_filesystem() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let entry = store.create_entry("entry", &[], -400 * DAY);
    let mount_point = entry.join("target/mounted");
    fs::create_dir(&mount_point).expect("created dir");
    let same_filesystem_file = env.root().join("bind-mounted/file");
    let same_filesystem_dir = same_filesystem_file.parent().expect("file has a parent");
    fs::create_dir(same_filesystem_dir).expect("created dir");
    fs::write(&same_filesystem_file, "").expect("wrote file");

    if !env.can_bind_mount_in_namespace(same_filesystem_dir, &mount_point) {
        return;
    }

    let targo = env!("CARGO_BIN_EXE_targo");
    let output = env
        .command_in_namespace_with_bind_mount(same_filesystem_dir, &mount_point, targo)
        .arg("gc")
        .output()
        .expect("ran targo");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.code() == Some(1)
            && stderr.starts_with("failed to remove `entry`: could not delete `")
            && stderr.contains("/target/mounted`: "),
        "targo exited with {}, stderr was:\n{stderr}",
        output.status
    );
    assert!(
        same_filesystem_file.is_file(),
        "gc stays out of a mount point, even one on the store's filesystem"
    );
}

#[test]
fn gc_stops_between_entries_when_stdout_is_closed() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let older = store.create_entry("older", &[], -(30 * DAY + 12 * HOUR));
    let newer = store.create_entry("newer", &[], -(20 * DAY + 12 * HOUR));
    // Closed before targo starts, as when `head` has already exited.
    let (reader, writer) = io::pipe().expect("created pipe");
    drop(reader);

    let output = env
        .targo()
        .arg("gc")
        .stdout(writer)
        .output()
        .expect("ran targo");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.is_empty(),
        "targo exited with {}, stderr was:\n{stderr}",
        output.status
    );
    // The entry that was under way is finished, and no other is started.
    assert!(!older.exists() && newer.exists());
    assert_eq!(names_in(&store.dir.join(".targo-trash")), [""; 0]);
}

#[test]
fn wrap_cargo_recreates_an_entry_that_gc_removed() {
    let env = TestEnv::new();
    let original_dir = env.create_workspace("original");
    let copy_dir = env.create_workspace("copy");
    run_wrap_cargo(&env, &original_dir);

    // As when a workspace is copied with its `target` symlink: nothing records the copy.
    let copy_link = copy_dir.join("target");
    let link_dest = original_dir
        .join("target")
        .read_link_utf8()
        .expect("target is a symlink");
    symlink(&link_dest, &copy_link).expect("created symlink");
    fs::remove_dir_all(&original_dir).expect("removed the original workspace");
    let entry_dir = link_dest.parent().expect("link destination has a parent");
    let built_file = link_dest.join("built-file");
    fs::write(&built_file, "").expect("wrote file");

    let no_grace = ["--orphan-grace", "0s"];
    let stdout = stdout_of_success(&run_gc(&env, &no_grace));
    assert!(
        stdout.starts_with("removed `") && stdout.ends_with(") and kept 0 entries\n"),
        "stdout was:\n{stdout}"
    );
    assert!(!entry_dir.exists(), "the entry is gone");
    assert!(
        copy_link.is_symlink() && !copy_link.exists(),
        "the link dangles"
    );

    run_wrap_cargo(&env, &copy_dir);
    assert_eq!(
        copy_link.read_link_utf8().expect("target is a symlink"),
        link_dest,
        "the link is kept as it was"
    );
    assert!(
        link_dest.is_dir() && !built_file.exists(),
        "the entry is new"
    );
    let metadata = fs::read_to_string(entry_dir.join("target-dir-metadata.json"))
        .expect("read entry metadata");
    let metadata: serde_json::Value = serde_json::from_str(&metadata).expect("parsed JSON");
    assert_eq!(metadata["backlinks"], serde_json::json!([copy_link]));
    assert_eq!(
        stdout_of_success(&run_gc(&env, &no_grace)),
        "removed 0 entries (0 B) and kept 1 entry: 1 live\n"
    );
}

#[test]
fn gc_honors_orphan_grace() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let gone_link = env.root().join("workspaces/gone/target");
    let entry = store.create_entry("orphan", &[&gone_link], -(HOUR + HOUR / 2));
    let size = human_size(disk_usage(&entry));
    let reason = format!("orphaned, last used 1h ago; backlinks: `{gone_link}` (missing)");

    let kept = "would remove 0 entries (0 B) and keep 1 entry: 1 orphaned within grace\n";
    let removed = format!(
        "would remove `orphan` ({size}): {reason}\n\
         would remove 1 entry ({size}) and keep 0 entries\n"
    );
    let data: [(&[&str], &str); 3] = [
        (&[], kept),
        (&["--orphan-grace", "2h"], kept),
        (&["--orphan-grace", "1h"], &removed),
    ];
    for (args, expected) in data {
        let output = run_dry_gc(&env, args);
        assert_eq!(stdout_of_success(&output), expected, "for {args:?}");
    }

    let kept = "removed 0 entries (0 B) and kept 1 entry: 1 orphaned within grace\n";
    for args in [&[][..], &["--orphan-grace", "2h"]] {
        assert_eq!(stdout_of_success(&run_gc(&env, args)), kept, "for {args:?}");
        assert!(entry.exists(), "for {args:?}");
    }
    let output = run_gc(&env, &["--orphan-grace", "1h"]);
    assert_eq!(
        stdout_of_success(&output),
        format!(
            "removed `orphan` ({size}): {reason}\n\
             removed 1 entry ({size}) and kept 0 entries\n"
        )
    );
    assert!(!entry.exists());
}

/// A store with live entries of every age, and what gc makes of it with a maximum age of 2 days.
struct MaxAgeScenario {
    store: TestStore,
    /// The entries that gc empties, in order, each with the reason that gc gives.
    emptyings: Vec<(Utf8PathBuf, String)>,
    /// The only entry that gc removes, with the reason that gc gives.
    orphan: (Utf8PathBuf, String),
    /// The lines for entries that are kept, after the verb.
    kept_lines: Vec<String>,
    /// The live entries that gc leaves as they are.
    untouched_live: Vec<Utf8PathBuf>,
}

impl MaxAgeScenario {
    const MAX_AGE: [&'static str; 2] = ["--max-age", "2d"];

    fn new(env: &TestEnv) -> Self {
        let store = TestStore::new(env);
        let outside_file = env.root().join("outside-file");
        fs::write(&outside_file, "").expect("wrote file");

        // Everything in the target directory goes, whatever made it.
        let (stale, stale_link) = store.create_live_entry(env, "stale", -(60 * DAY + 12 * HOUR));
        fs::create_dir_all(stale.join("target/debug/deps")).expect("created dir");
        fs::create_dir_all(stale.join("target/rust-analyzer/flycheck0")).expect("created dir");
        write_synced(
            &stale.join("target/debug/deps/built-file"),
            &[b'x'; 64 * 1024],
        );
        write_synced(&stale.join("target/rust-analyzer/flycheck0/file"), b"x");
        write_synced(&stale.join("target/CACHEDIR.TAG"), b"x");
        write_synced(&stale.join("target/.rustc_info.json"), b"{}");
        symlink(&outside_file, stale.join("target/link")).expect("created symlink");
        store.create_build_dir(
            &stale,
            "target/debug",
            OutputLayout::Deps,
            -(45 * DAY + 12 * HOUR),
        );

        // The oldest, so it is emptied first even though its name sorts last.
        let (unbuilt, unbuilt_link) =
            store.create_live_entry(env, "unbuilt", -(90 * DAY + 12 * HOUR));
        fs::create_dir(unbuilt.join("target/doc")).expect("created dir");
        write_synced(&unbuilt.join("target/doc/index.html"), b"x");

        let (fresh, _) = store.create_live_entry(env, "fresh", -HOUR);
        write_synced(&fresh.join("target/built-file"), b"x");
        let (built_recently, _) = store.create_live_entry(env, "built-recently", -400 * DAY);
        store.create_build_dir(
            &built_recently,
            "target/debug",
            OutputLayout::Units,
            -(HOUR + HOUR / 2),
        );
        let (already_empty, _) = store.create_live_entry(env, "already-empty", -400 * DAY);

        let gone_link = env.root().join("workspaces/gone/target");
        let orphan = store.create_entry("orphan", &[&gone_link], -(8 * DAY + 12 * HOUR));
        write_synced(&orphan.join("target/built-file"), b"x");
        // Past the maximum age, which is for live entries only.
        let recent_orphan =
            store.create_entry("recent-orphan", &[&gone_link], -(3 * DAY + 12 * HOUR));
        write_synced(&recent_orphan.join("target/built-file"), b"x");

        let looping_link = env.root().join("workspaces/looping/target");
        let unknown = store.create_entry("unknown", &[&looping_link], -400 * DAY);
        write_synced(&unknown.join("target/built-file"), b"x");
        create_symlink(Utf8Path::new("target"), &looping_link);
        let loop_error =
            fs::metadata(&looping_link).expect_err("a looping symlink doesn't resolve");

        let no_metadata = store.dir.join("no-metadata");
        fs::create_dir_all(no_metadata.join("target")).expect("created dir");
        write_synced(&no_metadata.join("target/built-file"), b"x");

        Self {
            store,
            emptyings: vec![
                (
                    unbuilt,
                    format!("live, last used 90d ago; backlinks: `{unbuilt_link}` (live)"),
                ),
                (
                    stale,
                    format!("live, last built 45d ago; backlinks: `{stale_link}` (live)"),
                ),
            ],
            orphan: (
                orphan,
                format!("orphaned, last used 8d ago; backlinks: `{gone_link}` (missing)"),
            ),
            kept_lines: vec![
                "`no-metadata`: unrecognized; it has no `target-dir-metadata.json`".to_owned(),
                format!(
                    "`unknown`: backlink state unknown; backlinks: `{looping_link}` \
                     (unknown: {loop_error})"
                ),
            ],
            untouched_live: vec![fresh, built_recently, already_empty],
        }
    }

    /// The report of a run with the maximum age, with the verbs of a dry run or of a real one.
    fn report(&self, [remove, empty, keep]: [&str; 3], [and_empty, and_keep]: [&str; 2]) -> String {
        let mut emptied_lines = String::new();
        let mut emptied_size = 0;
        for (dir, reason) in &self.emptyings {
            let name = dir.file_name().expect("entry has a name");
            let size = contents_usage(&dir.join("target"));
            emptied_size += size;
            emptied_lines.push_str(&format!(
                "{empty} `{name}` ({}): {reason}\n",
                human_size(size)
            ));
        }
        let (orphan_dir, orphan_reason) = &self.orphan;
        let orphan_size = human_size(disk_usage(orphan_dir));
        let kept_lines: String = self
            .kept_lines
            .iter()
            .map(|line| format!("{keep} {line}\n"))
            .collect();
        format!(
            "{emptied_lines}{remove} `orphan` ({orphan_size}): {orphan_reason}\n{kept_lines}\
             {remove} 1 entry ({orphan_size}), {and_empty} 2 entries ({}), and {and_keep} \
             6 entries: 2 live, 1 already empty, 1 orphaned within grace, \
             1 with unknown backlinks, 1 unrecognized\n",
            human_size(emptied_size)
        )
    }
}

#[test]
fn gc_empties_stale_live_entries_and_touches_nothing_else() {
    let env = TestEnv::new();
    let scenario = MaxAgeScenario::new(&env);
    let store_dir = &scenario.store.dir;
    let expected = scenario.report(["removed", "emptied", "kept"], ["emptied", "kept"]);
    let emptied_targets: Vec<_> = scenario
        .emptyings
        .iter()
        .map(|(dir, _)| dir.join("target"))
        .collect();

    let before = env.snapshot();
    let output = run_gc(&env, &MaxAgeScenario::MAX_AGE);
    assert_eq!(stdout_of_success(&output), expected);

    // What is in the target directories is gone, and so is the orphan. The entries, their
    // metadata, and the target directories themselves stay.
    let mut expected = before;
    expected.retain(|path, _| {
        let in_emptied_target = emptied_targets
            .iter()
            .any(|target| path.starts_with(target) && path != target);
        !in_emptied_target && !path.starts_with(&scenario.orphan.0)
    });
    let mut after = env.snapshot();
    // Taking things out of a directory changes when it was modified.
    for snapshot in [&mut expected, &mut after] {
        for dir in emptied_targets.iter().chain([store_dir]) {
            match snapshot.remove(dir) {
                Some(Entry::Dir(_)) => {}
                entry => panic!("`{dir}` is {entry:?}"),
            }
        }
    }
    match after.remove(&store_dir.join("gc.lock")) {
        Some(Entry::File(_, contents)) => assert_eq!(contents, ""),
        entry => panic!("the gc lock is {entry:?}"),
    }
    match after.remove(&store_dir.join(".targo-trash")) {
        Some(Entry::Dir(_)) => {}
        entry => panic!("the trash is {entry:?}"),
    }
    assert_eq!(
        after, expected,
        "only the contents and the orphan are gone, and the trash is empty"
    );
    for (dir, _) in &scenario.emptyings {
        let name = dir.file_name().expect("entry has a name");
        let link = env.root().join("workspaces").join(name).join("target");
        assert!(
            link.is_symlink() && link.is_dir() && names_in(&link).is_empty(),
            "the link of `{name}` leads to its empty target directory"
        );
    }

    // What was emptied is still live and old, and a second run leaves it alone.
    let output = run_gc(&env, &MaxAgeScenario::MAX_AGE);
    assert_eq!(
        stdout_of_success(&output),
        format!(
            "{}removed 0 entries (0 B), emptied 0 entries (0 B), and kept 8 entries: 2 live, \
             3 already empty, 1 orphaned within grace, 1 with unknown backlinks, \
             1 unrecognized\n",
            scenario
                .kept_lines
                .iter()
                .map(|line| format!("kept {line}\n"))
                .collect::<String>(),
        )
    );
}

#[test]
fn gc_dry_run_reports_what_it_would_empty_and_changes_nothing() {
    let env = TestEnv::new();
    let scenario = MaxAgeScenario::new(&env);
    let expected = scenario.report(
        ["would remove", "would empty", "would keep"],
        ["empty", "keep"],
    );

    let before = env.snapshot();
    let output = run_dry_gc(&env, &MaxAgeScenario::MAX_AGE);
    assert_eq!(stdout_of_success(&output), expected);
    assert_eq!(env.snapshot(), before, "a dry run changes nothing");
}

#[test]
fn gc_empties_nothing_without_a_maximum_age() {
    let env = TestEnv::new();
    let scenario = MaxAgeScenario::new(&env);
    let (orphan_dir, orphan_reason) = &scenario.orphan;
    let orphan_size = human_size(disk_usage(orphan_dir));
    let report = |[remove, keep, and_keep]: [&str; 3]| {
        let kept_lines: String = scenario
            .kept_lines
            .iter()
            .map(|line| format!("{keep} {line}\n"))
            .collect();
        format!(
            "{remove} `orphan` ({orphan_size}): {orphan_reason}\n{kept_lines}\
             {remove} 1 entry ({orphan_size}) and {and_keep} 8 entries: 5 live, \
             1 orphaned within grace, 1 with unknown backlinks, 1 unrecognized\n"
        )
    };
    let dry_report = report(["would remove", "would keep", "keep"]);
    let real_report = report(["removed", "kept", "kept"]);

    let before = env.snapshot();
    assert_eq!(stdout_of_success(&run_dry_gc(&env, &[])), dry_report);
    assert_eq!(stdout_of_success(&run_gc(&env, &[])), real_report);
    let after = env.snapshot();
    let live_dirs = scenario.emptyings.iter().map(|(dir, _)| dir);
    for dir in live_dirs.chain(&scenario.untouched_live) {
        let (before, after) = (subtree(&before, dir), subtree(&after, dir));
        assert!(
            !before.is_empty() && after == before,
            "`{dir}` is untouched"
        );
    }
}

#[test]
fn gc_takes_the_maximum_age_from_the_environment() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let (entry, link) = store.create_live_entry(&env, "entry", -(60 * DAY + 12 * HOUR));
    write_synced(&entry.join("target/built-file"), b"x");
    let size = human_size(contents_usage(&entry.join("target")));
    let run = |max_age_env: Option<&str>, args: &[&str]| {
        let mut targo = env.targo();
        if let Some(max_age) = max_age_env {
            targo.env("TARGO_GC_MAX_AGE", max_age);
        }
        targo.arg("gc").args(args).output().expect("ran targo")
    };

    let orphans_only = "would remove 0 entries (0 B) and keep 1 entry: 1 live\n".to_owned();
    let kept = "would remove 0 entries (0 B), empty 0 entries (0 B), and keep 1 entry: 1 live\n"
        .to_owned();
    let emptied = format!(
        "would empty `entry` ({size}): live, last used 60d ago; backlinks: `{link}` (live)\n\
         would remove 0 entries (0 B), empty 1 entry ({size}), and keep 0 entries\n"
    );
    // Each case is the variable, the arguments, and the report of a dry run.
    let data: [(Option<&str>, &[&str], &String); 6] = [
        (None, &[], &orphans_only),
        (Some("30d"), &[], &emptied),
        (Some("90d"), &[], &kept),
        // The argument wins.
        (Some("90d"), &["--max-age", "30d"], &emptied),
        (Some("30d"), &["--max-age", "90d"], &kept),
        (None, &["--max-age", "30d"], &emptied),
    ];
    let before = env.snapshot();
    for (max_age_env, args, expected) in data {
        let output = run(max_age_env, &[&["--dry-run"], args].concat());
        assert_eq!(
            &stdout_of_success(&output),
            expected,
            "for {max_age_env:?} and {args:?}"
        );
    }

    // A value that is no duration is an error, not a run without a maximum age.
    let zero_problem = "`0` needs a time unit: `0s` empties every entry that is not being built \
                        in; to empty none, leave `--max-age` and `TARGO_GC_MAX_AGE` unset";
    let problems = [
        ("", "value was empty"),
        ("soon", "expected number at 0"),
        // It reads as "off", and as a duration it would empty every entry.
        ("0", zero_problem),
    ];
    for (value, problem) in problems {
        for mode_args in MODE_ARGS {
            let output = run(Some(value), mode_args);
            assert_eq!(
                (
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout).as_ref(),
                    String::from_utf8_lossy(&output.stderr).as_ref()
                ),
                (
                    Some(2),
                    "",
                    format!(
                        "error: invalid value '{value}' for '--max-age <DURATION>': {problem}\n\n\
                         For more information, try '--help'.\n"
                    )
                    .as_str()
                ),
                "for {value:?} and {mode_args:?}"
            );
        }
    }
    assert_eq!(env.snapshot(), before, "nothing is changed");

    let output = run(Some("30d"), &[]);
    assert_eq!(
        stdout_of_success(&output),
        format!(
            "emptied `entry` ({size}): live, last used 60d ago; backlinks: `{link}` (live)\n\
             removed 0 entries (0 B), emptied 1 entry ({size}), and kept 0 entries\n"
        )
    );
    assert_eq!(names_in(&entry.join("target")), [""; 0]);
}

#[test]
fn gc_skips_a_live_entry_that_cargo_is_building_in() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let (entry, link) = store.create_live_entry(&env, "entry", -(60 * DAY + 12 * HOUR));
    let lock_path = create_cargo_lock(&entry, "target/debug");
    write_synced(&entry.join("target/debug/built-file"), b"x");
    let size = human_size(contents_usage(&entry.join("target")));
    let max_age = ["--max-age", "30d"];

    // As Cargo holds it for the length of a build.
    let cargo_lock = open_lock_file(&lock_path);
    cargo_lock.lock().expect("locked as Cargo does");
    let before = env.snapshot();
    assert_eq!(
        stdout_of_success(&run_gc(&env, &max_age)),
        format!(
            "skipped `entry`: in use, Cargo holds the lock at `{lock_path}`\n\
             removed 0 entries (0 B), emptied 0 entries (0 B), and kept 1 entry: 1 in use\n"
        )
    );
    let after = env.snapshot();
    let (before, after) = (subtree(&before, &entry), subtree(&after, &entry));
    assert!(
        !before.is_empty() && after == before,
        "the entry is untouched"
    );

    drop(cargo_lock);
    assert_eq!(
        stdout_of_success(&run_gc(&env, &max_age)),
        format!(
            "emptied `entry` ({size}): live, last used 60d ago; backlinks: `{link}` (live)\n\
             removed 0 entries (0 B), emptied 1 entry ({size}), and kept 0 entries\n"
        )
    );
    assert_eq!(names_in(&entry.join("target")), [""; 0]);
}

#[test]
fn gc_finishes_an_emptying_that_was_interrupted() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let (entry, link) = store.create_live_entry(&env, "entry", -(60 * DAY + 12 * HOUR));
    // As a run leaves things if it dies after moving `debug` and before moving `release`.
    let moved = store.dir.join(".targo-trash/interrupted");
    fs::create_dir_all(moved.join("debug")).expect("created leftover");
    write_synced(&moved.join("debug/built-file"), &[b'x'; 64 * 1024]);
    fs::create_dir(entry.join("target/release")).expect("created dir");
    write_synced(&entry.join("target/release/built-file"), b"x");
    let moved_size = human_size(disk_usage(&moved));
    let size = human_size(contents_usage(&entry.join("target")));
    assert!(link.is_dir(), "the link leads to the target directory");

    assert_eq!(
        stdout_of_success(&run_gc(&env, &["--max-age", "30d"])),
        format!(
            "removed leftover `interrupted` ({moved_size}) from an earlier run\n\
             emptied `entry` ({size}): live, last used 60d ago; backlinks: `{link}` (live)\n\
             removed 0 entries (0 B), emptied 1 entry ({size}), and kept 0 entries\n"
        )
    );
    assert_eq!(names_in(&store.dir.join(".targo-trash")), [""; 0]);
    assert_eq!(names_in(&entry), ["target", "target-dir-metadata.json"]);
    assert!(link.is_dir() && names_in(&link).is_empty());
}

#[test]
fn gc_reports_an_entry_that_it_cannot_empty_and_carries_on() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    // The older one, so it comes first.
    let (stuck, _) = store.create_live_entry(&env, "stuck", -(90 * DAY + 12 * HOUR));
    let (fine, fine_link) = store.create_live_entry(&env, "fine", -(60 * DAY + 12 * HOUR));
    write_synced(&fine.join("target/built-file"), b"x");
    let fine_size = human_size(contents_usage(&fine.join("target")));
    write_synced(&stuck.join("target/built-file"), b"x");
    let stuck_dir = stuck.join("target/read-only");
    fs::create_dir(&stuck_dir).expect("created dir");
    write_synced(&stuck_dir.join("file"), b"");
    // A directory can't be moved to another parent without write permission on it.
    let Some(_read_only) = ReadOnlyDirs::new(&env, &stuck_dir) else {
        return;
    };

    let output = run_gc(&env, &["--max-age", "30d"]);
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).as_ref(),
            String::from_utf8_lossy(&output.stderr).as_ref()
        ),
        (
            Some(1),
            format!(
                "emptied `fine` ({fine_size}): live, last used 60d ago; \
                 backlinks: `{fine_link}` (live)\n\
                 removed 0 entries (0 B), emptied 1 entry ({fine_size}), \
                 failed to empty 1 entry, and kept 0 entries\n"
            )
            .as_str(),
            format!(
                "failed to empty `stuck`: could not move `{stuck_dir}`: Permission denied \
                 (os error 13); the rest of the target directory was deleted\n"
            )
            .as_str()
        )
    );
    assert_eq!(names_in(&stuck.join("target")), ["read-only"]);
    assert_eq!(names_in(&stuck_dir), ["file"]);
    assert_eq!(names_in(&store.dir.join(".targo-trash")), [""; 0]);
}

#[test]
fn gc_reports_contents_that_it_cannot_delete_and_tries_again() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let trash = store.dir.join(".targo-trash");
    let (entry, link) = store.create_live_entry(&env, "entry", -(60 * DAY + 12 * HOUR));
    let stuck_dir = entry.join("target/debug/read-only");
    fs::create_dir_all(&stuck_dir).expect("created dir");
    write_synced(&stuck_dir.join("file"), b"");
    // `debug` can be moved, but nothing can be removed from the directory in it.
    let Some(read_only) = ReadOnlyDirs::new(&env, &stuck_dir) else {
        return;
    };
    let max_age = ["--max-age", "30d"];

    let output = run_gc(&env, &max_age);
    let trash_names = names_in(&trash);
    let [trash_name] = trash_names.as_slice() else {
        panic!("the trash has {trash_names:?} in it");
    };
    let left_dir = trash.join(trash_name);
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).as_ref(),
            String::from_utf8_lossy(&output.stderr).as_ref()
        ),
        (
            Some(1),
            "removed 0 entries (0 B), emptied 0 entries (0 B), failed to empty 1 entry, \
             and kept 0 entries\n",
            format!(
                "failed to empty `entry`: could not delete `{left_dir}/debug/read-only/file`: \
                 Permission denied (os error 13); what is left is in `{left_dir}`, where the \
                 next gc run tries again\n"
            )
            .as_str()
        )
    );
    assert!(link.is_dir() && names_in(&link).is_empty());
    assert_eq!(names_in(&left_dir.join("debug")), ["read-only"]);

    drop(read_only);
    let stdout = stdout_of_success(&run_gc(&env, &max_age));
    assert!(
        stdout.starts_with(&format!("removed leftover `{trash_name}` ("))
            && stdout.ends_with(
                ") from an earlier run\nremoved 0 entries (0 B), emptied 0 entries (0 B), \
                 and kept 1 entry: 1 already empty\n"
            ),
        "stdout was:\n{stdout}"
    );
    assert_eq!(names_in(&trash), [""; 0]);
}

#[test]
fn cargo_rebuilds_in_an_entry_that_gc_emptied() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    let link = workspace_dir.join("target");
    let check = |mut command: Command, args: &[&str]| {
        let output = command
            .current_dir(&workspace_dir)
            .args(args)
            .output()
            .expect("ran the command");
        assert!(
            output.status.success(),
            "{args:?} exited with {}, stderr was:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let assert_emptied = |when: &str| {
        // The age and the size depend on timing and on the filesystem.
        let stdout = stdout_of_success(&run_gc(&env, &["--max-age", "0s"]));
        assert!(
            stdout.starts_with("emptied `")
                && stdout.contains("): live, last ")
                && stdout.ends_with("), and kept 0 entries\n"),
            "{when}, stdout was:\n{stdout}"
        );
        assert!(
            link.is_symlink() && link.is_dir() && names_in(&link).is_empty(),
            "{when}, the link leads to an empty target directory"
        );
    };

    check(env.targo(), &["wrap-cargo", "check", "--offline"]);
    let link_dest = link.read_link_utf8().expect("target is a symlink");
    assert!(names_in(&link).contains(&"debug".to_owned()));
    assert_emptied("after a check through targo");

    // As rust-analyzer and scripts run it: Cargo itself, through the link.
    check(env.cargo(), &["check", "--offline"]);
    assert!(names_in(&link).contains(&"debug".to_owned()));
    // With a last use long ago, only that build shows that the entry is in use.
    let entry = link_dest.parent().expect("the link leads into an entry");
    let metadata_path = entry.join("target-dir-metadata.json");
    let metadata = fs::read_to_string(&metadata_path).expect("read metadata");
    let mut metadata: serde_json::Value = serde_json::from_str(&metadata).expect("parsed metadata");
    metadata["last-used"] = "2020-01-01T00:00:00+00:00".into();
    write_synced(&metadata_path, metadata.to_string().as_bytes());
    assert_eq!(
        stdout_of_success(&run_gc(&env, &["--max-age", "1h"])),
        "removed 0 entries (0 B), emptied 0 entries (0 B), and kept 1 entry: 1 live\n",
        "an emptied entry that Cargo alone built in again is not emptied again"
    );
    assert_emptied("after a check with Cargo alone");

    check(env.targo(), &["wrap-cargo", "check", "--offline"]);
    assert!(names_in(&link).contains(&"debug".to_owned()));
    assert_eq!(
        link.read_link_utf8().expect("target is a symlink"),
        link_dest,
        "the link is kept as it was"
    );
    assert_eq!(
        stdout_of_success(&run_gc(&env, &["--max-age", "1h"])),
        "removed 0 entries (0 B), emptied 0 entries (0 B), and kept 1 entry: 1 live\n"
    );
}

#[test]
fn gc_dry_run_follows_the_link_that_wrap_cargo_makes() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    run_wrap_cargo(&env, &workspace_dir);

    // With no grace, only the link keeps the entry.
    let no_grace = ["--orphan-grace", "0s"];
    assert_eq!(
        stdout_of_success(&run_dry_gc(&env, &no_grace)),
        "would remove 0 entries (0 B) and keep 1 entry: 1 live\n"
    );

    fs::remove_file(workspace_dir.join("target")).expect("removed the link");
    let stdout = stdout_of_success(&run_dry_gc(&env, &no_grace));
    // The age and the size in between depend on timing and on the filesystem.
    assert!(
        stdout.starts_with("would remove `")
            && stdout.contains("(missing)\nwould remove 1 entry (")
            && stdout.ends_with(") and keep 0 entries\n"),
        "stdout was:\n{stdout}"
    );
}

#[test]
fn gc_sees_live_entry_through_store_alias() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    // As when the store path is a symlink to another disk.
    let store_alias = env.root().join("store-alias");
    symlink(&store.dir, &store_alias).expect("created store symlink");

    let real_link = env.root().join("workspaces/real/target");
    let alias_link = env.root().join("workspaces/alias/target");
    store.create_entry("real", &[&real_link], -400 * DAY);
    store.create_entry("alias", &[&alias_link], -400 * DAY);
    create_symlink(&store.dir.join("real/target"), &real_link);
    create_symlink(&store_alias.join("alias/target"), &alias_link);
    let orphan = store.create_entry("orphan", &[], -400 * DAY);
    let size = human_size(disk_usage(&orphan));

    let run = |store_dir: &Utf8Path, mode_args: &[&str]| {
        let mut targo = env.targo();
        let output = targo
            .env("TARGO_STORE_DIR", store_dir)
            .arg("gc")
            .args(mode_args);
        stdout_of_success(&output.output().expect("ran targo"))
    };
    for store_dir in [&store.dir, &store_alias] {
        assert_eq!(
            run(store_dir, &["--dry-run"]),
            format!(
                "would remove `orphan` ({size}): orphaned, last used 400d ago; no backlinks\n\
                 would remove 1 entry ({size}) and keep 2 entries: 2 live\n"
            ),
            "for the store at `{store_dir}`"
        );
    }
    assert_eq!(
        run(&store_alias, &[]),
        format!(
            "removed `orphan` ({size}): orphaned, last used 400d ago; no backlinks\n\
             removed 1 entry ({size}) and kept 2 entries: 2 live\n"
        ),
    );
    assert!(!orphan.exists());
}

#[test]
fn gc_does_not_create_a_store() {
    let env = TestEnv::new();
    let before = env.snapshot();

    for mode_args in MODE_ARGS {
        assert_eq!(
            stdout_of_success(&run_gc(&env, mode_args)),
            format!(
                "nothing to collect: there is no targo store at `{}`\n",
                env.store_dir()
            ),
            "for {mode_args:?}"
        );
    }

    let output = env
        .targo()
        .env_remove("TARGO_STORE_DIR")
        .args(["gc", "--dry-run"])
        .output();
    assert_eq!(
        stdout_of_success(&output.expect("ran targo")),
        format!(
            "nothing to collect: there is no targo store at `{}`\n",
            env.cargo_home().join("targo")
        ),
        "without the override, where only a dry run is safe to try"
    );

    assert_eq!(env.snapshot(), before, "nothing is created");
}

#[test]
fn gc_refuses_a_store_it_cannot_use() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let store_dir = &store.dir;
    // An entry that would be removed if the store were used.
    store.create_entry("orphan", &[], -400 * DAY);
    let metadata_path = store_dir.join("targo-metadata.json");
    fs::remove_file(&metadata_path).expect("removed store metadata");

    let data = [
        (
            None,
            format!(
                "`{store_dir}` is not a targo store: it has no `targo-metadata.json`, \
                 which `targo wrap-cargo` writes when it creates a store"
            ),
        ),
        (
            Some(r#"{"store-version":2,"min-version":"0.9.0"}"#),
            format!(
                "targo store directory at `{store_dir}` is too new (this version of targo \
                 supports up to store version 1, but metadata had version = 2) \
                 -- upgrade to targo version `0.9.0` or newer"
            ),
        ),
        (
            Some(r#"{"store-version":0,"min-version":"0.1.0"}"#),
            format!(
                "targo store directory at `{store_dir}` is from an older version of targo: \
                 run any Cargo command through `targo wrap-cargo` to upgrade it"
            ),
        ),
        (
            Some("{"),
            format!("failed to deserialize metadata from `{metadata_path}`"),
        ),
    ];
    for (metadata, expected) in data {
        if let Some(metadata) = metadata {
            fs::write(&metadata_path, metadata).expect("wrote store metadata");
        }
        let before = env.snapshot();

        for mode_args in MODE_ARGS {
            let output = run_gc(&env, mode_args);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.code() == Some(1) && stderr.contains(&expected),
                "for {metadata:?} and {mode_args:?}, expected failure with {expected:?}, \
                 stderr was:\n{stderr}"
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                "",
                "for {metadata:?} and {mode_args:?}, nothing is reported"
            );
            assert_eq!(
                env.snapshot(),
                before,
                "for {metadata:?} and {mode_args:?}, nothing is changed"
            );
        }
    }
}

#[test]
fn gc_rejects_bad_store_dir_override() {
    let env = TestEnv::new();
    let before = env.snapshot();

    let output = env
        .targo()
        .env("TARGO_STORE_DIR", "relative/store")
        .arg("gc")
        .output()
        .expect("ran targo");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success()
            && stderr.contains(
                "`TARGO_STORE_DIR` must be an absolute path, but is set to `relative/store`"
            ),
        "stderr was:\n{stderr}"
    );
    assert_eq!(env.snapshot(), before, "nothing is created");
}

#[test]
fn gc_dry_run_stops_quietly_when_stdout_is_closed() {
    let env = TestEnv::new();
    // Closed before targo starts, as when `head` has already exited.
    let (reader, writer) = io::pipe().expect("created pipe");
    drop(reader);

    let output = env
        .targo()
        .args(["gc", "--dry-run"])
        .stdout(writer)
        .output()
        .expect("ran targo");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.is_empty(),
        "targo exited with {}, stderr was:\n{stderr}",
        output.status
    );
}

#[test]
fn gc_dry_run_logs_to_stderr() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = store.create_entry("orphan", &[], -(8 * DAY + 12 * HOUR));
    // Gc logs what it can't measure. This is too deep for it to look for a build in.
    let unreadable = orphan.join("target/debug/deps/unreadable");
    fs::create_dir_all(&unreadable).expect("created dir");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
        .expect("removed permissions");
    let enforced = fs::read_dir(&unreadable).is_err();

    let quiet = run_dry_gc(&env, &[]);
    let logged = env
        .targo()
        .env("TARGO_LOG", "debug")
        .args(["gc", "--dry-run"])
        .output()
        .expect("ran targo");
    // Before the assertions: the temp dir can't be removed while this is unreadable.
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755))
        .expect("restored permissions");
    if !enforced {
        eprintln!("skipped: permissions are not enforced for this user");
        return;
    }

    let cannot_measure =
        format!("could not measure `{unreadable}`: Permission denied (os error 13)\n");
    let report = stdout_of_success(&quiet);
    assert!(report.contains(&cannot_measure), "stdout was:\n{report}");
    assert_eq!(
        (
            logged.status.code(),
            String::from_utf8_lossy(&logged.stdout).as_ref()
        ),
        (Some(0), report.as_str()),
        "the log is kept out of the report"
    );
    let stderr = String::from_utf8_lossy(&logged.stderr);
    assert!(
        stderr.contains(&format!(" DEBUG targo::gc: {cannot_measure}")),
        "the log is on stderr, which was:\n{stderr}"
    );
}

/// A store written by hand, so that entries can have any last-used time.
pub(crate) struct TestStore {
    pub(crate) dir: Utf8PathBuf,
    now: DateTime<Utc>,
}

/// Where rustc's outputs go in a build directory.
#[derive(Clone, Copy, Debug)]
enum OutputLayout {
    /// In `deps`, as Cargo has it up to 1.99.
    Deps,
    /// In a directory for each unit, as Cargo has it since 1.100.
    Units,
}

impl TestStore {
    pub(crate) fn new(env: &TestEnv) -> Self {
        let dir = env.store_dir();
        fs::create_dir(&dir).expect("created store dir");
        fs::write(
            dir.join("targo-metadata.json"),
            r#"{"store-version":1,"min-version":"0.1.0"}"#,
        )
        .expect("wrote store metadata");
        // As `wrap-cargo` leaves it.
        fs::write(dir.join("targo.lock"), "").expect("wrote store lock file");
        Self {
            dir,
            now: Utc::now(),
        }
    }

    /// Creates a build directory in an entry, last built `built_secs` after the store was
    /// created.
    fn create_build_dir(
        &self,
        entry_dir: &Utf8Path,
        build_dir: &str,
        layout: OutputLayout,
        built_secs: i64,
    ) {
        create_cargo_lock(entry_dir, build_dir);
        let output = match layout {
            OutputLayout::Deps => "deps",
            OutputLayout::Units => "build/package/0f0f0f0f0f0f0f0f/out",
        };
        let output = entry_dir.join(build_dir).join(output);
        fs::create_dir_all(&output).expect("created output dir");
        let built = SystemTime::from(self.now + TimeDelta::seconds(built_secs));
        let output = fs::File::open(&output).expect("opened output dir");
        output.set_modified(built).expect("set modification time");
    }

    /// Creates an entry that a workspace links to, and returns its directory and the link.
    pub(crate) fn create_live_entry(
        &self,
        env: &TestEnv,
        name: &str,
        last_used_secs: i64,
    ) -> (Utf8PathBuf, Utf8PathBuf) {
        let link = env.root().join("workspaces").join(name).join("target");
        let entry_dir = self.create_entry(name, &[&link], last_used_secs);
        create_symlink(&entry_dir.join("target"), &link);
        (entry_dir, link)
    }

    /// Creates an entry last used `last_used_secs` after the store was created.
    ///
    /// Half a unit away from a whole one, the age shown can't depend on how long the test takes.
    pub(crate) fn create_entry(
        &self,
        name: &str,
        backlinks: &[&Utf8Path],
        last_used_secs: i64,
    ) -> Utf8PathBuf {
        let entry_dir = self.dir.join(name);
        fs::create_dir_all(entry_dir.join("target")).expect("created entry dir");
        let last_used = self.now + TimeDelta::seconds(last_used_secs);
        // Targo records local time, so the offset is rarely zero.
        let offset = FixedOffset::east_opt(19_800).expect("+05:30 is a valid offset");
        let metadata = serde_json::json!({
            "backlinks": backlinks,
            "last-used": last_used.with_timezone(&offset).to_rfc3339(),
        });
        write_synced(
            &entry_dir.join("target-dir-metadata.json"),
            metadata.to_string().as_bytes(),
        );
        entry_dir
    }
}

/// Writes a file and syncs it, so that the size the filesystem reports for it is settled.
pub(crate) fn write_synced(path: &Utf8Path, contents: &[u8]) {
    let mut file = fs::File::create(path).expect("created file");
    file.write_all(contents).expect("wrote file");
    file.sync_all().expect("synced file");
}

pub(crate) fn create_symlink(dest: &Utf8Path, link: &Utf8Path) {
    fs::create_dir_all(link.parent().expect("link has a parent")).expect("created dir");
    symlink(dest, link).expect("created symlink");
}

/// The arguments that pick each of gc's modes: a dry run, and a real one.
const MODE_ARGS: [&[&str]; 2] = [&["--dry-run"], &[]];

fn run_dry_gc(env: &TestEnv, args: &[&str]) -> Output {
    run_gc(env, &[&["--dry-run"], args].concat())
}

fn run_gc(env: &TestEnv, args: &[&str]) -> Output {
    env.targo()
        .arg("gc")
        .args(args)
        .output()
        .expect("ran targo")
}

/// Runs `targo wrap-cargo version` in `workspace_dir`, which must succeed.
fn run_wrap_cargo(env: &TestEnv, workspace_dir: &Utf8Path) {
    let output = env
        .targo()
        .current_dir(workspace_dir)
        .args(["wrap-cargo", "version"])
        .output()
        .expect("ran targo");
    assert!(
        output.status.success(),
        "targo exited with {}, stderr was:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The part of `snapshot` that is at or under `dir`.
fn subtree<'a>(
    snapshot: &'a BTreeMap<Utf8PathBuf, Entry>,
    dir: &Utf8Path,
) -> BTreeMap<&'a Utf8PathBuf, &'a Entry> {
    snapshot
        .iter()
        .filter(|(path, _)| path.starts_with(dir))
        .collect()
}

pub(crate) fn names_in(dir: &Utf8Path) -> Vec<String> {
    let mut names: Vec<_> = dir
        .read_dir_utf8()
        .expect("read directory")
        .map(|entry| entry.expect("read entry").file_name().to_owned())
        .collect();
    names.sort();
    names
}

/// Creates a `.cargo-lock` in the build directory `build_dir` of an entry, and returns its path.
fn create_cargo_lock(entry_dir: &Utf8Path, build_dir: &str) -> Utf8PathBuf {
    let lock_path = entry_dir.join(build_dir).join(".cargo-lock");
    fs::create_dir_all(entry_dir.join(build_dir)).expect("created build dir");
    write_synced(&lock_path, b"");
    lock_path
}

/// Directories named `read-only` that nothing can be removed from, until this is dropped.
///
/// Gc moves them, so they are found again by name.
pub(crate) struct ReadOnlyDirs(Utf8PathBuf);

impl ReadOnlyDirs {
    /// Returns `None` if permissions are not enforced, as when running as root.
    pub(crate) fn new(env: &TestEnv, dir: &Utf8Path) -> Option<Self> {
        assert_eq!(dir.file_name(), Some("read-only"));
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555))
            .expect("removed write permission");
        let read_only = Self(env.root().to_owned());
        let probe = dir.join("probe");
        match fs::write(&probe, "") {
            Ok(()) => {
                fs::remove_file(&probe).expect("removed probe");
                eprintln!("skipped: permissions are not enforced for this user");
                None
            }
            Err(_) => Some(read_only),
        }
    }
}

impl Drop for ReadOnlyDirs {
    fn drop(&mut self) {
        // Otherwise the temp dir can't be removed.
        restore_read_only_dirs(&self.0);
    }
}

fn restore_read_only_dirs(dir: &Utf8Path) {
    for entry in dir.read_dir_utf8().expect("read dir") {
        let path = entry.expect("read dir entry").into_path();
        if path.symlink_metadata().expect("read metadata").is_dir() {
            if path.file_name() == Some("read-only") {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                    .expect("restored permissions");
            }
            restore_read_only_dirs(&path);
        }
    }
}

fn stdout_of_success(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.is_empty(),
        "targo exited with {}, stderr was:\n{stderr}",
        output.status
    );
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

/// The disk usage of a tree that has no hard links in it.
fn disk_usage(path: &Utf8Path) -> u64 {
    let metadata = path.symlink_metadata().expect("read metadata");
    let mut bytes = metadata.blocks() * 512;
    if metadata.is_dir() {
        for entry in path.read_dir_utf8().expect("read dir") {
            bytes += disk_usage(entry.expect("read dir entry").path());
        }
    }
    bytes
}

/// The disk usage of what is in `dir`, which is a tree that has no hard links in it.
fn contents_usage(dir: &Utf8Path) -> u64 {
    disk_usage(dir) - dir.symlink_metadata().expect("read metadata").blocks() * 512
}

/// Formats a size as targo does. Only for multiples of 512 bytes below 1 MiB.
fn human_size(bytes: u64) -> String {
    assert!(
        bytes.is_multiple_of(512) && bytes < 1024 * 1024,
        "{bytes} bytes is a size that this can format"
    );
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{}.{} KiB", bytes / 1024, (bytes % 1024) / 512 * 5)
    }
}
