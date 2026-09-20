//! Getting the browser layer running without making the user do it by hand.
//!
//! Browser Harness ships an idempotent, self-healing `ensure_daemon()`: it starts the daemon,
//! launches Chrome if it is closed, replaces a stale daemon, and opens
//! `chrome://inspect#remote-debugging` when the Allow box has not been ticked. Any
//! `browser-harness` invocation triggers it, so the runtime does not reimplement any of that —
//! it just asks for it.
//!
//! The line this module holds: **starting** something the user already installed is expected and
//! happens automatically; **installing** new software is not, and only ever happens from
//! `jev-browser-relay setup`, which says what it is about to do.

use crate::state;
use jev_browser_relay_core::error::{RelayError, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Serializes provisioning across every task in this process.
///
/// Without it, two sessions starting at once both see "no daemon" and both spawn one. Browser
/// Harness has its own cross-process spawn lock, so the duplicate would be reaped — but only
/// after paying for a subprocess and a Chrome launch twice. This is the cheap half of the
/// guard: the half that stops *us* being the source of the duplication.
static PROVISION_LOCK: Mutex<()> = Mutex::const_new(());

/// How long a Browser Harness update check is considered current.
const UPDATE_CHECK_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;

/// What `ensure` had to do to get a working browser layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// A daemon was already running and answering. The common case, and the cheap one.
    Reused,
    /// Nothing was running, so Browser Harness was asked to start.
    Started,
    /// An out-of-date Browser Harness was upgraded first, because nothing was using it.
    Upgraded,
}

impl ProvisionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reused => "reused",
            Self::Started => "started",
            Self::Upgraded => "upgraded",
        }
    }
}

/// Where a `uv tool install` or `pip install --user` puts console scripts. Checked because a
/// freshly installed `browser-harness` is often not on the PATH of the already-running process.
fn extra_bin_dirs() -> Vec<std::path::PathBuf> {
    let home = std::env::var("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    vec![home.join(".local/bin"), home.join("bin")]
}

/// The `browser-harness` executable, if it can be found.
pub fn harness_binary() -> Option<std::path::PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("browser-harness");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    extra_bin_dirs().into_iter().map(|dir| dir.join("browser-harness")).find(|p| p.is_file())
}

pub fn is_harness_installed() -> bool {
    harness_binary().is_some()
}

/// How this machine can install Browser Harness, best option first.
pub fn install_plan() -> Option<(String, Vec<String>)> {
    let candidates: [(&str, &[&str]); 3] = [
        ("uv", &["tool", "install", "browser-harness"]),
        ("pipx", &["install", "browser-harness"]),
        ("pip3", &["install", "--user", "browser-harness"]),
    ];
    for (program, args) in candidates {
        if which(program).is_some() {
            return Some((program.to_string(), args.iter().map(|s| s.to_string()).collect()));
        }
    }
    None
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(program)).find(|p| p.is_file())
}

/// Get to a working browser layer, doing as little as possible.
///
/// Reuse what is running, start it if it is not, and upgrade it only when upgrading costs
/// nobody anything. Every front end goes through this one function: a second implementation of
/// "start one if none is running" is a second way to end up with two.
///
/// `idle` says whether this runtime currently holds any browser session. It gates the upgrade
/// and nothing else — see [`upgrade_if_idle`].
pub async fn ensure(timeout: Duration, idle: bool) -> Result<ProvisionOutcome> {
    let ipc = crate::HarnessIpc::new();
    // Unlocked fast path: the overwhelmingly common case is a daemon that is already up, and it
    // should not queue behind anybody.
    if ipc.is_available().await {
        if idle {
            if let Some(outcome) = upgrade_if_idle(timeout).await {
                return Ok(outcome);
            }
        }
        return Ok(ProvisionOutcome::Reused);
    }

    let _guard = PROVISION_LOCK.lock().await;
    // Re-check under the lock: while we waited, whoever held it may have started the daemon,
    // and starting a second one is exactly what this guard exists to prevent.
    if ipc.is_available().await {
        return Ok(ProvisionOutcome::Reused);
    }

    ensure_daemon(timeout).await?;
    Ok(ProvisionOutcome::Started)
}

