//! Usage-based recommendations (Google Cloud Recommender) and observed usage
//! (Cloud Monitoring, commitments), fetched in the background.

use super::args::RunArgs;
use super::common::rt;
use super::ui;
use costerra::config::Config;
use costerra::cost::Report;
use costerra::insights::{self, Insight};
use costerra::observed::{self, Observed};
use costerra::pricing::catalog;
use costerra::project::Project;
use std::time::{Duration, Instant};

/// Short deadline for cached / quick sources, so that runs stay instant.
const FETCH_DEADLINE: Duration = Duration::from_millis(2500);
/// Observed usage is needed before pricing: it starts with the price prefetch.
const OBSERVED_DEADLINE: Duration = Duration::from_millis(3000);

pub type PendingObserved = Option<std::thread::JoinHandle<Observed>>;

/// Start reading the observed usage of the resources of these projects.
pub fn start_observed(a: &RunArgs, config: &Config, sets: &[&[Project]]) -> PendingObserved {
    if a.eval.offline || a.no_observed || !config.observed_usage.unwrap_or(true) {
        return None;
    }
    let targets = observed::Targets::from_projects(sets);
    if targets.is_empty() {
        return None;
    }
    let quota = insights::quota_project(a.quota_project.as_deref().or(config.quota_project.as_deref()));
    let cache = catalog::default_cache_dir();
    Some(std::thread::spawn(move || {
        let until = Instant::now() + OBSERVED_DEADLINE;
        let Ok(rt) = rt() else { return Observed::default() };
        rt.block_on(observed::collect(&cache, &targets, quota.as_deref(), until))
    }))
}

pub fn finish_observed(pending: PendingObserved) -> Option<Observed> {
    let h = pending?;
    ui::phase("Reading observed usage");
    h.join().ok()
}

pub struct Pending {
    handle: std::thread::JoinHandle<(Vec<Insight>, Vec<String>)>,
}

pub fn start(a: &RunArgs, config: &Config, sets: &[&[Project]], billing_account: Option<String>) -> Option<Pending> {
    if a.eval.offline || a.no_insights || !a.show_recommendations() || !config.gcp_recommendations.unwrap_or(true) {
        return None;
    }
    let queries = insights::gcp_queries(sets, billing_account.as_deref());
    if queries.is_empty() {
        return None;
    }
    let quota = insights::quota_project(a.quota_project.as_deref().or(config.quota_project.as_deref()));
    let cache = catalog::default_cache_dir();
    let handle = std::thread::spawn(move || {
        let until = Instant::now() + FETCH_DEADLINE;
        let Ok(rt) = rt() else { return (vec![], vec![]) };
        let f = rt.block_on(insights::fetch_gcp(&cache, &queries, quota.as_deref(), until));
        (f.insights, f.notes)
    });
    Some(Pending { handle })
}

pub fn finish(pending: Option<Pending>, report: &mut Report, projects: &[Project]) {
    let Some(p) = pending else { return };
    ui::phase("Collecting usage-based recommendations");
    if let Ok((found, notes)) = p.handle.join() {
        insights::attach(report, projects, found, notes);
    }
}
