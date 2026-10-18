//! Observed usage of the deployed resources, read from Google Cloud when the
//! credentials allow it:
//! * Cloud Monitoring metrics over the last 30 days (VM CPU and uptime, bucket
//!   and Cloud SQL storage, Cloud Run requests),
//! * active Compute Engine commitments.
//!
//! Every source is optional: what cannot be read (no OAuth credentials, API
//! disabled, permission denied, deadline reached) is skipped and the
//! estimate falls back to the usage file and the code. Results are cached
//! (6 h) so that runs stay instant.

use crate::pricing::sync::{Auth, resolve_auth};
use crate::project::Project;
use crate::resources::machine;
use crate::tf::evaluate::ResourceInstance;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Length of the observation window.
pub const WINDOW_DAYS: u64 = 30;
const CACHE_TTL_SECS: u64 = 6 * 3600;
const CACHE_RETENTION_SECS: u64 = 7 * 86400;
/// Less data than this is not used (resource created very recently).
const MIN_SPAN_HOURS: f64 = 72.0;
const MAX_PAGES: usize = 10;
/// Names per `one_of(…)` filter.
const CHUNK: usize = 50;
const HOURS_PER_MONTH: f64 = 730.0;

/// CPU utilization of a VM over the window (fractions, 0–1).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct CpuStats {
    pub mean: f64,
    /// 95th percentile of hourly means.
    pub p95: f64,
    /// Days covered by the data.
    pub days: f64,
}

/// What was observed for one resource of the code.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ObservedResource {
    /// Usage values, with the same keys as usage files (`monthly_hrs`,
    /// `storage_gb`, `monthly_requests`). A usage file value wins.
    pub usage: BTreeMap<String, f64>,
    pub cpu: Option<CpuStats>,
    /// Share (0–1) of the VM's vCPUs already covered by active commitments.
    pub committed_fraction: Option<f64>,
}

/// Observed data for the analysed code, keyed by (project name, address).
#[derive(Debug, Default)]
pub struct Observed {
    pub resources: HashMap<(String, String), ObservedResource>,
    /// Sources that returned data (e.g. "Cloud Monitoring").
    pub sources: BTreeSet<String>,
    /// Sources that could not be read, and how to fix it.
    pub notes: Vec<String>,
}

