use crate::{
    helpers::{DirWithPath, ExclusiveLock},
    metadata::{TargetDirMetadata, TargoStoreMetadata},
};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std::{ambient_authority, fs_utf8::Dir};
use color_eyre::{eyre::Context, Result};
use std::{fs, io};
use xxhash_rust::xxh3::xxh3_64;

/// The targo store, with `targo.lock` held exclusively for as long as this value exists.
///
/// Everything that modifies the store takes a `LockedStore`.
#[derive(Debug)]
#[must_use]
pub(crate) struct LockedStore {
    store_dir: DirWithPath,
    lock: ExclusiveLock,
}

impl LockedStore {
    const LOCK_FILE_NAME: &'static str = "targo.lock";

    /// Opens the store at `store_dir_path`, creating it if needed, and locks it.
    pub(crate) fn open(store_dir_path: Utf8PathBuf) -> Result<Self> {
        let authority = ambient_authority();
        Dir::create_ambient_dir_all(&store_dir_path, authority).wrap_err_with(|| {
            format!("failed to create targo store directory `{store_dir_path}`")
        })?;
        let store_dir = Dir::open_ambient_dir(&store_dir_path, authority)
            .wrap_err_with(|| format!("failed to open targo store directory `{store_dir_path}`"))?;
        let store_dir = DirWithPath::new(store_dir, store_dir_path);

        let lock = ExclusiveLock::acquire(&store_dir, Self::LOCK_FILE_NAME)?;
        let store = Self { store_dir, lock };

        // Does the directory already have Targo metadata stored in it?
        let metadata = store.read_store_metadata()?;

        let metadata_to_write = match &metadata {
            Some(metadata) => metadata.upgrade_if_necessary(),
            None => Some(TargoStoreMetadata::new()),
        };

        if let Some(to_write) = metadata_to_write {
            // TODO: also upgrade metadata within the directory if required
            store.write_store_metadata(&to_write)?;
        }

        Ok(store)
    }

    /// Releases the store lock.
    pub(crate) fn unlock(self) -> Result<()> {
        self.lock.unlock()
    }

    /// Points `target_dir` into the store, unless a real directory is in the way.
    pub(crate) fn set_up_target_dir(
        self,
        workspace_dir: &Utf8Path,
        target_dir: &Utf8Path,
    ) -> Result<TargetDirSetup> {
        match self.determine_target_dir(target_dir)? {
            TargetDirKind::DoesNotExist => self.link_target_dir(workspace_dir, target_dir)?,
            TargetDirKind::Directory => {
                self.unlock()?;
                return Ok(TargetDirSetup::DirectoryInTheWay);
            }
            TargetDirKind::TargoSymlink { encoded } => {
                ManagedTargetDir::new(&self, target_dir.to_owned(), &encoded)?;
            }
            TargetDirKind::Other => {}
        }
        Ok(TargetDirSetup::Done(self))
    }

