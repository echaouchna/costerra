//! External optimization insights, based on observed usage:
//! * Google Cloud Recommender (rightsizing, idle resources, commitments…),
//! * Rabbit (followrabbit.ai) saved recommendations for the repository.
//!
//! Results are cached (6 h) so that runs stay instant, and matched to the
//! resources of the analysed code when possible.

use crate::cost::{Recommendation, Report, Severity};
use crate::pricing::sync::{Auth, resolve_auth};
use crate::project::Project;
use futures::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const CACHE_TTL_SECS: u64 = 6 * 3600;
const MAX_QUERIES: usize = 200;
/// Most result pages read per Recommender query.
const MAX_PAGES: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Insight {
    pub source: String,
    pub kind: String,
    pub title: String,
    pub detail: Option<String>,
    pub monthly_savings: Option<f64>,
    pub priority: Option<String>,
    /// Resources concerned: GCP full resource names or Terraform addresses.
    pub resources: Vec<String>,
    /// Location of the recommendation (region / zone / global).
    #[serde(default)]
    pub location: Option<String>,
    /// Where it comes from (recommender id, gcloud describe command, URL…).
    #[serde(default)]
    pub reference: Option<String>,
}

/// One Recommender API list call.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Query {
    pub parent: String,
    pub location: String,
    pub recommender: &'static str,
}

impl Query {
    fn key(&self) -> String {
        format!("gcp|{}|{}|{}", self.parent, self.location, self.recommender)
    }
}

/// Recommenders relevant to a resource type, and the location granularity.
fn recommenders_for(rtype: &str) -> &'static [(&'static str, Loc)] {
    match rtype {
        "google_compute_instance" => {
            &[("google.compute.instance.MachineTypeRecommender", Loc::Zone), ("google.compute.instance.IdleResourceRecommender", Loc::Zone)]
        }
        "google_compute_instance_group_manager" | "google_compute_region_instance_group_manager" => {
            &[("google.compute.instanceGroupManager.MachineTypeRecommender", Loc::Zone)]
        }
        "google_compute_disk" => &[("google.compute.disk.IdleResourceRecommender", Loc::Zone)],
        "google_compute_address" => &[("google.compute.address.IdleResourceRecommender", Loc::Region)],
        "google_compute_global_address" => &[("google.compute.address.IdleResourceRecommender", Loc::Global)],
        "google_compute_image" => &[("google.compute.image.IdleResourceRecommender", Loc::Global)],
        "google_sql_database_instance" => &[
            ("google.cloudsql.instance.IdleRecommender", Loc::Region),
            ("google.cloudsql.instance.OverprovisionedRecommender", Loc::Region),
        ],
        "google_container_cluster" => &[("google.container.DiagnosisRecommender", Loc::Region)],
        "google_bigquery_dataset" | "google_bigquery_table" => &[("google.bigquery.table.PartitionClusterRecommender", Loc::Region)],
        _ => &[],
    }
}

#[derive(Clone, Copy)]
enum Loc {
    Zone,
    Region,
    Global,
}

fn region_of(zone_or_region: &str) -> String {
    crate::pricing::query::normalize_region(zone_or_region)
}

/// Recommender calls needed for the resources of these projects.
pub fn gcp_queries(sets: &[&[Project]], billing_account: Option<&str>) -> Vec<Query> {
    let mut out: BTreeSet<Query> = BTreeSet::new();
    let mut regions: BTreeSet<String> = BTreeSet::new();
    for r in sets.iter().flat_map(|s| s.iter()).flat_map(|p| p.resources.iter()) {
        let Some(project) = r.attrs.get("project").and_then(|v| v.as_str()).or(r.provider.project.as_deref()) else { continue };
        if project.starts_with(crate::REF_PREFIX) || project.contains('/') {
            continue;
        }
        let zone = r.attrs.get("zone").and_then(|v| v.as_str()).or(r.provider.zone.as_deref());
        let region = r
            .attrs
            .get("region")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| zone.map(region_of))
            .or_else(|| r.provider.region.clone());
        if let Some(rg) = &region {
            regions.insert(rg.clone());
        }
        for (rec, loc) in recommenders_for(&r.rtype) {
            let location = match loc {
                Loc::Zone => zone.map(str::to_string),
                Loc::Region => region.clone(),
                Loc::Global => Some("global".into()),
            };
            if let Some(location) = location {
                out.insert(Query { parent: format!("projects/{project}"), location, recommender: rec });
            }
        }
    }
    if let Some(ba) = billing_account {
        for rg in &regions {
            out.insert(Query {
                parent: format!("billingAccounts/{ba}"),
                location: rg.clone(),
                recommender: "google.compute.commitment.UsageCommitmentRecommender",
            });
        }
        out.insert(Query {
            parent: format!("billingAccounts/{ba}"),
            location: "global".into(),
            recommender: "google.cloudbilling.commitment.SpendBasedCommitmentRecommender",
        });
    }
    out.into_iter().take(MAX_QUERIES).collect()
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    entries: HashMap<String, (u64, Vec<Insight>)>,
}

fn cache_path(dir: &Path) -> PathBuf {
    dir.join("insights.json")
}

fn load_cache(dir: &Path) -> Cache {
    crate::util::read_json(&cache_path(dir))
}

type CacheEntry = (String, (u64, Vec<Insight>));

/// Merge new entries into the cache file. The Recommender, Rabbit and review
/// fetchers run concurrently (and so may other costerra processes): the file
/// is re-read under a lock and written atomically, so no entry is lost.
fn store_cache(dir: &Path, entries: Vec<CacheEntry>) {
    if entries.is_empty() {
        return;
    }
    let now = crate::pricing::sync::now();
    crate::util::update_json(&cache_path(dir), |c: &mut Cache| {
        c.entries.extend(entries);
        c.entries.retain(|_, (at, _)| now.saturating_sub(*at) < CACHE_RETENTION_SECS);
    });
}

/// Cache entries older than this are dropped.
const CACHE_RETENTION_SECS: u64 = 7 * 86400;

// ---------------------------------------------------------------------------
// Google Cloud Recommender
// ---------------------------------------------------------------------------

/// Quota project for APIs that require one with user credentials: explicit
/// setting, `GOOGLE_CLOUD_QUOTA_PROJECT`, the ADC file, or gcloud config.
pub fn quota_project(explicit: Option<&str>) -> Option<String> {
    if let Some(q) = explicit.filter(|q| !q.is_empty()) {
        return Some(q.to_string());
    }
    if let Ok(q) = std::env::var("GOOGLE_CLOUD_QUOTA_PROJECT") {
        return Some(q);
    }
    let adc = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .map(PathBuf::from)
        .ok()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config/gcloud/application_default_credentials.json")));
    if let Some(q) = adc
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|j| j["quota_project_id"].as_str().map(str::to_string))
    {
        return Some(q);
    }
    None
}

