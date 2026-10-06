use crate::{
    gc::{create_symlink, names_in, write_synced, ReadOnlyDirs, TestStore, DAY, HOUR},
    support::{open_lock_file, TestEnv},
    wrap_cargo::run_wrap_cargo,
};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use std::{
    fs,
    io::{self, BufRead, BufReader},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::Path,
    process::{Command, Output, Stdio},
};

/// A targo command with automatic gc at its default, which is on; `TestEnv` turns it off.
fn auto_targo(env: &TestEnv) -> Command {
    let mut command = env.targo();
    command.env_remove("TARGO_AUTO_GC");
    command
}

fn state_root(env: &TestEnv) -> Utf8PathBuf {
    env.root().join("home/.local/state/targo")
}

/// Found by listing, since the name is an encoding of the store's path.
fn state_dir(env: &TestEnv) -> Option<Utf8PathBuf> {
    let root = state_root(env);
    if !root.exists() {
        return None;
    }
    let names = names_in(&root);
    let [name] = names.as_slice() else {
        panic!("`{root}` has {names:?} in it, and not one directory");
    };
    Some(root.join(name))
}

fn log_path(env: &TestEnv) -> Utf8PathBuf {
    state_dir(env).expect("there is a state dir").join("gc.log")
}

fn read_log(env: &TestEnv) -> String {
    fs::read_to_string(log_path(env)).expect("read the gc log")
}

fn state_path(env: &TestEnv) -> Utf8PathBuf {
    state_dir(env)
        .expect("there is a state dir")
        .join("gc-state.json")
}

fn read_state(env: &TestEnv) -> serde_json::Value {
    let state = fs::read_to_string(state_path(env)).expect("read the state file");
    serde_json::from_str(&state).expect("parsed JSON")
}

/// Blocks until the background run exits: until then, it holds the log locked.
fn wait_for_background_gc(env: &TestEnv) {
    wait_for_unlock(log_path(env).as_std_path());
}

fn wait_for_unlock(log_path: &Path) {
    open_lock_file(log_path)
        .lock()
        .expect("locked the gc log once the run was over");
}

/// For a run that starts a background gc, which must be over before the temp dir is removed.
fn wrap_cargo_and_wait(env: &TestEnv, command: &mut Command, workspace_dir: &Utf8Path) -> Output {
    let output = run_wrap_cargo(command, workspace_dir);
    wait_for_background_gc(env);
    output
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

fn started_of(state: &serde_json::Value) -> DateTime<Utc> {
    state["last-run"]["started"]
        .as_str()
        .expect("the start time is a string")
        .parse()
        .expect("parsed the start time")
}

fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn run_headers(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|line| line.starts_with("--- "))
        .collect()
}

/// Bytes that a filesystem that compresses can't store in less than their length.
fn incompressible(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[3]
        })
        .collect()
}

/// An orphan past the grace period, large enough for its removal to be worth a notice.
fn create_large_orphan(store: &TestStore) -> Utf8PathBuf {
    let orphan = store.create_entry("orphan", &[], -400 * DAY);
    write_synced(
        &orphan.join("target/built-file"),
        &incompressible(2 * 1024 * 1024),
    );
    orphan
}

fn set_last_run(env: &TestEnv, field: &str, value: serde_json::Value) {
    let mut state = read_state(env);
    state["last-run"][field] = value;
    fs::write(state_path(env), state.to_string()).expect("wrote the state file");
}

fn set_last_run_started(env: &TestEnv, started: DateTime<Utc>) {
    set_last_run(env, "started", serde_json::json!(started));
}

/// The id of a process, with those of its parent and of its process group.
#[derive(Debug, PartialEq, Eq)]
struct ProcessIds {
    pid: u32,
    parent: u32,
    group: u32,
}

