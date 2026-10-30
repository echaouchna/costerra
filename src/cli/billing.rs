//! `--billing-history`: the actual billing of the resources of the code,
//! read in the background while the code is priced.

use super::args::RunArgs;
use super::common::rt;
use super::ui::{self, notice};
use anyhow::{Result, bail};
use costerra::billing::{self, Billing};
use costerra::config::Config;
use costerra::cost::Report;
use costerra::insights;
use costerra::output::{self, Style};
use costerra::pricing::catalog;
use costerra::project::Project;
use std::time::{Duration, Instant};

/// The user asked for it: a query on the export can take a few seconds.
const DEADLINE: Duration = Duration::from_secs(30);

pub type Pending = Option<std::thread::JoinHandle<Result<Billing, String>>>;

/// Refuse `--billing-history` when Google Cloud cannot be read.
pub fn check(a: &RunArgs) -> Result<()> {
    if a.wants_billing() && a.eval.offline {
        bail!("--billing-history reads the Cloud Billing export: remove --offline");
    }
    Ok(())
}

/// Start reading the billing of the GCP projects of these projects.
pub fn start(a: &RunArgs, config: &Config, sets: &[&[Project]], billing_account: Option<String>) -> Pending {
    if !a.wants_billing() {
        return None;
    }
    let req = billing::Request {
        table: a.billing_export_table.clone().or(config.billing_export_table.clone()),
        billing_account,
        stack: billing::stack_of(sets, |p| billing::env_name(p, config)),
        quota: insights::quota_project(a.quota_project.as_deref().or(config.quota_project.as_deref())),
        by_env: a.billing_by_env,
    };
    let cache = catalog::default_cache_dir();
    Some(std::thread::spawn(move || {
        let until = Instant::now() + DEADLINE;
        let rt = rt().map_err(|e| e.to_string())?;
        rt.block_on(billing::fetch(&cache, &req, until))
    }))
}

/// The billing, or a warning saying why it could not be read.
pub fn finish(pending: Pending) -> Option<Billing> {
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

/// The charts (one per environment when the code spans several), after the table.
pub fn table(b: &Billing, report: &Report, st: &Style) -> String {
    b.charts().iter().map(|h| billing::table(h, output::billing_estimate(report, h), st.color, output::term_width())).collect()
}

/// The `billing_history` key of the JSON output (the total), and
/// `billing_history_by_env` when the code spans several environments.
pub fn json(b: &Billing, obj: &mut serde_json::Map<String, serde_json::Value>) -> Result<()> {
    obj.insert("billing_history".into(), serde_json::to_value(&b.total)?);
    if !b.by_env.is_empty() {
        obj.insert("billing_history_by_env".into(), serde_json::to_value(&b.by_env)?);
    }
    Ok(())
}