fn money(v: &serde_json::Value) -> f64 {
    let units = v["units"].as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v["units"].as_f64()).unwrap_or(0.0);
    units + v["nanos"].as_f64().unwrap_or(0.0) / 1e9
}

/// `gcloud recommender recommendations describe …` for a recommendation name
/// like `billingAccounts/B/locations/L/recommenders/R/recommendations/ID`.
fn gcloud_describe(name: &str, quota: Option<&str>) -> Option<String> {
    let segs: Vec<&str> = name.split('/').collect();
    let get = |k: &str| segs.iter().position(|s| *s == k).and_then(|i| segs.get(i + 1)).copied();
    let parent = match (get("projects"), get("billingAccounts"), get("organizations"), get("folders")) {
        (Some(p), ..) => format!("--project={p}"),
        (_, Some(b), ..) => format!("--billing-account={b}"),
        (_, _, Some(o), _) => format!("--organization={o}"),
        (_, _, _, Some(f)) => format!("--folder={f}"),
        _ => return None,
    };
    Some(format!(
        "gcloud recommender recommendations describe {} {parent} --location={} --recommender={}{}",
        get("recommendations")?,
        get("locations")?,
        get("recommenders")?,
        quota.map(|q| format!(" --billing-project={q}")).unwrap_or_default()
    ))
}

fn short_recommender(name: &str) -> String {
    name.trim_start_matches("google.").replace('.', "-").to_lowercase()
}

fn parse_gcp(j: &serde_json::Value, recommender: &str, quota: Option<&str>) -> Vec<Insight> {
    let mut out = vec![];
    for r in j["recommendations"].as_array().into_iter().flatten() {
        if r["stateInfo"]["state"].as_str().is_some_and(|s| s != "ACTIVE") {
            continue;
        }
        let cost = &r["primaryImpact"]["costProjection"];
        let monthly_savings = cost.get("cost").map(|c| {
            let v = -money(c);
            let secs = cost["duration"].as_str().and_then(|d| d.trim_end_matches('s').parse::<f64>().ok()).unwrap_or(2_592_000.0);
            v * (730.0 * 3600.0) / secs.max(1.0)
        });
        let mut resources: Vec<String> =
            r["targetResources"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
        for g in r["content"]["operationGroups"].as_array().into_iter().flatten() {
            for op in g["operations"].as_array().into_iter().flatten() {
                if let Some(res) = op["resource"].as_str() {
                    resources.push(res.to_string());
                }
            }
        }
        if let Some(n) = r["content"]["overview"]["resourceName"].as_str() {
            resources.push(n.to_string());
        }
        resources.sort();
        resources.dedup();
        out.push(Insight {
            source: "gcp".into(),
            kind: short_recommender(recommender),
            title: r["description"].as_str().unwrap_or("Google Cloud recommendation").to_string(),
            detail: r["recommenderSubtype"].as_str().map(|s| s.replace('_', " ").to_lowercase()),
            monthly_savings: monthly_savings.filter(|s| *s > 0.0),
            priority: r["priority"].as_str().map(str::to_string),
            resources,
            location: r["name"].as_str().and_then(|n| n.split("/locations/").nth(1)).and_then(|l| l.split('/').next()).map(str::to_string),
            reference: Some(match r["name"].as_str().and_then(|n| gcloud_describe(n, quota)) {
                Some(cmd) => format!("{recommender} · details: {cmd}"),
                None => recommender.to_string(),
            }),
        });
    }
    out
}

pub struct Fetched {
    pub insights: Vec<Insight>,
    pub notes: Vec<String>,
}

/// Fetch Recommender results (cached for 6 h), within `until`.
pub async fn fetch_gcp(cache_dir: &Path, queries: &[Query], quota: Option<&str>, until: std::time::Instant) -> Fetched {
    let cache = load_cache(cache_dir);
    let now = crate::pricing::sync::now();
    let mut insights = vec![];
    let mut todo = vec![];
    for q in queries {
        match cache.entries.get(&q.key()) {
            Some((at, v)) if now.saturating_sub(*at) < CACHE_TTL_SECS => insights.extend(v.iter().cloned()),
            _ => todo.push(q.clone()),
        }
    }
    let mut notes = vec![];
    if todo.is_empty() {
        return Fetched { insights, notes };
    }
    let auth = match resolve_auth(None).await {
        Ok(a @ Auth::Bearer(_)) => a,
        _ => return Fetched { insights, notes },
    };
    let Ok(client) = reqwest::Client::builder().user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION"))).gzip(true).build() else {
        return Fetched { insights, notes };
    };
    let quota = quota.map(str::to_string);
    let until = tokio::time::Instant::from_std(until);
    let results: Vec<(Query, Result<Vec<Insight>, String>)> = futures::stream::iter(todo.into_iter().map(|q| {
        let (client, auth, quota) = (&client, &auth, quota.clone());
        async move {
            let url = format!(
                "https://recommender.googleapis.com/v1/{}/locations/{}/recommenders/{}/recommendations",
                q.parent, q.location, q.recommender
            );
            // Follow `nextPageToken` (bounded) so that large projects are complete.
            let mut found = vec![];
            let mut token: Option<String> = None;
            let mut pages = 0;
            let res = loop {
                let mut rq = client.get(&url).query(&[("pageSize", "100")]);
                if let Some(t) = &token {
                    rq = rq.query(&[("pageToken", t.as_str())]);
                }
                let rq = match auth {
                    Auth::Bearer(t) => rq.bearer_auth(t),
                    Auth::ApiKey(_) => rq,
                };
                let rq = match &quota {
                    Some(qp) => rq.header("x-goog-user-project", qp),
                    None => rq,
                };
                match tokio::time::timeout_at(until, rq.send()).await {
                    Err(_) => break Err("deadline".to_string()),
                    Ok(Err(e)) => break Err(e.to_string()),
                    Ok(Ok(r)) => {
                        let status = r.status();
                        let j: serde_json::Value = r.json().await.unwrap_or_default();
                        if !status.is_success() {
                            break Err(format!("{status}: {}", j["error"]["message"].as_str().unwrap_or_default()));
                        }
                        found.extend(parse_gcp(&j, q.recommender, quota.as_deref()));
                        pages += 1;
                        match j["nextPageToken"].as_str().filter(|t| !t.is_empty()) {
                            Some(t) if pages < MAX_PAGES => token = Some(t.to_string()),
                            _ => break Ok(std::mem::take(&mut found)),
                        }
                    }
                }
            };
            (q, res)
        }
    }))
    .buffer_unordered(16)
    .collect()
    .await;
    let mut fresh: Vec<CacheEntry> = vec![];
    for (q, r) in results {
        match r {
            Ok(v) => {
                fresh.push((q.key(), (now, v.clone())));
                insights.extend(v);
            }
            Err(e) if e.contains("quota project") || e.contains("has not been used") || e.contains("SERVICE_DISABLED") => {
                let msg = "Google Cloud recommendations unavailable: set a quota project with the Recommender API enabled (--quota-project or quota_project: in costerra.yml)".to_string();
                if !notes.contains(&msg) {
                    notes.push(msg);
                }
            }
            Err(e) if e.contains("403") || e.contains("PERMISSION") => {
                // No access to this project's recommendations: remember the empty result.
                fresh.push((q.key(), (now, vec![])));
            }
            Err(e) => tracing::debug!("recommender {} {} {}: {e}", q.parent, q.location, q.recommender),
        }
    }
    store_cache(cache_dir, fresh);
    Fetched { insights, notes }
}