/// Every process there is, zombies among them. Reads `/proc` as Linux lays it out.
#[cfg(target_os = "linux")]
fn all_processes() -> Vec<ProcessIds> {
    fs::read_dir("/proc")
        .expect("read /proc")
        .filter_map(|entry| {
            let dir = entry.ok()?.path();
            let pid = dir.file_name()?.to_str()?.parse().ok()?;
            // A process can exit while this runs, so one that can't be read is left out.
            let stat = fs::read_to_string(dir.join("stat")).ok()?;
            // The fields after the command name, which can itself hold spaces.
            let (_, fields) = stat.rsplit_once(')')?;
            let mut fields = fields.split_whitespace();
            let _state = fields.next()?;
            let parent = fields.next()?.parse().ok()?;
            let group = fields.next()?.parse().ok()?;
            Some(ProcessIds { pid, parent, group })
        })
        .collect()
}

/// Every process there is, zombies among them.
#[cfg(not(target_os = "linux"))]
fn all_processes() -> Vec<ProcessIds> {
    // One `-o` for each column: a header runs to the end of its argument.
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "pgid="])
        .stderr(Stdio::inherit())
        .output()
        .expect("ran ps");
    assert!(output.status.success(), "ps exited with {}", output.status);
    let stdout = String::from_utf8(output.stdout).expect("the output of ps is UTF-8");
    stdout
        .lines()
        .map(|line| {
            let ids: Option<Vec<u32>> = line.split_whitespace().map(|id| id.parse().ok()).collect();
            let Some(&[pid, parent, group]) = ids.as_deref() else {
                panic!("ps printed {line:?}, and not three ids");
            };
            ProcessIds { pid, parent, group }
        })
        .collect()
}

#[test]
fn wrap_cargo_runs_gc_in_the_background_and_reports_it_once() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = create_large_orphan(&store);
    let workspace_dir = env.create_workspace("workspace");

    let output = wrap_cargo_and_wait(&env, &mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "", "there is nothing to report yet");
    assert!(!orphan.exists(), "the background run removed the orphan");
    let link_dest = workspace_dir
        .join("target")
        .read_link_utf8()
        .expect("target is a symlink");
    assert!(link_dest.is_dir(), "the workspace's own entry is kept");

    let state = read_state(&env);
    let started = started_of(&state);
    assert_eq!(state["version"], 1);
    let end = &state["last-run"]["end"];
    assert_eq!(end["notice"], "pending");
    let collected = &end["result"]["collected"];
    assert_eq!(
        (
            &collected["removed"],
            &collected["emptied"],
            &collected["failed"]
        ),
        (
            &serde_json::json!(1),
            &serde_json::json!(0),
            &serde_json::json!(0)
        ),
        "the state is {state}"
    );

    let log = read_log(&env);
    let lines: Vec<_> = log.lines().collect();
    let [header, removed, summary] = lines.as_slice() else {
        panic!("the log was:\n{log}");
    };
    assert_eq!(
        *header,
        format!(
            "--- {}: gc started in the background by a Cargo command in `{workspace_dir}` ---",
            timestamp(started)
        )
    );
    assert!(
        removed.starts_with("removed `orphan` ("),
        "the log was:\n{log}"
    );
    let size = summary
        .strip_prefix("removed 1 entry (")
        .and_then(|rest| rest.strip_suffix(") and kept 1 entry: 1 live"))
        .unwrap_or_else(|| panic!("the log was:\n{log}"));

    let output = run_wrap_cargo(&mut auto_targo(&env), &workspace_dir);
    assert_eq!(
        stderr_of(&output),
        format!(
            "[targo] gc: the background run of {} removed 1 entry ({size}); see `{}`\n",
            timestamp(started),
            log_path(&env)
        )
    );
    let state = read_state(&env);
    assert_eq!(state["last-run"]["end"]["notice"], "shown");
    assert_eq!(started_of(&state), started, "no other run was started");
    assert_eq!(read_log(&env), log, "no other run was started");

    let output = run_wrap_cargo(&mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "", "the notice is shown once");
    assert_eq!(read_state(&env), state);
    assert_eq!(read_log(&env), log);
}

