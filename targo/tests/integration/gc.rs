use crate::support::TestEnv;
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, FixedOffset, TimeDelta, Utc};
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{symlink, MetadataExt},
    process::Output,
};

const HOUR: i64 = 60 * 60;
const DAY: i64 = 24 * HOUR;

#[test]
fn gc_dry_run_reports_and_changes_nothing() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let workspaces = env.root().join("workspaces");

    let live_link = workspaces.join("live/target");
    let live_target = store
        .create_entry("live", &[&live_link], -400 * DAY)
        .join("target");
    create_symlink(&live_target, &live_link);

    let gone_link = workspaces.join("gone/target");
    let orphan = store.create_entry("orphan", &[&gone_link], -(3 * DAY + 12 * HOUR));
    write_synced(&orphan.join("target/built-file"), &[b'x'; 64 * 1024]);

    // Older than the other orphan, so it is reported first even though its name sorts last.
    let relinked_link = workspaces.join("relinked/target");
    let replaced_link = workspaces.join("replaced/target");
    let stale = store.create_entry(
        "stale",
        &[&relinked_link, &replaced_link],
        -(40 * DAY + 12 * HOUR),
    );
    create_symlink(&live_target, &relinked_link);
    fs::create_dir_all(&replaced_link).expect("created dir");

    store.create_entry("no-backlinks", &[], -(2 * DAY + 12 * HOUR));
    store.create_entry("recent-orphan", &[&gone_link], -HOUR);
    store.create_entry("future", &[&gone_link], 2 * DAY + 12 * HOUR);

    let looping_link = workspaces.join("looping/target");
    store.create_entry("unknown", &[&gone_link, &looping_link], -400 * DAY);
    create_symlink(Utf8Path::new("target"), &looping_link);
    let loop_error = fs::metadata(&looping_link).expect_err("a looping symlink doesn't resolve");

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

    let (stale_size, orphan_size, no_backlinks_size) = (
        disk_usage(&stale),
        disk_usage(&orphan),
        disk_usage(&store.dir.join("no-backlinks")),
    );
    let expected = format!(
        "would remove `stale` ({}): orphaned, last used 40d ago; \
         backlinks: `{relinked_link}` (points elsewhere), `{replaced_link}` (not a symlink)\n\
         would remove `orphan` ({}): orphaned, last used 3d ago; \
         backlinks: `{gone_link}` (missing)\n\
         would remove `no-backlinks` ({}): orphaned, last used 2d ago; no backlinks\n\
         would keep `corrupt`: unrecognized; \
         failed to deserialize metadata from `{corrupt_metadata_path}`: {json_error}\n\
         would keep `empty`: unrecognized; it has no `target-dir-metadata.json`\n\
         would keep `future`: last used 2d in the future; backlinks: `{gone_link}` (missing)\n\
         would keep `other-view`: backlink state unknown; backlinks: `{other_view_link}` \
         (unknown: the link names this entry by a path that does not resolve here)\n\
         would keep `unknown`: backlink state unknown; \
         backlinks: `{gone_link}` (missing), `{looping_link}` (unknown: {loop_error})\n\
         would remove 3 entries ({}) and keep 7 entries: 1 live, 1 orphaned within grace, \
         2 with unknown backlinks, 2 unrecognized, 1 last used in the future\n",
        human_size(stale_size),
        human_size(orphan_size),
        human_size(no_backlinks_size),
        human_size(stale_size + orphan_size + no_backlinks_size),
    );

    let before = env.snapshot();
    let output = run_gc(&env, &[]);
    assert_eq!(stdout_of_success(&output), expected);
    assert_eq!(env.snapshot(), before, "a dry run changes nothing");
}

#[test]
fn gc_dry_run_honors_orphan_grace() {
    let env = TestEnv::new();
    let store = TestStore::new(&env);
    let gone_link = env.root().join("workspaces/gone/target");
    let entry = store.create_entry("orphan", &[&gone_link], -(HOUR + HOUR / 2));
    let size = human_size(disk_usage(&entry));

    let kept = "would remove 0 entries (0 B) and keep 1 entry: 1 orphaned within grace\n";
    let removed = format!(
        "would remove `orphan` ({size}): orphaned, last used 1h ago; \
         backlinks: `{gone_link}` (missing)\n\
         would remove 1 entry ({size}) and keep 0 entries\n"
    );
    let data: [(&[&str], &str); 3] = [
        (&[], kept),
        (&["--orphan-grace", "2h"], kept),
        (&["--orphan-grace", "1h"], &removed),
    ];
    for (args, expected) in data {
        let output = run_gc(&env, args);
        assert_eq!(stdout_of_success(&output), expected, "for {args:?}");
    }
}