// ---------------------------------------------------------------------------
// Rabbit (followrabbit.ai)
// ---------------------------------------------------------------------------

/// Rabbit API key: `FOLLOWRABBIT_API_KEY`, or the `followrabbit` CLI credentials.
pub fn rabbit_key() -> Option<String> {
    if let Ok(k) = std::env::var("FOLLOWRABBIT_API_KEY") {
        return Some(k).filter(|k| !k.is_empty());
    }
    let dir =
        std::env::var("RABBIT_CONFIG_DIR").map(PathBuf::from).ok().or_else(|| dirs::home_dir().map(|h| h.join(".config/followrabbit")))?;
    let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("credentials.json")).ok()?).ok()?;
    ["api_key", "apiKey", "key"].iter().find_map(|k| j[*k].as_str().map(str::to_string))
}

/// `origin` remote URL of the repository containing `path`, without
/// credentials (CI remotes embed tokens: `https://gitlab-ci-token:…@gitlab.com/…`).
pub fn repo_url(path: &Path) -> Option<String> {
    let dir = if path.is_file() { path.parent()? } else { path };
    let o = std::process::Command::new("git").arg("-C").arg(dir).args(["remote", "get-url", "origin"]).output().ok()?;
    o.status.success().then(|| crate::util::strip_url_credentials(String::from_utf8_lossy(&o.stdout).trim())).filter(|s| !s.is_empty())
}

fn find_array(j: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    if let Some(a) = j.as_array() {
        return Some(a);
    }
    for k in ["recommendations", "recos", "items", "results", "data"] {
        if let Some(a) = j.get(k).and_then(find_array) {
            return Some(a);
        }
    }
    None
}

fn parse_rabbit(j: &serde_json::Value) -> Vec<Insight> {
    let str_of = |r: &serde_json::Value, keys: &[&str]| keys.iter().find_map(|k| r[*k].as_str().map(str::to_string));
    let num_of = |r: &serde_json::Value, keys: &[&str]| {
        keys.iter().find_map(|k| r[*k].as_f64().or_else(|| r[*k].as_str().and_then(|s| s.parse().ok())))
    };
    find_array(j)
        .into_iter()
        .flatten()
        .filter(|r| str_of(r, &["status"]).is_none_or(|s| s == "open"))
        .map(|r| Insight {
            source: "rabbit".into(),
            kind: str_of(r, &["type", "category"]).unwrap_or_else(|| "recommendation".into()),
            title: str_of(r, &["title", "summary", "name"]).unwrap_or_else(|| "Rabbit recommendation".into()),
            detail: str_of(r, &["description", "details", "rationale", "summary"]),
            monthly_savings: num_of(r, &["estimated_savings_monthly", "estimatedSavingsMonthly", "monthly_savings", "estimated_savings"]),
            priority: str_of(r, &["priority", "severity", "confidence"]),
            resources: r["affected_resources"]
                .as_array()
                .or_else(|| r["resources"].as_array())
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(str::to_string).or_else(|| str_of(x, &["address", "name", "resource", "id"])))
                .collect(),
            location: str_of(r, &["location", "region"]),
            reference: str_of(r, &["url", "link", "app_url", "appUrl"])
                .or_else(|| str_of(r, &["id", "recommendation_id"]).map(|id| format!("recommendation {id}")))
                .or_else(|| r["created_at"].as_str().map(|c| format!("created {c}"))),
        })
        .collect()
}

/// `git@gitlab.com:org/repo.git`, `ssh://git@gitlab.com:2222/org/repo.git`,
/// `https://user:token@gitlab.com/org/repo.git` -> `https://gitlab.com/org/repo`.
pub fn normalize_repo(url: &str) -> String {
    let u = url.trim().trim_end_matches('/').trim_end_matches(".git");
    // scp-like syntax: [user@]host:path (no scheme).
    if !u.contains("://")
        && let Some((host, path)) = u.split_once(':')
    {
        let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
        return format!("https://{host}/{}", path.trim_start_matches('/'));
    }
    let Some((scheme, rest)) = u.split_once("://") else { return u.to_string() };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    // The SSH port has no meaning for the https form; an explicit https port is kept.
    let host = match (scheme, host.rsplit_once(':')) {
        ("ssh" | "git+ssh" | "git", Some((h, port))) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    };
    let scheme = if scheme == "http" { "http" } else { "https" };
    if path.is_empty() { format!("{scheme}://{host}") } else { format!("{scheme}://{host}/{path}") }
}

enum RabbitReply {
    Recos(Vec<Insight>),
    RateLimited,
    Unauthorized,
    Failed,
}

async fn rabbit_get(client: &reqwest::Client, base: &str, key: &str, repo: &str, until: tokio::time::Instant) -> RabbitReply {
    let rq = client
        .get(format!("{}/cli/recos", base.trim_end_matches('/')))
        .query(&[("repo", repo), ("status", "open")])
        .header("X-Rabbit-Api-Key", key);
    match tokio::time::timeout_at(until, rq.send()).await {
        Ok(Ok(r)) if r.status().is_success() => RabbitReply::Recos(parse_rabbit(&r.json().await.unwrap_or_default())),
        Ok(Ok(r)) if r.status().as_u16() == 429 => RabbitReply::RateLimited,
        Ok(Ok(r)) if r.status().as_u16() == 401 || r.status().as_u16() == 403 => RabbitReply::Unauthorized,
        Ok(Ok(r)) => {
            tracing::debug!("rabbit: HTTP {}", r.status());
            RabbitReply::Failed
        }
        Ok(Err(e)) => {
            tracing::debug!("rabbit: {e}");
            RabbitReply::Failed
        }
        Err(_) => RabbitReply::Failed,
    }
}