#[test]
fn wrap_cargo_runs_gc_again_a_day_later() {
    let env = TestEnv::new();
    let _store = TestStore::new(&env);
    let workspace_dir = env.create_workspace("workspace");

    wrap_cargo_and_wait(&env, &mut auto_targo(&env), &workspace_dir);
    assert_eq!(run_headers(&read_log(&env)).len(), 1);

    let almost_a_day_ago = Utc::now() - TimeDelta::hours(23);
    set_last_run_started(&env, almost_a_day_ago);
    let output = run_wrap_cargo(&mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "");
    assert_eq!(run_headers(&read_log(&env)).len(), 1, "no run is due");
    assert_eq!(started_of(&read_state(&env)), almost_a_day_ago);

    let over_a_day_ago = Utc::now() - TimeDelta::hours(25);
    set_last_run_started(&env, over_a_day_ago);
    let output = wrap_cargo_and_wait(&env, &mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "");
    assert_eq!(run_headers(&read_log(&env)).len(), 2, "a run was due");
    assert!(started_of(&read_state(&env)) > over_a_day_ago);
}

#[test]
fn wrap_cargo_does_not_run_gc_when_it_is_turned_off() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = create_large_orphan(&store);
    let workspace_dir = env.create_workspace("workspace");

    let output = run_wrap_cargo(auto_targo(&env).env("TARGO_AUTO_GC", "0"), &workspace_dir);
    assert_eq!(stderr_of(&output), "");
    // A start would be recorded before the run is spawned, so this can't pass by timing.
    assert!(
        !env.root().join("home").exists(),
        "no state is written, and so no run was started"
    );
    assert!(orphan.exists());
}

#[test]
fn wrap_cargo_refuses_a_setting_that_it_does_not_know() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = create_large_orphan(&store);
    let workspace_dir = env.create_workspace("workspace");

    let output = run_wrap_cargo(
        auto_targo(&env).env("TARGO_AUTO_GC", "false"),
        &workspace_dir,
    );
    assert_eq!(
        stderr_of(&output),
        "[targo] skipped automatic gc: `TARGO_AUTO_GC` must be `0` (off) or `1` (on), \
         but is set to \"false\"\n"
    );
    assert!(!env.root().join("home").exists(), "no run was started");
    assert!(orphan.exists());
}

#[test]
fn background_gc_leaves_a_store_with_no_live_entry_alone() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = create_large_orphan(&store);
    let workspace_dir = env.create_workspace("workspace");
    // A `target` that leads outside the store, so wrap-cargo makes no entry for the workspace.
    let elsewhere = env.root().join("elsewhere");
    fs::create_dir(&elsewhere).expect("created dir");
    create_symlink(&elsewhere, &workspace_dir.join("target"));

    let output = wrap_cargo_and_wait(&env, &mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "");
    assert!(orphan.exists(), "nothing is removed");
    let log = read_log(&env);
    let lines: Vec<_> = log.lines().collect();
    let [_header, reason] = lines.as_slice() else {
        panic!("the log was:\n{log}");
    };
    assert!(
        reason.starts_with(&format!(
            "left the store at `{}` alone: none of its entries is live",
            store.dir
        )),
        "the log was:\n{log}"
    );
    let state = read_state(&env);
    assert_eq!(
        state["last-run"]["end"]["result"],
        serde_json::json!("no-live-entry")
    );

    let output = run_wrap_cargo(&mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "", "a run that did nothing is not news");
    assert_eq!(read_state(&env), state);

    // A gc run by hand is not held back.
    let output = env.targo().arg("gc").output().expect("ran targo");
    assert!(output.status.success());
    assert!(!orphan.exists());
}

#[test]
fn background_gc_gives_way_to_a_gc_that_is_running() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = create_large_orphan(&store);
    let workspace_dir = env.create_workspace("workspace");
    let gc_lock = fs::File::create(store.dir.join("gc.lock")).expect("created the gc lock");
    gc_lock.lock().expect("locked the gc lock");

    wrap_cargo_and_wait(&env, &mut auto_targo(&env), &workspace_dir);
    assert!(orphan.exists());
    assert_eq!(
        read_state(&env)["last-run"]["end"]["result"],
        "another-gc-running"
    );
    let output = run_wrap_cargo(&mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "", "a run that did nothing is not news");
}

