//! Pricing catalog for a run: billing account detection, catalog download,
//! and refresh of the contract prices of the SKUs the run uses.

use super::args::RunArgs;
use super::common::{currency_of, rt};
use super::ui::{self, notice};
use anyhow::{Context, Result, bail};
use costerra::config::Config;
use costerra::engine::Engine;
use costerra::pricing::account;
use costerra::pricing::catalog::{self, Catalog};
use costerra::pricing::query::Pricer;
use costerra::pricing::sync;
use costerra::project::Project;
use costerra::usage::UsageFile;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const ACCOUNT_CATALOG_MAX_AGE_DAYS: u64 = 7;
const DEFAULT_PRICE_CACHE_TTL: u64 = 15 * 60;
/// Longest time spent refreshing contract prices in the foreground (from the
/// first request); the rest is refreshed in the background.
const REFRESH_BUDGET_TIME: Duration = Duration::from_millis(1500);

type PriceMap = HashMap<String, sync::AccountPrice>;

/// Lock held by the process refreshing contract prices in the background.
pub fn refresh_lock_path() -> std::path::PathBuf {
    catalog::default_cache_dir().join("refresh")
}

/// GCP project ids referenced by the code, most used first.
fn project_ids(sets: &[&[Project]]) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for p in sets.iter().flat_map(|s| s.iter()) {
        for r in &p.resources {
            let ids = [r.provider.project.as_deref(), r.attrs.get("project").and_then(|v| v.as_str())];
            for id in ids.into_iter().flatten() {
                if !id.is_empty() && !id.starts_with(costerra::REF_PREFIX) && !id.contains('/') && !id.contains(' ') {
                    *counts.entry(id.to_string()).or_default() += 1;
                }
            }
        }
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().map(|(k, _)| k).collect()
}

/// Billing account to price with: explicit setting, otherwise detected from
/// the projects referenced by the code.
fn resolve_billing_account(a: &RunArgs, config: &Config, sets: &[&[Project]]) -> Option<String> {
    if a.list_prices {
        return None;
    }
    match config.billing_account.as_deref() {
        Some("none") | Some("") => return None,
        Some(ba) => return Some(ba.to_string()),
        None => {}
    }
    let ids = project_ids(sets);
    let t = Instant::now();
    let ba = rt().ok()?.block_on(account::detect(&catalog::default_cache_dir(), &ids, a.eval.offline));
    if let Some(ba) = &ba {
        tracing::info!("detected billing account {ba} in {:?}", t.elapsed());
    }
    ba
}

fn download_catalog(currency: &str, billing_account: Option<&str>) -> Result<Catalog> {
    let cache = catalog::default_cache_dir();
    let rt = rt()?;
    let mut f = rt.block_on(sync::sync(std::env::var("COSTERRA_API_KEY").ok(), currency, false, false))?;
    catalog::save(&catalog::catalog_path(&cache, currency), &f)?;
    if let Some(ba) = billing_account {
        let prices = rt.block_on(sync::fetch_account_prices(None, ba, currency, false))?;
        sync::apply_account_prices(&mut f, ba, &prices);
        catalog::save(&catalog::account_catalog_path(&cache, currency, ba), &f)?;
    }
    Ok(Catalog::from_file(f))
}

/// Load the pricing catalog (list prices, or contract prices of a billing
/// account), downloading it when missing or when account prices are stale.
fn load_catalog(currency: &str, billing_account: Option<&str>, offline: bool) -> Result<Catalog> {
    let cache = catalog::default_cache_dir();
    if let Some(ba) = billing_account {
        let path = catalog::account_catalog_path(&cache, currency, ba);
        let existing = path.exists().then(|| catalog::load(&path)).transpose().ok().flatten();
        let age = existing.as_ref().map(|c| sync::now().saturating_sub(c.fetched_at) / 86400);
        match (existing, age) {
            (Some(c), Some(age)) if age <= ACCOUNT_CATALOG_MAX_AGE_DAYS || offline => return Ok(c),
            (existing, _) if !offline => {
                ui::phase(&format!("Downloading the price list of billing account {ba}"));
                match download_catalog(currency, Some(ba)) {
                    Ok(c) => return Ok(c),
                    Err(e) => {
                        notice("warning", format!("could not download prices of billing account {ba}: {e:#}"));
                        if let Some(c) = existing {
                            return Ok(c);
                        }
                    }
                }
            }
            _ => notice("warning", format!("no contract prices cached for billing account {ba}, using list prices")),
        }
    }
    let path = catalog::catalog_path(&cache, currency);
    let loaded = if path.exists() { catalog::load(&path).map_err(|e| tracing::debug!("{e:#}")).ok() } else { None };
    if let Some(c) = loaded {
        let age_days = sync::now().saturating_sub(c.fetched_at) / 86400;
        if age_days > 30 {
            notice("warning", format!("pricing catalog is {age_days} days old, run `costerra pricing sync` to refresh"));
        }
        return Ok(c);
    }
    if offline {
        bail!("no usable pricing catalog at {} (run `costerra pricing sync`)", path.display());
    }
    ui::phase("Downloading Google Cloud list prices");
    download_catalog(currency, None)
}