#[test]
fn gc_dry_run_follows_the_link_that_wrap_cargo_makes() {
    let env = TestEnv::new();
    let workspace_dir = env.create_workspace("workspace");
    let output = env
        .targo()
        .current_dir(&workspace_dir)
        .args(["wrap-cargo", "version"])
        .output()
        .expect("ran targo");
    assert!(output.status.success(), "wrap-cargo set up the target dir");

    // With no grace, only the link keeps the entry.
    let no_grace = ["--orphan-grace", "0s"];
    assert_eq!(
        stdout_of_success(&run_gc(&env, &no_grace)),
        "would remove 0 entries (0 B) and keep 1 entry: 1 live\n"
    );

    fs::remove_file(workspace_dir.join("target")).expect("removed the link");
    let stdout = stdout_of_success(&run_gc(&env, &no_grace));
    // The age and the size in between depend on timing and on the filesystem.
    assert!(
        stdout.starts_with("would remove `")
            && stdout.contains("(missing)\nwould remove 1 entry (")
            && stdout.ends_with(") and keep 0 entries\n"),
        "stdout was:\n{stdout}"
    );
}

#[test]
fn gc_dry_run_sees_live_entry_through_store_alias() {
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

    for store_dir in [&store.dir, &store_alias] {
        let output = env
            .targo()
            .env("TARGO_STORE_DIR", store_dir)
            .args(["gc", "--dry-run"])
            .output()
            .expect("ran targo");
        assert_eq!(
            stdout_of_success(&output),
            "would remove 0 entries (0 B) and keep 2 entries: 2 live\n",
            "for the store at `{store_dir}`"
        );
    }
}

#[test]
fn gc_dry_run_does_not_create_a_store() {
    let env = TestEnv::new();
    let before = env.snapshot();

    let output = run_gc(&env, &[]);
    assert_eq!(
        stdout_of_success(&output),
        format!(
            "nothing to collect: there is no targo store at `{}`\n",
            env.store_dir()
        )
    );

    let output = env
        .targo()
        .env_remove("TARGO_STORE_DIR")
        .args(["gc", "--dry-run"])
        .output()
        .expect("ran targo");
    assert_eq!(
        stdout_of_success(&output),
        format!(
            "nothing to collect: there is no targo store at `{}`\n",
            env.cargo_home().join("targo")
        )
    );

    assert_eq!(env.snapshot(), before, "nothing is created");
}

#[test]
fn gc_dry_run_refuses_a_store_it_cannot_use() {
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

        let output = run_gc(&env, &[]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains(&expected),
            "for {metadata:?}, expected failure with {expected:?}, stderr was:\n{stderr}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "",
            "for {metadata:?}, nothing is reported"
        );
        assert_eq!(
            env.snapshot(),
            before,
            "for {metadata:?}, nothing is changed"
        );
    }
}

#[test]
fn gc_rejects_bad_store_dir_override() {
    let env = TestEnv::new();
    let before = env.snapshot();

    let output = env
        .targo()
        .env("TARGO_STORE_DIR", "relative/store")
        .args(["gc", "--dry-run"])
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

/// A store written by hand, so that entries can have any last-used time.
struct TestStore {
    dir: Utf8PathBuf,
    now: DateTime<Utc>,
}

impl TestStore {
    fn new(env: &TestEnv) -> Self {
        let dir = env.store_dir();
        fs::create_dir(&dir).expect("created store dir");
        fs::write(
            dir.join("targo-metadata.json"),
            r#"{"store-version":1,"min-version":"0.1.0"}"#,
        )
        .expect("wrote store metadata");
        Self {
            dir,
            now: Utc::now(),
        }
    }

    /// Creates an entry last used `last_used_secs` after the store was created.
    ///
    /// Half a unit away from a whole one, the age shown can't depend on how long the test takes.
    fn create_entry(
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
fn write_synced(path: &Utf8Path, contents: &[u8]) {
    let mut file = fs::File::create(path).expect("created file");
    file.write_all(contents).expect("wrote file");
    file.sync_all().expect("synced file");
}

fn create_symlink(dest: &Utf8Path, link: &Utf8Path) {
    fs::create_dir_all(link.parent().expect("link has a parent")).expect("created dir");
    symlink(dest, link).expect("created symlink");
}

fn run_gc(env: &TestEnv, args: &[&str]) -> Output {
    env.targo()
        .args(["gc", "--dry-run"])
        .args(args)
        .output()
        .expect("ran targo")
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
