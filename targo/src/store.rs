use crate::{
    helpers::{AsLockedCtx, DirWithPath, ExclusiveRoot, UnlockedRoot},
    metadata::{TargetDirMetadata, TargoStoreMetadata},
};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std::{ambient_authority, fs_utf8::Dir};
use color_eyre::{eyre::Context, Result};
use xxhash_rust::xxh3::xxh3_64;

#[derive(Debug)]
pub(crate) struct TargoStore {
    store_dir: DirWithPath,
}

impl TargoStore {
    pub(crate) fn new(store_dir_path: Utf8PathBuf) -> Result<Self> {
        let authority = ambient_authority();
        Dir::create_ambient_dir_all(&store_dir_path, authority).wrap_err_with(|| {
            format!("failed to create targo store directory `{store_dir_path}`")
        })?;
        let store_dir = Dir::open_ambient_dir(&store_dir_path, authority)
            .wrap_err_with(|| format!("failed to open targo store directory `{store_dir_path}`"))?;
        let store_dir = DirWithPath::new(store_dir, store_dir_path);

        let store = Self { store_dir };

        let store = UnlockedRoot::new(store)?.lock_exclusive()?;

        // TODO: hold lock open while TargoStore is held, so per-directory metadata can be written
        // safely

        // Does the directory already have Targo metadata stored in it?
        let metadata = Self::read_store_metadata(&store)?;

        let metadata_to_write = match &metadata {
            Some(metadata) => metadata.upgrade_if_necessary(),
            None => Some(TargoStoreMetadata::new()),
        };

        if let Some(to_write) = metadata_to_write {
            // TODO: also upgrade metadata within the directory if required
            Self::write_store_metadata(&store, &to_write)?;
        }

        Ok(store.unlock())
    }

    pub(crate) fn determine_target_dir(
        &self,
        workspace_dir: &Utf8Path,
        target_dir: &Utf8Path,
    ) -> Result<TargetDirKind> {
        let symlink_metadata = match target_dir.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TargetDirKind::DoesNotExist {
                    workspace_dir: workspace_dir.to_owned(),
                    target_dir: target_dir.to_owned(),
                })
            }
            Err(err) => {
                return Err(err).wrap_err_with(|| {
                    format!("failed to read metadata for target dir `{target_dir}`")
                })
            }
        };

        let kind = if symlink_metadata.is_dir() {
            // This is a directory and is eligible for being converted to Targo.
            TargetDirKind::Directory {
                workspace_dir: workspace_dir.to_owned(),
                target_dir: target_dir.to_owned(),
            }
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
                let managed_dir = ManagedTargetDir::new(self, target_dir.to_owned(), encoded)?;
                TargetDirKind::TargoSymlink(managed_dir)
            } else {
                TargetDirKind::Other
            }
        } else {
            TargetDirKind::Other
        };

        Ok(kind)
    }

    pub(crate) fn actualize_kind(&self, kind: TargetDirKind) -> Result<Option<ManagedTargetDir>> {
        match kind {
            TargetDirKind::DoesNotExist {
                workspace_dir,
                target_dir,
            } => {
                let managed_dir = self.setup_target_dir(workspace_dir, target_dir, false)?;
                Ok(Some(managed_dir))
            }
            TargetDirKind::Directory {
                workspace_dir,
                target_dir,
            } => {
                let managed_dir = self.setup_target_dir(workspace_dir, target_dir, true)?;
                Ok(Some(managed_dir))
            }
            TargetDirKind::TargoSymlink(managed_dir) => Ok(Some(managed_dir)),
            TargetDirKind::Other => Ok(None),
        }
    }

    // ---
    // Helper methods
    // ---

    fn read_store_metadata(store: &ExclusiveRoot<Self>) -> Result<Option<TargoStoreMetadata>> {
        let metadata: Option<TargoStoreMetadata> = store
            .ctx
            .store_dir
            .read_metadata(TargoStoreMetadata::METADATA_FILE_NAME)?;
        let metadata = if let Some(metadata) = metadata {
            Some(metadata.verify(store.ctx.store_dir.path())?)
        } else {
            None
        };
        Ok(metadata)
    }

    fn write_store_metadata(
        store: &ExclusiveRoot<Self>,
        metadata: &TargoStoreMetadata,
    ) -> Result<()> {
        store
            .ctx
            .store_dir
            .write_metadata(TargoStoreMetadata::METADATA_FILE_NAME, metadata)
    }

    fn setup_target_dir(
        &self,
        workspace_dir: Utf8PathBuf,
        target_dir: Utf8PathBuf,
        exists: bool,
    ) -> Result<ManagedTargetDir> {
        if exists {
            // TODO: do something better than rm -rf target/ here!
            match std::fs::remove_dir_all(&target_dir) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    // The directory doesn't exist. Skip this.
                }
                Err(err) => {
                    Err::<(), _>(err).wrap_err_with(|| {
                        format!("failed to remove old target dir `{target_dir}`")
                    })?;
                }
            }
        }

        // Create the managed target directory and symlink.
        let encoded = encode_workspace_path(&workspace_dir);
        let managed_dir = ManagedTargetDir::new(self, target_dir, &encoded)?;

        // Create the symlink.
        // TODO: Windows
        std::os::unix::fs::symlink(&managed_dir.target_dir, &managed_dir.source_link)
            .wrap_err_with(|| {
                format!(
                    "failed to create symlink from `{}` to `{}`",
                    managed_dir.source_link, managed_dir.target_dir
                )
            })?;

        Ok(managed_dir)
    }
}

impl AsLockedCtx for TargoStore {
    fn dir_and_lock_name(&self) -> (&DirWithPath, &str) {
        (&self.store_dir, "targo.lock")
    }
}

#[derive(Debug)]
pub(crate) enum TargetDirKind {
    DoesNotExist {
        workspace_dir: Utf8PathBuf,
        target_dir: Utf8PathBuf,
    },
    Directory {
        workspace_dir: Utf8PathBuf,
        target_dir: Utf8PathBuf,
    },
    TargoSymlink(ManagedTargetDir),
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
    fn new(store: &TargoStore, source_link: Utf8PathBuf, encoded: &str) -> Result<Self> {
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

        let mut metadata =
            Self::read_dir_metadata(&dest_dir)?.unwrap_or_else(TargetDirMetadata::new);
        // TODO: check existing backlinks
        metadata.backlinks.insert(source_link.clone());
        metadata.update_last_used();

        Self::write_dir_metadata(&dest_dir, &metadata)?;

        Ok(Self {
            source_link,
            dest_dir,
            target_dir,
        })
    }

    fn read_dir_metadata(dest_dir: &DirWithPath) -> Result<Option<TargetDirMetadata>> {
        dest_dir.read_metadata(TargetDirMetadata::METADATA_FILE_NAME)
    }

    fn write_dir_metadata(dest_dir: &DirWithPath, metadata: &TargetDirMetadata) -> Result<()> {
        dest_dir.write_metadata(TargetDirMetadata::METADATA_FILE_NAME, metadata)
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