    fn determine_target_dir(&self, target_dir: &Utf8Path) -> Result<TargetDirKind> {
        let symlink_metadata = match target_dir.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TargetDirKind::DoesNotExist)
            }
            Err(err) => {
                return Err(err).wrap_err_with(|| {
                    format!("failed to read metadata for target dir `{target_dir}`")
                })
            }
        };

        let kind = if symlink_metadata.is_dir() {
            // This is a directory and is eligible for being converted to Targo.
            TargetDirKind::Directory
        } else if symlink_metadata.is_symlink() {
            // TODO: read link in a TOCTTOU-safe manner
            let data = target_dir
                .read_link()
                .wrap_err_with(|| format!("failed to read `{target_dir}` as symlink"))?;
            let dest_dir = Utf8PathBuf::try_from(data).wrap_err_with(|| {
                format!("destination of symlink at `{target_dir}` is invalid UTF-8")
            })?;

            // Is this a symlink managed by this installation of Targo?
            // (TODO: be able to operate on other installations of Targo maybe?)
            if let Some(encoded) = get_encoded_workspace(self.store_dir.path(), &dest_dir) {
                TargetDirKind::TargoSymlink {
                    encoded: encoded.to_owned(),
                }
            } else {
                TargetDirKind::Other
            }
        } else {
            TargetDirKind::Other
        };

        Ok(kind)
    }

    // ---
    // Helper methods
    // ---

    fn read_store_metadata(&self) -> Result<Option<TargoStoreMetadata>> {
        let metadata: Option<TargoStoreMetadata> = self
            .store_dir
            .read_metadata(TargoStoreMetadata::METADATA_FILE_NAME)?;
        let metadata = if let Some(metadata) = metadata {
            Some(metadata.verify(self.store_dir.path())?)
        } else {
            None
        };
        Ok(metadata)
    }

    fn write_store_metadata(&self, metadata: &TargoStoreMetadata) -> Result<()> {
        self.store_dir
            .write_metadata(TargoStoreMetadata::METADATA_FILE_NAME, metadata)
    }

    fn link_target_dir(&self, workspace_dir: &Utf8Path, target_dir: &Utf8Path) -> Result<()> {
        // Create the managed target directory and symlink.
        let encoded = encode_workspace_path(workspace_dir);
        let managed_dir = ManagedTargetDir::new(self, target_dir.to_owned(), &encoded)?;

        // Create the symlink.
        // TODO: Windows
        std::os::unix::fs::symlink(&managed_dir.target_dir, &managed_dir.source_link).wrap_err_with(
            || {
                format!(
                    "failed to create symlink from `{}` to `{}`",
                    managed_dir.source_link, managed_dir.target_dir
                )
            },
        )
    }
}

/// Removes the real directory at `target_dir`, which can take minutes.
pub(crate) fn remove_target_dir(target_dir: &Utf8Path) -> Result<()> {
    // TODO: do something better than rm -rf target/ here!
    match fs::remove_dir_all(target_dir) {
        Ok(()) => Ok(()),
        Err(err) => match err.kind() {
            // Another process got there first. The caller looks again under the lock.
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => {
                tracing::debug!("`{target_dir}` was removed or replaced by another process: {err}");
                Ok(())
            }
            _ => {
                Err(err).wrap_err_with(|| format!("failed to remove old target dir `{target_dir}`"))
            }
        },
    }
}

/// The result of [`LockedStore::set_up_target_dir`].
#[derive(Debug)]
#[must_use]
pub(crate) enum TargetDirSetup {
    /// The target dir is in the store, or is something targo leaves alone.
    Done(LockedStore),
    /// A real directory is in the way. Removing it is slow, so the lock has been released.
    DirectoryInTheWay,
}

#[derive(Debug)]
enum TargetDirKind {
    DoesNotExist,
    Directory,
    TargoSymlink {
        encoded: String,
    },
    /// Includes non-Targo symlinks and other situations that won't be touched.
    Other,
}

#[derive(Debug)]
pub(crate) struct ManagedTargetDir {
    source_link: Utf8PathBuf,
    #[allow(dead_code)]
    dest_dir: DirWithPath,
    target_dir: Utf8PathBuf,
}

impl ManagedTargetDir {
    fn new(store: &LockedStore, source_link: Utf8PathBuf, encoded: &str) -> Result<Self> {
        // Create the directory if it doesn't exist.
        let dest_dir_path = store.store_dir.path().join(encoded);
        let target_dir = dest_dir_path.join("target");
        store
            .store_dir
            .dir()
            .create_dir_all(Utf8Path::new(encoded).join("target"))
            .wrap_err_with(|| {
                format!("failed to create managed target directory `{dest_dir_path}`")
            })?;
        let dest_dir = store.store_dir.dir().open_dir(encoded).wrap_err_with(|| {
            format!("failed to open managed target directory `{dest_dir_path}`")
        })?;
        let dest_dir = DirWithPath::new(dest_dir, dest_dir_path);

        // A read-modify-write, safe only because `store` holds the lock.
        let mut metadata = dest_dir
            .read_metadata(TargetDirMetadata::METADATA_FILE_NAME)?
            .unwrap_or_else(TargetDirMetadata::new);
        // TODO: check existing backlinks
        metadata.backlinks.insert(source_link.clone());
        metadata.update_last_used();

        dest_dir.write_metadata(TargetDirMetadata::METADATA_FILE_NAME, &metadata)?;

        Ok(Self {
            source_link,
            dest_dir,
            target_dir,
        })
    }
}

