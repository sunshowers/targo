use crate::support::{StoreLockState, TestEnv};
use camino::Utf8Path;
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{BufRead, BufReader},
    os::unix::{ffi::OsStrExt, fs::symlink},
    process::{Command, Stdio},
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
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    let target_dir = workspace_dir.join("target");
    fs::create_dir_all(target_dir.join("debug")).expect("created target dir");
    fs::write(target_dir.join("debug/old-file"), "").expect("wrote old file");

    run_wrap_cargo(&mut env.targo(), &workspace_dir);

    assert_linked_into_store(&workspace_dir, &env.store_dir());
    let entries = fs::read_dir(&target_dir).expect("read new target dir");
    assert_eq!(entries.count(), 0, "the old contents are gone");
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

/// Runs `targo wrap-cargo version` in `workspace_dir`, which must succeed.
fn run_wrap_cargo(command: &mut Command, workspace_dir: &Utf8Path) {
    let output = command
        .current_dir(workspace_dir)
        .args(["wrap-cargo", "version"])
        .output()
        .expect("ran targo");
    assert!(
        output.status.success(),
        "targo failed with stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
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