/// Open Rabbit recommendations saved for this repository (cached for 6 h).
/// The remote URL is tried as-is (like the `followrabbit` CLI), then in its
/// normalized https form.
pub async fn fetch_rabbit(cache_dir: &Path, key: &str, repo: &str, until: std::time::Instant) -> Fetched {
    // Never send, cache or print credentials embedded in the remote URL.
    let repo_clean = crate::util::strip_url_credentials(repo);
    let repo = repo_clean.as_str();
    let ck = format!("rabbit|{repo}");
    let cache = load_cache(cache_dir);
    let now = crate::pricing::sync::now();
    let no_recos_note = |n: usize| {
        (n == 0).then(|| format!("Rabbit: no open recommendations saved for {}", normalize_repo(repo))).into_iter().collect::<Vec<_>>()
    };
    if let Some((at, v)) = cache.entries.get(&ck)
        && now.saturating_sub(*at) < CACHE_TTL_SECS
    {
        return Fetched { insights: v.clone(), notes: no_recos_note(v.len()) };
    }
    let base = std::env::var("RABBIT_API_URL").unwrap_or_else(|_| "https://api.agentic.followrabbit.ai".into());
    let Ok(client) = reqwest::Client::builder().user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION"))).build() else {
        return Fetched { insights: vec![], notes: vec![] };
    };
    let until = tokio::time::Instant::from_std(until);
    let mut forms = vec![repo.to_string()];
    let normalized = normalize_repo(repo);
    if normalized != repo {
        forms.push(normalized);
    }
    let stale = cache.entries.get(&ck).map(|(_, v)| v.clone()).unwrap_or_default();
    for form in &forms {
        match rabbit_get(&client, &base, key, form, until).await {
            RabbitReply::Recos(v) if !v.is_empty() || form == forms.last().unwrap() => {
                store_cache(cache_dir, vec![(ck.clone(), (now, v.clone()))]);
                let notes = no_recos_note(v.len());
                return Fetched { insights: v, notes };
            }
            RabbitReply::Recos(_) => continue,
            RabbitReply::RateLimited => {
                return Fetched {
                    insights: stale,
                    notes: vec!["Rabbit rate limit reached (10 requests/minute per key), previous results used".into()],
                };
            }
            RabbitReply::Unauthorized => {
                return Fetched { insights: vec![], notes: vec!["Rabbit recommendations unavailable: invalid or revoked API key".into()] };
            }
            RabbitReply::Failed => return Fetched { insights: stale, notes: vec![] },
        }
    }
    Fetched { insights: stale, notes: vec![] }
}

// ---------------------------------------------------------------------------
// Matching to resources
// ---------------------------------------------------------------------------

/// Terraform types a GCP resource collection corresponds to.
fn types_for_collection(service: &str, collection: &str) -> &'static [&'static str] {
    match (service, collection) {
        ("compute", "instances") => &["google_compute_instance"],
        ("compute", "disks") => &["google_compute_disk", "google_compute_region_disk"],
        ("compute", "addresses") => &["google_compute_address", "google_compute_global_address"],
        ("compute", "images") => &["google_compute_image"],
        ("compute", "instanceGroupManagers") => &["google_compute_instance_group_manager", "google_compute_region_instance_group_manager"],
        ("sqladmin", "instances") => &["google_sql_database_instance"],
        ("container", "clusters") => &["google_container_cluster"],
        ("bigquery", "datasets") => &["google_bigquery_dataset"],
        ("bigquery", "tables") => &["google_bigquery_table"],
        _ => &[],
    }
}

/// (service, project, collection, name) of `//compute.googleapis.com/projects/p/zones/z/instances/i`.
fn parse_gcp_name(n: &str) -> Option<(String, String, String, String)> {
    let rest = n.strip_prefix("//")?;
    let (host, path) = rest.split_once('/')?;
    let service = host.split('.').next()?.to_string();
    let segs: Vec<&str> = path.split('/').collect();
    let project = segs.iter().position(|s| *s == "projects").and_then(|i| segs.get(i + 1)).map(|s| s.to_string())?;
    let name = segs.last()?.to_string();
    let collection = segs.get(segs.len().checked_sub(2)?)?.to_string();
    Some((service, project, collection, name))
}

fn severity(priority: Option<&str>, source: &str) -> Severity {
    match priority.unwrap_or("") {
        "P1" | "HIGH" | "high" => Severity::High,
        "P2" | "MEDIUM" | "medium" => Severity::Medium,
        "P3" | "LOW" | "low" => Severity::Low,
        _ if source == "rabbit" => Severity::Medium,
        _ => Severity::Info,
    }
}

/// Merge Google's 1-year / 3-year commitment alternatives for the same
/// resource family and location into one recommendation (they are not
/// cumulative): the best saving is kept, both are described.
fn merge_commitments(insights: Vec<Insight>) -> Vec<Insight> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^Purchase a (\d) year (?:new )?(.+?) for (.+)$").expect("valid regex"));
    static RE_WHAT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i) for (.+)$").expect("valid regex"));
    let (re, re_what) = (&*RE, &*RE_WHAT);
    let mut groups: indexmap::IndexMap<String, Vec<(u32, Insight)>> = indexmap::IndexMap::new();
    let mut out = vec![];
    for i in insights {
        match re.captures(&i.title) {
            Some(c) if i.source == "gcp" => {
                let years: u32 = c[1].parse().unwrap_or(1);
                let key = format!("{}|{}|{}", &c[3], i.location.clone().unwrap_or_default(), i.resources.join(","));
                groups.entry(key).or_default().push((years, i));
            }
            _ => out.push(i),
        }
    }
    for (_, mut alts) in groups {
        alts.sort_by(|a, b| b.1.monthly_savings.unwrap_or(0.0).total_cmp(&a.1.monthly_savings.unwrap_or(0.0)));
        let what = re_what.captures(&alts[0].1.title).map(|c| c[1].to_string()).unwrap_or_default();
        // Google proposes several coverage targets per term: keep the best of each term.
        let mut per_term: Vec<(u32, f64)> = vec![];
        for (y, i) in &alts {
            let v = i.monthly_savings.unwrap_or(0.0);
            match per_term.iter_mut().find(|(t, _)| t == y) {
                Some((_, best)) => *best = best.max(v),
                None => per_term.push((*y, v)),
            }
        }
        per_term.sort_by_key(|t| std::cmp::Reverse(t.0));
        let options: Vec<String> = per_term.iter().map(|(y, v)| format!("{y}-year saves ~${v:.0}/mo")).collect();
        let (_, mut best) = alts.remove(0);
        best.title = format!("Commit to {what}");
        best.detail = Some(format!("resource-based committed use discount: {}", options.join(", ")));
        out.push(best);
    }
    out
}

