use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use papilla::vault;
pub use papilla::vault::{ALGORITHM, KEY_LEN, Key, new_key};
use papilla::{Gallery, Profile};

use crate::{BoxError, Fail, check_name, valid_name};

const EXTENSION: &str = "papv";
const BAD_DESCRIPTOR: i32 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    PerUser,
    Single(u32),
}

pub struct FileStore {
    dir: PathBuf,
    layout: Layout,
    key: Key,
    writes: Mutex<()>,
}

pub fn private_dir(dir: &Path) -> Result<(), BoxError> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    if mode != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn write_private(path: &Path, data: &[u8]) -> Result<(), BoxError> {
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let dir = match std::fs::File::open(dir) {
        Ok(dir) => Some(dir),
        Err(e) if cannot_sync_directories(&e) => None,
        Err(e) => return Err(e.into()),
    };
    let tmp = path.with_extension("tmp");
    let placed = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).and_then(|mut file| {
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    });
    if let Err(e) = placed {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    match dir.map(|dir| dir.sync_all()) {
        Some(Err(e)) if !cannot_sync_directories(&e) => Err(e.into()),
        _ => Ok(()),
    }
}

fn undone_if_new(path: &Path, fresh: bool, written: Result<(), BoxError>) -> Result<(), BoxError> {
    if written.is_err() && fresh {
        let _ = std::fs::remove_file(path);
    }
    written
}

fn cannot_sync_directories(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::Unsupported | ErrorKind::InvalidInput | ErrorKind::IsADirectory) || e.raw_os_error() == Some(BAD_DESCRIPTOR)
}

pub fn contains_templates(dir: &Path) -> bool {
    fn any(dir: &Path, depth: usize) -> bool {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|e| {
                let path = e.path();
                path.extension().is_some_and(|x| x == EXTENSION) || (depth > 0 && path.is_dir() && any(&path, depth - 1))
            })
        })
    }
    any(dir, 1)
}

impl FileStore {
    pub fn new(dir: PathBuf, layout: Layout, key: Key) -> Result<Self, BoxError> {
        private_dir(&dir)?;
        Ok(FileStore { dir, layout, key, writes: Mutex::new(()) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn allows(&self, uid: u32) -> bool {
        match self.layout {
            Layout::Single(own) => uid == own,
            Layout::PerUser => true,
        }
    }

    fn user_dir(&self, uid: u32) -> Result<PathBuf, Fail> {
        if !self.allows(uid) {
            return Err(Fail::new("store-foreign-user").num("uid", uid));
        }
        Ok(match self.layout {
            Layout::Single(_) => self.dir.clone(),
            Layout::PerUser => self.dir.join(uid.to_string()),
        })
    }

    fn path(&self, uid: u32, name: &str) -> Result<PathBuf, Fail> {
        check_name(name)?;
        Ok(self.user_dir(uid)?.join(format!("{name}.{EXTENSION}")))
    }

    fn context(uid: u32, name: &str, binding: &[u8]) -> Vec<u8> {
        let mut c = format!("elanfp/v1/uid={uid}/name={name}/sensor=").into_bytes();
        c.extend_from_slice(binding);
        c
    }

    pub fn has_templates(&self) -> bool {
        contains_templates(&self.dir)
    }

    pub fn exists(&self, uid: u32, name: &str) -> Result<bool, BoxError> {
        Ok(self.path(uid, name)?.exists())
    }

    pub fn names(&self, uid: u32) -> Result<Vec<String>, BoxError> {
        let mut names: Vec<String> = match std::fs::read_dir(self.user_dir(uid)?) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == EXTENSION))
                .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .filter(|n| valid_name(n))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        names.sort();
        Ok(names)
    }

    fn writing(&self) -> MutexGuard<'_, ()> {
        self.writes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self, uid: u32, name: &str, gallery: &Gallery, binding: &[u8]) -> Result<(), BoxError> {
        let path = self.path(uid, name)?;
        private_dir(&self.user_dir(uid)?)?;
        let plain = papilla::encode(gallery, binding).map_err(|e| Fail::new(e.key()).args(e.args()))?;
        let sealed = vault::seal(&self.key, &Self::context(uid, name, binding), &plain).map_err(|e| Fail::new(e.key()).args(e.args()))?;
        write_private(&path, &sealed)
    }

    pub fn save(&self, uid: u32, name: &str, gallery: &Gallery, binding: &[u8]) -> Result<(), BoxError> {
        let _writing = self.writing();
        let path = self.path(uid, name)?;
        let fresh = !path.exists();
        undone_if_new(&path, fresh, self.write(uid, name, gallery, binding))
    }

