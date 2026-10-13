//! Fixed-path, write-only management credentials. The atomic bundle keeps URL/token paired.
use super::valid_id;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
pub const MAX_BUNDLE: usize = 20 << 10;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub url: String,
    #[serde(default)]
    pub bearer_token: Option<String>,
}
// No Debug implementation: URL and bearer are credentials.
/// Build a fixed managed-bundle path only after validating the destination ID.
fn path(root: &Path, id: &str) -> Result<PathBuf, &'static str> {
    if !valid_id(id) {
        return Err("invalid_destination");
    }
    Ok(root.join(".notification-credentials").join(format!("{id}.json")))
}
/// Treat unreadable bundle paths as occupied so credential IDs cannot be silently reused.
pub fn present(root: &Path, id: &str) -> bool {
    path(root, id).is_ok_and(|path| match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    })
}
/// Require a bounded regular file or directory with private Unix ownership and permissions.
fn private(metadata: &fs::Metadata, directory: bool) -> Result<(), &'static str> {
    if metadata.file_type().is_symlink()
        || if directory { !metadata.is_dir() } else { !metadata.is_file() || metadata.len() > MAX_BUNDLE as u64 }
    {
        return Err("insecure_credential_file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        let uid = unsafe { geteuid() };
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 || (!directory && metadata.nlink() != 1) {
            return Err("insecure_credential_file");
        }
    }
    Ok(())
}
/// Reject symlinked auth paths and create the private bundle directory when requested.
fn directory(root: &Path, create: bool) -> Result<PathBuf, &'static str> {
    let mut component = PathBuf::new();
    for part in root.components() {
        component.push(part);
        // A Windows drive/UNC prefix is not a filesystem entry until its root is appended.
        if matches!(part, std::path::Component::Prefix(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&component).map_err(|_| "credential_unavailable")?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("insecure_credential_directory");
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        let uid = unsafe { geteuid() };
        let metadata = fs::symlink_metadata(root).map_err(|_| "credential_unavailable")?;
        // The startup auth root is operator-selected; it must not be writable by other users.
        if (metadata.uid() != uid && metadata.uid() != 0) || metadata.mode() & 0o022 != 0 {
            return Err("insecure_credential_directory");
        }
    }
    let dir = root.join(".notification-credentials");
    match fs::symlink_metadata(&dir) {
        Ok(metadata) => private(&metadata, true)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&dir).map_err(|_| "credential_unavailable")?;
            private(&fs::symlink_metadata(&dir).map_err(|_| "credential_unavailable")?, true)?;
        }
        Err(_) => return Err("credential_unavailable"),
    }
    Ok(dir)
}
/// Read one bounded Unix bundle, checking ownership and file identity before using its contents.
pub fn read(root: &Path, id: &str) -> Result<Bundle, &'static str> {
    if !cfg!(unix) {
        return Err("credential_unavailable");
    }
    directory(root, false)?;
    let path = path(root, id)?;
    let before = fs::symlink_metadata(&path).map_err(|_| "credential_unavailable")?;
    private(&before, false)?;
    let file = File::open(path).map_err(|_| "credential_unavailable")?;
    let after = file.metadata().map_err(|_| "credential_unavailable")?;
    private(&after, false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err("insecure_credential_file");
        }
    }
    let mut bytes = Vec::new();
    file.take((MAX_BUNDLE + 1) as u64).read_to_end(&mut bytes).map_err(|_| "credential_unavailable")?;
    if bytes.len() > MAX_BUNDLE {
        return Err("credential_too_large");
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid_credential")
}
/// Persist directory-entry changes on Unix after a bundle is replaced or removed.
fn sync(dir: &Path) -> Result<(), &'static str> {
    #[cfg(unix)]
    File::open(dir).and_then(|file| file.sync_all()).map_err(|_| "credential_unavailable")?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
/// Atomically persist a complete URL/token bundle; refuse writes without Unix permission protection.
pub fn save(root: &Path, id: &str, bundle: &Bundle) -> Result<(), &'static str> {
    // Managed writes fail closed where this implementation cannot enforce private ACLs.
    if !cfg!(unix) {
        return Err("managed_credentials_unsupported_platform");
    }
    let path = path(root, id)?;
    let dir = directory(root, true)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => private(&metadata, false)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("credential_unavailable"),
    }
    let bytes = serde_json::to_vec(bundle).map_err(|_| "invalid_credential")?;
    if bytes.len() > MAX_BUNDLE {
        return Err("credential_too_large");
    }
    let temp = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|_| "credential_unavailable")?;
        file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|_| "credential_unavailable")?;
        fs::rename(&temp, &path).map_err(|_| "credential_unavailable")?;
        sync(&dir)?;
        sync(root)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
/// Delete only a validated private bundle; an already absent bundle is a successful removal.
pub fn remove(root: &Path, id: &str) -> Result<(), &'static str> {
    if !cfg!(unix) {
        return Err("managed_credentials_unsupported_platform");
    }
    let path = path(root, id)?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("credential_unavailable"),
        Ok(metadata) => private(&metadata, false)?,
    }
    let dir = directory(root, false)?;
    fs::remove_file(path).map_err(|_| "credential_unavailable")?;
    sync(&dir)
}
