//! `--billing-history`: the actual billing of the resources of the code,
//! read in the background while the code is priced.

use super::args::RunArgs;
use super::common::rt;
use super::ui::{self, notice};
use anyhow::{Result, bail};
use costerra::billing::{self, BillingHistory};
use costerra::config::Config;
use costerra::cost::Report;
use costerra::insights;
use costerra::output::{self, Style};
use costerra::pricing::catalog;
use costerra::project::Project;
use std::time::{Duration, Instant};

/// The user asked for it: a query on the export can take a few seconds.
const DEADLINE: Duration = Duration::from_secs(30);

pub type Pending = Option<std::thread::JoinHandle<Result<BillingHistory, String>>>;

/// Refuse `--billing-history` when Google Cloud cannot be read.
pub fn check(a: &RunArgs) -> Result<()> {
    if a.billing_history && a.eval.offline {
        bail!("--billing-history reads the Cloud Billing export: remove --offline");
    }
    Ok(())
}

/// Start reading the billing of the GCP projects of these projects.
pub fn start(a: &RunArgs, config: &Config, sets: &[&[Project]], billing_account: Option<String>) -> Pending {
    if !a.billing_history {
        return None;
    }
    let req = billing::Request {
        table: a.billing_export_table.clone().or(config.billing_export_table.clone()),
        billing_account,
        stack: billing::stack_of(sets),
        quota: insights::quota_project(a.quota_project.as_deref().or(config.quota_project.as_deref())),
    };
    let cache = catalog::default_cache_dir();
    Some(std::thread::spawn(move || {
        let until = Instant::now() + DEADLINE;
        let rt = rt().map_err(|e| e.to_string())?;
        rt.block_on(billing::fetch(&cache, &req, until))
    }))
}

/// The billing, or a warning saying why it could not be read.
pub fn finish(pending: Pending) -> Option<BillingHistory> {
    let h = pending?;
    ui::phase("Reading the billing export");
    match h.join() {
        Ok(Ok(history)) => Some(history),
        Ok(Err(e)) => {
            notice("warning", format!("--billing-history: {e}"));
            None
        }
        Err(_) => None,
    }
}

/// The chart, after the table.
pub fn table(h: &BillingHistory, report: &Report, st: &Style) -> String {
    billing::table(h, output::billing_estimate(report, h), st.color, output::term_width())
}