/// Attach insights to the report: matched to resources when possible,
/// otherwise listed as external recommendations.
pub fn attach(report: &mut Report, projects: &[Project], insights: Vec<Insight>, notes: Vec<String>) {
    let insights = merge_commitments(insights);
    let mut seen = BTreeSet::new();
    for ins in insights {
        let key = format!("{}|{}|{}", ins.source, ins.title, ins.resources.join(","));
        if !seen.insert(key) {
            continue;
        }
        let mut matched = false;
        for (pi, p) in projects.iter().enumerate() {
            for r in &p.resources {
                let hit = ins.resources.iter().any(|res| {
                    if res == &r.address || r.address.ends_with(&format!(".{res}")) {
                        return true;
                    }
                    let Some((svc, project, coll, name)) = parse_gcp_name(res) else { return false };
                    let rproject = r.attrs.get("project").and_then(|v| v.as_str()).or(r.provider.project.as_deref());
                    types_for_collection(&svc, &coll).contains(&r.rtype.as_str())
                        && r.attrs.get("name").and_then(|v| v.as_str()) == Some(name.as_str())
                        && rproject.is_none_or(|rp| rp == project)
                });
                if hit {
                    if let Some(pr) = report.projects.iter_mut().find(|x| x.name == projects[pi].name) {
                        pr.recommendations.push(Recommendation {
                            id: format!("{}-{}", ins.source, ins.kind),
                            source: ins.source.clone(),
                            source_ref: ins.reference.clone(),
                            severity: severity(ins.priority.as_deref(), &ins.source),
                            address: r.address.clone(),
                            title: ins.title.clone(),
                            detail: ins.detail.clone().unwrap_or_default(),
                            monthly_savings: ins.monthly_savings,
                        });
                    }
                    matched = true;
                }
            }
        }
        if !matched {
            let scope = ins
                .resources
                .first()
                .map(|res| match res.split_once("billingAccounts/") {
                    Some((_, ba)) => format!("billing account {} (all projects)", ba.trim_end_matches('/')),
                    None => res.trim_start_matches("//").to_string(),
                })
                .unwrap_or_default();
            let address = match &ins.location {
                Some(l) if !scope.is_empty() => format!("{scope} · {l}"),
                Some(l) => l.clone(),
                None => scope,
            };
            report.external_recommendations.push(Recommendation {
                id: format!("{}-{}", ins.source, ins.kind),
                source: ins.source.clone(),
                source_ref: ins.reference.clone(),
                severity: severity(ins.priority.as_deref(), &ins.source),
                address,
                title: ins.title.clone(),
                detail: ins.detail.clone().unwrap_or_default(),
                monthly_savings: ins.monthly_savings,
            });
        }
    }
    report.external_recommendations.sort_by(|a, b| b.monthly_savings.unwrap_or(0.0).total_cmp(&a.monthly_savings.unwrap_or(0.0)));
    report.insight_notes = notes;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcp_names() {
        assert_eq!(
            parse_gcp_name("//compute.googleapis.com/projects/p1/zones/europe-west1-b/instances/vm-1"),
            Some(("compute".into(), "p1".into(), "instances".into(), "vm-1".into()))
        );
    }

    #[test]
    fn gcp_parse() {
        let j = serde_json::json!({"recommendations": [{
            "description": "Save cost by changing machine type from e2-standard-8 to e2-standard-4.",
            "priority": "P2",
            "stateInfo": {"state": "ACTIVE"},
            "primaryImpact": {"category": "COST", "costProjection": {"cost": {"units": "-100", "nanos": 0}, "duration": "2592000s"}},
            "content": {"operationGroups": [{"operations": [{"resource": "//compute.googleapis.com/projects/p/zones/z/instances/vm"}]}]}
        }]});
        let v = parse_gcp(&j, "google.compute.instance.MachineTypeRecommender", None);
        assert_eq!(v.len(), 1);
        assert!((v[0].monthly_savings.unwrap() - 101.388).abs() < 0.01);
        assert_eq!(v[0].resources, vec!["//compute.googleapis.com/projects/p/zones/z/instances/vm"]);
    }

    #[test]
    fn repo_urls() {
        assert_eq!(normalize_repo("git@gitlab.com:example/infra.git"), "https://gitlab.com/example/infra");
        assert_eq!(normalize_repo("https://github.com/example/infra.git"), "https://github.com/example/infra");
        assert_eq!(normalize_repo("ssh://git@gitlab.com:2222/example/infra.git"), "https://gitlab.com/example/infra");
        assert_eq!(normalize_repo("ssh://git@gitlab.com/example/infra.git"), "https://gitlab.com/example/infra");
        assert_eq!(normalize_repo("https://gitlab-ci-token:glcbt-x@gitlab.com/example/infra.git"), "https://gitlab.com/example/infra");
        assert_eq!(normalize_repo("https://git.example.com:8443/example/infra"), "https://git.example.com:8443/example/infra");
        assert_eq!(normalize_repo("gitlab.com:example/infra.git"), "https://gitlab.com/example/infra");
    }

    #[test]
    fn rabbit_envelope() {
        let j = serde_json::json!({"recommendations": [], "total_count": 0, "filtered_count": 0});
        assert!(parse_rabbit(&j).is_empty());
    }

    #[test]
    fn rabbit_parse() {
        let j = serde_json::json!({"data": {"recommendations": [{"type": "rightsizing", "summary": "Downsize vm", "estimated_savings_monthly": 42.5, "affected_resources": ["google_compute_instance.vm"], "status": "open"}]}});
        let v = parse_rabbit(&j);
        assert_eq!(v[0].monthly_savings, Some(42.5));
        assert_eq!(v[0].resources, vec!["google_compute_instance.vm"]);
    }
}

// ---------------------------------------------------------------------------
// Rabbit AI cost review (opt-in: uploads Terraform files, uses Rabbit quota)
// ---------------------------------------------------------------------------

const REVIEW_BUDGET: usize = 512 * 1024;
const REVIEW_SKILLS: &[&str] = &["cost-impact", "partition-check", "best-practices"];

/// Terraform files selected for a Rabbit review.
#[derive(Debug, Default)]
pub struct ReviewFiles {
    /// (relative path, content after redaction), within the upload budget.
    pub files: Vec<(String, String)>,
    /// Files left out because the upload budget was reached.
    pub over_budget: Vec<String>,
    /// Files left out by `exclude_paths`.
    pub excluded: usize,
    /// Secret values replaced by `<redacted>`.
    pub redacted: usize,
}

impl ReviewFiles {
    pub fn bytes(&self) -> usize {
        self.files.iter().map(|(_, c)| c.len()).sum()
    }
}

fn is_review_file(name: &str) -> bool {
    name.ends_with(".tf") || name.ends_with(".tfvars") || name.ends_with(".tfvars.json") || name.ends_with(".hcl")
}

/// Attribute / variable names whose literal values are treated as secrets.
static SECRET_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(password|passwd|secret|token|api_?key|private_?key|credential|access_?key|connection_?string|auth_?string|sas_?key|webhook_?url)",
    )
    .expect("valid regex")
});
/// `name = "literal"` (HCL, also inside `{ "…" }`) or `"name": "literal"` (JSON).
static SECRET_ASSIGN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)((?:^|[\s{,(])"?([A-Za-z0-9_\-]+)"?\s*[=:]\s*)"(?:[^"\\\n]|\\.)*""#).expect("valid regex"));
/// `name = <<EOT` / `name = <<-EOT` heredoc start.
static HEREDOC_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*([A-Za-z0-9_\-]+)\s*=\s*)<<-?([A-Za-z_][A-Za-z0-9_]*)\s*$").expect("valid regex"));
static PRIVATE_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----").expect("valid regex"));

