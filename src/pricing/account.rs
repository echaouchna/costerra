//! Billing account detection and on-demand refresh of contract prices.

use super::sync::{AccountPrice, Auth, Throttled, get_json, get_json_opts, parse_account_price, resolve_auth};
use anyhow::Result;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DETECTION_TTL_SECS: u64 = 24 * 3600;

#[derive(Default, Serialize, Deserialize)]
struct DetectionCache {
    /// project id -> (billing account or "" when none, checked at)
    projects: HashMap<String, (String, u64)>,
    /// sole accessible open billing account (or "")
    default: Option<(String, u64)>,
}

fn cache_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("billing-accounts.json")
}

fn load_cache(cache_dir: &Path) -> DetectionCache {
    crate::util::read_json(&cache_path(cache_dir))
}

fn fresh(at: u64) -> bool {
    super::sync::now().saturating_sub(at) < DETECTION_TTL_SECS
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION")))
        .gzip(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

async fn oauth() -> Option<Auth> {
    match resolve_auth(None).await {
        Ok(a @ Auth::Bearer(_)) => Some(a),
        _ => None,
    }
}

/// Detect the billing account to use: the account linked to the GCP projects
/// referenced by the code (most used first), otherwise the only open billing
/// account the caller can access. Results are cached for a day.
pub async fn detect(cache_dir: &Path, projects: &[String], offline: bool) -> Option<String> {
    // The most used projects are enough to find the account.
    let projects = &projects[..projects.len().min(3)];
    let cache = load_cache(cache_dir);
    let cached = |c: &DetectionCache, p: &String| c.projects.get(p).filter(|(_, at)| fresh(*at) || offline).map(|(ba, _)| ba.clone());
    if let Some(ba) = projects.iter().filter_map(|p| cached(&cache, p)).find(|ba| !ba.is_empty()) {
        return Some(ba);
    }
    let all_checked = projects.iter().all(|p| cached(&cache, p).is_some());
    if all_checked
        && let Some((ba, at)) = &cache.default
        && (fresh(*at) || offline)
    {
        return (!ba.is_empty()).then(|| ba.clone());
    }
    if offline {
        return None;
    }
    let auth = oauth().await?;
    let client = client().ok()?;
    let now = super::sync::now();
    let lookups = projects.iter().map(|p| {
        let (client, auth) = (&client, &auth);
        async move {
            let url = format!("https://cloudbilling.googleapis.com/v1/projects/{p}/billingInfo");
            let ba = match get_json(client, auth, &url, &[]).await {
                Ok(j) if j["billingEnabled"].as_bool() == Some(true) => {
                    j["billingAccountName"].as_str().and_then(|n| n.strip_prefix("billingAccounts/")).map(str::to_string)
                }
                Ok(_) => None,
                Err(e) => {
                    tracing::debug!("billing info of {p}: {}", format!("{e:#}").lines().next().unwrap_or_default());
                    None
                }
            };
            (p.clone(), ba)
        }
    });
    let results = futures::future::join_all(lookups).await;
    let mut found = None;
    let mut checked = vec![];
    for (p, ba) in results {
        checked.push((p, (ba.clone().unwrap_or_default(), now)));
        found = found.or(ba);
    }
    let mut default = None;
    if found.is_none() {
        let url = "https://cloudbilling.googleapis.com/v1/billingAccounts";
        let open: Vec<String> = match get_json(&client, &auth, url, &[("pageSize", "50")]).await {
            Ok(j) => j["billingAccounts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| a["open"].as_bool() == Some(true))
                .filter_map(|a| a["name"].as_str().and_then(|n| n.strip_prefix("billingAccounts/")).map(str::to_string))
                .collect(),
            Err(e) => {
                tracing::debug!("listing billing accounts: {e:#}");
                vec![]
            }
        };
        let sole = if open.len() == 1 { open[0].clone() } else { String::new() };
        default = Some((sole.clone(), now));
        if !sole.is_empty() {
            found = Some(sole);
        }
    }
    // Merge under a lock: other costerra processes may update the file too.
    crate::util::update_json(&cache_path(cache_dir), |c: &mut DetectionCache| {
        c.projects.extend(checked);
        if default.is_some() {
            c.default = default;
        }
        c.projects.retain(|_, (_, at)| super::sync::now().saturating_sub(*at) < 30 * 86400);
    });
    found
}

/// Outcome of a price refresh.
pub struct Refreshed {
    pub prices: HashMap<String, AccountPrice>,
    /// SKUs not refreshed (rate limited / deadline reached / errors).
    pub skipped: usize,
    /// The API throttled us: stop refreshing for a while.
    pub throttled: bool,
}

/// Fetch current prices of specific SKUs for a billing account, in parallel,
/// giving up after `deadline` (cached prices are used for the rest).
/// `on_done` is called after each SKU (for progress reporting).
pub async fn refresh(
    billing_account: &str,
    currency: &str,
    skus: &[String],
    until: std::time::Instant,
    on_done: &(dyn Fn() + Sync),
) -> Result<Refreshed> {
    let t = std::time::Instant::now();
    let Some(auth) = oauth().await else { anyhow::bail!("no OAuth credentials to refresh billing account prices") };
    tracing::debug!("access token in {:?}", t.elapsed());
    let client = client()?;
    let until = tokio::time::Instant::from_std(until);
    // Circuit breaker: after the first 429, remaining SKUs keep their cached price.
    let throttled = std::sync::atomic::AtomicBool::new(false);
    let results: Vec<Option<(String, AccountPrice)>> = futures::stream::iter(skus.iter().map(|id| {
        let (client, auth, throttled) = (&client, &auth, &throttled);
        async move {
            if throttled.load(std::sync::atomic::Ordering::Relaxed) {
                on_done();
                return None;
            }
            let url = format!("https://cloudbilling.googleapis.com/v1beta/billingAccounts/{billing_account}/skus/{id}/price");
            let r = match tokio::time::timeout_at(until, get_json_opts(client, auth, &url, &[("currencyCode", currency)], false)).await {
                Ok(Ok(j)) => parse_account_price(&j),
                Ok(Err(e)) => {
                    if e.is::<Throttled>() {
                        throttled.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    tracing::debug!("price of {id}: {e:#}");
                    None
                }
                Err(_) => {
                    tracing::debug!("price of {id}: deadline reached");
                    None
                }
            };
            on_done();
            r
        }
    }))
    .buffer_unordered(12)
    .collect()
    .await;
    let prices: HashMap<String, AccountPrice> = results.into_iter().flatten().collect();
    Ok(Refreshed { skipped: skus.len() - prices.len(), throttled: throttled.into_inner(), prices })
}

/// SKUs used by the previous run of a path, so that the next run can refresh
/// their prices while the code is still being evaluated.
#[derive(Default, Serialize, Deserialize)]
struct RefreshState {
    /// run key -> (billing account, SKU ids, unix time of the run).
    #[serde(default)]
    runs: HashMap<String, RunInfo>,
    /// "account|currency|sku" -> unix time of the last successful refresh.
    #[serde(default)]
    refreshed: HashMap<String, u64>,
    /// No refresh before this unix time (after the API throttled us).
    #[serde(default)]
    throttled_until: u64,
    /// "identity|account" -> unix time the contract prices were refused.
    #[serde(default)]
    denied: HashMap<String, u64>,
}

const THROTTLE_COOLDOWN_SECS: u64 = 60;
/// How long a refusal of the contract prices is remembered, as for observed
/// usage and recommendations.
pub const DENIED_TTL_SECS: u64 = 6 * 3600;
/// Most SKUs refreshed in one run (oldest first); the rest follow in later runs.
pub const REFRESH_BUDGET: usize = 100;

/// Most remembered runs (paths analysed); the oldest are forgotten first.
const MAX_RUNS: usize = 200;
/// Runs older than this are forgotten.
const RUN_RETENTION_SECS: u64 = 30 * 86400;

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(from = "RunRepr")]
struct RunInfo {
    account: String,
    skus: Vec<String>,
    at: u64,
}

/// Stored forms of a run: the current struct, or the `(account, skus)` tuple
/// of earlier versions (read without losing the rest of the state file).
#[derive(Deserialize)]
#[serde(untagged)]
enum RunRepr {
    Current {
        account: String,
        skus: Vec<String>,
        #[serde(default)]
        at: u64,
    },
    Legacy(String, Vec<String>),
}

impl From<RunRepr> for RunInfo {
    fn from(r: RunRepr) -> Self {
        match r {
            RunRepr::Current { account, skus, at } => RunInfo { account, skus, at },
            // Unknown age: kept until the next cleanup by age.
            RunRepr::Legacy(account, skus) => RunInfo { account, skus, at: super::sync::now() },
        }
    }
}

/// Remember that the API throttled us.
pub fn mark_throttled(cache_dir: &Path) {
    update_state(cache_dir, |s| s.throttled_until = super::sync::now() + THROTTLE_COOLDOWN_SECS);
}

/// True while in the cool-down after being throttled.
pub fn cooling_down(cache_dir: &Path) -> bool {
    load_state(cache_dir).throttled_until > super::sync::now()
}

fn denied_key(account: &str) -> String {
    format!("{}|{account}", super::sync::impersonation().unwrap_or_default())
}

/// Remember that the contract prices of `account` were refused (HTTP 403),
/// or forget it (`denied: false`) once they are read.
pub fn mark_denied(cache_dir: &Path, account: &str, denied: bool) {
    let now = super::sync::now();
    update_state(cache_dir, |s| {
        s.denied.retain(|_, at| now.saturating_sub(*at) < DENIED_TTL_SECS);
        if denied {
            s.denied.insert(denied_key(account), now);
        } else {
            s.denied.remove(&denied_key(account));
        }
    });
}

/// True if the contract prices of `account` were refused less than
/// `DENIED_TTL_SECS` ago.
pub fn denied(cache_dir: &Path, account: &str) -> bool {
    load_state(cache_dir).denied.get(&denied_key(account)).is_some_and(|at| super::sync::now().saturating_sub(*at) < DENIED_TTL_SECS)
}

fn load_state(cache_dir: &Path) -> RefreshState {
    crate::util::read_json(&state_path(cache_dir))
}

/// Update the state file under a lock (the background refresh process and
/// parallel runs write it too), atomically.
fn update_state(cache_dir: &Path, f: impl FnOnce(&mut RefreshState)) {
    crate::util::update_json(&state_path(cache_dir), f)
}

/// SKUs whose cached contract price is older than `ttl_secs`, oldest first,
/// at most `REFRESH_BUDGET`.
pub fn stale(cache_dir: &Path, account: &str, currency: &str, ttl_secs: u64, skus: &[String]) -> Vec<String> {
    let s = load_state(cache_dir);
    let now = super::sync::now();
    let mut v: Vec<(u64, &String)> = skus
        .iter()
        .map(|id| (s.refreshed.get(&format!("{account}|{currency}|{id}")).copied().unwrap_or(0), id))
        .filter(|(t, _)| ttl_secs == 0 || now.saturating_sub(*t) >= ttl_secs)
        .collect();
    v.sort();
    v.into_iter().take(REFRESH_BUDGET).map(|(_, id)| id.clone()).collect()
}

/// Record a successful refresh of these SKUs.
pub fn mark_refreshed(cache_dir: &Path, account: &str, currency: &str, skus: impl Iterator<Item = String>) {
    let now = super::sync::now();
    let keys: Vec<String> = skus.map(|id| format!("{account}|{currency}|{id}")).collect();
    update_state(cache_dir, |s| {
        for k in keys {
            s.refreshed.insert(k, now);
        }
        // Forget entries older than a week.
        s.refreshed.retain(|_, t| now.saturating_sub(*t) < 7 * 86400);
    });
}

fn state_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("refresh-state.json")
}

/// (billing account, SKU ids) used by the last run with this key.
pub fn last_run(cache_dir: &Path, key: &str) -> Option<(String, Vec<String>)> {
    load_state(cache_dir).runs.get(key).map(|r| (r.account.clone(), r.skus.clone()))
}

/// Remember the SKUs used by a run. Old runs are forgotten so that the
/// state file does not grow with every path ever analysed.
pub fn save_run(cache_dir: &Path, key: &str, account: &str, skus: &[String]) {
    let now = super::sync::now();
    let info = RunInfo { account: account.to_string(), skus: skus.to_vec(), at: now };
    update_state(cache_dir, |s| {
        s.runs.insert(key.to_string(), info);
        s.runs.retain(|_, r| now.saturating_sub(r.at) < RUN_RETENTION_SECS);
        if s.runs.len() > MAX_RUNS {
            let mut by_age: Vec<(u64, String)> = s.runs.iter().map(|(k, r)| (r.at, k.clone())).collect();
            by_age.sort();
            for (_, k) in by_age.into_iter().take(s.runs.len() - MAX_RUNS) {
                s.runs.remove(&k);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_state_is_read() {
        let j = r#"{"runs":{"/a|USD":["012345-6789AB-CDEF01",["sku1"]],"/b|USD":{"account":"X","skus":[],"at":5}},"refreshed":{"k":1},"throttled_until":0}"#;
        let s: RefreshState = serde_json::from_str(j).unwrap();
        assert_eq!(s.runs["/a|USD"].skus, vec!["sku1"]);
        assert_eq!(s.runs["/b|USD"].at, 5);
        assert_eq!(s.refreshed["k"], 1);
    }

    #[test]
    fn runs_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_RUNS + 10 {
            save_run(dir.path(), &format!("/p{i}|USD"), "A", &["s".to_string()]);
        }
        assert_eq!(load_state(dir.path()).runs.len(), MAX_RUNS);
    }
}