impl Observed {
    pub fn get(&self, project: &str, address: &str) -> Option<&ObservedResource> {
        self.resources.get(&(project.to_string(), address.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Targets: resources of the code that can be observed
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Kind {
    Vm,
    Bucket,
    Run,
    Sql,
}

impl Kind {
    fn of(rtype: &str) -> Option<Kind> {
        match rtype {
            "google_compute_instance" => Some(Kind::Vm),
            "google_storage_bucket" => Some(Kind::Bucket),
            "google_cloud_run_v2_service" | "google_cloud_run_service" | "google_cloudfunctions2_function" => Some(Kind::Run),
            "google_sql_database_instance" => Some(Kind::Sql),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Vm => "vm",
            Kind::Bucket => "bucket",
            Kind::Run => "run",
            Kind::Sql => "sql",
        }
    }
}

/// A resource of the code that can be observed.
#[derive(Debug, Clone)]
struct Target {
    /// costerra project name and Terraform address.
    project: String,
    address: String,
    kind: Kind,
    gcp_project: String,
    name: String,
    /// VMs: on-demand machine type and zone (for commitments).
    vm: Option<(String, String)>,
}

/// The resources of the code to observe (cheap to extract, owned).
#[derive(Debug, Default)]
pub struct Targets(Vec<Target>);

impl Targets {
    pub fn from_projects(sets: &[&[Project]]) -> Targets {
        let mut out = vec![];
        for p in sets.iter().flat_map(|s| s.iter()) {
            for r in &p.resources {
                let (Some(kind), Some(gcp), Some(name)) = (Kind::of(&r.rtype), gcp_project(r), known_name(r)) else { continue };
                let vm = (kind == Kind::Vm && !is_spot(r))
                    .then(|| Some((r.attrs.get("machine_type")?.as_str()?.to_string(), zone_of(r)?.to_string())))
                    .flatten();
                out.push(Target {
                    project: p.name.clone(),
                    address: r.address.clone(),
                    kind,
                    gcp_project: gcp.to_string(),
                    name: name.to_string(),
                    vm,
                });
            }
        }
        Targets(out)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn gcp_project(r: &ResourceInstance) -> Option<&str> {
    let p = r.attrs.get("project").and_then(|v| v.as_str()).or(r.provider.project.as_deref())?;
    (!p.is_empty() && !p.starts_with(crate::REF_PREFIX) && !p.contains('/')).then_some(p)
}

fn known_name(r: &ResourceInstance) -> Option<&str> {
    r.attrs.get("name").and_then(|v| v.as_str()).filter(|n| !n.is_empty() && !n.starts_with(crate::REF_PREFIX))
}

fn zone_of(r: &ResourceInstance) -> Option<&str> {
    r.attrs.get("zone").and_then(|v| v.as_str()).or(r.provider.zone.as_deref()).filter(|z| !z.starts_with(crate::REF_PREFIX))
}

/// Whether a VM of the code runs on Spot / preemptible capacity (never covered by commitments).
fn is_spot(r: &ResourceInstance) -> bool {
    let s = r.attrs.path("scheduling.0");
    s.and_then(|s| s.get("provisioning_model")).and_then(|v| v.as_str()) == Some("SPOT")
        || s.and_then(|s| s.get("preemptible")).and_then(|v| v.as_bool()) == Some(true)
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// Data points of one resource, and the hours since the oldest one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Series {
    points: Vec<f64>,
    span_hours: f64,
}

/// Why a source returned no data (cached, so that notes stay visible).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Denial {
    Disabled,
    Denied,
    Refused,
}

/// Per-name series of one query, or why there are none.
type Values = Result<BTreeMap<String, Series>, Denial>;

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    entries: HashMap<String, (u64, Values)>,
}

fn cache_path(dir: &Path) -> PathBuf {
    dir.join("observed.json")
}

fn store_cache(dir: &Path, entries: Vec<(String, (u64, Values))>) {
    if entries.is_empty() {
        return;
    }
    let now = crate::pricing::sync::now();
    crate::util::update_json(&cache_path(dir), |c: &mut Cache| {
        c.entries.extend(entries);
        c.entries.retain(|_, (at, _)| now.saturating_sub(*at) < CACHE_RETENTION_SECS);
    });
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// Why a source could not be read.
#[derive(Debug, PartialEq)]
enum Failure {
    Disabled,
    Denied,
    /// Other refusal (unknown project, invalid request): no data, no note.
    Refused,
    /// Network error, server error or deadline: retried on the next run.
    Transient,
}

fn classify(status: u16, body: &str) -> Failure {
    if body.contains("SERVICE_DISABLED") || body.contains("has not been used") || body.contains("quota project") {
        Failure::Disabled
    } else if status == 403 || status == 401 {
        Failure::Denied
    } else if status == 429 || status >= 500 {
        Failure::Transient
    } else {
        Failure::Refused
    }
}

/// Base URLs of the APIs (replaced by a local server in tests).
struct Endpoints {
    monitoring: String,
    compute: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints { monitoring: "https://monitoring.googleapis.com".into(), compute: "https://compute.googleapis.com".into() }
    }
}

struct Http<'a> {
    client: reqwest::Client,
    auth: Auth,
    quota: Option<&'a str>,
    until: tokio::time::Instant,
    api: &'a Endpoints,
}

impl Http<'_> {
    async fn get(&self, url: &str, query: &[(&str, String)]) -> Result<serde_json::Value, Failure> {
        let mut rq = self.client.get(url).query(query);
        if let Auth::Bearer(t) = &self.auth {
            rq = rq.bearer_auth(t);
        }
        if let Some(q) = self.quota {
            rq = rq.header("x-goog-user-project", q);
        }
        match tokio::time::timeout_at(self.until, rq.send()).await {
            Ok(Ok(r)) if r.status().is_success() => r.json().await.map_err(|_| Failure::Transient),
            Ok(Ok(r)) => {
                let status = r.status().as_u16();
                let body = r.text().await.unwrap_or_default();
                tracing::debug!("{url}: HTTP {status}: {}", body.chars().take(300).collect::<String>());
                Err(classify(status, &body))
            }
            Ok(Err(e)) => {
                tracing::debug!("{url}: {e}");
                Err(Failure::Transient)
            }
            Err(_) => Err(Failure::Transient),
        }
    }

    /// Cloud Monitoring time series, as values per `label`.
    async fn series(
        &self,
        project: &str,
        filter: String,
        align_secs: u64,
        aligner: &str,
        label: &str,
    ) -> Result<BTreeMap<String, Series>, Failure> {
        let url = format!("{}/v3/projects/{project}/timeSeries", self.api.monitoring);
        let end = crate::pricing::sync::now();
        let start = end - WINDOW_DAYS * 86400;
        let mut query = vec![
            ("filter", filter),
            ("interval.startTime", rfc3339(start)),
            ("interval.endTime", rfc3339(end)),
            ("aggregation.alignmentPeriod", format!("{align_secs}s")),
            ("aggregation.perSeriesAligner", aligner.to_string()),
            ("aggregation.crossSeriesReducer", "REDUCE_SUM".to_string()),
            ("aggregation.groupByFields", label.to_string()),
            ("pageSize", "1000".to_string()),
        ];
        let mut out: BTreeMap<String, Series> = BTreeMap::new();
        for _ in 0..MAX_PAGES {
            let j = self.get(&url, &query).await?;
            for ts in j["timeSeries"].as_array().into_iter().flatten() {
                let key = label_value(ts, label);
                let Some(key) = key else { continue };
                let s = out.entry(key).or_default();
                for p in ts["points"].as_array().into_iter().flatten() {
                    let Some(v) = point_value(&p["value"]) else { continue };
                    s.points.push(v);
                    if let Some(t) = p["interval"]["startTime"].as_str().and_then(parse_rfc3339) {
                        s.span_hours = s.span_hours.max(end.saturating_sub(t) as f64 / 3600.0);
                    }
                }
            }
            match j["nextPageToken"].as_str().filter(|t| !t.is_empty()) {
                Some(t) => {
                    query.retain(|(k, _)| *k != "pageToken");
                    query.push(("pageToken", t.to_string()));
                }
                None => break,
            }
        }
        Ok(out)
    }

    /// Active commitments of a project: vCPUs per "region|family".
    async fn commitments(&self, project: &str) -> Result<BTreeMap<String, Series>, Failure> {
        let url = format!("{}/compute/v1/projects/{project}/aggregated/commitments", self.api.compute);
        let j = self.get(&url, &[]).await?;
        let mut out: BTreeMap<String, Series> = BTreeMap::new();
        for scope in j["items"].as_object().into_iter().flat_map(|m| m.values()) {
            for c in scope["commitments"].as_array().into_iter().flatten() {
                if c["status"].as_str() != Some("ACTIVE") {
                    continue;
                }
                let region = c["region"].as_str().and_then(|r| r.rsplit('/').next()).unwrap_or_default();
                let Some(family) = c["type"].as_str().and_then(commitment_family) else { continue };
                let vcpus: f64 = c["resources"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r["type"].as_str() == Some("VCPU"))
                    .filter_map(|r| r["amount"].as_str().and_then(|a| a.parse::<f64>().ok()).or_else(|| r["amount"].as_f64()))
                    .sum();
                if vcpus > 0.0 {
                    out.entry(format!("{region}|{family}")).or_default().points.push(vcpus);
                }
            }
        }
        Ok(out)
    }
}

/// `resource.label.bucket_name` → value of that label in a time series.
fn label_value(ts: &serde_json::Value, label: &str) -> Option<String> {
    let (scope, name) = label.split_once(".label.").or_else(|| label.split_once(".labels."))?;
    let scope = if scope == "resource" { &ts["resource"]["labels"] } else { &ts["metric"]["labels"] };
    scope[name].as_str().map(str::to_string)
}

fn point_value(v: &serde_json::Value) -> Option<f64> {
    v["doubleValue"].as_f64().or_else(|| v["int64Value"].as_str().and_then(|s| s.parse().ok())).or_else(|| v["int64Value"].as_f64())
}

/// Machine family covered by a commitment type (`GENERAL_PURPOSE_N2D` → `n2d`).
fn commitment_family(t: &str) -> Option<String> {
    Some(
        match t {
            "GENERAL_PURPOSE" => "n1",
            "COMPUTE_OPTIMIZED" => "c2",
            "MEMORY_OPTIMIZED" => "m1",
            "ACCELERATOR_OPTIMIZED" => "a2",
            "GRAPHICS_OPTIMIZED" => "g2",
            "TYPE_UNSPECIFIED" | "" => return None,
            _ => return t.rsplit('_').next().map(str::to_lowercase),
        }
        .to_string(),
    )
}

fn same_family(commitment: &str, vm: &str) -> bool {
    commitment == vm || (commitment == "m1" && vm == "m2")
}

/// `2026-10-18T12:00:00Z` for a Unix timestamp.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Unix timestamp of `2026-10-18T12:00:00Z` (fractional seconds and offsets ignored).
fn parse_rfc3339(s: &str) -> Option<u64> {
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86400 + hh * 3600 + mm * 60 + ss).ok()
}

fn one_of(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("\"{}\"", n.replace('"', ""))).collect();
    format!("one_of({})", quoted.join(","))
}