/// Replace literal secret values (passwords, tokens, keys, private key blocks)
/// before content leaves the machine. Returns the content and the number of
/// values replaced. References (`var.x`, `data.…`) are kept: they carry no secret.
pub fn redact_secrets(content: &str) -> (String, usize) {
    let mut n = 0;
    let mut out = String::with_capacity(content.len());
    let mut heredoc: Option<String> = None;
    for line in content.split_inclusive('\n') {
        if let Some(marker) = &heredoc {
            if line.trim() == marker {
                heredoc = None;
            }
            continue;
        }
        if let Some(c) = HEREDOC_START.captures(line.trim_end_matches(['\r', '\n']))
            && SECRET_NAME.is_match(&c[2])
        {
            heredoc = Some(c[3].to_string());
            n += 1;
            out.push_str(&format!("{}\"<redacted>\"\n", &c[1]));
            continue;
        }
        out.push_str(line);
    }
    let out = SECRET_ASSIGN.replace_all(&out, |c: &regex::Captures| {
        if SECRET_NAME.is_match(&c[2]) && !c[0].ends_with("\"<redacted>\"") {
            n += 1;
            format!("{}\"<redacted>\"", &c[1])
        } else {
            c[0].to_string()
        }
    });
    let out = PRIVATE_KEY.replace_all(&out, |_: &regex::Captures| {
        n += 1;
        "<redacted private key>"
    });
    (out.into_owned(), n)
}

/// Terraform files under `root` to upload: `.gitignore`d, hidden,
/// `.terraform` and `node_modules` paths are skipped, as well as paths
/// matching `exclude` (globs relative to `root`) or listed in a
/// `.costerraignore` file. Secret values are redacted. Files that do not fit
/// in the upload budget are reported, not silently dropped.
pub fn review_files(root: &Path, exclude: &[String]) -> ReviewFiles {
    let mut gs = globset::GlobSetBuilder::new();
    for p in exclude {
        if let Ok(g) = globset::Glob::new(p) {
            gs.add(g);
        }
    }
    let excludes = gs.build().unwrap_or_else(|_| globset::GlobSet::empty());
    let mut out = ReviewFiles::default();
    let mut found: Vec<(String, PathBuf)> = vec![];
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .require_git(false)
        .add_custom_ignore_filename(".costerraignore")
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("node_modules" | ".terraform" | ".terragrunt-cache")))
        .build();
    for e in walker.flatten() {
        if !e.file_type().is_some_and(|t| t.is_file()) || !is_review_file(&e.file_name().to_string_lossy()) {
            continue;
        }
        let Ok(rel) = e.path().strip_prefix(root) else { continue };
        if excludes.is_match(rel) {
            out.excluded += 1;
            continue;
        }
        found.push((rel.to_string_lossy().replace('\\', "/"), e.path().to_path_buf()));
    }
    found.sort();
    let mut used = 0;
    for (rel, path) in found {
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let (content, n) = redact_secrets(&content);
        if used + content.len() > REVIEW_BUDGET {
            out.over_budget.push(rel);
            continue;
        }
        used += content.len();
        out.redacted += n;
        out.files.push((rel, content));
    }
    out
}

/// What `--rabbit-review` would upload, without uploading anything.
pub fn review_dry_run(root: &Path, exclude: &[String]) -> String {
    let rf = review_files(root, exclude);
    let mut s = format!(
        "Rabbit AI cost review would upload {} files ({:.1} KiB of {} KiB) from {}:\n",
        rf.files.len(),
        rf.bytes() as f64 / 1024.0,
        REVIEW_BUDGET / 1024,
        root.display()
    );
    for (p, c) in &rf.files {
        s.push_str(&format!("  {p}  ({:.1} KiB)\n", c.len() as f64 / 1024.0));
    }
    if rf.redacted > 0 {
        s.push_str(&format!("{} secret values would be replaced by <redacted>\n", rf.redacted));
    }
    if rf.excluded > 0 {
        s.push_str(&format!("{} files excluded by exclude_paths\n", rf.excluded));
    }
    if !rf.over_budget.is_empty() {
        s.push_str(&format!("{} files left out (upload limit reached):\n", rf.over_budget.len()));
        for p in &rf.over_budget {
            s.push_str(&format!("  {p}\n"));
        }
    }
    s.push_str(".gitignore'd and hidden files are never uploaded; add paths to .costerraignore or exclude_paths to skip more.\n");
    s
}

/// Line of `resource "TYPE" "NAME"` in a file (1-based).
fn resource_line(content: &str, rtype: &str, name: &str) -> Option<usize> {
    let needle = format!("\"{rtype}\" \"{name}\"");
    content.lines().position(|l| l.trim_start().starts_with("resource") && l.contains(&needle)).map(|i| i + 1)
}

fn review_context(root: &Path, projects: &[Project], rf: &ReviewFiles) -> Option<(serde_json::Value, String)> {
    let files = &rf.files;
    if files.is_empty() {
        return None;
    }
    let by_path: HashMap<&str, &str> = files.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
    let mut resources = vec![];
    let mut seen = BTreeSet::new();
    for r in projects.iter().flat_map(|p| p.resources.iter()) {
        let Some(file) = r.file.as_ref().and_then(|f| f.strip_prefix(root).ok()).map(|f| f.to_string_lossy().replace('\\', "/")) else {
            continue;
        };
        // Only resources of uploaded files (not .gitignore'd / excluded ones).
        let Some(content) = by_path.get(file.as_str()) else { continue };
        if !seen.insert((file.clone(), r.rtype.clone(), r.name.clone())) {
            continue;
        }
        let attributes: serde_json::Map<String, serde_json::Value> = r
            .attrs
            .as_map()
            .into_iter()
            .flatten()
            .filter(|(k, v)| {
                v.as_str().is_some_and(|s| !s.starts_with(crate::REF_PREFIX))
                    && !matches!(k.as_str(), "id" | "self_link" | "self_link_unique")
                    && !SECRET_NAME.is_match(k)
            })
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.as_str().unwrap_or_default().to_string())))
            .collect();
        let line = resource_line(content, &r.rtype, &r.name);
        resources.push(serde_json::json!({"type": r.rtype, "name": r.name, "file": file, "line": line, "attributes": attributes}));
    }
    let ctx = serde_json::json!({
        "terraform": {
            "resources": resources,
            "modules": [],
            "environments": null,
            "files": files.iter().map(|(p, c)| serde_json::json!({"path": p, "content": c})).collect::<Vec<_>>(),
        }
    });
    let s = ctx.to_string();
    let hash = crate::tf::source::fnv_hash(&s);
    Some((ctx, hash))
}

