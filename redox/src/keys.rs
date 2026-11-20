use std::path::{Path, PathBuf};

use elanfp_engine::store::{FileStore, Key, Layout, contains_templates, new_key, private_dir, write_private};
use elanfp_engine::{BoxError, Fail, library_fail};
use zeroize::Zeroizing;

pub const STATE: &str = "/var/lib/elanfp";
pub const KEY_FILE: &str = "master.key";

pub fn store_at(state: &Path) -> Result<FileStore, BoxError> {
    private_dir(state)?;
    let path = state.join(KEY_FILE);
    let key: Key = if path.exists() {
        let raw = Zeroizing::new(std::fs::read(&path)?);
        Key::from_slice(&raw).ok_or_else(|| Fail::new("key-corrupt"))?
    } else {
        if contains_templates(state) {
            return Err(Fail::new("key-lost").arg("path", path.display()).into());
        }
        let key = new_key().map_err(|e| library_fail(e.key(), e.args()))?;
        write_private(&path, key.as_slice())?;
        key
    };
    FileStore::new(state.to_path_buf(), Layout::PerUser, key)
}

pub fn system_store() -> Result<FileStore, BoxError> {
    store_at(&PathBuf::from(STATE))
}
