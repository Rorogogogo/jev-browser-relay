//! On-disk runtime state: which browser tabs this runtime owns, and when it last checked for a
//! Browser Harness update.
//!
//! Small, self-healing and never authoritative. Every reader tolerates a missing, truncated or
//! garbage file by starting over, because the cost of a corrupt state file must never be a
//! runtime that will not start.
//!
//! Concurrency is real here, not theoretical: two MCP servers starting at once — Claude Code and
//! Codex, or two editor windows — will read-modify-write this file at the same moment. So every
//! mutation takes an advisory lock, and every write goes through a temp file unique to the
//! writing process. An earlier version shared one temp path and produced a genuinely corrupt
//! file when two writers overlapped.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::debug;

/// `~/.config/jev-browser-relay`, or `$XDG_CONFIG_HOME/jev-browser-relay`.
pub fn state_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".config"));
    base.join("jev-browser-relay")
}

fn home() -> PathBuf {
    std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).map(PathBuf::from).unwrap_or_default()
}

fn ensure_dir() -> Option<PathBuf> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir)
}

/// A Chrome tab this runtime opened, and the process that owns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedTarget {
    pub target_id: String,
    /// The jev-browser-relay process that created it. When that process is gone and the tab is
    /// not, the tab is a leak.
    pub owner_pid: u32,
    pub url: String,
    pub created_at_epoch_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeState {
    #[serde(default)]
    pub targets: Vec<OwnedTarget>,
    /// Unix ms of the last Browser Harness update check, so it happens at most once a day.
    #[serde(default)]
    pub last_update_check_epoch_ms: u64,
}

fn state_path() -> Option<PathBuf> {
    Some(ensure_dir()?.join("state.json"))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn load() -> RuntimeState {
    let Some(path) = state_path() else { return RuntimeState::default() };
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
            debug!(error = %error, "runtime state was unreadable; starting fresh");
            RuntimeState::default()
        }),
        Err(_) => RuntimeState::default(),
    }
}

/// Write atomically, so a concurrent reader never sees half a file.
///
/// The temp file carries this process's pid and a counter, because a shared temp path is not a
/// staging area — it is a second place for two writers to collide, and the rename then publishes
/// whatever mixture they produced.
pub fn save(state: &RuntimeState) {
    let Some(path) = state_path() else { return };
    let Ok(encoded) = serde_json::to_string_pretty(state) else { return };

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_extension(format!("json.{}.{unique}.tmp", std::process::id()));

    if std::fs::write(&temporary, encoded).is_ok() {
        if std::fs::rename(&temporary, &path).is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
    } else {
        let _ = std::fs::remove_file(&temporary);
    }
}

/// Read, modify, write, under an advisory lock held for the whole sequence.
///
/// Without the lock, two runtimes starting together each read the file before either writes, and
/// the second write drops the first one's tab. A dropped record is a tab nothing will ever
/// reclaim — precisely the leak this module exists to prevent — so it is worth the lock.
pub fn update(edit: impl FnOnce(&mut RuntimeState)) {
    let _guard = lock_state();
    let mut state = load();
    edit(&mut state);
    save(&state);
}

/// An advisory lock on the state file, released when dropped — or by the kernel if the process
/// dies holding it, so a crash can never wedge every later run.
#[cfg(unix)]
struct StateLock(std::fs::File);

#[cfg(unix)]
impl Drop for StateLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(unix)]
fn lock_state() -> Option<StateLock> {
    use std::os::unix::io::AsRawFd;
    let path = ensure_dir()?.join("state.lock");
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(path).ok()?;
    // Blocking: the critical section is a small read and write, so waiting is measured in
    // microseconds and is always preferable to dropping somebody's record.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return None;
    }
    Some(StateLock(file))
}

/// Windows has no flock. Unique temp names still prevent a corrupt file; the remaining risk is a
/// lost record, which costs one stray tab rather than a broken runtime.
#[cfg(not(unix))]
fn lock_state() -> Option<()> {
    None
}

pub fn record_target(target_id: &str, url: &str) {
    let record = OwnedTarget {
        target_id: target_id.to_string(),
        owner_pid: std::process::id(),
        url: url.to_string(),
        created_at_epoch_ms: now_ms(),
    };
    update(|state| {
        state.targets.retain(|t| t.target_id != record.target_id);
        state.targets.push(record);
    });
}

pub fn forget_target(target_id: &str) {
    update(|state| state.targets.retain(|t| t.target_id != target_id));
}

/// Is that process still running?
///
/// On Windows this always answers "alive", deliberately: without a cheap liveness check, the
/// safe failure is to leave a tab open rather than to close one that something else is using.
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // Signal 0 tests for existence without delivering anything. EPERM means it exists and
        // belongs to somebody else.
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Tabs recorded by a process that no longer exists.
pub fn orphaned_targets() -> Vec<OwnedTarget> {
    let me = std::process::id();
    load().targets.into_iter().filter(|t| t.owner_pid != me && !is_pid_alive(t.owner_pid)).collect()
}