/// A path from the review, if it is a plain relative path inside the repository.
fn safe_relative(path: &str) -> Option<&Path> {
    let p = Path::new(path);
    p.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir)).then_some(p)
}

static MONEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~?\$([0-9][0-9,]*(?:\.[0-9]+)?)\s*/\s*mo(?:nth)?").expect("valid regex"));
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\*\*(?:Files?|Location|Resources?):\*\*\s*`?([\w./\-]+\.(?:tf|hcl|tfvars|json))(?::(\d+))?`?(?:[^\n]*?lines?\s+(\d+))?",
    )
    .expect("valid regex")
});
static DECL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"resource\s+"([a-z0-9_]+)"\s+"([A-Za-z0-9_\-]+)""#).expect("valid regex"));
static REF_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`?\b(google_[a-z0-9_]+)\.([A-Za-z0-9_\-]+)\b").expect("valid regex"));
static IMPACT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*Impact:\*\*\s*([A-Za-z\-]+)").expect("valid regex"));

/// Parse the markdown review ("### N. Title", "**File:** `path:line`",
/// "**Impact:** High (~$85.00/month savings)", description).
fn parse_review_markdown(md: &str, root: &Path, projects: &[Project]) -> Vec<Insight> {
    let (money_re, file_re, decl_re, ref_re, impact_re) = (&*MONEY_RE, &*FILE_RE, &*DECL_RE, &*REF_RE, &*IMPACT_RE);
    let mut out = vec![];
    for section in md.split("\n### ").skip(1) {
        let mut lines = section.lines();
        let title = lines.next().unwrap_or_default().trim();
        let title =
            title.split_once(". ").filter(|(n, _)| n.chars().all(|c| c.is_ascii_digit())).map(|(_, t)| t).unwrap_or(title).to_string();
        let body: String = section.split("```").next().unwrap_or_default().to_string();
        let file = file_re.captures(&body).map(|c| {
            let line = c.get(2).or(c.get(3)).and_then(|l| l.as_str().parse::<usize>().ok());
            (c[1].to_string(), line)
        });
        let impact = impact_re.captures(&body).map(|c| c[1].to_string());
        let savings = body
            .lines()
            .find(|l| l.contains("**Impact:**"))
            .and_then(|l| money_re.captures(l))
            .and_then(|c| c[1].replace(',', "").parse::<f64>().ok());
        let detail = body
            .lines()
            .skip(1)
            .map(str::trim)
            .filter(|l| {
                let label = l.strip_prefix("**").and_then(|x| x.split_once(":**")).map(|(k, _)| k.to_lowercase());
                !l.is_empty()
                    && !l.starts_with("---")
                    && !l.starts_with('#')
                    && !matches!(label.as_deref(), Some("file" | "files" | "location" | "resource" | "resources" | "impact"))
            })
            .collect::<Vec<_>>()
            .join(" ");
        // Resources: the one declared at file:line, plus those named in the
        // finding (text or snippet) as `resource "T" "N"` or `T.N`.
        let mut named: Vec<(String, String)> = vec![];
        if let Some((path, Some(line))) = &file
            && let Some(rel) = safe_relative(path)
        {
            // The resource block enclosing file:line (nearest declaration at or above it).
            if let Ok(content) = std::fs::read_to_string(root.join(rel)) {
                let lines: Vec<&str> = content.lines().collect();
                let start = line.saturating_sub(1).min(lines.len().saturating_sub(1));
                if let Some(c) = (0..=start)
                    .rev()
                    .find_map(|i| {
                        let l = lines.get(i)?;
                        if i < start
                            && (l.starts_with("variable") || l.starts_with("module") || l.starts_with("locals") || l.starts_with("output"))
                        {
                            return Some(None);
                        }
                        decl_re.captures(l).map(Some)
                    })
                    .flatten()
                {
                    named.push((c[1].to_string(), c[2].to_string()));
                }
            }
        }
        // Without a declaration at file:line, fall back to the names mentioned.
        if named.is_empty() {
            for c in decl_re.captures_iter(section) {
                named.push((c[1].to_string(), c[2].to_string()));
            }
            for c in ref_re.captures_iter(section) {
                named.push((c[1].to_string(), c[2].to_string()));
            }
        }
        named.sort();
        named.dedup();
        let mut resources: Vec<String> = projects
            .iter()
            .flat_map(|p| p.resources.iter())
            .filter(|r| named.iter().any(|(t, n)| &r.rtype == t && &r.name == n))
            .filter(|r| file.as_ref().is_none_or(|(p, _)| r.file.as_ref().is_none_or(|f| f.ends_with(p)) || named.len() > 1))
            .map(|r| r.address.clone())
            .collect();
        resources.sort();
        resources.dedup();
        let location = file.as_ref().map(|(p, l)| l.map(|l| format!("{p}:{l}")).unwrap_or_else(|| p.clone()));
        out.push(Insight {
            source: "rabbit".into(),
            kind: "ai-review".into(),
            title,
            detail: Some(detail).filter(|d| !d.is_empty()),
            monthly_savings: savings,
            priority: impact.map(|i| match i.to_lowercase().as_str() {
                "high" => "HIGH".into(),
                "medium" | "medium-high" => "MEDIUM".into(),
                _ => "LOW".into(),
            }),
            resources,
            location,
            reference: None,
        });
    }
    out
}

/// Structured findings, when the API returns them.
fn parse_review_findings(j: &serde_json::Value) -> Vec<Insight> {
    let mut v = parse_rabbit(&serde_json::json!({"recommendations": j["findings"].clone()}));
    for i in v.iter_mut() {
        i.kind = "ai-review".into();
    }
    v
}