#[test]
fn background_gc_does_not_change_what_the_cargo_command_prints() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    create_large_orphan(&store);
    let workspace_dir = env.create_workspace("workspace");
    let run = |command: &mut Command, cargo_args: &[&str]| {
        let output = command
            .current_dir(&workspace_dir)
            .arg("wrap-cargo")
            .args(cargo_args)
            .output()
            .expect("ran targo");
        (output.status.code(), output.stdout, output.stderr)
    };

    // One command that succeeds and one that fails; the second run of each starts a gc.
    for (cargo_args, expected_code) in [
        (&["version"][..], Some(0)),
        (&["no-such-subcommand"][..], Some(101)),
    ] {
        let off = run(env.targo().env("TARGO_AUTO_GC", "0"), cargo_args);
        assert_eq!(off.0, expected_code, "for {cargo_args:?}");
        assert!(state_dir(&env).is_none(), "nothing has run yet");

        let on = run(&mut auto_targo(&env), cargo_args);
        wait_for_background_gc(&env);
        assert_eq!(on, off, "for {cargo_args:?}");
        assert_eq!(run_headers(&read_log(&env)).len(), 1, "a gc was started");

        fs::remove_dir_all(env.root().join("home")).expect("removed the state");
    }
}

#[test]
fn wrap_cargo_runs_cargo_when_the_state_cannot_be_written() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = create_large_orphan(&store);
    let workspace_dir = env.create_workspace("workspace");
    let state_home = env.root().join("read-only");
    fs::create_dir(&state_home).expect("created dir");
    let Some(_read_only) = ReadOnlyDirs::new(&env, &state_home) else {
        return;
    };

    for _ in 0..2 {
        let output = run_wrap_cargo(
            auto_targo(&env).env("XDG_STATE_HOME", &state_home),
            &workspace_dir,
        );
        let stderr = stderr_of(&output);
        assert!(
            stderr.lines().count() == 1
                && stderr.starts_with(&format!(
                    "[targo] skipped automatic gc: failed to create the directory `{state_home}/"
                ))
                && stderr.ends_with(
                    ": Permission denied (os error 13) (set `TARGO_AUTO_GC=0` to turn it off)\n"
                ),
            "stderr was:\n{stderr}"
        );
    }
    assert_eq!(names_in(&state_home), [""; 0], "nothing was started");
    assert!(orphan.exists());
}

#[test]
fn wrap_cargo_replaces_a_corrupt_state_file() {
    let env = TestEnv::new();
    let _store = TestStore::new(&env);
    let workspace_dir = env.create_workspace("workspace");
    wrap_cargo_and_wait(&env, &mut auto_targo(&env), &workspace_dir);
    let state_path = state_path(&env);
    fs::write(&state_path, "{").expect("wrote the state file");

    let output = wrap_cargo_and_wait(&env, &mut auto_targo(&env), &workspace_dir);
    let stderr = stderr_of(&output);
    assert!(
        stderr.lines().count() == 1
            && stderr.starts_with(&format!(
                "[targo] automatic gc replaced its state file `{state_path}`: \
                 it could not be parsed ("
            )),
        "stderr was:\n{stderr}"
    );
    // With no record of the first run, another is started.
    assert_eq!(run_headers(&read_log(&env)).len(), 2);
    assert_eq!(read_state(&env)["version"], 1);

    let output = run_wrap_cargo(&mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "");
}