/// Shared deadline for all foreground refreshes of this run.
fn refresh_deadline() -> Instant {
    static DEADLINE: OnceLock<Instant> = OnceLock::new();
    *DEADLINE.get_or_init(|| Instant::now() + REFRESH_BUDGET_TIME)
}

fn price_ttl(a: &RunArgs, config: &Config) -> u64 {
    a.price_cache_ttl
        .or_else(|| config.price_cache_ttl.as_deref().and_then(|t| costerra::util::parse_duration_secs(t).ok()))
        .unwrap_or(DEFAULT_PRICE_CACHE_TTL)
}

/// Finish refreshing stale contract prices in a detached process, so that
/// the current command returns immediately and the next run is fresh. Not
/// started while another refresh is running (parallel runs, CI jobs).
fn refresh_in_background(account: &str, currency: &str, skus: &[String]) {
    if costerra::util::try_lock(&refresh_lock_path()).is_none() {
        tracing::debug!("background refresh already running");
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--cache-dir")
        .arg(catalog::default_cache_dir())
        .args(["pricing", "refresh", "--billing-account", account, "--currency", currency, "--skus", &skus.join(",")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Err(e) = cmd.spawn() {
        tracing::debug!("background refresh: {e}");
    }
}

fn refresh_enabled(a: &RunArgs, config: &Config) -> bool {
    !a.eval.offline
        && !a.list_prices
        && !a.no_refresh
        && config.refresh_prices.unwrap_or(true)
        && config.billing_account.as_deref() != Some("none")
        && !account::cooling_down(&catalog::default_cache_dir())
}

fn run_key(a: &RunArgs, currency: &str) -> String {
    let p = std::path::absolute(&a.eval.path).unwrap_or_else(|_| a.eval.path.clone());
    format!("{}|{currency}", p.display())
}

/// Contract prices being refreshed in the background for the SKUs used by
/// the previous run of this path.
pub struct Prefetch {
    account: String,
    skus: Vec<String>,
    handle: std::thread::JoinHandle<Result<account::Refreshed>>,
}

pub fn start_prefetch(a: &RunArgs, config: &Config) -> Option<Prefetch> {
    if !refresh_enabled(a, config) {
        return None;
    }
    let currency = currency_of(a, config);
    let cache = catalog::default_cache_dir();
    let (acc, used) = account::last_run(&cache, &run_key(a, &currency))?;
    if config.billing_account.as_deref().is_some_and(|b| b != acc) {
        return None;
    }
    let skus = account::stale(&cache, &acc, &currency, price_ttl(a, config), &used);
    if skus.is_empty() {
        return None;
    }
    ui::meter("Refreshing your contract prices", skus.len() as u64);
    let tick = ui::ticker();
    let (acc2, ids) = (acc.clone(), skus.clone());
    let until = refresh_deadline();
    let handle = std::thread::spawn(move || rt()?.block_on(account::refresh(&acc2, &currency, &ids, until, &tick)));
    Some(Prefetch { account: acc, skus, handle })
}

/// Build the pricer for a run. With a billing account, the contract prices of
/// the SKUs used by these projects are refreshed from the Pricing API (most of
/// them already in the background, see `start_prefetch`).
pub fn build_pricer(a: &RunArgs, config: &Config, sets: &[&[Project]], usage: &UsageFile, prefetch: Option<Prefetch>) -> Result<Pricer> {
    let currency = currency_of(a, config);
    ui::phase("Finding your billing account");
    let ba = resolve_billing_account(a, config, sets);
    ui::phase("Loading prices");
    let t = Instant::now();
    let mut cat = load_catalog(&currency, ba.as_deref(), a.eval.offline)?;
    tracing::info!("loaded {} SKUs in {:?}", cat.sku_count(), t.elapsed());
    let Some(ba) = cat.billing_account.clone().filter(|_| refresh_enabled(a, config)) else {
        return Ok(Pricer::new(Some(cat), config.include_free_tier));
    };
    let t = Instant::now();
    // Dry run to learn which SKUs the estimate needs.
    let probe = Pricer::new(Some(cat), config.include_free_tier);
    {
        let engine = Engine {
            pricer: &probe,
            list_pricer: None,
            usage_profile: a.usage_profile,
            examples: false,
            usage,
            config,
            recommendations: false,
        };
        for s in sets {
            engine.report(s);
        }
    }
    let ids = probe.used_skus();
    cat = probe.into_catalog().context("catalog still shared")?;

    let cache = catalog::default_cache_dir();
    let ttl = price_ttl(a, config);
    let mut fresh = PriceMap::new();
    let mut attempted = false;
    let mut throttled = false;
    let mut errors = vec![];
    let mut todo = account::stale(&cache, &ba, &currency, ttl, &ids);
    if let Some(pf) = prefetch {
        let same = pf.account == ba;
        match pf.handle.join() {
            Ok(Ok(r)) if same => {
                fresh = r.prices;
                attempted = true;
                throttled |= r.throttled;
            }
            Ok(Err(e)) if same => errors.push(e),
            _ => {}
        }
        if same {
            let done: std::collections::HashSet<&String> = pf.skus.iter().collect();
            todo.retain(|id| !done.contains(id));
        }
    }
    let time_left = refresh_deadline().saturating_duration_since(Instant::now());
    if !todo.is_empty() && !throttled && time_left > Duration::from_millis(300) {
        ui::meter("Refreshing your contract prices", todo.len() as u64);
        let tick = ui::ticker();
        match rt()?.block_on(account::refresh(&ba, &currency, &todo, refresh_deadline(), &tick)) {
            Ok(r) => {
                fresh.extend(r.prices);
                attempted = true;
                throttled |= r.throttled;
            }
            Err(e) => errors.push(e),
        }
    }
    if throttled {
        account::mark_throttled(&cache);
    }
    // SKUs not refreshed in time are finished in the background.
    let mut pending: Vec<String> =
        account::stale(&cache, &ba, &currency, ttl, &ids).into_iter().filter(|id| !fresh.contains_key(id)).collect();
    pending.sort();
    if !throttled && !pending.is_empty() {
        refresh_in_background(&ba, &currency, &pending);
    }
    account::save_run(&cache, &run_key(a, &currency), &ba, &ids);
    if !attempted && errors.is_empty() {
        tracing::info!("all {} contract prices cached within the TTL", ids.len());
    } else if let Some(e) = errors.first().filter(|_| fresh.is_empty()) {
        notice("warning", format!("could not refresh contract prices, using cached prices: {e:#}"));
    } else {
        account::mark_refreshed(&cache, &ba, &currency, fresh.keys().cloned());
        let changed = cat.apply_account_prices(&fresh);
        if changed > 0 {
            // Persist by merging into the file (a background refresh may have updated it).
            catalog::update_account_prices(&catalog::account_catalog_path(&cache, &currency, &ba), &fresh)?;
        }
        let summary = format!(
            "{} contract prices refreshed in {:.1}s{}{}",
            fresh.len(),
            t.elapsed().as_secs_f64(),
            if changed > 0 { format!(" · {changed} changed since the last sync") } else { " · no change".into() },
            match (throttled, pending.len()) {
                (true, _) => " · throttled by Google, cached prices used for the rest".to_string(),
                (false, 0) => String::new(),
                (false, n) => format!(" · {n} more refreshing in the background"),
            }
        );
        if ui::enabled() {
            ui::println(&format!("\x1b[32m💰\x1b[0m \x1b[2m{summary}\x1b[0m"));
        } else if changed > 0 || throttled {
            notice("info", summary);
        }
        tracing::info!("refreshed {} of {} contract prices", fresh.len(), ids.len());
    }
    Ok(Pricer::new(Some(cat), config.include_free_tier))
}
