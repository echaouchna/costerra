//! Helpers shared by the commands: configuration, evaluation, pricing, output.

use super::args::{EvalArgs, RunArgs};
use super::ui::{self, notice};
use anyhow::{Context, Result};
use costerra::config::Config;
use costerra::cost::Report;
use costerra::engine::Engine;
use costerra::output::Style;
use costerra::pricing::catalog;
use costerra::pricing::query::Pricer;
use costerra::project::{self, LoadOptions, Project};
use costerra::resources::registry;
use costerra::usage::UsageFile;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

/// One Tokio runtime for the whole process (Recommender, Rabbit, Pricing API).
pub fn rt() -> Result<&'static tokio::runtime::Runtime> {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    if let Some(rt) = RT.get() {
        return Ok(rt);
    }
    let built = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
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
    for w in warnings.iter().take(20) {
        notice("warning", w);
    }
    if warnings.len() > 20 {
        notice("warning", format!("… {} more usage file warnings", warnings.len() - 20));
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
            if important && shown.insert(w.clone()) && shown.len() <= 15 {
                notice("warning", format!("{}: {w}", p.name));
            }
        }
    }
    if shown.len() > 15 {
        notice("warning", format!("… {} more (use --show-all to see all warnings)", shown.len() - 15));
    }
    Ok(projects)
}

pub fn price(a: &RunArgs, projects: &[Project], config: &Config, pricer: &Pricer, usage: &UsageFile) -> Report {
    let t = Instant::now();
    let list = pricer.list_view();
    let engine = Engine {
        pricer,
        list_pricer: list.as_ref(),
        usage_profile: a.usage_profile,
        examples: true,
        usage,
        config,
        recommendations: !a.no_recommendations,
    };
    let mut report = engine.report(projects);
    report.usage_profile = a.usage_profile.map(|p| p.name().to_string());
    tracing::info!("priced {} projects in {:?}", report.projects.len(), t.elapsed());
    report
}

pub fn style(a: &RunArgs, color: bool) -> Style {
    Style { color: color && a.out.is_none(), show_all: a.show_all, show_warnings: a.show_all, group: !a.no_group }
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

pub fn start_ui(phase: &str) {
    ui::start(ui::enabled(), phase);
}