fn get_encoded_workspace<'b>(store_dir: &Utf8Path, path: &'b Utf8Path) -> Option<&'b str> {
    // Don't touch relative symlinks.
    if !path.is_absolute() {
        return None;
    }

    let suffix = path.strip_prefix(store_dir).ok()?;
    // Ensure the suffix has two components.
    if suffix.components().count() == 2 {
        suffix.iter().next()
    } else {
        None
    }
}

/// Maximum length of the encoded workspace path in bytes.
const MAX_ENCODED_LEN: usize = 96;

/// Length of the hash suffix appended to truncated paths.
///
/// Between the first many bytes and this, we should ideally have more than
/// enough entropy to disambiguate repos.
const HASH_SUFFIX_LEN: usize = 8;

/// Encodes a workspace path into a directory-safe string.
///
/// The encoding is bijective (reversible) and produces valid directory names on all
/// platforms. The encoding scheme uses underscore as an escape character:
///
/// - `_` → `__` (escape underscore first)
/// - `/` → `_s` (Unix path separator)
/// - `\` → `_b` (Windows path separator)
/// - `:` → `_c` (Windows drive letter separator)
/// - `*` → `_a` (asterisk, invalid on Windows)
/// - `"` → `_q` (double quote, invalid on Windows)
/// - `<` → `_l` (less than, invalid on Windows)
/// - `>` → `_g` (greater than, invalid on Windows)
/// - `|` → `_p` (pipe, invalid on Windows)
/// - `?` → `_m` (question mark, invalid on Windows)
///
/// If the encoded path exceeds 96 bytes, it is truncated at a valid UTF-8 boundary
/// and an 8-character hash suffix is appended to maintain uniqueness.
///
/// # Examples
///
/// - `/home/rain/dev/nextest` → `_shome_srain_sdev_snextest`
/// - `C:\Users\rain\dev` → `C_c_bUsers_brain_bdev`
/// - `/path_with_underscore` → `_spath__with__underscore`
/// - `/weird*path?` → `_sweird_apath_m`
fn encode_workspace_path(path: &Utf8Path) -> String {
    let mut encoded = String::with_capacity(path.as_str().len() * 2);

    for ch in path.as_str().chars() {
        match ch {
            '_' => encoded.push_str("__"),
            '/' => encoded.push_str("_s"),
            '\\' => encoded.push_str("_b"),
            ':' => encoded.push_str("_c"),
            '*' => encoded.push_str("_a"),
            '"' => encoded.push_str("_q"),
            '<' => encoded.push_str("_l"),
            '>' => encoded.push_str("_g"),
            '|' => encoded.push_str("_p"),
            '?' => encoded.push_str("_m"),
            _ => encoded.push(ch),
        }
    }

    truncate_with_hash(encoded)
}

