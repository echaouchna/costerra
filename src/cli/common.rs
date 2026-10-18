//! Helpers shared by the commands: configuration, evaluation, pricing, output.

use super::args::{EvalArgs, RunArgs};
use super::ui::{self, notice};
use anyhow::{Context, Result};
use costerra::config::Config;
use costerra::cost::{ObservedSummary, Report};
use costerra::engine::Engine;
use costerra::observed::Observed;
use costerra::output::Style;
use costerra::pricing::catalog;
use costerra::pricing::query::Pricer;
use costerra::project::{self, LoadOptions, Project};
use costerra::resources::registry;
use costerra::usage::UsageFile;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

/// One Tokio runtime for the whole process (Recommender, Pricing API).
pub fn rt() -> Result<&'static tokio::runtime::Runtime> {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    if let Some(rt) = RT.get() {
        return Ok(rt);
    }
    // Network calls only: two workers are enough, and each one costs memory.
    let built = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let _ = RT.set(built);
    RT.get().context("tokio runtime")
}

/// Configuration from `costerra.yml` (or `--config`); also applies its cache directory.
pub fn load_eval_config(e: &EvalArgs) -> Result<Config> {
    let c = match &e.config {
        Some(p) => Config::load(p)?,
        None => Config::discover(&e.path)?,
    };
    if let Some(d) = &c.cache_dir {
        let d = match &c.dir {
            Some(base) if d.is_relative() => base.join(d),
            _ => d.clone(),
        };
        catalog::set_cache_dir(d);
    }
    // The command line (set first) wins over the configuration.
    if let Some(sa) = &c.impersonate_service_account {
        costerra::pricing::sync::set_impersonation(sa);
    }
    Ok(c)
}

/// Configuration with the command-line overrides of a run applied.
pub fn load_config(a: &RunArgs) -> Result<Config> {
    let mut c = load_eval_config(&a.eval)?;
    if a.region.is_some() {
        c.default_region = a.region.clone();
    }
    if a.include_free_tier {
        c.include_free_tier = true;
    }
    if a.billing_account.is_some() {
        c.billing_account = a.billing_account.clone();
    }
    if a.list_prices {
        c.billing_account = None;
    }
    Ok(c)
}

/// Paths excluded by the command line and the configuration.
pub fn excludes(e: &EvalArgs, config: &Config) -> Vec<String> {
    let mut exclude = e.exclude.clone();
    exclude.extend(config.exclude_paths.iter().cloned());
    exclude
}

pub fn load_opts(e: &EvalArgs, config: &Config) -> LoadOptions {
    LoadOptions {
        var_files: e.var_files.clone(),
        vars: e.vars.clone(),
        workspace: e.workspace.clone(),
        offline: e.offline,
        allow_run_cmd: e.allow_run_cmd,
        exclude: excludes(e, config),
        cache_dir: catalog::default_cache_dir(),
        no_env_discovery: e.no_env_discovery,
        remote_state: !e.no_remote_state && config.remote_state.unwrap_or(true),
    }
}

pub fn currency_of(a: &RunArgs, config: &Config) -> String {
    a.currency.clone().or(config.currency.clone()).unwrap_or_else(|| "USD".into())
}

pub fn load_usage(a: &RunArgs, config: &Config) -> Result<UsageFile> {
    let path = a.usage_file.clone().or_else(|| {
        let p = config.usage_file.clone()?;
        Some(match &config.dir {
            Some(d) if p.is_relative() => d.join(p),
            _ => p,
        })
    });
    match path.or_else(|| discover_usage_file(&a.eval.path)) {
        Some(p) => {
            tracing::info!("using usage file {}", p.display());
            UsageFile::load(&p)
        }
        None => Ok(UsageFile::default()),
    }
}

/// Find `costerra-usage.yml` next to the analysed path or in its parents (up
/// to the repository root).
fn discover_usage_file(path: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(path).ok()?;
    let mut cur = Some(if abs.is_file() { abs.parent()?.to_path_buf() } else { abs });
    while let Some(d) = cur {
        for name in ["costerra-usage.yml", "costerra-usage.yaml"] {
            let p = d.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        if d.join(".git").exists() {
            break;
        }
        cur = d.parent().map(Path::to_path_buf);
    }
    None
}

fn usage_keys(rtype: &str) -> Option<Vec<&'static str>> {
    registry().get(rtype).map(|s| s.usage.iter().map(|u| u.key).collect())
}