#[test]
fn wrap_cargo_reports_a_background_gc_that_died() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = create_large_orphan(&store);
    let workspace_dir = env.create_workspace("workspace");
    // `soon` is not a duration, so the background run stops at its arguments.
    let targo = || {
        let mut command = auto_targo(&env);
        command.env("TARGO_GC_MAX_AGE", "soon");
        command
    };

    let output = wrap_cargo_and_wait(&env, &mut targo(), &workspace_dir);
    assert_eq!(stderr_of(&output), "", "the Cargo command is not affected");
    assert!(orphan.exists());
    let state = read_state(&env);
    let started = started_of(&state);
    assert_eq!(state["last-run"]["end"], serde_json::Value::Null);
    let log = read_log(&env);
    assert!(
        log.contains("invalid value 'soon' for '--max-age <DURATION>'"),
        "the log was:\n{log}"
    );

    let output = run_wrap_cargo(&mut targo(), &workspace_dir);
    assert_eq!(
        stderr_of(&output),
        format!(
            "[targo] gc: the background run of {} ended without recording a result; see `{}`\n",
            timestamp(started),
            log_path(&env)
        )
    );
    assert_eq!(
        read_state(&env)["last-run"]["end"],
        serde_json::json!({"result": "died", "notice": "shown"})
    );

    let output = run_wrap_cargo(&mut targo(), &workspace_dir);
    assert_eq!(stderr_of(&output), "", "the notice is shown once");
    assert_eq!(read_log(&env), log, "no other run was started");
}

#[test]
fn wrap_cargo_reports_a_background_gc_that_failed() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = create_large_orphan(&store);
    let workspace_dir = env.create_workspace("workspace");
    // gc gives up on a trash that is not a directory.
    let trash = store.dir.join(".targo-trash");
    fs::write(&trash, "").expect("wrote file");

    let output = wrap_cargo_and_wait(&env, &mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "");
    assert!(orphan.exists());
    let started = started_of(&read_state(&env));

    let output = run_wrap_cargo(&mut auto_targo(&env), &workspace_dir);
    let stderr = stderr_of(&output);
    assert!(
        stderr.lines().count() == 1
            && stderr.starts_with(&format!(
                "[targo] gc: the background run of {} failed: `{trash}` must be a directory",
                timestamp(started)
            ))
            && stderr.ends_with(&format!("; see `{}`\n", log_path(&env))),
        "stderr was:\n{stderr}"
    );

    let output = run_wrap_cargo(&mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "", "the notice is shown once");
}

#[test]
fn background_gc_keeps_the_entry_of_the_workspace_that_started_it() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let workspace_dir = env.create_workspace("workspace");
    // Makes the workspace's entry, which is then made to look long unused.
    run_wrap_cargo(&mut env.targo(), &workspace_dir);
    let target_dir = workspace_dir
        .join("target")
        .read_link_utf8()
        .expect("target is a symlink");
    let built_file = target_dir.join("built-file");
    write_synced(&built_file, b"x");
    let metadata_path = target_dir
        .parent()
        .expect("the target dir is in an entry")
        .join("target-dir-metadata.json");
    let metadata = fs::read_to_string(&metadata_path).expect("read entry metadata");
    let mut metadata: serde_json::Value = serde_json::from_str(&metadata).expect("parsed JSON");
    metadata["last-used"] = serde_json::json!(Utc::now() - TimeDelta::days(400));
    fs::write(&metadata_path, metadata.to_string()).expect("wrote entry metadata");

    // Another entry as old, to show that the run does empty what is unused.
    let (control, _) = store.create_live_entry(&env, "control", -400 * DAY);
    write_synced(&control.join("target/built-file"), b"x");

    wrap_cargo_and_wait(
        &env,
        auto_targo(&env).env("TARGO_GC_MAX_AGE", "30d"),
        &workspace_dir,
    );
    assert_eq!(
        names_in(&control.join("target")),
        [""; 0],
        "the log was:\n{}",
        read_log(&env)
    );
    assert!(
        built_file.is_file(),
        "the command that started the run had just used the entry"
    );
}