/// Replace an out-of-date Browser Harness, but only while nothing is using it.
///
/// **Why this is conditional rather than automatic.** Upgrading restarts the daemon, which drops
/// its CDP connection and every tab the runtime is driving through it. Upgrading on sight would
/// mean an agent that happened to start a session silently killing a browser task already in
/// flight — possibly someone else's, since the daemon is machine-global and shared with other
/// Browser Use tools.
///
/// Idle is the case where that objection disappears: nothing is running, so nothing is lost, and
/// the upgrade costs a second of startup nobody notices. Anything in flight and this declines.
///
/// Returns `None` for every reason not to act — recently checked, no installer, already current,
/// the upgrade failed. **A failure here is never fatal**, because the version already installed
/// still works, and refusing to run because an optional upgrade did not happen would be worse
/// than being one version behind.
pub async fn upgrade_if_idle(timeout: Duration) -> Option<ProvisionOutcome> {
    let state = state::load();
    let now = state::now_ms();
    if now.saturating_sub(state.last_update_check_epoch_ms) < UPDATE_CHECK_INTERVAL_MS {
        return None;
    }
    // Record the attempt before making it. A check that hangs or fails must not cause another
    // check on the very next call.
    state::update(|s| s.last_update_check_epoch_ms = now);

    let binary = harness_binary()?;
    debug!("checking for a Browser Harness update");

    // `--update -y` is Browser Harness's own updater; it is a no-op when already current.
    let run = Command::new(&binary)
        .arg("--update")
        .arg("-y")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();

    let output = match tokio::time::timeout(timeout, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            debug!(error = %error, "update check could not run; continuing on the installed version");
            return None;
        }
        Err(_) => {
            warn!("update check timed out; continuing on the installed version");
            return None;
        }
    };

    if !output.status.success() {
        debug!("update check reported a failure; continuing on the installed version");
        return None;
    }

    let combined =
        format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    // Only claim an upgrade when one actually happened, so the outcome stays honest.
    let changed = ["updated", "upgrading", "installed", "new version"]
        .iter()
        .any(|marker| combined.to_lowercase().contains(marker))
        && !combined.to_lowercase().contains("already up to date");
    if !changed {
        return None;
    }

    info!("upgraded Browser Harness while idle");
    Some(ProvisionOutcome::Upgraded)
}

/// Close tabs left behind by a jev-browser-relay process that is no longer running.
///
/// A crashed or killed runtime cannot close its own tabs, so they sit in Chrome forever. Each
/// one is recorded with the pid that opened it; when that pid is gone, the tab is a leak and is
/// safe to reclaim. Tabs belonging to a *live* process are never touched, including this one's.
///
/// Best-effort throughout: reclamation failing is not a reason to refuse to start.
pub async fn reclaim_orphaned_tabs() -> usize {
    let orphans = state::orphaned_targets();
    if orphans.is_empty() {
        return 0;
    }
    let ipc = crate::HarnessIpc::new();
    if !ipc.is_available().await {
        return 0;
    }

    let mut reclaimed = 0;
    for orphan in orphans {
        let closed =
            ipc.cdp("Target.closeTarget", None, serde_json::json!({ "targetId": orphan.target_id })).await;
        // Forget it either way: a tab that cannot be closed is usually one Chrome already
        // closed, and retrying it on every start would never stop.
        state::forget_target(&orphan.target_id);
        if closed.is_ok() {
            reclaimed += 1;
            debug!(target = %orphan.target_id, url = %orphan.url, "reclaimed an orphaned tab");
        }
    }
    if reclaimed > 0 {
        info!(reclaimed, "closed tabs left behind by earlier runs");
    }
    reclaimed
}

/// Ask Browser Harness to make itself ready: daemon up, Chrome attached.
///
/// Browser Harness runs its own `ensure_daemon()` — which starts the daemon, launches Chrome if
/// it is closed, replaces a stale daemon, and opens `chrome://inspect#remote-debugging` when the
/// Allow box has not been ticked — on exactly one path: when it is given a script to run on
/// stdin. `doctor` deliberately only *reports*, so asking it to fix anything is asking the wrong
/// question. A `pass` is therefore the whole script: the provisioning is the side effect, and
/// this runtime verifies the result itself by pinging the socket rather than trusting the exit
/// code.
///
/// The wait is bounded because Chrome may be sitting on an approval sheet, and a hang here would
/// look like the runtime being broken rather than Chrome waiting for a click.
pub async fn ensure_daemon(timeout: Duration) -> Result<()> {
    let Some(binary) = harness_binary() else {
        return Err(RelayError::Config(match install_plan() {
            Some((program, args)) => format!(
                "Browser Harness is not installed. Install it with:\n    {program} {}\n  \
                 or run `jev-browser-relay setup`, which does it for you.",
                args.join(" ")
            ),
            None => "Browser Harness is not installed, and no installer (uv, pipx, pip3) was found \
                     on this machine. Install uv from https://docs.astral.sh/uv/ first."
                .into(),
        }));
    };

    info!("starting the Browser Harness daemon");
    let mut child = Command::new(&binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| RelayError::Config(format!("could not run {}: {error}", binary.display())))?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        // The script is a no-op on purpose; closing stdin is what makes it run.
        let _ = stdin.write_all(b"pass\n").await;
        let _ = stdin.shutdown().await;
        drop(stdin);
    }

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(RelayError::Config(format!("could not run {}: {error}", binary.display())))
        }
        Err(_) => {
            return Err(RelayError::BrowserDisconnected(format!(
                "Browser Harness did not become ready within {}s. This usually means Chrome is \
                 waiting for you: open chrome://inspect/#remote-debugging and tick the box that \
                 allows remote debugging, then try again.",
                timeout.as_secs()
            )))
        }
    };

    if output.status.success() {
        debug!("browser-harness reported ready");
        return Ok(());
    }

    // The harness writes setup and permission problems to stderr as instructions for the calling
    // agent. Pass them through rather than replacing them with something vaguer.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    let detail: String = if detail.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().chars().take(400).collect()
    } else {
        detail.chars().take(400).collect()
    };
    Err(RelayError::BrowserDisconnected(format!(
        "Browser Harness could not connect to Chrome.\n  {detail}\n  \
         Run `browser-harness doctor` for the full diagnosis."
    )))
}