/// Warn about usage file entries that have no effect (typos in keys or
/// addresses), for the global usage file and the per-project ones.
pub fn lint_usage(usage: &UsageFile, projects: &[Project]) {
    let all: Vec<(&str, &str)> = projects.iter().flat_map(|p| p.resources.iter()).map(|r| (r.rtype.as_str(), r.address.as_str())).collect();
    let mut warnings = usage.lint(&all, usage_keys);
    for p in projects {
        let Some(f) = ["costerra-usage.yml", "costerra-usage.yaml"].iter().map(|n| p.path.join(n)).find(|f| f.is_file()) else {
            continue;
        };
        match UsageFile::load(&f) {
            Ok(local) => {
                let res: Vec<(&str, &str)> = p.resources.iter().map(|r| (r.rtype.as_str(), r.address.as_str())).collect();
                warnings.extend(local.lint(&res, usage_keys).into_iter().map(|w| format!("{}: {w}", p.name)));
            }
            Err(e) => warnings.push(format!("{e:#}")),
        }
    }
    warnings.sort();
    warnings.dedup();
    let max = ui::max_notices();
    for w in warnings.iter().take(max) {
        notice("warning", w);
    }
    if warnings.len() > max {
        notice("warning", format!("… {} more usage file warnings (--details shows them)", warnings.len() - max));
    }
}

/// Evaluate a path into projects.
pub fn load_projects(e: &EvalArgs, path: &Path, config: &Config) -> Result<Vec<Project>> {
    let t = Instant::now();
    let projects = project::load(path, &load_opts(e, config))?;
    if projects.is_empty() {
        notice("warning", format!("no Terraform or Terragrunt configuration found in {}", path.display()));
    }
    tracing::info!("evaluated {} projects in {:?}", projects.len(), t.elapsed());
    // Problems that make resources disappear from the estimate are always shown.
    let mut shown = std::collections::HashSet::new();
    for p in &projects {
        for w in &p.warnings {
            let important = w.starts_with("parse error") || w.contains("cannot load") || w.contains("could not be evaluated");
            if important && shown.insert(w.clone()) && shown.len() <= ui::max_notices() {
                notice("warning", format!("{}: {w}", p.name));
            }
        }
    }
    if shown.len() > ui::max_notices() {
        notice("warning", format!("… {} more (--details shows them)", shown.len() - ui::max_notices()));
    }
    Ok(projects)
}

pub fn price(
    a: &RunArgs,
    projects: &[Project],
    config: &Config,
    pricer: &Pricer,
    usage: &UsageFile,
    observed: Option<&Observed>,
    recommendations: bool,
) -> Report {
    let t = Instant::now();
    let list = pricer.list_view();
    let engine = Engine {
        pricer,
        list_pricer: list.as_ref(),
        usage_profile: a.usage_profile,
        examples: true,
        usage,
        config,
        recommendations,
        observed,
    };
    let mut report = engine.report(projects);
    if !a.show_free_tier() {
        report.free_tiers.clear();
    }
    report.usage_profile = a.usage_profile.map(|p| p.name().to_string());
    if let Some(o) = observed {
        let names: std::collections::HashSet<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        let resources = o.resources.keys().filter(|(p, _)| names.contains(p.as_str())).count();
        if resources > 0 {
            report.observed = Some(ObservedSummary {
                sources: o.sources.iter().cloned().collect(),
                resources,
                window_days: costerra::observed::WINDOW_DAYS,
            });
        }
        report.insight_notes.extend(o.notes.iter().cloned());
    }
    tracing::info!("priced {} projects in {:?}", report.projects.len(), t.elapsed());
    report
}

pub fn style(a: &RunArgs, color: bool) -> Style {
    let all = a.show_all || a.details;
    Style { color: color && a.out.is_none(), show_all: all, show_warnings: all, group: !a.no_group, details: a.details }
}

pub fn emit(a: &RunArgs, text: String) -> Result<()> {
    match &a.out {
        Some(p) => std::fs::write(p, text).with_context(|| format!("writing {}", p.display())),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

/// `--fail-above` / `policies.max_monthly_cost`: exit code 2 when exceeded.
pub fn check_total_policy(a: &RunArgs, config: &Config, report: &Report) -> i32 {
    match a.fail_above.or(config.policies.max_monthly_cost) {
        Some(l) if report.monthly_cost > l => {
            notice("policy", format!("total monthly cost {:.2} exceeds limit {l:.2}", report.monthly_cost));
            2
        }
        _ => 0,
    }
}

/// Say when impersonation failed: the Google data was then not read.
pub fn report_auth_problems() {
    if let Some(e) = costerra::pricing::sync::impersonation_error() {
        notice("warning", format!("{e} (observed usage, remote states and recommendations were not read)"));
    }
}

pub fn start_ui(phase: &str) {
    ui::start(ui::enabled(), phase);
}
