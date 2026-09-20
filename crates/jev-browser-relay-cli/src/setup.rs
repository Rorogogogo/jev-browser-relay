//! `setup` — get from a fresh machine to a working MCP server in one command.
//!
//! Everything here is idempotent: running it twice is a no-op plus a report. It is the only
//! place that installs software, and it says what it is about to do before doing it, because
//! installing things on someone's machine is not a decision a browser runtime gets to make
//! silently.

use anyhow::{Context, Result};
use jev_browser_relay_browser::provision;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub struct Step {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

fn step(name: &'static str, ok: bool, detail: impl Into<String>) -> Step {
    let step = Step { name, ok, detail: detail.into() };
    println!("  [{}] {:<22} {}", if step.ok { "ok  " } else { "FAIL" }, step.name, step.detail);
    step
}

/// Run a command, returning (success, combined output).
async fn run(program: &str, args: &[&str], timeout: Duration) -> (bool, String) {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, child).await {
        Ok(Ok(output)) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            (output.status.success(), combined)
        }
        Ok(Err(error)) => (false, error.to_string()),
        Err(_) => (false, format!("{program} timed out")),
    }
}

fn first_line(text: &str) -> String {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(160).collect()
}

pub async fn run_setup(yes: bool, register: bool) -> Result<()> {
    println!("jev-browser-relay setup\n");
    let mut steps = Vec::new();

    // 1. The API key. Never printed, only detected.
    let key_present = jev_browser_relay_jev::TypeSafeClient::key_present();
    steps.push(step(
        "typesafe_api_key",
        key_present,
        if key_present {
            "TYPESAFE_API_KEY is set".to_string()
        } else {
            "TYPESAFE_API_KEY is not set — export it before running a task".to_string()
        },
    ));

    // 2. Browser Harness. The one thing that may need installing.
    if provision::is_harness_installed() {
        steps.push(step("browser_harness", true, "already installed"));
    } else {
        let Some((program, args)) = provision::install_plan() else {
            steps.push(step(
                "browser_harness",
                false,
                "not installed, and no installer (uv, pipx, pip3) was found. \
                 Install uv from https://docs.astral.sh/uv/ first.",
            ));
            summarize(&steps);
            anyhow::bail!("cannot install Browser Harness on this machine");
        };
        let command = format!("{program} {}", args.join(" "));
        if !yes {
            println!("\n  Browser Harness is not installed. This will run:\n    {command}\n");
            if !confirm("  Install it now? [y/N] ")? {
                steps.push(step("browser_harness", false, format!("declined — install it with: {command}")));
                summarize(&steps);
                return Ok(());
            }
        }
        println!("  installing Browser Harness…");
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let (ok, output) = run(&program, &borrowed, Duration::from_secs(300)).await;
        steps.push(step(
            "browser_harness",
            ok,
            if ok { format!("installed via {program}") } else { first_line(&output) },
        ));
        if !ok {
            summarize(&steps);
            anyhow::bail!("Browser Harness install failed");
        }
    }

    // 3. Daemon + Chrome. This is where the one unautomatable step shows up.
    println!("  starting the browser layer (this may open Chrome)…");
    match provision::ensure(Duration::from_secs(120), true).await {
        Ok(outcome) => {
            steps.push(step("browser_layer", true, format!("daemon {}", outcome.as_str())));
            let reclaimed = provision::reclaim_orphaned_tabs().await;
            if reclaimed > 0 {
                steps.push(step("stray_tabs", true, format!("closed {reclaimed} left by earlier runs")));
            }
        }
        Err(error) => {
            steps.push(step("browser_layer", false, error.to_string()));
            println!(
                "\n  Chrome needs your permission once, and nothing can click it for you:\n    \
                 open chrome://inspect/#remote-debugging and tick the box that allows remote\n    \
                 debugging, then run `jev-browser-relay setup` again.\n"
            );
            summarize(&steps);
            return Ok(());
        }
    }

    // 4. Register with the host agents that are present.
    if register {
        let binary = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "jev-browser-relay".into());

        if which("claude").is_some() {
            let (ok, output) = run(
                "claude",
                &["mcp", "add", "jev-browser-relay", "--", &binary, "mcp"],
                Duration::from_secs(30),
            )
            .await;
            // Re-registering an existing server is a success as far as setup is concerned.
            let already = output.to_lowercase().contains("already exists");
            steps.push(step(
                "claude_code",
                ok || already,
                if already {
                    "already registered".into()
                } else if ok {
                    "registered".into()
                } else {
                    first_line(&output)
                },
            ));
        } else {
            steps.push(step("claude_code", true, "claude CLI not found — skipped"));
        }

        if which("codex").is_some() {
            let (ok, output) = run(
                "codex",
                &["mcp", "add", "jev-browser-relay", "--", &binary, "mcp"],
                Duration::from_secs(30),
            )
            .await;
            let already = output.to_lowercase().contains("already");
            if ok || already {
                steps.push(step("codex", true, if already { "already registered" } else { "registered" }));
            } else {
                // Codex versions differ in whether they have `mcp add`; fall back to telling the
                // user the config rather than pretending it worked.
                steps.push(step("codex", true, "add manually — see the block printed below"));
                println!(
                    "\n  ~/.codex/config.toml\n    [mcp_servers.jev-browser-relay]\n    \
                     command = \"{binary}\"\n    args = [\"mcp\"]\n"
                );
            }
        } else {
            steps.push(step("codex", true, "codex CLI not found — skipped"));
        }
    }

    summarize(&steps);
    Ok(())
}

fn summarize(steps: &[Step]) {
    let failed: Vec<&Step> = steps.iter().filter(|s| !s.ok).collect();
    println!();
    if failed.is_empty() {
        println!("Ready. Ask your agent to do something in a browser.");
    } else {
        println!("{} step(s) still need you:", failed.len());
        for step in failed {
            println!("  - {}: {}", step.name, step.detail);
        }
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        // Non-interactive and no --yes: refuse rather than install unattended.
        println!("{prompt}(not a terminal; pass --yes to allow installing)");
        return Ok(false);
    }
    print!("{prompt}");
    std::io::stdout().flush().context("could not write the prompt")?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).context("could not read the answer")?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(program)).find(|p| p.is_file())
}