/// Truncates an encoded string to fit within [`MAX_ENCODED_LEN`] bytes.
///
/// If the string is already short enough, returns it unchanged. Otherwise,
/// truncates at a valid UTF-8 boundary and appends an 8-character hash suffix
/// derived from the full string.
fn truncate_with_hash(encoded: String) -> String {
    if encoded.len() <= MAX_ENCODED_LEN {
        return encoded;
    }

    // Compute hash of full string before truncation.
    let hash = xxh3_64(encoded.as_bytes());
    let hash_suffix = format!("{:08x}", hash & 0xFFFFFFFF);

    // Find the longest valid UTF-8 prefix that fits.
    let max_prefix_len = MAX_ENCODED_LEN - HASH_SUFFIX_LEN;
    let bytes = encoded.as_bytes();
    let truncated_bytes = &bytes[..max_prefix_len.min(bytes.len())];

    // Use utf8_chunks to find the valid UTF-8 portion.
    let mut valid_len = 0;
    for chunk in truncated_bytes.utf8_chunks() {
        valid_len += chunk.valid().len();
        // Stop at first invalid sequence (which would be an incomplete multi-byte char).
        if !chunk.invalid().is_empty() {
            break;
        }
    }

    let mut result = encoded[..valid_len].to_string();
    result.push_str(&hash_suffix);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;

    /// A temp dir with paths for a store and a workspace's target dir.
    struct TestDirs {
        // Held so that the directory is removed on drop.
        _temp_dir: camino_tempfile::Utf8TempDir,
        store_dir: Utf8PathBuf,
        workspace_dir: Utf8PathBuf,
        target_dir: Utf8PathBuf,
    }

    impl TestDirs {
        fn new() -> Self {
            let temp_dir = camino_tempfile::tempdir().expect("created temp dir");
            let root = temp_dir.path();
            let workspace_dir = root.join("workspace");
            fs::create_dir(&workspace_dir).expect("created workspace dir");
            Self {
                store_dir: root.join("store"),
                target_dir: workspace_dir.join("target"),
                workspace_dir,
                _temp_dir: temp_dir,
            }
        }

        fn open_store(&self) -> LockedStore {
            LockedStore::open(self.store_dir.clone()).expect("opened store")
        }

        /// Opens the lock file again, so that a lock taken through it conflicts with the store's.
        fn lock_probe(&self) -> fs::File {
            fs::File::open(self.store_dir.join(LockedStore::LOCK_FILE_NAME))
                .expect("opened lock file")
        }

        fn set_up_target_dir(&self, store: LockedStore) -> TargetDirSetup {
            store
                .set_up_target_dir(&self.workspace_dir, &self.target_dir)
                .expect("set up target dir")
        }
    }

    #[test]
    fn test_store_lock_held_until_unlock() {
        let dirs = TestDirs::new();
        let mut store = dirs.open_store();
        let probe = dirs.lock_probe();
        assert_contended(&probe, "after the store is opened");

        // First through a missing target dir, then through the symlink the first pass creates.
        for pass in ["first setup", "second setup"] {
            store = match dirs.set_up_target_dir(store) {
                TargetDirSetup::Done(store) => store,
                TargetDirSetup::DirectoryInTheWay => panic!("{pass} found a real directory"),
            };
            assert!(dirs.target_dir.is_symlink(), "{pass} links the target dir");
            assert_contended(&probe, pass);
        }

        store.unlock().expect("unlocked store");
        probe
            .try_lock_exclusive()
            .expect("lock is free once the store is unlocked");
    }

    #[test]
    fn test_store_unlocks_instead_of_removing_target_dir() {
        let dirs = TestDirs::new();
        let old_file = dirs.target_dir.join("old-file");
        fs::create_dir(&dirs.target_dir).expect("created target dir");
        fs::write(&old_file, "").expect("wrote old file");

        let store = dirs.open_store();
        let probe = dirs.lock_probe();
        match dirs.set_up_target_dir(store) {
            TargetDirSetup::Done(_) => panic!("a real directory is not set up under the lock"),
            TargetDirSetup::DirectoryInTheWay => {}
        }
        probe
            .try_lock_exclusive()
            .expect("lock is free while the directory is still to be removed");
        assert!(old_file.exists(), "the directory is left to the caller");
        // The store below blocks on the lock unless the probe lets go of it.
        drop(probe);

        remove_target_dir(&dirs.target_dir).expect("removed target dir");
        match dirs.set_up_target_dir(dirs.open_store()) {
            TargetDirSetup::Done(_) => {}
            TargetDirSetup::DirectoryInTheWay => panic!("the directory was removed"),
        }
        assert!(dirs.target_dir.is_symlink());
    }

    #[test]
    fn test_remove_target_dir_leaves_non_directories() {
        let dirs = TestDirs::new();
        remove_target_dir(&dirs.target_dir).expect("a missing target dir is fine");

        fs::write(&dirs.target_dir, "").expect("wrote file");
        remove_target_dir(&dirs.target_dir).expect("a file is left for the caller to look at");
        assert!(dirs.target_dir.is_file());
    }

    // A shared probe is contended only by an exclusive lock.
    fn assert_contended(probe: &fs::File, when: &str) {
        match FileExt::try_lock_shared(probe) {
            Ok(()) => panic!("{when}, the store lock is free, but the store should hold it"),
            Err(error) => assert_eq!(
                error.raw_os_error(),
                fs2::lock_contended_error().raw_os_error(),
                "{when}, taking the store lock fails because it is held: {error}"
            ),
        }
    }

    #[test]
    fn test_get_encoded_workspace() {
        assert_eq!(
            get_encoded_workspace("/foo/bar".into(), "/foo/bar/baz/quux".into()),
            Some("baz"),
        );
        assert_eq!(
            get_encoded_workspace("/foo/bar".into(), "/foo/bar/baz".into()),
            None
        );
        assert_eq!(get_encoded_workspace("/foo/bar".into(), "/".into()), None);
        assert_eq!(get_encoded_workspace("/foo/bar".into(), "".into()), None);
        assert_eq!(
            get_encoded_workspace("/foo/bar".into(), "../foo".into()),
            None
        );
    }

    // Basic encoding tests.
    #[test]
    fn test_encode_workspace_path() {
        let cases = [
            ("", ""),
            ("simple", "simple"),
            ("/home/user", "_shome_suser"),
            ("/home/user/project", "_shome_suser_sproject"),
            ("C:\\Users\\name", "C_c_bUsers_bname"),
            ("D:\\dev\\project", "D_c_bdev_bproject"),
            ("/path_with_underscore", "_spath__with__underscore"),
            ("C:\\path_name", "C_c_bpath__name"),
            ("/a/b/c", "_sa_sb_sc"),
            // Windows-invalid characters.
            ("/weird*path", "_sweird_apath"),
            ("/path?query", "_spath_mquery"),
            ("/file<name>", "_sfile_lname_g"),
            ("/path|pipe", "_spath_ppipe"),
            ("/\"quoted\"", "_s_qquoted_q"),
            // All Windows-invalid characters combined.
            ("*\"<>|?", "_a_q_l_g_p_m"),
        ];

        for (input, expected) in cases {
            let encoded = encode_workspace_path(Utf8Path::new(input));
            assert_eq!(
                encoded, expected,
                "encoding failed for {input:?}: expected {expected:?}, got {encoded:?}"
            );
        }
    }

    // Bijectivity tests: different inputs must produce different outputs.
    #[test]
    fn test_encoding_is_bijective() {
        // These pairs were problematic with the simple dash-based encoding.
        let pairs = [
            ("/-", "-/"),
            ("/a", "_a"),
            ("_s", "/"),
            ("a_", "a/"),
            ("__", "_"),
            ("/", "\\"),
            // New escape sequences for Windows-invalid characters.
            ("_a", "*"),
            ("_q", "\""),
            ("_l", "<"),
            ("_g", ">"),
            ("_p", "|"),
            ("_m", "?"),
            // Ensure Windows-invalid chars don't collide with each other.
            ("*", "?"),
            ("<", ">"),
            ("|", "\""),
        ];

        for (a, b) in pairs {
            let encoded_a = encode_workspace_path(Utf8Path::new(a));
            let encoded_b = encode_workspace_path(Utf8Path::new(b));
            assert_ne!(
                encoded_a, encoded_b,
                "bijectivity violated: {a:?} and {b:?} both encode to {encoded_a:?}"
            );
        }
    }

    // Truncation tests.
    #[test]
    fn test_short_paths_not_truncated() {
        // A path that encodes to exactly 96 bytes should not be truncated.
        let short_path = "/a/b/c/d";
        let encoded = encode_workspace_path(Utf8Path::new(short_path));
        assert!(
            encoded.len() <= MAX_ENCODED_LEN,
            "short path should not be truncated: {encoded:?} (len={})",
            encoded.len()
        );
        // Should not contain a hash suffix (no truncation occurred).
        assert_eq!(encoded, "_sa_sb_sc_sd");
    }

    #[test]
    fn test_long_paths_truncated_with_hash() {
        // Create a path that will definitely exceed 96 bytes when encoded.
        // Each `/x` becomes `_sx` (3 bytes), so we need > 32 components.
        let long_path = "/a".repeat(50); // 100 bytes raw, 150 bytes encoded
        let encoded = encode_workspace_path(Utf8Path::new(&long_path));

        assert_eq!(
            encoded.len(),
            MAX_ENCODED_LEN,
            "truncated path should be exactly {MAX_ENCODED_LEN} bytes: {encoded:?} (len={})",
            encoded.len()
        );

        // Should end with an 8-character hex hash.
        let hash_suffix = &encoded[encoded.len() - HASH_SUFFIX_LEN..];
        assert!(
            hash_suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "hash suffix should be hex digits: {hash_suffix:?}"
        );
    }

    #[test]
    fn test_truncation_preserves_uniqueness() {
        // Two different long paths should produce different truncated results.
        let path_a = "/a".repeat(50);
        let path_b = "/b".repeat(50);

        let encoded_a = encode_workspace_path(Utf8Path::new(&path_a));
        let encoded_b = encode_workspace_path(Utf8Path::new(&path_b));

        assert_ne!(
            encoded_a, encoded_b,
            "different paths should produce different encodings even when truncated"
        );
    }

    #[test]
    fn test_truncation_with_unicode() {
        // Create a path with multi-byte UTF-8 characters that would be split.
        // '日' is 3 bytes in UTF-8.
        let unicode_path = "/日本語".repeat(20); // Each repeat is 10 bytes raw.
        let encoded = encode_workspace_path(Utf8Path::new(&unicode_path));

        assert!(
            encoded.len() <= MAX_ENCODED_LEN,
            "encoded path should not exceed {MAX_ENCODED_LEN} bytes: len={}",
            encoded.len()
        );

        // Verify the result is valid UTF-8 (this would panic if not).
        let _ = encoded.as_str();

        // Verify the hash suffix is present and valid hex.
        let hash_suffix = &encoded[encoded.len() - HASH_SUFFIX_LEN..];
        assert!(
            hash_suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "hash suffix should be hex digits: {hash_suffix:?}"
        );
    }

    #[test]
    fn test_truncation_boundary_at_96_bytes() {
        // Create paths of varying lengths around the 96-byte boundary.
        // The encoding doubles some characters, so we need to be careful.

        // A path that encodes to exactly 96 bytes should not be truncated.
        // 'a' stays as 'a', so we can use a string of 96 'a's.
        let exactly_96 = "a".repeat(96);
        let encoded = encode_workspace_path(Utf8Path::new(&exactly_96));
        assert_eq!(encoded.len(), 96);
        assert_eq!(encoded, exactly_96); // No hash suffix.

        // A path that encodes to 97 bytes should be truncated.
        let just_over = "a".repeat(97);
        let encoded = encode_workspace_path(Utf8Path::new(&just_over));
        assert_eq!(encoded.len(), 96);
        // Should have hash suffix.
        let hash_suffix = &encoded[90..];
        assert!(hash_suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_truncation_different_suffixes_same_prefix() {
        // Two paths with the same prefix but different endings should get different hashes.
        let base = "a".repeat(90);
        let path_a = format!("{base}XXXXXXX");
        let path_b = format!("{base}YYYYYYY");

        let encoded_a = encode_workspace_path(Utf8Path::new(&path_a));
        let encoded_b = encode_workspace_path(Utf8Path::new(&path_b));

        // Both should be truncated (97 chars each).
        assert_eq!(encoded_a.len(), 96);
        assert_eq!(encoded_b.len(), 96);

        // The hash suffixes should be different.
        assert_ne!(
            &encoded_a[90..],
            &encoded_b[90..],
            "different paths should have different hash suffixes"
        );
    }
}