    pub fn update(&self, uid: u32, name: &str, gallery: &Gallery, binding: &[u8]) -> Result<bool, BoxError> {
        let _writing = self.writing();
        if !self.path(uid, name)?.exists() {
            return Ok(false);
        }
        self.write(uid, name, gallery, binding)?;
        Ok(true)
    }

    fn note_path(&self, uid: u32, note: &'static str) -> Result<PathBuf, Fail> {
        Ok(self.user_dir(uid)?.join(note))
    }

    pub fn read_note(&self, uid: u32, note: &'static str) -> Result<Option<Vec<u8>>, BoxError> {
        match std::fs::read(self.note_path(uid, note)?) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn write_note(&self, uid: u32, note: &'static str, data: &[u8]) -> Result<(), BoxError> {
        let _writing = self.writing();
        private_dir(&self.user_dir(uid)?)?;
        write_private(&self.note_path(uid, note)?, data)
    }

    pub fn clear_note(&self, uid: u32, note: &'static str) -> Result<(), BoxError> {
        let _writing = self.writing();
        match std::fs::remove_file(self.note_path(uid, note)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn load(&self, uid: u32, name: &str, profile: &Profile, binding: &[u8]) -> Result<Gallery, BoxError> {
        let path = self.path(uid, name)?;
        let sealed = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Fail::new("finger-unknown").arg("finger", name).into()),
            Err(e) => return Err(e.into()),
        };
        let unreadable = |key: &str, args: Vec<(&'static str, String)>| Fail::new("template-unreadable").arg("finger", name).sub("detail", key).args(args);
        let plain = vault::open(&self.key, &Self::context(uid, name, binding), &sealed).map_err(|e| unreadable(e.key(), e.args()))?;
        Ok(papilla::decode(&plain, *profile, binding).map_err(|e| unreadable(e.key(), e.args()))?)
    }

    pub fn delete(&self, uid: u32, name: &str) -> Result<(), BoxError> {
        let _writing = self.writing();
        match std::fs::remove_file(self.path(uid, name)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Fail::new("finger-unknown").arg("finger", name).into()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FAST, FakeSensor, PID, Recorder, TempDir, VID, fail_key, policy};
    use crate::flows::enroll;
    use crate::session::Session;
    use elanfp_core::Sensor;

    const BINDING: [u8; 11] = [0x04, 0xf3, 0x0c, 0x4b, 0x26, 0x30, 0x21, 0x4f, 0x00, 0x4f, 0x00];

    fn filled(dir: &TempDir, layout: Layout, uid: u32) -> FileStore {
        let store = FileStore::new(dir.0.clone(), layout, vault::new_key().unwrap()).unwrap();
        let mut session = Session::open_with(Sensor::new(FakeSensor::with_touches(&[3, 3, 3])), VID, PID, FAST, &mut Recorder::default()).unwrap();
        enroll(&policy(), &mut session, &store, uid, "right-index", &mut Recorder::default()).unwrap();
        store
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_write_that_fails_leaves_neither_a_new_file_nor_a_draft() {
        let dir = TempDir::new("unwritten");
        let target = dir.0.join("closed/right-index.papv");
        private_dir(&dir.0.join("closed")).unwrap();
        std::fs::set_permissions(dir.0.join("closed"), std::fs::Permissions::from_mode(0o300)).unwrap();
        let refused = write_private(&target, b"sealed");
        std::fs::set_permissions(dir.0.join("closed"), std::fs::Permissions::from_mode(0o700)).unwrap();
        if std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&dir.0).unwrap()) != 0 {
            assert!(refused.is_err());
            assert_eq!(std::fs::read_dir(dir.0.join("closed")).unwrap().count(), 0);
        }
        assert!(write_private(&dir.0.join("no-such-directory/right-index.papv"), b"sealed").is_err());
        write_private(&target, b"sealed").unwrap();
        std::fs::create_dir(target.with_extension("tmp")).unwrap();
        assert!(write_private(&target, b"sealed again").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"sealed");
        std::fs::remove_dir(target.with_extension("tmp")).unwrap();
        let occupied = dir.0.join("closed/left-thumb.papv");
        std::fs::create_dir(&occupied).unwrap();
        std::fs::write(occupied.join("kept"), b"x").unwrap();
        assert!(write_private(&occupied, b"sealed").is_err());
        assert!(!occupied.with_extension("tmp").exists() && occupied.join("kept").exists());
        let mut left: Vec<String> =
            std::fs::read_dir(dir.0.join("closed")).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["left-thumb.papv", "right-index.papv"]);
        let failed = || -> Result<(), BoxError> { Err(Fail::new("usb-error").into()) };
        assert!(undone_if_new(&target, false, failed()).is_err() && target.exists());
        assert!(undone_if_new(&target, true, Ok(())).is_ok() && target.exists());
        assert!(undone_if_new(&target, true, failed()).is_err() && !target.exists());
    }

    #[test]
    fn files_and_directories_are_private() {
        let dir = TempDir::new("perm");
        let store = filled(&dir, Layout::PerUser, 1000);
        assert_eq!(mode(&dir.0), 0o700);
        assert_eq!(mode(&dir.0.join("1000")), 0o700);
        assert_eq!(mode(&dir.0.join("1000/right-index.papv")), 0o600);
        assert!(store.has_templates());
        assert!(!dir.0.join("1000/right-index.tmp").exists());
    }

    #[test]
    fn a_template_moved_to_another_user_name_or_sensor_is_unreadable() {
        let dir = TempDir::new("bind");
        let store = filled(&dir, Layout::PerUser, 1000);
        let profile = policy().profile;
        assert!(store.load(1000, "right-index", &profile, &BINDING).is_ok());
        std::fs::create_dir_all(dir.0.join("1001")).unwrap();
        std::fs::copy(dir.0.join("1000/right-index.papv"), dir.0.join("1001/right-index.papv")).unwrap();
        std::fs::copy(dir.0.join("1000/right-index.papv"), dir.0.join("1000/other.papv")).unwrap();
        for result in [
            store.load(1001, "right-index", &profile, &BINDING),
            store.load(1000, "other", &profile, &BINDING),
            store.load(1000, "right-index", &profile, &[0u8; 11]),
        ] {
            assert_eq!(fail_key(&result.expect_err("must be refused")), "template-unreadable");
        }
        let other_key = FileStore::new(dir.0.clone(), Layout::PerUser, vault::new_key().unwrap()).unwrap();
        assert_eq!(fail_key(&other_key.load(1000, "right-index", &profile, &BINDING).err().unwrap()), "template-unreadable");
    }

    #[test]
    fn tampering_is_detected() {
        let dir = TempDir::new("tamper");
        let store = filled(&dir, Layout::PerUser, 1000);
        let path = dir.0.join("1000/right-index.papv");
        let mut data = std::fs::read(&path).unwrap();
        let middle = data.len() / 2;
        data[middle] ^= 0x01;
        std::fs::write(&path, data).unwrap();
        assert_eq!(fail_key(&store.load(1000, "right-index", &policy().profile, &BINDING).err().unwrap()), "template-unreadable");
    }

    #[test]
    fn single_user_layout_refuses_other_users() {
        let dir = TempDir::new("single");
        let store = filled(&dir, Layout::Single(1000), 1000);
        assert!(dir.0.join("right-index.papv").exists());
        assert!(store.allows(1000) && !store.allows(0));
        assert_eq!(fail_key(&store.names(1001).err().unwrap()), "store-foreign-user");
        assert_eq!(fail_key(&store.delete(0, "right-index").err().unwrap()), "store-foreign-user");
    }

    #[test]
    fn names_listing_delete_and_hostile_names() {
        let dir = TempDir::new("names");
        let store = filled(&dir, Layout::PerUser, 1000);
        std::fs::write(dir.0.join("1000/-bad.papv"), b"x").unwrap();
        std::fs::write(dir.0.join("1000/notes.txt"), b"x").unwrap();
        assert_eq!(store.names(1000).unwrap(), vec!["right-index".to_string()]);
        assert!(store.names(4242).unwrap().is_empty());
        for bad in ["../1001/right-index", "", ".", "a/b", "x\0"] {
            assert_eq!(fail_key(&store.exists(1000, bad).err().unwrap()), "finger-name-invalid", "{bad:?}");
            assert_eq!(fail_key(&store.delete(1000, bad).err().unwrap()), "finger-name-invalid");
        }
        store.delete(1000, "right-index").unwrap();
        assert_eq!(fail_key(&store.delete(1000, "right-index").err().unwrap()), "finger-unknown");
        assert_eq!(fail_key(&store.load(1000, "right-index", &policy().profile, &BINDING).err().unwrap()), "finger-unknown");
    }
}
