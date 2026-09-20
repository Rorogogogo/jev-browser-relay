//! `doctor` — check everything needed for a real run, and print nothing secret.

use jev_browser_relay_browser::harness::{self, HarnessIpc};
use jev_browser_relay_jev::{TypeSafeClient, API_KEY_ENV};
use serde_json::{json, Value};

pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    pub fix: Option<String>,
}

pub async fn run() -> Vec<Check> {
    let mut checks = Vec::new();

    // 1. The only required model API key. Presence only — never the value, never a prefix.
    let key_present = TypeSafeClient::key_present();
    checks.push(Check {
        name: "typesafe_api_key",
        ok: key_present,
        detail: if key_present {
            format!("{API_KEY_ENV} is set")
        } else {
            format!("{API_KEY_ENV} is not set")
        },
        fix: (!key_present)
            .then(|| format!("export {API_KEY_ENV}=... — it is the only model API key this runtime needs.")),
    });

    // 2. Confirm no second model key is required. This is a project guarantee, so it is asserted.
    let extras: Vec<&str> =
        ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "OPENROUTER_API_KEY", "MERCURY_API_KEY", "GEMINI_API_KEY"]
            .into_iter()
            .filter(|key| std::env::var(key).is_ok())
            .collect();
    checks.push(Check {
        name: "no_second_model_api",
        ok: true,
        detail: if extras.is_empty() {
            "no other model API keys needed or used".into()
        } else {
            format!("{} present in the environment but unused by this runtime", extras.join(", "))
        },
        fix: None,
    });

    // 3. The browser harness daemon.
    let ipc = HarnessIpc::new();
    let socket = harness::socket_path();
    match ipc.ping().await {
        Ok(response) if response.get("pong") == Some(&json!(true)) => {
            let kind = response.get("browser_kind").and_then(Value::as_str).unwrap_or("chrome");
            checks.push(Check {
                name: "browser_harness_daemon",
                ok: true,
                detail: format!("running ({kind}) at {}", socket.display()),
                fix: None,
            });

            match ipc.connection_status().await {
                Ok(status) => {
                    let page = status.get("page").and_then(Value::as_str).unwrap_or("");
                    checks.push(Check {
                        name: "chrome_connection",
                        ok: status.get("session_id").is_some(),
                        detail: if page.is_empty() {
                            "attached to Chrome".into()
                        } else {
                            format!("attached to Chrome, current page: {page}")
                        },
                        fix: None,
                    });
                }
                Err(error) => checks.push(Check {
                    name: "chrome_connection",
                    ok: false,
                    detail: error.to_string(),
                    fix: Some(
                        "Run `browser-harness --doctor` and allow remote debugging when Chrome asks.".into(),
                    ),
                }),
            }
        }
        result => {
            let detail = match result {
                Err(error) => error.to_string(),
                Ok(_) => "something is listening but it is not the harness daemon".into(),
            };
            checks.push(Check {
                name: "browser_harness_daemon",
                ok: false,
                detail: format!("not reachable at {}: {detail}", socket.display()),
                fix: Some(
                    "Install and connect Browser Harness:\n      uv tool install browser-harness\n      \
                     browser-harness --doctor\n    Then tick the box on chrome://inspect/#remote-debugging."
                        .into(),
                ),
            });
            checks.push(Check {
                name: "chrome_connection",
                ok: false,
                detail: "skipped: the daemon is not reachable".into(),
                fix: None,
            });
        }
    }

    // 4. The scripted backend always works, so `run --fixture` and the benchmarks are available
    //    even with nothing else installed.
    checks.push(Check {
        name: "offline_mode",
        ok: true,
        detail: "scripted backend available: `benchmark` and `run --fixture` need no Chrome".into(),
        fix: None,
    });

    checks
}

pub fn print(checks: &[Check]) -> bool {
    let mut all_ok = true;
    println!("jev-browser-relay doctor\n");
    for check in checks {
        let mark = if check.ok { "ok  " } else { "FAIL" };
        println!("  [{mark}] {:<24} {}", check.name, check.detail);
        if let Some(fix) = &check.fix {
            println!("         → {fix}");
        }
        all_ok &= check.ok;
    }
    println!();
    if all_ok {
        println!("Ready. Connect it to Claude Code with:");
        println!("  claude mcp add jev-browser-relay -- jev-browser-relay mcp");
    } else {
        println!("Some checks failed. The runtime still works offline:");
        println!("  jev-browser-relay benchmark --trials 3");
    }
    all_ok
}
