use std::collections::BTreeSet;

use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Local};
use color_eyre::{eyre::bail, Result};
use semver::Version;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct TargoStoreMetadata {
    store_version: u32,
    min_version: Version,
}

impl TargoStoreMetadata {
    pub(crate) const METADATA_FILE_NAME: &'static str = "targo-metadata.json";
    pub(crate) const STORE_VERSION: u32 = 1;
    pub(crate) const MIN_VERSION: Version = Version::new(0, 1, 0);

    pub(crate) fn new() -> Self {
        Self {
            store_version: Self::STORE_VERSION,
            min_version: Self::MIN_VERSION,
        }
    }

    pub(crate) fn upgrade_if_necessary(&self) -> Option<Self> {
        (self.store_version < Self::STORE_VERSION).then(move || {
            let mut metadata = self.clone();
            metadata.store_version = Self::STORE_VERSION;
            metadata.min_version = Self::MIN_VERSION;
            metadata
        })
    }

    pub(crate) fn verify(self, store_dir: &Utf8Path) -> Result<Self> {
        if self.store_version > Self::STORE_VERSION {
            bail!(
                "targo store directory at `{store_dir}` is too new \
                 (this version of targo supports up to store version {}, \
                 but metadata had version = {}) -- upgrade to targo version `{}` or newer",
                Self::STORE_VERSION,
                self.store_version,
                self.min_version,
            );
        }
        Ok(self)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct TargetDirMetadata {
    pub(crate) backlinks: BTreeSet<Utf8PathBuf>,
    pub(crate) last_used: DateTime<Local>,
}

impl TargetDirMetadata {
    pub(crate) const METADATA_FILE_NAME: &'static str = "target-dir-metadata.json";

    pub(crate) fn new() -> Self {
        Self {
            backlinks: BTreeSet::new(),
            last_used: Local::now(),
        }
    }

    pub(crate) fn update_last_used(&mut self) {
        self.last_used = Local::now();
    }
}
