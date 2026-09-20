//! De-duplication: one task, one session, one tab — and the idle gate that decides when the
//! runtime is allowed to do anything disruptive.

use async_trait::async_trait;
use jev_browser_relay_browser::state;
use jev_browser_relay_core::browser::BrowserBackend;
use jev_browser_relay_core::config::RuntimeConfig;
use jev_browser_relay_core::registry::{new_session_id, SessionRegistry};
use jev_browser_relay_core::session::{task_fingerprint, RunBudget, Session};
use jev_browser_relay_core::snapshot::Operation;
use jev_browser_relay_core::testing::{Planned, ScriptedJev};
use jev_browser_relay_core::{RelayError, Result};
use jev_browser_relay_mcp::{BackendFactory, RelayServer};
use jev_browser_relay_tests::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// Counts how many browsers were created, and what `idle` it was told each time.
struct CountingFactory {
    fixture: String,
    created: AtomicU32,
    saw_idle: AtomicBool,
    saw_busy: AtomicBool,
}

impl CountingFactory {
    fn new(fixture: &str) -> Arc<Self> {
        Arc::new(Self {
            fixture: fixture.to_string(),
            created: AtomicU32::new(0),
            saw_idle: AtomicBool::new(false),
            saw_busy: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl BackendFactory for CountingFactory {
    async fn create(&self, _url: &str, idle: bool) -> Result<Box<dyn BrowserBackend>> {
        self.created.fetch_add(1, Ordering::SeqCst);
        if idle {
            self.saw_idle.store(true, Ordering::SeqCst)
        } else {
            self.saw_busy.store(true, Ordering::SeqCst)
        }
        Ok(Box::new(jev_browser_relay_browser::ScriptedBackend::from_json(&self.fixture)?))
    }

    fn describe(&self) -> String {
        "counting".into()
    }
}

fn server(factory: Arc<CountingFactory>, plan: Vec<Planned>) -> RelayServer {
    RelayServer::new(
        Arc::new(ScriptedJev::new(plan)),
        factory,
        RuntimeConfig { today: "2026-09-20".into(), ..test_config() },
    )
}

async fn tool(server: &RelayServer, id: u32, name: &str, arguments: Value) -> Value {
    let frame = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                        "params": { "name": name, "arguments": arguments } })
    .to_string();
    let response = server.handle_line(&frame).await.expect("a response");
    serde_json::to_value(&response).unwrap()["result"]["structuredContent"].clone()
}

const START_ARGS: fn() -> Value = || {
    json!({
        "url": "https://fixture.test/docs",
        "goal": "Open the authentication guide",
        "context": { "topic": "rotating an API token" }
    })
};

#[tokio::test]
async fn starting_the_same_task_twice_reuses_the_live_session() {
    let factory = CountingFactory::new(DOCS);
    let server = server(factory.clone(), vec![]);

    let first = tool(&server, 1, "jev_browser_start", START_ARGS()).await;
    let second = tool(&server, 2, "jev_browser_start", START_ARGS()).await;

    assert_eq!(first["reused"], false);
    assert_eq!(second["reused"], true, "the second start should have resumed, not restarted");
    assert_eq!(first["session_id"], second["session_id"]);
    // The thing that actually matters: no second browser tab was opened.
    assert_eq!(factory.created.load(Ordering::SeqCst), 1, "a duplicate task must not open a second tab");
    assert!(second["next"].as_str().unwrap().contains("already open"));
}

#[tokio::test]
async fn a_paused_task_is_resumed_rather_than_abandoned() {
    // The failure this exists to prevent: an agent hits needs_input, gets confused, and starts
    // over — stranding the paused session and its tab.
    let factory = CountingFactory::new(FLIGHTS);
    let server = server(
        factory.clone(),
        vec![
            Planned::targeting(Operation::TypeText, "Where from?"),
            Planned::targeting(Operation::TypeText, "Where to?"),
        ],
    );
    let args = json!({
        "url": "https://fixture.test/flights",
        "goal": "Find one-way flights from Sydney",
        "context": { "origin": "Sydney" }
    });

    let started = tool(&server, 1, "jev_browser_start", args.clone()).await;
    let session_id = started["session_id"].as_str().unwrap().to_string();
    let paused = tool(&server, 2, "jev_browser_run", json!({ "session_id": session_id })).await;
    assert_eq!(paused["status"], "needs_input");

    // The agent "starts over".
    let again = tool(&server, 3, "jev_browser_start", args).await;

    assert_eq!(again["reused"], true);
    assert_eq!(again["session_id"], session_id.as_str());
    // Crucially it comes back still paused, so the agent is pointed at the pending request
    // rather than being handed a fresh task that silently lost the work.
    assert_eq!(again["status"], "needs_input");
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn different_tasks_get_their_own_sessions() {
    let factory = CountingFactory::new(DOCS);
    let server = server(factory.clone(), vec![]);

    let first = tool(&server, 1, "jev_browser_start", START_ARGS()).await;
    let different_goal = tool(
        &server,
        2,
        "jev_browser_start",
        json!({ "url": "https://fixture.test/docs", "goal": "Find the deployment guide instead" }),
    )
    .await;

    assert_ne!(first["session_id"], different_goal["session_id"]);
    assert_eq!(different_goal["reused"], false);
    assert_eq!(factory.created.load(Ordering::SeqCst), 2, "genuinely different tasks must not be merged");
}

#[tokio::test]
async fn force_new_opts_out_of_reuse() {
    let factory = CountingFactory::new(DOCS);
    let server = server(factory.clone(), vec![]);

    let first = tool(&server, 1, "jev_browser_start", START_ARGS()).await;
    let mut forced = START_ARGS();
    forced["force_new"] = json!(true);
    let second = tool(&server, 2, "jev_browser_start", forced).await;

    assert_ne!(first["session_id"], second["session_id"]);
    assert_eq!(second["reused"], false);
    assert_eq!(factory.created.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_finished_task_is_not_resumed() {
    // A completed task is not one to continue. Handing it back would look like the new request
    // silently did nothing.
    let factory = CountingFactory::new(DOCS);
    let server = server(
        factory.clone(),
        vec![Planned::targeting(Operation::Click, "Guides"), Planned::new(Operation::Done)],
    );

    let first = tool(&server, 1, "jev_browser_start", START_ARGS()).await;
    let session_id = first["session_id"].as_str().unwrap().to_string();
    let done = tool(&server, 2, "jev_browser_run", json!({ "session_id": session_id })).await;
    assert_eq!(done["status"], "done");

    let again = tool(&server, 3, "jev_browser_start", START_ARGS()).await;

    assert_eq!(again["reused"], false, "a finished task should start fresh");
    assert_ne!(again["session_id"], session_id.as_str());
    assert_eq!(factory.created.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn the_idle_flag_tracks_whether_a_task_is_in_flight() {
    // The idle flag gates upgrading the browser layer and reclaiming stray tabs — both of which
    // would disrupt a task already running.
    let factory = CountingFactory::new(DOCS);
    let server = server(factory.clone(), vec![]);

    tool(&server, 1, "jev_browser_start", START_ARGS()).await;
    assert!(factory.saw_idle.load(Ordering::SeqCst), "the first session starts on an idle runtime");
    assert!(!factory.saw_busy.load(Ordering::SeqCst));

    // A second, different task while the first is live must report busy.
    tool(
        &server,
        2,
        "jev_browser_start",
        json!({ "url": "https://fixture.test/docs", "goal": "A different job entirely" }),
    )
    .await;
    assert!(factory.saw_busy.load(Ordering::SeqCst), "a live session means the runtime is not idle");
}

#[tokio::test]
async fn the_registry_reports_idle_only_when_nothing_is_live() {
    let registry = SessionRegistry::new();
    assert!(registry.is_idle().await, "an empty registry is idle");

    let jev = Arc::new(ScriptedJev::new(vec![Planned::new(Operation::Done)]));
    let backend = jev_browser_relay_browser::ScriptedBackend::from_json(DOCS).unwrap();
    let id = new_session_id();
    let session =
        Session::start(id.clone(), "Open guides".into(), None, jev, Box::new(backend), test_config())
            .await
            .unwrap();
    let handle = registry.insert(session).await;

    assert!(!registry.is_idle().await, "a ready session is live work");

    // Run it to completion; a terminal session no longer holds anything.
    handle.lock().await.run(RunBudget { max_steps: 5, max_duration_ms: 5_000 }).await;
    assert!(registry.is_idle().await, "a finished session does not keep the runtime busy");
}

#[tokio::test]
async fn task_fingerprints_ignore_cosmetic_differences_only() {
    // Trailing slash, case and whitespace are the same task.
    assert_eq!(
        task_fingerprint("https://Example.test/Flights/", "Find  flights\n to Tokyo"),
        task_fingerprint("https://example.test/flights", "find flights to tokyo")
    );
    // A different goal, or a different page, is a different task.
    assert_ne!(
        task_fingerprint("https://example.test/a", "same goal"),
        task_fingerprint("https://example.test/b", "same goal")
    );
    assert_ne!(
        task_fingerprint("https://example.test/a", "goal one"),
        task_fingerprint("https://example.test/a", "goal two")
    );
}

#[tokio::test]
async fn tab_ownership_is_recorded_and_released() {
    // The record is what lets a later run tell a leaked tab from one in use.
    let target = format!("test-target-{}", std::process::id());
    state::record_target(&target, "https://fixture.test/owned");

    let mine: Vec<_> = state::load().targets.into_iter().filter(|t| t.target_id == target).collect();
    assert_eq!(mine.len(), 1, "the tab should be recorded exactly once");
    assert_eq!(mine[0].owner_pid, std::process::id());

    // Recording the same target again must not duplicate the entry.
    state::record_target(&target, "https://fixture.test/owned");
    assert_eq!(state::load().targets.iter().filter(|t| t.target_id == target).count(), 1);

    // A tab owned by a live process is never an orphan — including this one's.
    assert!(!state::orphaned_targets().iter().any(|t| t.target_id == target));

    state::forget_target(&target);
    assert!(!state::load().targets.iter().any(|t| t.target_id == target));
}

#[tokio::test]
async fn a_dead_owner_makes_its_tabs_reclaimable() {
    // pid 1 is alive; a very high pid almost certainly is not. Use the runtime's own liveness
    // check rather than asserting on a specific number.
    assert!(state::is_pid_alive(1), "init is always running");
    assert!(!state::is_pid_alive(0), "pid 0 is never a reclaimable owner");

    let dead_pid = (1..u32::MAX).rev().take(4_000_000).find(|p| !state::is_pid_alive(*p));
    let Some(dead_pid) = dead_pid else { return };

    let target = format!("orphan-target-{dead_pid}");
    state::update(|s| {
        s.targets.retain(|t| t.target_id != target);
        s.targets.push(state::OwnedTarget {
            target_id: target.clone(),
            owner_pid: dead_pid,
            url: "https://fixture.test/leaked".into(),
            created_at_epoch_ms: state::now_ms(),
        });
    });

    assert!(
        state::orphaned_targets().iter().any(|t| t.target_id == target),
        "a tab whose owner is gone is a leak and must be reclaimable"
    );

    state::forget_target(&target);
}

#[tokio::test]
async fn a_backend_that_cannot_start_surfaces_the_install_instruction() {
    // When the browser layer is missing entirely, the error has to say what to run — that is
    // the whole point of one-command setup.
    struct Missing;
    #[async_trait]
    impl BackendFactory for Missing {
        async fn create(&self, _url: &str, _idle: bool) -> Result<Box<dyn BrowserBackend>> {
            Err(RelayError::Config(
                "Browser Harness is not installed. Install it with:\n    uv tool install browser-harness\n  \
                 or run `jev-browser-relay setup`, which does it for you."
                    .into(),
            ))
        }
        fn describe(&self) -> String {
            "missing".into()
        }
    }

    let server = RelayServer::new(Arc::new(ScriptedJev::new(vec![])), Arc::new(Missing), test_config());
    let result = tool(&server, 1, "jev_browser_start", START_ARGS()).await;

    assert_eq!(result["error"]["code"], "config_error");
    assert!(result["error"]["message"].as_str().unwrap().contains("jev-browser-relay setup"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_writers_never_corrupt_or_lose_the_state_file() {
    // The regression this guards: an earlier version staged every write through one shared
    // temp path, so two overlapping writers interleaved into it and the rename published the
    // mixture. The file came back as invalid JSON, and `load` silently started over — throwing
    // away every recorded tab, which is the leak this module exists to prevent.
    const WRITERS: u32 = 24;
    let tag = format!("stress-{}", std::process::id());

    let mut handles = Vec::new();
    for n in 0..WRITERS {
        let target = format!("{tag}-{n}");
        handles.push(tokio::spawn(async move {
            state::record_target(&target, "https://fixture.test/stress");
        }));
    }
    for handle in handles {
        handle.await.expect("writer did not panic");
    }

    // The file must still parse. A corrupt file reads as default, so an empty result here is
    // the corruption signature, not an absence of work.
    let raw = std::fs::read_to_string(state::state_dir().join("state.json")).expect("state file exists");
    serde_json::from_str::<serde_json::Value>(&raw).expect("state file must still be valid JSON");

    let recorded = state::load();
    let mine: Vec<_> = recorded.targets.iter().filter(|t| t.target_id.starts_with(&tag)).collect();
    assert_eq!(mine.len() as u32, WRITERS, "every concurrent write must survive; none may be lost");

    // No temp files left behind.
    let strays: Vec<_> = std::fs::read_dir(state::state_dir())
        .expect("state dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(strays.is_empty(), "temp files were left behind: {strays:?}");

    for n in 0..WRITERS {
        state::forget_target(&format!("{tag}-{n}"));
    }
}