#[test]
fn background_gc_is_not_left_as_a_child_of_cargo() {
    let env = TestEnv::new();
    let _store = TestStore::new(&env);
    let workspace_dir = env.create_workspace("workspace");

    let mut child = auto_targo(&env)
        .env("CARGO", env.create_waiting_cargo())
        .current_dir(&workspace_dir)
        .args(["wrap-cargo", "build"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawned targo");
    // The stand-in cargo is the process targo exec'ed, and stays until stdin is closed.
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("read from stand-in cargo");
    assert_eq!(line, "ready\n", "targo ran the stand-in cargo");
    wait_for_background_gc(&env);

    // A process that targo started and didn't wait for would be listed here, alive or as a zombie.
    let mut children = all_processes();
    children.retain(|process| process.parent == child.id());
    assert_eq!(children, [], "Cargo has no children");

    drop(child.stdin.take());
    let status = child.wait().expect("waited for stand-in cargo");
    assert!(status.success(), "stand-in cargo exited with {status}");
}

#[test]
fn background_gc_does_not_run_twice_at_once() {
    let env = TestEnv::new();
    let _store = TestStore::new(&env);
    let workspace_dir = env.create_workspace("workspace");
    wrap_cargo_and_wait(&env, &mut auto_targo(&env), &workspace_dir);
    let log = read_log(&env);

    // As a run that started over a day ago and is still going: no result, and the log locked.
    let held = open_lock_file(log_path(&env));
    held.lock().expect("locked the gc log");
    set_last_run_started(&env, Utc::now() - TimeDelta::seconds(25 * HOUR));
    set_last_run(&env, "end", serde_json::Value::Null);
    let state = read_state(&env);
    let output = run_wrap_cargo(&mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "");
    assert_eq!(read_log(&env), log, "no other run is started");
    assert_eq!(read_state(&env), state, "the run is not taken to have died");
    drop(held);
}

#[test]
fn background_gc_is_detached_from_the_command_that_started_it() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let orphan = store.create_entry("orphan", &[], -400 * DAY);
    store.create_live_entry(&env, "live", 0);
    // Not `/dev/null`, so that the gc's stdin can be told from the starter's.
    let starter_stdin = env.root().join("starter-stdin");
    fs::write(&starter_stdin, "").expect("wrote file");
    // Locked and handed on as wrap-cargo does it.
    let log_path = env.root().join("log").into_std_path_buf();
    let log = fs::File::create(&log_path).expect("created the log");
    log.lock().expect("locked the log");
    let clone_log = || log.try_clone().expect("cloned the log");
    // With the store locked, the gc can't get past removing the orphan.
    let store_lock = fs::File::create(store.dir.join("targo.lock")).expect("created store lock");
    store_lock.lock().expect("locked the store");

    let mut starter = env
        .targo()
        .args(["spawn-auto-gc", &Utc::now().to_rfc3339()])
        .stdin(fs::File::open(&starter_stdin).expect("opened file"))
        .stdout(clone_log())
        .stderr(clone_log())
        // A process group of its own, as wrap-cargo gives it.
        .process_group(0)
        .spawn()
        .expect("spawned the starter");
    let status = starter.wait().expect("waited for the starter");
    assert!(status.success(), "the starter exited with {status}");

    // The gc is still running: it can't end while the store is locked.
    let mut left_in_group = all_processes();
    left_in_group.retain(|process| process.group == starter.id());
    assert_eq!(left_in_group, [], "the gc left the starter's process group");
    #[cfg(target_os = "linux")]
    assert_proc_shows_a_detached_gc(&log_path);

    drop(store_lock);
    drop(log);
    wait_for_unlock(&log_path);
    assert!(
        !orphan.exists(),
        "the gc ran to its end, and the log was:\n{}",
        fs::read_to_string(&log_path).expect("read the log")
    );
}

// Reads `/proc` as Linux lays it out.
#[cfg(target_os = "linux")]
fn assert_proc_shows_a_detached_gc(log_path: &Path) {
    let link_of = |pid: u32, name: &str| fs::read_link(format!("/proc/{pid}/{name}"));
    let gc = all_processes()
        .into_iter()
        .find(|process| link_of(process.pid, "fd/1").is_ok_and(|dest| dest == log_path))
        .expect("the gc is running, with the log as its stdout");
    assert_eq!(gc.group, gc.pid, "the gc leads a process group of its own");
    let dest_of = |name: &str| link_of(gc.pid, name).expect("read link");
    assert_eq!(dest_of("cwd"), Path::new("/"));
    assert_eq!(
        dest_of("fd/0"),
        Path::new("/dev/null"),
        "the gc is not left with the starter's stdin"
    );
}

#[test]
fn wrap_cargo_runs_cargo_when_a_notice_cannot_be_printed() {
    let env = TestEnv::new();
    let _store = TestStore::new(&env);
    let workspace_dir = env.create_workspace("workspace");
    wrap_cargo_and_wait(&env, &mut auto_targo(&env), &workspace_dir);
    // As a run that failed records it.
    let failed = serde_json::json!({"result": {"failed": {"message": "x"}}, "notice": "pending"});
    set_last_run(&env, "end", failed);

    // Nothing reads from the pipe, so a write to stderr fails.
    let (reader, writer) = io::pipe().expect("created pipe");
    drop(reader);
    run_wrap_cargo(auto_targo(&env).stderr(writer), &workspace_dir);
    assert_eq!(read_state(&env)["last-run"]["end"]["notice"], "shown");
}

#[test]
fn wrap_cargo_runs_cargo_when_the_gc_cannot_be_started() {
    let env = TestEnv::new();
    let _store = TestStore::new(&env);
    let workspace_dir = env.create_workspace("workspace");
    // A copy of targo that is removed while it runs, as when targo is reinstalled.
    let targo = env.root().join("targo-copy");
    fs::copy(env!("CARGO_BIN_EXE_targo"), &targo).expect("copied targo");
    // targo runs Cargo once before it starts a gc, to find the workspace.
    let removing_cargo = env.root().join("removing-cargo");
    let script = format!(
        "#!/bin/sh\nrm -f {}\nexec {} \"$@\"\n",
        shell_words::quote(targo.as_str()),
        shell_words::quote(env!("CARGO"))
    );
    fs::write(&removing_cargo, script).expect("wrote stand-in cargo");
    fs::set_permissions(&removing_cargo, fs::Permissions::from_mode(0o755))
        .expect("made stand-in cargo executable");

    let output = run_wrap_cargo(
        env.confined_command(targo.as_str())
            .env_remove("TARGO_AUTO_GC")
            .env("CARGO", &removing_cargo),
        &workspace_dir,
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.lines().count() == 1
            && stderr.starts_with(&format!(
                "[targo] skipped automatic gc: failed to run `{targo}"
            ))
            && stderr.ends_with(" (set `TARGO_AUTO_GC=0` to turn it off)\n"),
        "stderr was:\n{stderr}"
    );
    let end = &read_state(&env)["last-run"]["end"];
    assert!(
        end["result"]["failed"]["message"].is_string() && end["notice"] == "shown",
        "the end was recorded as {end}"
    );

    // Reported once, and not tried again until a day has passed.
    let output = run_wrap_cargo(&mut auto_targo(&env), &workspace_dir);
    assert_eq!(stderr_of(&output), "");
    assert_eq!(run_headers(&read_log(&env)).len(), 1);
}

#[test]
fn background_gc_collects_the_store_that_wrap_cargo_used() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    // A relative `CARGO_HOME` names a store by the directory that a command runs in.
    let mut targo = auto_targo(&env);
    targo
        .env_remove("TARGO_STORE_DIR")
        .env("CARGO_HOME", "relative-cargo-home");

    wrap_cargo_and_wait(&env, &mut targo, &workspace_dir);
    let store_dir = workspace_dir.join("relative-cargo-home/targo");
    assert!(
        store_dir.join("gc.lock").exists(),
        "the gc ran on `{store_dir}`, and the log was:\n{}",
        read_log(&env)
    );
}
