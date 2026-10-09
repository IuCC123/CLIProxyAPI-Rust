//! Private, bounded crash-durable state and outbox; never contains credentials.
use super::{
    state::{Event, Subscription},
    valid_id,
};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
pub const MAX_PENDING: usize = 2048;
#[derive(Clone, Serialize, Deserialize)]
pub struct Pending {
    pub destination: String,
    pub event: Event,
    pub attempt: u8,
    pub next: DateTime<Utc>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Log {
    pub timestamp: DateTime<Utc>,
    pub destination: String,
    pub event: String,
    pub subscription: Option<String>,
    pub window: Option<String>,
    pub attempt: u8,
    pub outcome: String,
    pub http_status: Option<u16>,
    pub detail: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Journal {
    pub installation: String,
    pub subscriptions: BTreeMap<String, Subscription>,
    pub pending: VecDeque<Pending>,
    pub logs: VecDeque<Log>,
}
impl Default for Journal {
    /// Create an empty credential-free journal with a fresh installation-local identity salt.
    fn default() -> Self {
        Self {
            installation: uuid::Uuid::new_v4().to_string(),
            subscriptions: BTreeMap::new(),
            pending: VecDeque::new(),
            logs: VecDeque::new(),
        }
    }
}
pub struct Store {
    _lock: File,
    path: PathBuf,
    pub journal: Journal,
}
/// Open a bounded regular secret file and verify Unix ownership, permissions, and file identity.
pub fn safe_file(path: &Path) -> Result<File, &'static str> {
    let before = fs::symlink_metadata(path).map_err(|_| "credential_unavailable")?;
    if !before.is_file() || before.file_type().is_symlink() || before.len() > 8192 {
        return Err("insecure_credential_file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Root-owned read-only container secrets or secrets owned by this process.
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        // POSIX geteuid cannot fail and is available on supported Unix platforms.
        let owner = unsafe { geteuid() };
        if before.mode() & 0o077 != 0 || (before.uid() != 0 && before.uid() != owner) {
            return Err("insecure_credential_file");
        }
    }
    let file = File::open(path).map_err(|_| "credential_unavailable")?;
    let after = file.metadata().map_err(|_| "credential_unavailable")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err("insecure_credential_file");
        }
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        if after.mode() & 0o077 != 0 || (after.uid() != 0 && after.uid() != unsafe { geteuid() }) {
            return Err("insecure_credential_file");
        }
    }
    if !after.is_file() || after.len() > 8192 {
        return Err("insecure_credential_file");
    }
    Ok(file)
}
/// Create read/write state files with private Unix permissions.
fn options() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts
}
/// Reject symlinked or incorrectly permissioned state paths before opening them.
fn private(path: &Path, dir: bool) -> Result<(), &'static str> {
    let meta = fs::symlink_metadata(path).map_err(|_| "state_unavailable")?;
    if meta.file_type().is_symlink() || (dir && !meta.is_dir()) || (!dir && !meta.is_file()) {
        return Err("insecure_state_path");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.mode() & 0o077 != 0 {
            return Err("insecure_state_permissions");
        }
        fs::set_permissions(path, fs::Permissions::from_mode(if dir { 0o700 } else { 0o600 }))
            .map_err(|_| "state_unavailable")?;
    }
    Ok(())
}
impl Store {
    /// Lock one auth directory exclusively, validate its journal, and persist initialization before use.
    pub fn open(root: &Path) -> Result<Self, &'static str> {
        let dir = root.join(".quota-notifications");
        if !dir.exists() {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&dir).map_err(|_| "state_unavailable")?;
        }
        private(&dir, true)?;
        let lock_path = dir.join("owner.lock");
        if lock_path.exists() {
            private(&lock_path, false)?;
        }
        let lock = options().open(&lock_path).map_err(|_| "state_unavailable")?;
        lock.try_lock_exclusive().map_err(|_| "another_monitor_owns_state")?;
        let path = dir.join("state.json");
        let journal = if path.exists() {
            private(&path, false)?;
            let file = File::open(&path).map_err(|_| "state_unavailable")?;
            if file.metadata().map_err(|_| "state_unavailable")?.len() > 8 << 20 {
                return Err("state_too_large");
            }
            serde_json::from_reader(file.take((8 << 20) + 1)).map_err(|_| "state_invalid")?
        } else {
            Journal::default()
        };
        validate(&journal)?;
        let store = Self { _lock: lock, path, journal };
        store.save()?;
        Ok(store)
    }
    /// Atomically replace and sync the bounded journal before its deliveries can be dispatched.
    pub fn save(&self) -> Result<(), &'static str> {
        let dir = self.path.parent().ok_or("state_unavailable")?;
        let tmp = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let bytes = serde_json::to_vec(&self.journal).map_err(|_| "state_invalid")?;
            if bytes.len() > 8 << 20 {
                return Err("state_too_large");
            }
            let mut file = options().create_new(true).open(&tmp).map_err(|_| "state_unavailable")?;
            file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|_| "state_unavailable")?;
            #[cfg(not(windows))]
            fs::rename(&tmp, &self.path).map_err(|_| "state_unavailable")?;
            #[cfg(windows)]
            replace_windows(&tmp, &self.path)?;
            #[cfg(unix)]
            File::open(dir).and_then(|f| f.sync_all()).map_err(|_| "state_unavailable")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }
    /// Retain only the latest 200 sanitized delivery records.
    pub fn log(&mut self, log: Log) {
        self.journal.logs.push_back(log);
        while self.journal.logs.len() > 200 {
            self.journal.logs.pop_front();
        }
    }
}
/// Reject oversized journals and unexpected identifiers, event types, windows, or diagnostic fields.
fn validate(j: &Journal) -> Result<(), &'static str> {
    let opaque = |s: &str| s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let window = |s: &str| {
        matches!(
            s,
            "5h" | "week"
                | "week opus"
                | "week sonnet"
                | "week overage"
                | "day"
                | "unknown"
                | "unknown opus"
                | "unknown sonnet"
                | "all"
                | "test"
        )
    };
    let event =
        |s: &str| matches!(s, "quota.exhausted" | "quota.window_recovered" | "quota.available" | "notification.test");
    if uuid::Uuid::parse_str(&j.installation).is_err()
        || j.pending.len() > MAX_PENDING
        || j.logs.len() > 200
        || j.subscriptions.len() > 4096
    {
        return Err("state_invalid");
    }
    for (id, sub) in &j.subscriptions {
        if !opaque(id) || !matches!(sub.provider.as_str(), "claude" | "codex") || sub.windows.len() > 16 {
            return Err("state_invalid");
        }
        for w in sub.windows.values() {
            if !window(&w.name)
                || w.model.as_deref().is_some_and(|m| !matches!(m, "opus" | "sonnet"))
                || w.used.is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v))
            {
                return Err("state_invalid");
            }
        }
    }
    for p in &j.pending {
        let e = &p.event;
        if !valid_id(&p.destination)
            || !opaque(&e.subscription)
            || !event(&e.event)
            || !window(&e.window)
            || uuid::Uuid::parse_str(&e.id).is_err()
            || !matches!(e.provider.as_str(), "claude" | "codex")
            || e.model.as_deref().is_some_and(|m| !matches!(m, "opus" | "sonnet"))
            || e.remaining_blockers.len() > 16
            || e.remaining_blockers.iter().any(|w| !window(w))
            || p.attempt > 8
            || e.version != 1
            || e.used.is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v))
        {
            return Err("state_invalid");
        }
    }
    for l in &j.logs {
        if !valid_id(&l.destination)
            || !event(&l.event)
            || l.subscription.as_deref().is_some_and(|s| !opaque(s))
            || l.window.as_deref().is_some_and(|s| !window(s))
            || !matches!(l.outcome.as_str(), "delivered" | "retrying" | "terminal_failure" | "cancelled" | "expired")
            || !matches!(
                l.detail.as_str(),
                "accepted"
                    | "credential_unavailable"
                    | "insecure_credential_file"
                    | "insecure_credential_directory"
                    | "credential_too_large"
                    | "invalid_credential"
                    | "invalid_url"
                    | "unsafe_url"
                    | "invalid_bearer"
                    | "invalid_destination"
                    | "dns_timeout"
                    | "dns_failed"
                    | "blocked_address"
                    | "transport_unavailable"
                    | "network_error"
                    | "http_rejected"
                    | "response_too_large"
                    | "response_read_failed"
                    | "acknowledgement_failed"
                    | "superseded"
                    | "retention_expired"
                    | "ca_unavailable"
                    | "invalid_ca_file"
            )
        {
            return Err("state_invalid");
        }
    }
    Ok(())
}
#[cfg(windows)]
/// Use a write-through Windows replacement so existing journals can be updated atomically.
fn replace_windows(from: &Path, to: &Path) -> Result<(), &'static str> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 1 | 8) } == 0 { Err("state_unavailable") } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        /// Create an isolated temporary test directory without using production credentials.
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("notifications-store-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        /// Release the test task or remove its temporary files when the fixture leaves scope.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    /// Verify that durable state deduplicates restart and exclusive owner blocks a second monitor.
    fn durable_state_deduplicates_restart_and_exclusive_owner_blocks_a_second_monitor() {
        let temp = Temp::new();
        let mut store = Store::open(&temp.0).unwrap();
        assert!(matches!(Store::open(&temp.0), Err("another_monitor_owns_state")));
        let mut evidence = crate::notifications::state::Evidence::default();
        evidence
            .observe(&[crate::quota::Window { name: "5h".into(), used: 100.0, resets_at: None, model: None }], true);
        let id = "0123456789abcdef01234567";
        let sub = store
            .journal
            .subscriptions
            .entry(id.into())
            .or_insert_with(|| Subscription { provider: "claude".into(), ..Default::default() });
        let events = sub.apply(id, &evidence.observations[0]);
        assert_eq!(events.len(), 1);
        store.journal.pending.push_back(Pending {
            destination: "ops".into(),
            event: events[0].clone(),
            attempt: 2,
            next: Utc::now(),
        });
        let installation = store.journal.installation.clone();
        store.save().unwrap();
        drop(store);
        let mut reopened = Store::open(&temp.0).unwrap();
        assert_eq!(reopened.journal.installation, installation);
        assert_eq!(reopened.journal.pending[0].attempt, 2);
        assert!(reopened.journal.subscriptions.get_mut(id).unwrap().apply(id, &evidence.observations[0]).is_empty());
    }
    #[cfg(unix)]
    #[test]
    /// Verify that credentials require private regular bounded files and reject symlinks.
    fn credentials_require_private_regular_bounded_files_and_reject_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = Temp::new();
        let file = temp.0.join("ops.url");
        fs::write(&file, "secret-url-sentinel").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(safe_file(&file).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(safe_file(&file).is_ok());
        let link = temp.0.join("link.url");
        symlink(&file, &link).unwrap();
        assert!(safe_file(&link).is_err());
        fs::write(&file, vec![b'x'; 8193]).unwrap();
        assert!(safe_file(&file).is_err());
    }
}
