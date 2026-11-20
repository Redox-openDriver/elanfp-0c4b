use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use elanfp_engine::store::{FileStore, KEY_LEN, Key, Layout, contains_templates, new_key, private_dir, write_private};
use elanfp_engine::{BoxError, Fail, library_fail};
use zeroize::Zeroizing;

use crate::users;

pub const CREDENTIAL_NAME: &str = "elanfp-master";
pub const KEY_FILE_VARIABLE: &str = "ELANFP_KEY_FILE";
pub const SYSTEM_KEY_FILE: &str = "/etc/elanfp/master.key";
const SYSTEMD_CREDS: [&str; 2] = ["/usr/bin/systemd-creds", "/bin/systemd-creds"];
const SYSTEM_STATE: &str = "/var/lib/elanfp";
const SEALED_KEY: &str = "master.cred";
const PLAIN_KEY: &str = "master.key";

fn sealer() -> Option<&'static str> {
    SYSTEMD_CREDS.iter().copied().find(|program| Path::new(program).exists())
}

fn creds(program: &str, args: &[&str], input: &[u8]) -> Result<Zeroizing<Vec<u8>>, BoxError> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Fail::new("creds-unavailable").arg("detail", e))?;
    let fed = child.stdin.take().map_or(Ok(()), |mut stdin| stdin.write_all(input));
    let out = child.wait_with_output()?;
    let stdout = Zeroizing::new(out.stdout);
    if !out.status.success() {
        return Err(Fail::new("creds-failed").arg("action", args[0]).arg("detail", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    fed?;
    Ok(stdout)
}

fn key_from(raw: &[u8]) -> Result<Key, Fail> {
    Key::from_slice(raw).ok_or_else(|| Fail::new("key-corrupt"))
}

fn key_within(file: File, path: &Path) -> Result<Key, BoxError> {
    let mut raw = Zeroizing::new(Vec::with_capacity(KEY_LEN + 1));
    file.take(KEY_LEN as u64 + 1).read_to_end(&mut raw).map_err(|e| Fail::new("key-file-unreadable").arg("path", path.display()).arg("detail", e))?;
    Ok(key_from(&raw)?)
}

fn plain_file(path: &Path) -> Result<(File, std::fs::Metadata), Fail> {
    let exposed = || Fail::new("key-file-exposed").arg("path", path.display());
    let unreadable = |e: std::io::Error| Fail::new("key-file-unreadable").arg("path", path.display()).arg("detail", e);
    let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Fail::new("key-file-missing").arg("path", path.display())),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(exposed()),
        Err(e) => return Err(unreadable(e)),
    };
    let metadata = file.metadata().map_err(unreadable)?;
    if metadata.is_file() { Ok((file, metadata)) } else { Err(exposed()) }
}

pub fn key_from_file(path: &Path) -> Result<Key, BoxError> {
    let (file, metadata) = plain_file(path)?;
    let trusted_owner = metadata.uid() == users::effective_uid() || metadata.uid() == 0;
    if metadata.permissions().mode() & 0o077 != 0 || !trusted_owner {
        return Err(Fail::new("key-file-exposed").arg("path", path.display()).into());
    }
    key_within(file, path)
}

pub fn user_store() -> Result<FileStore, BoxError> {
    let dir = match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("elanfp"),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| Fail::new("home-unset"))?).join(".local/share/elanfp"),
    };
    private_dir(&dir)?;
    let (sealed, plain) = (dir.join(SEALED_KEY), dir.join(PLAIN_KEY));
    let name = format!("--name={CREDENTIAL_NAME}");
    let fresh = || -> Result<Key, BoxError> {
        if contains_templates(&dir) {
            return Err(Fail::new("key-lost").arg("path", dir.display()).into());
        }
        Ok(new_key().map_err(|e| library_fail(e.key(), e.args()))?)
    };
    let key = match (sealed.exists(), plain.exists(), sealer()) {
        (true, _, Some(program)) => key_from(&creds(program, &["decrypt", "--user", &name, &sealed.to_string_lossy(), "-"], &[])?)?,
        (true, _, None) => return Err(Fail::new("creds-unavailable").arg("detail", SYSTEMD_CREDS[0]).into()),
        (false, true, _) => key_from_file(&plain)?,
        (false, false, Some(program)) => {
            let key = fresh()?;
            write_private(&sealed, &creds(program, &["encrypt", "--user", "--with-key=host+tpm2", &name, "-", "-"], key.as_slice())?)?;
            key
        }
        (false, false, None) => {
            let key = fresh()?;
            write_private(&plain, key.as_slice())?;
            key
        }
    };
    FileStore::new(dir, Layout::Single(users::current_uid()), key)
}

fn key_from_credential(path: &Path) -> Result<Key, BoxError> {
    let (file, _) = plain_file(path)?;
    key_within(file, path)
}

pub fn system_key() -> Result<Key, BoxError> {
    if let Some(credentials) = std::env::var_os("CREDENTIALS_DIRECTORY").filter(|d| !d.is_empty()) {
        return key_from_credential(&PathBuf::from(credentials).join(CREDENTIAL_NAME));
    }
    key_from_file(&std::env::var_os(KEY_FILE_VARIABLE).filter(|p| !p.is_empty()).map_or_else(|| PathBuf::from(SYSTEM_KEY_FILE), PathBuf::from))
}

pub fn system_store() -> Result<FileStore, BoxError> {
    let key = system_key()?;
    let state = std::env::var_os("STATE_DIRECTORY").filter(|d| !d.is_empty()).map_or_else(|| PathBuf::from(SYSTEM_STATE), PathBuf::from);
    FileStore::new(state, Layout::PerUser, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("elanfp-keys-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        private_dir(&dir).unwrap();
        dir
    }

    fn fail_key(error: BoxError) -> String {
        error.downcast_ref::<Fail>().map(|f| f.0.key.clone()).unwrap_or_default()
    }

    #[test]
    fn a_key_file_must_be_private_whole_and_present() {
        let dir = scratch("file");
        let path = dir.join("master.key");
        assert_eq!(fail_key(key_from_file(&path).err().unwrap()), "key-file-missing");
        write_private(&path, &[7u8; KEY_LEN]).unwrap();
        assert_eq!(key_from_file(&path).unwrap().as_slice(), &[7u8; KEY_LEN]);
        for mode in [0o640, 0o604, 0o660, 0o644] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(fail_key(key_from_file(&path).err().unwrap()), "key-file-exposed", "{mode:o}");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(key_from_file(&path).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_private(&path, &[7u8; KEY_LEN - 1]).unwrap();
        assert_eq!(fail_key(key_from_file(&path).err().unwrap()), "key-corrupt");
        write_private(&path, &[7u8; KEY_LEN + 1]).unwrap();
        assert_eq!(fail_key(key_from_file(&path).err().unwrap()), "key-corrupt");
        assert_eq!(fail_key(key_from_file(&dir).err().unwrap()), "key-file-exposed");
        write_private(&path, &[7u8; KEY_LEN]).unwrap();
        let link = dir.join("link.key");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(fail_key(key_from_file(&link).err().unwrap()), "key-file-exposed");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