/// One query: a kind of data for a chunk of names in a GCP project.
#[derive(Clone)]
struct Query {
    kind: Kind,
    project: String,
    names: Vec<String>,
}

impl Query {
    fn key(&self) -> String {
        format!("{}|{}|{}", self.kind.name(), self.project, self.names.join(","))
    }

    async fn run(&self, http: &Http<'_>) -> Result<BTreeMap<String, Series>, Failure> {
        let p = &self.project;
        match self.kind {
            Kind::Vm => {
                // Hourly means: their count is the uptime, their distribution the CPU use.
                let f = format!(
                    "metric.type=\"compute.googleapis.com/instance/cpu/utilization\" AND resource.type=\"gce_instance\" AND metric.label.instance_name={}",
                    one_of(&self.names)
                );
                http.series(p, f, 3600, "ALIGN_MEAN", "metric.label.instance_name").await
            }
            Kind::Bucket => {
                let f = format!(
                    "metric.type=\"storage.googleapis.com/storage/total_bytes\" AND resource.type=\"gcs_bucket\" AND resource.label.bucket_name={}",
                    one_of(&self.names)
                );
                http.series(p, f, 86400, "ALIGN_MEAN", "resource.label.bucket_name").await
            }
            Kind::Run => {
                let f = format!(
                    "metric.type=\"run.googleapis.com/request_count\" AND resource.type=\"cloud_run_revision\" AND resource.label.service_name={}",
                    one_of(&self.names)
                );
                http.series(p, f, 86400, "ALIGN_SUM", "resource.label.service_name").await
            }
            Kind::Sql => {
                let ids: Vec<String> = self.names.iter().map(|n| format!("{p}:{n}")).collect();
                let f = format!(
                    "metric.type=\"cloudsql.googleapis.com/database/disk/bytes_used\" AND resource.type=\"cloudsql_database\" AND resource.label.database_id={}",
                    one_of(&ids)
                );
                let v = http.series(p, f, 86400, "ALIGN_MEAN", "resource.label.database_id").await?;
                Ok(v.into_iter().map(|(k, v)| (k.split_once(':').map(|(_, n)| n.to_string()).unwrap_or(k), v)).collect())
            }
        }
    }
}

