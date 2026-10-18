//! Usage-based recommendations (Google Cloud Recommender, Rabbit), fetched in
//! the background while resources are priced.

use super::args::RunArgs;
use super::common::{excludes, rt};
use super::ui::{self, notice};
use costerra::config::Config;
use costerra::cost::Report;
use costerra::insights::{self, Insight};
use costerra::pricing::catalog;
use costerra::project::Project;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Short deadline for cached / quick sources, so that runs stay instant.
const FETCH_DEADLINE: Duration = Duration::from_millis(2500);
/// The AI review takes 10–30 s.
const REVIEW_DEADLINE: Duration = Duration::from_secs(180);

pub struct Pending {
    handle: std::thread::JoinHandle<(Vec<Insight>, Vec<String>)>,
    review: bool,
}

/// Directory the Rabbit review covers: the analysed directory (or the
/// directory of the analysed file).
pub fn review_root(path: &Path) -> PathBuf {
    let root = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    if root.is_file() { root.parent().map(Path::to_path_buf).unwrap_or(root) } else { root }
}

pub fn start(a: &RunArgs, config: &Config, sets: &[&[Project]], billing_account: Option<String>) -> Option<Pending> {
    if a.eval.offline || a.no_insights || a.no_recommendations {
        return None;
    }
    let gcp = config.gcp_recommendations.unwrap_or(true);
    let rabbit = config.rabbit_recommendations.unwrap_or(true);
    let review = a.rabbit_review || config.rabbit_review.unwrap_or(false);
    let queries = if gcp { insights::gcp_queries(sets, billing_account.as_deref()) } else { vec![] };
    let key = insights::rabbit_key();
    if review && key.is_none() {
        notice(
            "warning",
            "--rabbit-review needs a Rabbit API key (FOLLOWRABBIT_API_KEY, see https://subscriptions.agentic.followrabbit.ai)",
        );
    }
    let rabbit_ctx = if rabbit { key.clone().and_then(|k| insights::repo_url(&a.eval.path).map(|r| (k, r))) } else { None };
    let review_ctx = if review {
        key.map(|k| {
            (k, review_root(&a.eval.path), sets.iter().flat_map(|s| s.iter().cloned()).collect::<Vec<Project>>(), excludes(&a.eval, config))
        })
    } else {
        None
    };
    if queries.is_empty() && rabbit_ctx.is_none() && review_ctx.is_none() {
        return None;
    }
    let review = review_ctx.is_some();
    let quota = insights::quota_project(a.quota_project.as_deref().or(config.quota_project.as_deref()));
    let cache = catalog::default_cache_dir();
    let handle = std::thread::spawn(move || {
        let until = Instant::now() + FETCH_DEADLINE;
        let review_until = Instant::now() + REVIEW_DEADLINE;
        let Ok(rt) = rt() else { return (vec![], vec![]) };
        rt.block_on(async {
            let g = insights::fetch_gcp(&cache, &queries, quota.as_deref(), until);
            let r = async {
                match &rabbit_ctx {
                    Some((k, repo)) => Some(insights::fetch_rabbit(&cache, k, repo, until).await),
                    None => None,
                }
            };
            let rv = async {
                match &review_ctx {
                    Some((k, root, projects, exclude)) => {
                        Some(insights::rabbit_review(&cache, k, root, projects, exclude, review_until).await)
                    }
                    None => None,
                }
            };
            let (g, r, rv) = futures::join!(g, r, rv);
            let mut found = g.insights;
            let mut notes = g.notes;
            for f in [r, rv].into_iter().flatten() {
                found.extend(f.insights);
                notes.extend(f.notes);
            }
            (found, notes)
        })
    });
    Some(Pending { handle, review })
}

pub fn finish(pending: Option<Pending>, report: &mut Report, projects: &[Project]) {
    let Some(p) = pending else { return };
    ui::phase(if p.review { "Waiting for the Rabbit AI cost review (can take ~30 s)" } else { "Collecting usage-based recommendations" });
    if let Ok((found, notes)) = p.handle.join() {
        insights::attach(report, projects, found, notes);
    }
}
