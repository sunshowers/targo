# targo

Wraps cargo to move target directories to a central location [super experimental]

To use,

```
cargo install --git https://github.com/sunshowers/targo --bin targo
```

Then, add this to your .zshrc/.bash_profile:

```
alias cargo='targo wrap-cargo'
```

To bypass targo, prefix the command with a backslash on the command line (should work in all Bourne shells at least):

```
$ \cargo build
```

Target directories are stored in `$CARGO_HOME/targo`. To store them somewhere else, set `TARGO_STORE_DIR` to an absolute path. The store can't be inside a workspace's `target` directory. A workspace whose `target` already links into another store keeps using that store until the link is removed.

## Cleaning up the store

targo cleans up the store on its own, in the background, at most once every 24 hours. A Cargo command run through targo starts a cleanup when one is due. The cleanup is detached from the command, and writes its report to a log instead of your terminal.

A background run removes *orphaned* entries. An entry is orphaned when none of the `target` links that targo recorded for it leads to it any more, as after its workspace is deleted. An orphaned entry is removed once it has gone 7 days without being used through targo or built in. Builds made outside targo (rust-analyzer, scripts that call `cargo` directly) count, because gc also looks at when Cargo last compiled something in the directory.

If a run freed 1 MiB or more, or if something failed, the next Cargo command prints one line to stderr, once:

```
[targo] gc: the background run of 2026-10-04T18:22:07Z removed 3 entries (5.2 GiB); see `/home/al/.local/state/targo/_shome_sal_s.cargo_stargo/gc.log`
```

The log (`gc.log`) and the record of the last run (`gc-state.json`) are in `$XDG_STATE_HOME/targo`, which is `~/.local/state/targo` by default, in a directory named after the store's path.

### Running gc by hand

```
$ targo gc
removed `_shome_sal_sdev_sold-prototype` (3.4 GiB): orphaned, last used 41d ago; backlinks: `/home/al/dev/old-prototype/target` (missing)
removed `_stmp_sscratch_srepro` (212.6 MiB): orphaned, last built 9d ago; backlinks: `/tmp/scratch/repro/target` (missing)
removed 2 entries (3.6 GiB) and kept 14 entries: 13 live, 1 orphaned within grace
```

`targo gc` runs the same cleanup in the foreground. It prints a line for each entry it removes, with the reason, and then a summary. To preview a run, use `targo gc --dry-run`: it prints the same report with "would remove", and changes nothing.

To change the grace period, pass `--orphan-grace` with a duration such as `12h` or `30d`. The default is `7d`. The flag applies to that run only: background runs always use 7 days.

`targo gc` exits with 0 on success, with 1 if something failed (it reports each entry that it could not remove or empty, and carries on with the rest), and with 75 if another gc is already running on the store, in which case it does nothing.

### Workspaces you haven't built in for a while

By default, gc leaves an entry alone for as long as a workspace links to it. To also reclaim the space of workspaces that still exist, set a maximum age:

```
$ targo gc --max-age 60d
```

gc then *empties* the target directory of an entry that a workspace still links to, once it has not been used or built in for that long. Emptying deletes the build output and keeps the entry and its directory, so the workspace's `target` link keeps working and the next build starts from scratch.

`--max-age` is unset by default. To set it for background runs as well, put `TARGO_GC_MAX_AGE=60d` in your environment; the flag wins if both are given. A bare `0` is refused, because it reads as "off" and means the opposite: `0s` empties the target directory of every workspace that is not being built in right now. To empty none, leave both unset.

### Turning automatic cleanup off

Set `TARGO_AUTO_GC=0` in your environment. No background run starts, and `targo gc` by hand works as before. The only other value accepted is `1`, which is the default. With any other value targo does not guess: it starts no background run, and every Cargo command prints a `[targo] skipped automatic gc` line that names the bad value.

### What gc leaves alone

* An entry that a workspace links to, unless `--max-age` is set.
* An entry that was used or built in within the grace period or the maximum age.
* An entry that is in use: Cargo is building in it, or it changed while gc was looking at it. gc skips it and says so.
* An entry whose `target` links it can't check, for example because of a permission error.
* A directory in the store that it doesn't recognize as an entry.
* Everything outside the store. gc never changes a workspace or its `target` link.

### Caveats

* gc judges an entry by whether its `target` links can be seen from where gc runs. If a store is shared between environments that see different paths (containers, a disk that is sometimes unmounted), entries used only from elsewhere look orphaned, and are removed once they have gone unused for the grace period. In such an environment, set `TARGO_AUTO_GC=0`, and give any `targo gc` you run by hand a longer `--orphan-grace`.
* gc removes or empties whole target directories. It does not trim stale build output inside one that is in regular use, so a long-lived target still grows. A plain `cargo clean` does not help: it removes only the `target` link (as of Cargo 1.99), and the next command through targo links the same entry again. `cargo clean --profile dev` and `cargo clean -p <package>` do delete through the link.
* Like the rest of targo, gc is Unix only.

## About

See [this comment on rust-lang/cargo](https://github.com/rust-lang/cargo/issues/11156#issuecomment-1285951209) for the execution model and considerations as of 2022-10-22.

targo-like behavior is currently [Rust RFC 3371](https://github.com/rust-lang/rfcs/pull/3371)! Hopefully it will be stabilized and eventually targo will go away.

## Looking for co-maintainers

This MVP works for me and I only plan to add features as I need them. If you're a Rust developer who cares about this issue, and would like to help drive this project forward, please reach out to me at the email I use for my git commits with:
* some information about yourself
* why you're interested
* where you'd like to take this project

There's plenty to do:

* write more tests
* finish target directory management: clean a workspace's entry on `cargo clean`, and trim stale build output inside target directories that are in use
* add configuration options
* stay up-to-date with upstream Cargo features
* add Windows support
