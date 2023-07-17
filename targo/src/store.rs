use crate::{
    helpers::{AsLockedCtx, DirWithPath, ExclusiveRoot, UnlockedRoot},
    metadata::{TargetDirMetadata, TargoStoreMetadata},
};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std::{ambient_authority, fs_utf8::Dir};
use color_eyre::{eyre::Context, Result};

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
            if let Some(hash) = get_workspace_hash(self.store_dir.path(), &dest_dir) {
                let managed_dir = ManagedTargetDir::new(self, target_dir.to_owned(), hash)?;
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
        let hash = hash_workspace_dir(&workspace_dir);
        let managed_dir = ManagedTargetDir::new(self, target_dir, &hash)?;

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
    fn new(store: &TargoStore, source_link: Utf8PathBuf, hash: &str) -> Result<Self> {
        // Create the directory if it doesn't exist.
        let dest_dir_path = store.store_dir.path().join(hash);
        let target_dir = dest_dir_path.join("target");
        store
            .store_dir
            .dir()
            .create_dir_all(Utf8Path::new(hash).join("target"))
            .wrap_err_with(|| {
                format!("failed to create managed target directory `{dest_dir_path}`")
            })?;
        let dest_dir = store.store_dir.dir().open_dir(hash).wrap_err_with(|| {
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

fn get_workspace_hash<'b>(store_dir: &Utf8Path, path: &'b Utf8Path) -> Option<&'b str> {
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

static TARGO_HASHER_KEY: &[u8; 32] = b"targo\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

fn hash_workspace_dir(workspace_dir: &Utf8Path) -> String {
    let mut hasher = blake3::Hasher::new_keyed(TARGO_HASHER_KEY);
    hasher.update(workspace_dir.as_str().as_bytes());
    bs58::encode(&hasher.finalize().as_bytes()[..20]).into_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_workspace_hash() {
        assert_eq!(
            get_workspace_hash("/foo/bar".into(), "/foo/bar/baz/quux".into()),
            Some("baz"),
        );
        assert_eq!(
            get_workspace_hash("/foo/bar".into(), "/foo/bar/baz".into()),
            None
        );
        assert_eq!(get_workspace_hash("/foo/bar".into(), "/".into()), None);
        assert_eq!(get_workspace_hash("/foo/bar".into(), "".into()), None);
        assert_eq!(get_workspace_hash("/foo/bar".into(), "../foo".into()), None);
    }
}
