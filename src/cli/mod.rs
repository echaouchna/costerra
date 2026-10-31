//! The `costerra` command line.

mod args;
mod billing;
mod breakdown;
mod common;
mod completions;
mod diff;
mod insights;
mod prices;
mod pricing;
mod ui;
mod usage;

use args::{Cli, Cmd, UsageCmd};
use clap::Parser;
use std::io::IsTerminal;

pub fn main() {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "costerra=warn",
        1 => "costerra=info",
        2 => "costerra=debug",
        _ => "costerra=trace",
    };
    if let Some(d) = &cli.cache_dir {
        costerra::pricing::catalog::set_cache_dir(d.clone());
    }
    if let Some(sa) = &cli.impersonate_service_account {
        costerra::pricing::sync::set_impersonation(sa);
    }
    let color = cli.color.enabled(cli.no_color, std::io::stdout().is_terminal());
    let stderr_color = cli.color.enabled(cli.no_color, std::io::stderr().is_terminal());
    ui::init(stderr_color, cli.verbose > 0);
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("COSTERRA_LOG").unwrap_or_else(|_| level.into()))
        .with_writer(std::io::stderr)
        .with_ansi(stderr_color)
        .with_target(false)
        .init();
    let res = match cli.cmd {
        Cmd::Breakdown(a) => breakdown::run(a, color),
        Cmd::Diff(a) => diff::run(a, color),
        Cmd::Pricing(p) => pricing::run(p),
        Cmd::Usage(UsageCmd::Init { eval, out, force }) => usage::init(&eval, &out, force),
        Cmd::Usage(UsageCmd::Profiles { rtype }) => usage::profiles(rtype),
        Cmd::Billing(b) => billing::run(b),
        Cmd::Resources { filter } => usage::resources(filter),
        Cmd::Completions { shell } => completions::print(shell),
    };
    match res {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            ui::clear();
            ui::notice("error", format!("{e:#}"));
            std::process::exit(1);
        }
    }
}
