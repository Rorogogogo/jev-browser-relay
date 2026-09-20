//! Tracing setup.
//!
//! Under MCP, stdout carries the protocol, so **every log line goes to stderr**. Getting this
//! wrong corrupts the JSON-RPC stream, so there is exactly one place that configures it.

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// `default_level` is the level used when neither `--verbose` nor `JEV_RELAY_LOG` says otherwise.
/// The MCP server logs at info because it is long-lived and unattended; the one-shot commands
/// stay quiet so their output is the report, not a log.
pub fn init(verbose: bool, json: bool, default_level: &str) {
    let filter = EnvFilter::try_from_env("JEV_RELAY_LOG").unwrap_or_else(|_| {
        EnvFilter::new(if verbose {
            "jev_browser_relay=debug,info".to_string()
        } else {
            format!("jev_browser_relay={default_level},warn")
        })
    });

    let registry = tracing_subscriber::registry().with(filter);
    if json {
        registry.with(fmt::layer().json().with_writer(std::io::stderr)).init();
    } else {
        registry
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_target(false)
                    .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr())),
            )
            .init();
    }
}