/// Notes about what was (not) uploaded.
fn upload_notes(rf: &ReviewFiles) -> Vec<String> {
    let mut notes = vec![];
    if rf.redacted > 0 {
        notes.push(format!("Rabbit AI cost review: {} secret values redacted before upload", rf.redacted));
    }
    if !rf.over_budget.is_empty() {
        notes.push(format!(
            "Rabbit AI cost review: {} files not reviewed (upload limit of {} KiB reached), e.g. {}; review a subdirectory or exclude paths",
            rf.over_budget.len(),
            REVIEW_BUDGET / 1024,
            rf.over_budget.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    notes
}

/// Rabbit AI cost review of the Terraform under `root`. Results are cached by
/// content hash, so unchanged code is not reviewed (nor billed) twice.
pub async fn rabbit_review(
    cache_dir: &Path,
    key: &str,
    root: &Path,
    projects: &[Project],
    exclude: &[String],
    until: std::time::Instant,
) -> Fetched {
    let rf = review_files(root, exclude);
    let Some((ctx, hash)) = review_context(root, projects, &rf) else {
        return Fetched { insights: vec![], notes: vec!["Rabbit AI cost review: no Terraform files to review".into()] };
    };
    let mut notes = upload_notes(&rf);
    let ck = format!("rabbit-review|{hash}");
    let cache = load_cache(cache_dir);
    let now = crate::pricing::sync::now();
    if let Some((_, v)) = cache.entries.get(&ck) {
        // Re-parse the raw review when available (parsing improvements apply for free).
        let raw = cache.entries.get(&format!("rabbit-review-raw|{hash}")).and_then(|(_, r)| r.first()).and_then(|i| i.detail.clone());
        let v: Vec<Insight> = match raw {
            Some(md) => {
                let reference = v.first().and_then(|i| i.reference.clone()).map(|r| r.split(" · ").take(3).collect::<Vec<_>>().join(" · "));
                let mut parsed = parse_review_markdown(&md, root, projects);
                for i in parsed.iter_mut() {
                    let loc = i.location.take();
                    i.reference = reference.clone().map(|r| match &loc {
                        Some(l) => format!("{r} · {l}"),
                        None => r,
                    });
                }
                parsed
            }
            None => v.clone(),
        };
        notes.push("Rabbit AI cost review: cached result for unchanged code (no quota used)".into());
        return Fetched { insights: v, notes };
    }
    let base = std::env::var("RABBIT_API_URL").unwrap_or_else(|_| "https://api.agentic.followrabbit.ai".into());
    let Ok(client) = reqwest::Client::builder().user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION"))).gzip(true).build() else {
        return Fetched { insights: vec![], notes };
    };
    let body = serde_json::json!({"context": ctx.to_string(), "skills": REVIEW_SKILLS});
    let rq = client.post(format!("{}/cli/engine", base.trim_end_matches('/'))).header("X-Rabbit-Api-Key", key).json(&body);
    let resp = tokio::time::timeout_at(tokio::time::Instant::from_std(until), rq.send()).await;
    let r = match resp {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            notes.push(format!("Rabbit AI cost review failed: {e}"));
            return Fetched { insights: vec![], notes };
        }
        Err(_) => {
            notes.push("Rabbit AI cost review timed out".into());
            return Fetched { insights: vec![], notes };
        }
    };
    let status = r.status();
    let j: serde_json::Value = r.json().await.unwrap_or_default();
    if !status.is_success() {
        let msg = j["error"]["message"].as_str().or(j["error"].as_str()).unwrap_or("request failed");
        notes.push(format!("Rabbit AI cost review: HTTP {status}: {msg}"));
        return Fetched { insights: vec![], notes };
    }
    let data = if j.get("data").is_some() { &j["data"] } else { &j };
    let mut insights = parse_review_findings(data);
    if insights.is_empty() {
        insights = parse_review_markdown(data["output"].as_str().unwrap_or_default(), root, projects);
    }
    let reference = format!(
        "AI cost review{}{}",
        data["model"].as_str().map(|m| format!(" · model {m}")).unwrap_or_default(),
        data["request_id"].as_str().map(|r| format!(" · request {r}")).unwrap_or_default()
    );
    for i in insights.iter_mut() {
        let loc = i.location.take();
        i.reference = Some(match loc {
            Some(l) => format!("{reference} · {l}"),
            None => reference.clone(),
        });
    }
    // Raw review, for inspection (`jq` on insights.json) and re-parsing.
    let raw = Insight {
        source: "rabbit".into(),
        kind: "raw".into(),
        title: "raw review".into(),
        detail: data["output"].as_str().map(str::to_string),
        monthly_savings: None,
        priority: None,
        resources: vec![],
        location: None,
        reference: None,
    };
    store_cache(cache_dir, vec![(ck, (now, insights.clone())), (format!("rabbit-review-raw|{hash}"), (now, vec![raw]))]);
    let cost = data["cost_usd"].as_f64().unwrap_or(0.0);
    notes.push(format!(
        "Rabbit AI cost review: {} findings, ${cost:.4} of your Rabbit quota",
        data["output"].as_str().map(|o| o.matches("\n### ").count()).unwrap_or(insights.len())
    ));
    Fetched { insights, notes }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn markdown_findings() {
        let md = "## Cost Optimization\n\n### 1. Remove Unattached Disk\n**File:** `main.tf:42`  \n**Impact:** High (~$85.00/month savings)  \nThe disk is not attached.\n\n```hcl\nresource {}\n```\n\n---\n\n### 2. Use Spot\n**File:** `main.tf:57`  \n**Impact:** High (~60–70% compute savings)  \nGPU training can run on Spot.\n";
        let v = parse_review_markdown(md, Path::new("/nonexistent"), &[]);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].title, "Remove Unattached Disk");
        assert_eq!(v[0].monthly_savings, Some(85.0));
        assert_eq!(v[0].priority.as_deref(), Some("HIGH"));
        assert_eq!(v[0].location.as_deref(), Some("main.tf:42"));
        assert_eq!(v[1].monthly_savings, None);
        assert_eq!(v[1].detail.as_deref(), Some("GPU training can run on Spot."));
    }

    #[test]
    fn secrets_are_redacted() {
        let src = "variable \"x\" {}\nresource \"google_sql_user\" \"u\" {\n  name     = \"app\"\n  password = \"hunter2\"\n  password_wo = var.pw\n}\ndb_token = <<EOT\nabc\ndef\nEOT\nkey = <<EOT\nkeep\nEOT\n{\"api_key\": \"k-123\", \"region\": \"eu\"}\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----\n";
        let (out, n) = redact_secrets(src);
        assert_eq!(n, 4, "{out}");
        assert!(!out.contains("hunter2") && !out.contains("abc") && !out.contains("k-123") && !out.contains("MIIE"), "{out}");
        assert!(out.contains("name     = \"app\"") && out.contains("password_wo = var.pw") && out.contains("keep"), "{out}");
        assert!(out.contains("\"region\": \"eu\""), "{out}");
    }

    #[test]
    fn review_files_respect_gitignore_and_excludes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "secrets.auto.tfvars\n").unwrap();
        std::fs::write(root.join("main.tf"), "resource \"google_compute_address\" \"a\" {}\n").unwrap();
        std::fs::write(root.join("secrets.auto.tfvars"), "db_password = \"x\"\n").unwrap();
        std::fs::create_dir_all(root.join("examples")).unwrap();
        std::fs::write(root.join("examples/demo.tf"), "\n").unwrap();
        std::fs::create_dir_all(root.join(".terraform/modules")).unwrap();
        std::fs::write(root.join(".terraform/modules/m.tf"), "\n").unwrap();
        std::fs::write(root.join("big.tf"), "#".repeat(REVIEW_BUDGET + 1)).unwrap();
        let rf = review_files(root, &["examples/**".to_string()]);
        let names: Vec<&str> = rf.files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(names, vec!["main.tf"]);
        assert_eq!(rf.excluded, 1);
        assert_eq!(rf.over_budget, vec!["big.tf"]);
        assert!(safe_relative("../etc/passwd").is_none() && safe_relative("/etc/passwd").is_none() && safe_relative("a/b.tf").is_some());
    }
}
