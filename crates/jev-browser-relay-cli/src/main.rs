//! `jev-browser-relay` — the command line entry point.
//!
//! `mcp` is the one that matters: it is what a host agent launches. The rest exist so the
//! runtime can be understood, checked and measured without a host agent in the loop.

mod benchmark;
mod doctor;
mod telemetry;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use jev_browser_relay_browser::{HarnessBackend, ScriptedBackend, DEFAULT_VIEWPORT};
use jev_browser_relay_core::browser::BrowserBackend;
use jev_browser_relay_core::config::RuntimeConfig;
use jev_browser_relay_core::jev::JevTransport;
use jev_browser_relay_core::session::{RunBudget, RunOutcome, Session};
use jev_browser_relay_jev::TypeSafeClient;
use jev_browser_relay_mcp::{HarnessFactory, RelayServer, ScriptedFactory};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "jev-browser-relay",
    version,
    about = "A high-speed browser runtime for AI coding agents.",
    long_about = "Jev makes the frequent browser decisions; your coding agent plans, fills gaps and \
                  verifies. TYPESAFE_API_KEY is the only model API key required."
)]
struct Cli {
    /// Verbose logging. Always goes to stderr, never stdout.
    #[arg(long, short, global = true)]
    verbose: bool,

    /// Structured JSON logs.
    #[arg(long, global = true)]
    log_json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve MCP over stdio. This is what Claude Code and Codex launch.
    Mcp {
        /// Serve against a scripted fixture instead of Chrome, so a host agent can be shown the
        /// whole flow with nothing installed. Still needs TYPESAFE_API_KEY for real decisions.
        #[arg(long, value_name = "NAME|PATH")]
        fixture: Option<String>,
    },

    /// Run one task to completion and print the result.
    Run {
        /// Page to start on. Omit when using --fixture.
        #[arg(long)]
        url: Option<String>,

        /// What a successful outcome looks like.
        #[arg(long)]
        goal: String,

        /// Task values as a JSON object, e.g. '{"origin":"Sydney"}'.
        #[arg(long)]
        context: Option<String>,

        /// Run against a scripted fixture instead of Chrome. Needs no browser and no daemon.
        #[arg(long, value_name = "NAME|PATH")]
        fixture: Option<String>,

        #[arg(long, default_value_t = 30)]
        max_steps: u32,

        #[arg(long, default_value_t = 60_000)]
        max_duration_ms: u64,

        /// Answer every pause automatically, for unattended demos. Input requests get this value.
        #[arg(long)]
        auto_answer: Option<String>,

        /// Approve gated consequential actions without asking. Off by default, deliberately.
        #[arg(long)]
        auto_confirm: bool,
    },

    /// Print what the runtime currently sees on a page: elements, text, and the choice space.
    Inspect {
        #[arg(long)]
        url: Option<String>,

        #[arg(long, value_name = "NAME|PATH")]
        fixture: Option<String>,
    },

    /// Compare a host-driven browser agent against jev-browser-relay.
    Benchmark {
        /// Trials per scenario per mode.
        #[arg(long, default_value_t = 5)]
        trials: u32,

        /// Only this scenario.
        #[arg(long)]
        scenario: Option<String>,

        /// Assumed cost of one host-model turn, in ms.
        #[arg(long, default_value_t = 3_500)]
        host_turn_ms: u64,

        /// Assumed cost of one Jev request, in ms.
        #[arg(long, default_value_t = 320)]
        jev_ms: u64,

        /// Use the real TypeSafe API for Mode B. Requires TYPESAFE_API_KEY and costs money.
        #[arg(long)]
        live: bool,

        /// Write the full result as JSON here.
        #[arg(long)]
        json_out: Option<String>,
    },

    /// Check the API key, the browser runtime, and the Chrome connection.
    Doctor,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Under `mcp`, stdout is the JSON-RPC channel; the subscriber only ever writes to stderr.
    let default_level = if matches!(cli.command, Command::Mcp { .. }) { "info" } else { "warn" };
    telemetry::init(cli.verbose, cli.log_json, default_level);