const COMMITMENTS: &str = "commitments";

/// What to cache for a fetch result (transient failures are retried).
fn cacheable(r: Result<BTreeMap<String, Series>, Failure>) -> Option<Values> {
    match r {
        Ok(v) => Some(Ok(v)),
        Err(Failure::Transient) => None,
        Err(Failure::Disabled) => Some(Err(Denial::Disabled)),
        Err(Failure::Denied) => Some(Err(Denial::Denied)),
        Err(Failure::Refused) => Some(Err(Denial::Refused)),
    }
}

/// Collect observed data for these resources, within `until`.
pub async fn collect(cache_dir: &Path, targets: &Targets, quota: Option<&str>, until: std::time::Instant) -> Observed {
    let auth = async || match resolve_auth(None).await {
        Ok(a @ Auth::Bearer(_)) => Some(a),
        _ => None,
    };
    collect_from(cache_dir, targets, quota, until, &Endpoints::default(), auth).await
}

/// `collect` with given APIs and credentials (resolved only when something is
/// not cached: getting a token can be slow).
async fn collect_from(
    cache_dir: &Path,
    targets: &Targets,
    quota: Option<&str>,
    until: std::time::Instant,
    api: &Endpoints,
    auth: impl AsyncFnOnce() -> Option<Auth>,
) -> Observed {
    let mut obs = Observed::default();
    // Names to observe, per (kind, GCP project).
    let mut wanted: BTreeMap<(Kind, String), BTreeSet<String>> = BTreeMap::new();
    let mut vm_projects: BTreeSet<String> = BTreeSet::new();
    for t in &targets.0 {
        wanted.entry((t.kind, t.gcp_project.clone())).or_default().insert(t.name.clone());
        if t.vm.is_some() {
            vm_projects.insert(t.gcp_project.clone());
        }
    }
    if wanted.is_empty() {
        return obs;
    }
    let mut queries = vec![];
    for ((kind, project), names) in &wanted {
        let names: Vec<String> = names.iter().cloned().collect();
        for chunk in names.chunks(CHUNK) {
            queries.push(Query { kind: *kind, project: project.clone(), names: chunk.to_vec() });
        }
    }

    let cache: Cache = crate::util::read_json(&cache_path(cache_dir));
    let now = crate::pricing::sync::now();
    let fresh = |k: &str| cache.entries.get(k).filter(|(at, _)| now.saturating_sub(*at) < CACHE_TTL_SECS).map(|(_, v)| v.clone());
    let mut results: Vec<(Kind, String, Values)> = vec![];
    let mut todo = vec![];
    for q in queries {
        match fresh(&q.key()) {
            Some(v) => results.push((q.kind, q.project.clone(), v)),
            None => todo.push(q),
        }
    }
    let mut commitments: HashMap<String, Values> = HashMap::new();
    let mut commit_todo = vec![];
    for p in &vm_projects {
        match fresh(&format!("{COMMITMENTS}|{p}")) {
            Some(v) => {
                commitments.insert(p.clone(), v);
            }
            None => commit_todo.push(p.clone()),
        }
    }

    let mut store = vec![];
    if !todo.is_empty() || !commit_todo.is_empty() {
        // Getting a token can be slow (gcloud): it counts in the deadline.
        let auth = tokio::time::timeout_at(tokio::time::Instant::from_std(until), auth()).await.ok().flatten();
        let client = reqwest::Client::builder().user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION"))).gzip(true).build().ok();
        if let (Some(auth), Some(client)) = (auth, client) {
            let http = Http { client, auth, quota, until: tokio::time::Instant::from_std(until), api };
            let http = &http;
            let series = futures::future::join_all(todo.iter().map(|q| async move { (q, q.run(http).await) }));
            let commits = futures::future::join_all(commit_todo.iter().map(|p| async move { (p, http.commitments(p).await) }));
            let (series, commits) = futures::join!(series, commits);
            for (q, r) in series {
                let Some(v) = cacheable(r) else { continue };
                store.push((q.key(), (now, v.clone())));
                results.push((q.kind, q.project.clone(), v));
            }
            for (p, r) in commits {
                let Some(v) = cacheable(r) else { continue };
                store.push((format!("{COMMITMENTS}|{p}"), (now, v.clone())));
                commitments.insert(p.clone(), v);
            }
        }
    }
    store_cache(cache_dir, store);

    let mut disabled: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut denied: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let failures = results
        .iter()
        .map(|(_, p, v)| ("Cloud Monitoring", p, v))
        .chain(commitments.iter().map(|(p, v)| ("Compute Engine commitments", p, v)));
    for (src, project, v) in failures {
        match v {
            Err(Denial::Disabled) => disabled.entry(src).or_default().insert(project.clone()),
            Err(Denial::Denied) => denied.entry(src).or_default().insert(project.clone()),
            _ => false,
        };
    }

    for (src, projects) in &disabled {
        obs.notes.push(format!(
            "{src}: API disabled or no quota project for {} (observed usage not used; see Permissions in the README)",
            projects.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    for (src, projects) in &denied {
        let role = if *src == "Cloud Monitoring" { "roles/monitoring.viewer" } else { "roles/compute.viewer" };
        obs.notes.push(format!(
            "{src}: permission denied for {} (grant {role} to use observed usage)",
            projects.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    // Per (kind, GCP project, name) values.
    let mut values: HashMap<(Kind, String, String), Series> = HashMap::new();
    for (kind, project, v) in results {
        for (name, s) in v.into_iter().flatten() {
            values.insert((kind, project.clone(), name), s);
        }
    }
    if !values.is_empty() {
        obs.sources.insert("Cloud Monitoring".into());
    }
    if commitments.values().any(|v| v.as_ref().is_ok_and(|m| !m.is_empty())) {
        obs.sources.insert("Compute Engine commitments".into());
    }

    for t in &targets.0 {
        let Some(s) = values.get(&(t.kind, t.gcp_project.clone(), t.name.clone())) else { continue };
        if let Some(o) = observe(t.kind, s) {
            obs.resources.insert((t.project.clone(), t.address.clone()), o);
        }
    }
    allocate_commitments(&mut obs, targets, &commitments);
    obs
}

/// Turn the points of one resource into usage values, scaled to a month
/// over the time the resource has existed in the window.
fn observe(kind: Kind, s: &Series) -> Option<ObservedResource> {
    let points = &s.points;
    let span = s.span_hours.min((WINDOW_DAYS * 24) as f64);
    if points.is_empty() || span < MIN_SPAN_HOURS {
        return None;
    }
    let mean = points.iter().sum::<f64>() / points.len() as f64;
    let mut o = ObservedResource::default();
    match kind {
        Kind::Vm => {
            // One point per hour the VM ran.
            let hours = points.len() as f64;
            let mut sorted = points.to_vec();
            sorted.sort_by(f64::total_cmp);
            let p95 = sorted[((sorted.len() as f64 * 0.95).ceil() as usize).clamp(1, sorted.len()) - 1];
            o.cpu = Some(CpuStats { mean, p95, days: span / 24.0 });
            o.usage.insert("monthly_hrs".into(), (hours / span * HOURS_PER_MONTH).min(HOURS_PER_MONTH).round());
        }
        Kind::Bucket | Kind::Sql => {
            o.usage.insert("storage_gb".into(), (mean / f64::from(1u32 << 30) * 100.0).round() / 100.0);
        }
        Kind::Run => {
            let total: f64 = points.iter().sum();
            o.usage.insert("monthly_requests".into(), (total / span * HOURS_PER_MONTH).round());
        }
    }
    Some(o)
}

/// Spread active commitments over the on-demand VMs of the code, per GCP
/// project, region and machine family, in address order.
fn allocate_commitments(obs: &mut Observed, targets: &Targets, commitments: &HashMap<String, Values>) {
    let mut left: HashMap<(String, String), f64> = HashMap::new();
    for (project, v) in commitments {
        for (key, s) in v.iter().flatten() {
            *left.entry((project.clone(), key.clone())).or_default() += s.points.iter().sum::<f64>();
        }
    }
    let mut vms: Vec<&Target> = targets.0.iter().filter(|t| t.vm.is_some()).collect();
    vms.sort_by(|a, b| a.project.cmp(&b.project).then(a.address.cmp(&b.address)));
    for t in vms {
        let Some((machine_type, zone)) = &t.vm else { continue };
        let project = t.gcp_project.as_str();
        if !matches!(commitments.get(project), Some(Ok(_))) {
            continue;
        }
        let Some(mt) = machine::parse(machine_type) else { continue };
        let region = crate::pricing::query::normalize_region(zone);
        let need = machine::billed_vcpus(&mt);
        let mut covered = 0.0;
        for ((p, key), avail) in left.iter_mut() {
            let Some((rg, fam)) = key.split_once('|') else { continue };
            if p != project || rg != region || !same_family(fam, &mt.family) {
                continue;
            }
            let take = avail.min(need - covered);
            *avail -= take;
            covered += take;
        }
        let entry = obs.resources.entry((t.project.clone(), t.address.clone())).or_default();
        entry.committed_fraction = Some(if need > 0.0 { covered / need } else { 0.0 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_792_324_800), "2026-10-18T12:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn families() {
        assert_eq!(commitment_family("GENERAL_PURPOSE_N2D").as_deref(), Some("n2d"));
        assert_eq!(commitment_family("GENERAL_PURPOSE").as_deref(), Some("n1"));
        assert_eq!(commitment_family("COMPUTE_OPTIMIZED_C3").as_deref(), Some("c3"));
        assert!(same_family("m1", "m2"));
    }

    #[test]
    fn parse_dates() {
        assert_eq!(parse_rfc3339("2026-10-18T12:00:00Z"), Some(1_792_324_800));
        assert_eq!(parse_rfc3339("2000-02-29T00:00:00.123456Z"), Some(951_782_400));
        assert_eq!(parse_rfc3339("nope"), None);
    }

    fn series(points: Vec<f64>, span_hours: f64) -> Series {
        Series { points, span_hours }
    }

    #[test]
    fn vm_points() {
        // Ran half of the window, mostly idle with a busy tail.
        let mut points = vec![0.05; 342];
        points.extend([0.9; 18]);
        let o = observe(Kind::Vm, &series(points.clone(), 720.0)).unwrap();
        assert_eq!(o.usage["monthly_hrs"], 365.0);
        let cpu = o.cpu.unwrap();
        assert!((cpu.p95 - 0.05).abs() < 1e-9);
        assert!((cpu.mean - 0.0925).abs() < 1e-9);
        // Created 15 days ago and always on since.
        let o = observe(Kind::Vm, &series(points, 360.0)).unwrap();
        assert_eq!(o.usage["monthly_hrs"], 730.0);
        // Too recent.
        assert!(observe(Kind::Vm, &series(vec![0.5; 10], 10.0)).is_none());
    }

    #[test]
    fn run_and_storage() {
        let o = observe(Kind::Run, &series(vec![1000.0; 30], 720.0)).unwrap();
        assert_eq!(o.usage["monthly_requests"], 30417.0);
        let o = observe(Kind::Bucket, &series(vec![f64::from(1u32 << 30) * 10.0; 30], 720.0)).unwrap();
        assert_eq!(o.usage["storage_gb"], 10.0);
    }

    #[test]
    fn commitments_are_spread_over_vms() {
        let vm = |address: &str, machine: &str| Target {
            project: "live/prod".into(),
            address: address.into(),
            kind: Kind::Vm,
            gcp_project: "p".into(),
            name: address.into(),
            vm: Some((machine.into(), "europe-west1-b".into())),
        };
        let targets = Targets(vec![vm("a", "n2-standard-8"), vm("b", "n2-standard-8"), vm("c", "e2-standard-4")]);
        let mut found = BTreeMap::new();
        found.insert("europe-west1|n2".to_string(), Series { points: vec![12.0], span_hours: 0.0 });
        let commitments: HashMap<String, Values> = [("p".to_string(), Ok(found))].into_iter().collect();
        let mut obs = Observed::default();
        allocate_commitments(&mut obs, &targets, &commitments);
        let f = |a: &str| obs.get("live/prod", a).and_then(|o| o.committed_fraction);
        assert_eq!(f("a"), Some(1.0));
        assert_eq!(f("b"), Some(0.5));
        assert_eq!(f("c"), Some(0.0));
    }

    // ---- API clients, against a local fake server --------------------------

    use crate::util::fake_http::{self, Request};
    use serde_json::json;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn target(kind: Kind, name: &str, vm: Option<(&str, &str)>) -> Target {
        Target {
            project: "live/prod".into(),
            address: format!("{}.{name}", kind.name()),
            kind,
            gcp_project: "proj".into(),
            name: name.into(),
            vm: vm.map(|(m, z)| (m.into(), z.into())),
        }
    }

    fn all_targets() -> Targets {
        Targets(vec![
            target(Kind::Vm, "vm-a", Some(("n2-standard-8", "europe-west1-b"))),
            target(Kind::Bucket, "logs", None),
            target(Kind::Run, "api", None),
            target(Kind::Sql, "db", None),
        ])
    }

    /// A time series with one point per `step` hours, the newest now.
    fn series_json(scope: &str, label: &str, name: &str, values: &[serde_json::Value], step: u64) -> serde_json::Value {
        let now = crate::pricing::sync::now();
        let points: Vec<_> = values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let start = now - (i as u64 + 1) * step * 3600;
                json!({"interval": {"startTime": rfc3339(start), "endTime": rfc3339(start + step * 3600)}, "value": v})
            })
            .collect();
        json!({scope: {"labels": {label: name}}, "points": points})
    }

    fn google(req: &Request) -> (u16, String) {
        let filter = req.param("filter").unwrap_or_default();
        let body = if req.path.ends_with("/aggregated/commitments") {
            json!({"items": {"regions/europe-west1": {"commitments": [
                {"status": "ACTIVE", "region": "https://x/regions/europe-west1", "type": "GENERAL_PURPOSE_N2", "resources": [{"type": "VCPU", "amount": "4"}, {"type": "MEMORY", "amount": "16384"}]},
                {"status": "EXPIRED", "region": "https://x/regions/europe-west1", "type": "GENERAL_PURPOSE_N2", "resources": [{"type": "VCPU", "amount": "64"}]}
            ]}}})
        } else if filter.contains("cpu/utilization") {
            // 360 hourly points, served in two pages.
            let second = req.param("pageToken") == Some("p2");
            let v: Vec<_> = (0..180).map(|i| json!({"doubleValue": if i % 10 == 0 { 0.3 } else { 0.1 }})).collect();
            let mut s = series_json("metric", "instance_name", "vm-a", &v, 1);
            if second {
                // Older half of the window.
                let now = crate::pricing::sync::now();
                for (i, p) in s["points"].as_array_mut().unwrap().iter_mut().enumerate() {
                    p["interval"]["startTime"] = json!(rfc3339(now - (181 + i as u64) * 3600));
                }
                json!({"timeSeries": [s]})
            } else {
                json!({"timeSeries": [s], "nextPageToken": "p2"})
            }
        } else if filter.contains("total_bytes") {
            let gib = f64::from(1u32 << 30);
            json!({"timeSeries": [series_json("resource", "bucket_name", "logs", &vec![json!({"doubleValue": 10.0 * gib}); 30], 24)]})
        } else if filter.contains("request_count") {
            json!({"timeSeries": [series_json("resource", "service_name", "api", &vec![json!({"int64Value": "1000"}); 30], 24)]})
        } else if filter.contains("bytes_used") {
            let gib = f64::from(1u32 << 30);
            json!({"timeSeries": [series_json("resource", "database_id", "proj:db", &vec![json!({"doubleValue": 5.0 * gib}); 30], 24)]})
        } else {
            return (400, json!({"error": {"message": "unexpected"}}).to_string());
        };
        (200, body.to_string())
    }

    fn run(server: &fake_http::Server, cache: &Path, targets: &Targets, auth: Option<Auth>) -> Observed {
        let api = Endpoints { monitoring: server.url.clone(), compute: server.url.clone() };
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        block_on(collect_from(cache, targets, Some("quota-proj"), until, &api, async || auth))
    }

    fn token() -> Option<Auth> {
        Some(Auth::Bearer("t0k3n".into()))
    }

    #[test]
    fn reads_monitoring_and_commitments() {
        let server = fake_http::serve(google);
        let cache = tempfile::tempdir().unwrap();
        let obs = run(&server, cache.path(), &all_targets(), token());
        assert!(obs.notes.is_empty(), "{:?}", obs.notes);
        assert_eq!(obs.sources.iter().map(String::as_str).collect::<Vec<_>>(), ["Cloud Monitoring", "Compute Engine commitments"]);
        let get = |a: &str| obs.get("live/prod", a).cloned().unwrap_or_else(|| panic!("{a} not observed"));
        let vm = get("vm.vm-a");
        assert_eq!(vm.usage["monthly_hrs"], 730.0);
        let cpu = vm.cpu.unwrap();
        assert!((cpu.mean - 0.12).abs() < 1e-9 && (cpu.p95 - 0.3).abs() < 1e-9 && cpu.days > 14.9, "{cpu:?}");
        // 4 of the 8 vCPUs are committed (the expired commitment is ignored).
        assert_eq!(vm.committed_fraction, Some(0.5));
        assert_eq!(get("bucket.logs").usage["storage_gb"], 10.0);
        assert_eq!(get("sql.db").usage["storage_gb"], 5.0);
        assert!((get("run.api").usage["monthly_requests"] - 30_417.0).abs() < 50.0);

        let reqs = server.requests.lock().unwrap().clone();
        assert_eq!(reqs.len(), 6, "4 metrics (one paged) and the commitments");
        assert!(reqs.iter().all(|r| r.header("authorization") == Some("Bearer t0k3n")));
        assert!(reqs.iter().all(|r| r.header("x-goog-user-project") == Some("quota-proj")));
        let cpu_req = reqs.iter().find(|r| r.param("filter").is_some_and(|f| f.contains("cpu/utilization"))).unwrap();
        assert!(cpu_req.param("filter").unwrap().contains(r#"metric.label.instance_name=one_of("vm-a")"#));
        assert_eq!(cpu_req.param("aggregation.alignmentPeriod"), Some("3600s"));
        assert_eq!(cpu_req.param("aggregation.groupByFields"), Some("metric.label.instance_name"));
        assert!(reqs.iter().any(|r| r.param("filter").is_some_and(|f| f.contains(r#"database_id=one_of("proj:db")"#))));

        // Cached: nothing is fetched again, and the token is not even needed.
        let again = run(&server, cache.path(), &all_targets(), None);
        assert_eq!(server.count(), 6);
        assert_eq!(again.get("live/prod", "vm.vm-a"), Some(&vm));
    }

    #[test]
    fn missing_permissions_become_notes_and_are_cached() {
        let server = fake_http::serve(|req| {
            if req.path.contains("commitments") {
                (403, json!({"error": {"message": "Compute Engine API has not been used in project proj before or it is disabled", "status": "PERMISSION_DENIED", "details": [{"reason": "SERVICE_DISABLED"}]}}).to_string())
            } else {
                (
                    403,
                    json!({"error": {"message": "Permission monitoring.timeSeries.list denied", "status": "PERMISSION_DENIED"}})
                        .to_string(),
                )
            }
        });
        let cache = tempfile::tempdir().unwrap();
        let obs = run(&server, cache.path(), &all_targets(), token());
        assert!(obs.resources.values().all(|o| o.usage.is_empty() && o.cpu.is_none()));
        assert!(obs.sources.is_empty());
        let notes = obs.notes.join("\n");
        assert!(notes.contains("Cloud Monitoring: permission denied for proj (grant roles/monitoring.viewer"), "{notes}");
        assert!(notes.contains("Compute Engine commitments: API disabled"), "{notes}");
        let n = server.count();
        // The refusal is cached: same notes, no new request.
        let again = run(&server, cache.path(), &all_targets(), token());
        assert_eq!(again.notes, obs.notes);
        assert_eq!(server.count(), n);
    }

    #[test]
    fn transient_errors_are_retried_next_run() {
        let server = fake_http::serve(|_| (503, "{}".into()));
        let cache = tempfile::tempdir().unwrap();
        let targets = Targets(vec![target(Kind::Bucket, "logs", None)]);
        let obs = run(&server, cache.path(), &targets, token());
        assert!(obs.resources.is_empty() && obs.notes.is_empty());
        run(&server, cache.path(), &targets, token());
        assert_eq!(server.count(), 2);
    }

    #[test]
    fn unknown_projects_are_skipped_silently() {
        let server = fake_http::serve(|_| (404, json!({"error": {"message": "not found"}}).to_string()));
        let cache = tempfile::tempdir().unwrap();
        let obs = run(&server, cache.path(), &all_targets(), token());
        assert!(obs.resources.values().all(|o| o.cpu.is_none()) && obs.notes.is_empty());
    }

    #[test]
    fn nothing_is_fetched_without_credentials() {
        let server = fake_http::serve(google);
        let cache = tempfile::tempdir().unwrap();
        let obs = run(&server, cache.path(), &all_targets(), None);
        assert!(obs.resources.is_empty() && obs.notes.is_empty());
        assert_eq!(server.count(), 0);
        let empty = run(&server, cache.path(), &Targets::default(), token());
        assert!(empty.resources.is_empty());
        assert_eq!(server.count(), 0);
    }
}