    match cli.command {
        Command::Mcp { fixture } => serve_mcp(fixture).await,
        Command::Run {
            url,
            goal,
            context,
            fixture,
            max_steps,
            max_duration_ms,
            auto_answer,
            auto_confirm,
        } => {
            run_task(url, goal, context, fixture, max_steps, max_duration_ms, auto_answer, auto_confirm).await
        }
        Command::Inspect { url, fixture } => inspect(url, fixture).await,
        Command::Benchmark { trials, scenario, host_turn_ms, jev_ms, live, json_out } => {
            run_benchmark(trials, scenario, host_turn_ms, jev_ms, live, json_out).await
        }
        Command::Doctor => {
            let checks = doctor::run().await;
            let ok = doctor::print(&checks);
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

async fn serve_mcp(fixture: Option<String>) -> Result<()> {
    let config = RuntimeConfig::from_env();
    let jev = Arc::new(
        TypeSafeClient::from_env(config.jev_timeout_ms).context("the MCP server needs a Jev client")?,
    );
    let factory: Arc<dyn jev_browser_relay_mcp::BackendFactory> = match fixture {
        Some(name) => Arc::new(ScriptedFactory { fixture: load_fixture(&name)? }),
        None => Arc::new(HarnessFactory::default()),
    };
    let server = RelayServer::new(jev, factory, config);
    server.serve_stdio().await.context("MCP stdio transport failed")?;
    Ok(())
}

/// Built-in fixtures, so `--fixture flights` works with nothing installed.
fn builtin_fixture(name: &str) -> Option<&'static str> {
    Some(match name {
        "flights" => benchmark::FLIGHTS,
        "docs" => benchmark::DOCS,
        "signup" => benchmark::SIGNUP,
        "ambiguous" => benchmark::AMBIGUOUS,
        "checkout" => benchmark::CHECKOUT,
        _ => return None,
    })
}

fn load_fixture(name: &str) -> Result<String> {
    if let Some(builtin) = builtin_fixture(name) {
        return Ok(builtin.to_string());
    }
    std::fs::read_to_string(name)
        .with_context(|| format!("no built-in fixture named \"{name}\" and no file at that path"))
}

async fn make_backend(url: Option<&str>, fixture: Option<&str>) -> Result<Box<dyn BrowserBackend>> {
    match (fixture, url) {
        (Some(name), _) => Ok(Box::new(ScriptedBackend::from_json(&load_fixture(name)?)?)),
        (None, Some(url)) => Ok(Box::new(HarnessBackend::connect(url, DEFAULT_VIEWPORT).await?)),
        (None, None) => anyhow::bail!("supply --url for a real browser, or --fixture to run offline"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_task(
    url: Option<String>,
    goal: String,
    context: Option<String>,
    fixture: Option<String>,
    max_steps: u32,
    max_duration_ms: u64,
    auto_answer: Option<String>,
    auto_confirm: bool,
) -> Result<()> {
    let config = RuntimeConfig::from_env();
    let context = context
        .map(|raw| serde_json::from_str(&raw).context("--context must be a JSON object"))
        .transpose()?;

    let jev: Arc<dyn JevTransport> =
        Arc::new(TypeSafeClient::from_env(config.jev_timeout_ms).context("`run` needs a Jev client")?);
    let backend = make_backend(url.as_deref(), fixture.as_deref()).await?;
    let mut session = Session::start("cli".into(), goal, context, jev, backend, config).await?;

    let budget = RunBudget { max_steps, max_duration_ms };
    loop {
        let outcome = session.run(budget).await;
        println!("{}", serde_json::to_string_pretty(&outcome)?);
        match outcome {
            RunOutcome::Done { .. } | RunOutcome::Blocked { .. } | RunOutcome::Error { .. } => break,
            RunOutcome::BudgetExceeded { .. } => continue,
            RunOutcome::NeedsInput { request_id, field, .. } => {
                let Some(value) = auto_answer.as_deref() else {
                    eprintln!("\nPaused: a value is needed for \"{}\". Re-run with --auto-answer, or use the MCP server so your agent can answer.", field.label);
                    break;
                };
                session.provide_input(&request_id, value, true)?;
            }
            RunOutcome::NeedsReasoning { request_id, reason, .. } => {
                let Some(guidance) = auto_answer.as_deref() else {
                    eprintln!("\nPaused for reasoning: {reason}");
                    break;
                };
                session.provide_reasoning(&request_id, guidance)?;
            }
            RunOutcome::NeedsConfirmation { request_id, question, .. } => {
                if !auto_confirm {
                    eprintln!("\nPaused for confirmation: {question}");
                    eprintln!("Nothing was executed. Pass --auto-confirm only if you mean it.");
                    break;
                }
                session.confirm(&request_id, true)?;
            }
        }
    }

    let metrics = session.stop().await?;
    eprintln!("\n{}", serde_json::to_string_pretty(&metrics)?);
    Ok(())
}

async fn inspect(url: Option<String>, fixture: Option<String>) -> Result<()> {
    let mut backend = make_backend(url.as_deref(), fixture.as_deref()).await?;
    let snapshot = backend.observe().await?;
    let space = jev_browser_relay_core::ActionSpace::build(&snapshot);

    println!("{}  —  {}", snapshot.title, snapshot.url);
    println!("\n{} interactive elements", space.elements.len());
    for element in &space.elements {
        let value = element.value.as_deref().filter(|v| !v.is_empty()).unwrap_or("empty");
        println!(
            "  [{}] {:<10} {:<40} {:<24} {}",
            element.index,
            element.role.as_deref().unwrap_or("-"),
            truncate(&element.label, 38),
            truncate(value, 22),
            element.operations.join(",")
        );
    }
    println!("\noperations offered: {:?}", space.offered_operations());
    if snapshot.omitted_actions > 0 {
        println!("({} further actions omitted by the snapshot cap)", snapshot.omitted_actions);
    }
    println!("\nvisible text:\n{}", truncate(&snapshot.text, 1200));
    backend.close().await?;
    Ok(())
}

async fn run_benchmark(
    trials: u32,
    scenario: Option<String>,
    host_turn_ms: u64,
    jev_ms: u64,
    live: bool,
    json_out: Option<String>,
) -> Result<()> {
    let live_jev: Option<Arc<dyn JevTransport>> = if live {
        Some(Arc::new(TypeSafeClient::from_env(25_000).context("--live needs TYPESAFE_API_KEY")?))
    } else {
        None
    };
    let timing = benchmark::Timing { host_turn_ms, jev_ms, live_jev: live };

    let results = benchmark::run(trials, timing, scenario.as_deref(), live_jev).await?;
    let summaries = benchmark::summarize(&results);
    benchmark::print_report(&summaries, timing);

    if let Some(path) = json_out {
        let report = benchmark::report_json(&summaries, &results, timing);
        std::fs::write(&path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("could not write {path}"))?;
        eprintln!("wrote {path}");
    }
    Ok(())
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}
