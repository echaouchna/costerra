//! Observed usage of the deployed resources, read from Google Cloud when the
//! credentials allow it:
//! * Cloud Monitoring metrics over the last 30 days (VM CPU and uptime, bucket
//!   and Cloud SQL storage, Cloud SQL CPU and memory, Cloud Run requests,
//!   BigQuery stored bytes),
//! * active Compute Engine commitments,
//! * logical and physical bytes of BigQuery datasets
//!   (`INFORMATION_SCHEMA.TABLE_STORAGE`, a metadata query).
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

/// CPU or memory utilization over the window (fractions, 0–1).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct CpuStats {
    pub mean: f64,
    /// 95th percentile of hourly means.
    pub p95: f64,
    /// Highest hourly value.
    #[serde(default)]
    pub max: f64,
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
    /// Memory utilization (Cloud SQL).
    #[serde(default)]
    pub memory: Option<CpuStats>,
    /// Share (0–1) of the VM's vCPUs already covered by active commitments.
    pub committed_fraction: Option<f64>,
    /// The resource in the Google Cloud console (its metrics).
    #[serde(default)]
    pub console: Option<String>,
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
    /// Bytes a bucket sent (reads), for cross-region data transfer.
    BucketEgress,
    Run,
    /// Cloud Run instances alive: tells "no requests" from "not deployed".
    RunInstances,
    /// Cloud Run CPU and memory use, as a share of the container limits.
    RunCpu,
    RunMemory,
    Sql,
    SqlCpu,
    SqlMemory,
    /// `INFORMATION_SCHEMA.TABLE_STORAGE`: logical and physical bytes.
    Dataset,
    /// The same bytes from the tables API (dataset-level permission), read
    /// only where `INFORMATION_SCHEMA` is refused.
    DatasetTables,
    /// Cloud Monitoring `stored_bytes`: logical bytes only (fallback).
    DatasetBytes,
}

impl Kind {
    /// What is observed for a resource type, the preferred source first.
    fn of(rtype: &str) -> &'static [Kind] {
        match rtype {
            "google_compute_instance" => &[Kind::Vm],
            "google_storage_bucket" => &[Kind::Bucket, Kind::BucketEgress],
            "google_cloud_run_v2_service" | "google_cloud_run_service" | "google_cloudfunctions2_function" => {
                &[Kind::Run, Kind::RunInstances, Kind::RunCpu, Kind::RunMemory]
            }
            "google_sql_database_instance" => &[Kind::Sql, Kind::SqlCpu, Kind::SqlMemory],
            "google_bigquery_dataset" => &[Kind::Dataset, Kind::DatasetBytes],
            _ => &[],
        }
    }

    fn is_dataset(self) -> bool {
        matches!(self, Kind::Dataset | Kind::DatasetTables | Kind::DatasetBytes)
    }

    /// Kinds whose resources can be matched by elimination when their name
    /// is only known after apply (a random suffix): the only unnamed
    /// instance of the code in a project and the only unclaimed one there.
    fn matches_unnamed(self) -> bool {
        matches!(self, Kind::Sql | Kind::SqlCpu | Kind::SqlMemory)
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Vm => "vm",
            Kind::Bucket => "bucket",
            Kind::BucketEgress => "bucket-egress",
            Kind::Run => "run",
            Kind::RunInstances => "run-instances",
            Kind::RunCpu => "run-cpu",
            Kind::RunMemory => "run-memory",
            Kind::Sql => "sql",
            Kind::SqlCpu => "sql-cpu",
            Kind::SqlMemory => "sql-memory",
            Kind::Dataset => "dataset",
            Kind::DatasetTables => "dataset-tables",
            Kind::DatasetBytes => "dataset-bytes",
        }
    }

    /// Where the data comes from (shown in notes and in the `Usage` line).
    fn source(self) -> &'static str {
        match self {
            Kind::Dataset | Kind::DatasetTables => "BigQuery storage",
            _ => "Cloud Monitoring",
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
    /// BigQuery datasets: location (`US`, `EU`, `europe-west1`…).
    location: Option<String>,
    /// For the console link: resource type, and zone (VMs) or region (Cloud Run).
    rtype: String,
    place: Option<String>,
}

/// The page of a resource in the Google Cloud console, where its metrics are.
fn console_link(rtype: &str, project: &str, name: &str, place: Option<&str>) -> Option<String> {
    let base = "https://console.cloud.google.com";
    Some(match rtype {
        "google_compute_instance" => match place {
            Some(zone) => format!("{base}/compute/instancesDetail/zones/{zone}/instances/{name}?project={project}&tab=monitoring"),
            // Zone unknown: the instances of the project.
            None => format!("{base}/compute/instances?project={project}"),
        },
        "google_sql_database_instance" => format!("{base}/sql/instances/{name}/overview?project={project}"),
        "google_cloud_run_v2_service" | "google_cloud_run_service" => {
            format!("{base}/run/detail/{}/{name}/metrics?project={project}", place?)
        }
        "google_cloudfunctions2_function" => format!("{base}/functions/details/{}/{name}?env=gen2&project={project}&tab=metrics", place?),
        "google_bigquery_dataset" => format!("{base}/bigquery?project={project}&ws=!1m4!1m3!3m2!1s{project}!2s{name}"),
        "google_storage_bucket" => format!("{base}/storage/browser/{name};tab=observability?project={project}"),
        _ => return None,
    })
}

/// The resources of the code to observe (cheap to extract, owned).
#[derive(Debug, Default)]
pub struct Targets(Vec<Target>);

impl Targets {
    pub fn from_projects(sets: &[&[Project]]) -> Targets {
        let mut out = vec![];
        for p in sets.iter().flat_map(|s| s.iter()) {
            for r in &p.resources {
                for &kind in Kind::of(&r.rtype) {
                    let name = if kind.is_dataset() { attr_str(r, "dataset_id") } else { known_name(r) };
                    // An empty name: matched by elimination (see `Kind::matches_unnamed`).
                    let name = name.or_else(|| kind.matches_unnamed().then_some(""));
                    let (Some(gcp), Some(name)) = (gcp_project(r), name) else { continue };
                    let location = (kind == Kind::Dataset).then(|| attr_str(r, "location").unwrap_or("US").to_lowercase());
                    let vm = (kind == Kind::Vm && !is_spot(r))
                        .then(|| Some((r.attrs.get("machine_type")?.as_str()?.to_string(), zone_of(r)?.to_string())))
                        .flatten();
                    let place = match r.rtype.as_str() {
                        "google_compute_instance" => zone_of(r).map(str::to_string),
                        _ => attr_str(r, "location").or(attr_str(r, "region")).or(r.provider.region.as_deref()).map(str::to_string),
                    };
                    out.push(Target {
                        project: p.name.clone(),
                        address: r.address.clone(),
                        kind,
                        gcp_project: gcp.to_string(),
                        name: name.to_string(),
                        vm,
                        location,
                        rtype: r.rtype.clone(),
                        place,
                    });
                }
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
    attr_str(r, "name")
}

/// A string attribute known before apply.
fn attr_str<'a>(r: &'a ResourceInstance, key: &str) -> Option<&'a str> {
    r.attrs.get(key).and_then(|v| v.as_str()).filter(|n| !n.is_empty() && !n.starts_with(crate::REF_PREFIX))
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
    dir.join(crate::pricing::sync::identity_file("observed"))
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
    bigquery: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            monitoring: "https://monitoring.googleapis.com".into(),
            compute: "https://compute.googleapis.com".into(),
            bigquery: "https://bigquery.googleapis.com".into(),
        }
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

    /// Cloud Monitoring time series, as values per `label` (summed over the
    /// other labels).
    async fn series(
        &self,
        project: &str,
        filter: String,
        align_secs: u64,
        aligner: &str,
        label: &str,
    ) -> Result<BTreeMap<String, Series>, Failure> {
        self.series_reduced(project, filter, align_secs, aligner, "REDUCE_SUM", label).await
    }

    /// Cloud Monitoring time series, as values per `label`, combined with `reducer`.
    async fn series_reduced(
        &self,
        project: &str,
        filter: String,
        align_secs: u64,
        aligner: &str,
        reducer: &str,
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
            ("aggregation.crossSeriesReducer", reducer.to_string()),
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

    async fn post(&self, url: &str, body: &serde_json::Value) -> Result<serde_json::Value, Failure> {
        let mut rq = self.client.post(url).json(body);
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

    /// Storage of BigQuery datasets, from `INFORMATION_SCHEMA.TABLE_STORAGE`
    /// (metadata only; billed as the 10 MB minimum of a query). The points of
    /// each dataset are the bytes in `DATASET_COLUMNS` order.
    async fn bq_storage(&self, project: &str, location: &str, datasets: &[String]) -> Result<BTreeMap<String, Series>, Failure> {
        let url = format!("{}/bigquery/v2/projects/{project}/queries", self.api.bigquery);
        let list: Vec<String> = datasets.iter().map(|d| format!("'{}'", d.replace(['\\', '\''], ""))).collect();
        let sums: Vec<String> = DATASET_COLUMNS.iter().map(|c| format!("SUM({c})")).collect();
        let query = format!(
            "SELECT table_schema, {} FROM `region-{}`.INFORMATION_SCHEMA.TABLE_STORAGE WHERE table_schema IN ({}) AND NOT deleted GROUP BY table_schema",
            sums.join(", "),
            location.replace('`', ""),
            list.join(", ")
        );
        let body = serde_json::json!({"query": query, "useLegacySql": false, "timeoutMs": 2500, "maxResults": 1000});
        let j = self.post(&url, &body).await?;
        if j["jobComplete"].as_bool() != Some(true) {
            return Err(Failure::Transient);
        }
        let mut out = BTreeMap::new();
        for row in j["rows"].as_array().into_iter().flatten() {
            let cells: Vec<&serde_json::Value> = row["f"].as_array().into_iter().flatten().map(|c| &c["v"]).collect();
            let Some(name) = cells.first().and_then(|v| v.as_str()) else { continue };
            let points = cells[1..].iter().map(|v| v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0)).collect();
            out.insert(name.to_string(), Series { points, span_hours: (WINDOW_DAYS * 24) as f64 });
        }
        Ok(out)
    }

    /// The same bytes as `bq_storage`, summed from the tables API: works with
    /// dataset-level permissions. Fail-safe bytes are not exposed (0). A
    /// dataset with too many tables to read is left out.
    async fn bq_tables(&self, project: &str, datasets: &[String]) -> Result<BTreeMap<String, Series>, Failure> {
        use futures::StreamExt;
        const MAX_TABLES: usize = 500;
        const FIELDS: &str =
            "numActiveLogicalBytes,numLongTermLogicalBytes,numTotalPhysicalBytes,numLongTermPhysicalBytes,numTimeTravelPhysicalBytes";
        let mut out = BTreeMap::new();
        for d in datasets {
            let base = format!("{}/bigquery/v2/projects/{project}/datasets/{d}/tables", self.api.bigquery);
            let mut ids: Vec<String> = vec![];
            let mut query = vec![("maxResults", "1000".to_string())];
            let listed = loop {
                let j = match self.get(&base, &query).await {
                    Ok(j) => j,
                    // An unknown dataset (not deployed yet): skipped.
                    Err(Failure::Refused) => break false,
                    Err(e) => return Err(e),
                };
                for t in j["tables"].as_array().into_iter().flatten() {
                    if matches!(t["type"].as_str(), Some("TABLE" | "MATERIALIZED_VIEW"))
                        && let Some(id) = t["tableReference"]["tableId"].as_str()
                    {
                        ids.push(id.to_string());
                    }
                }
                match j["nextPageToken"].as_str().filter(|t| !t.is_empty()) {
                    Some(t) if ids.len() <= MAX_TABLES => {
                        query.retain(|(k, _)| *k != "pageToken");
                        query.push(("pageToken", t.to_string()));
                    }
                    _ => break ids.len() <= MAX_TABLES,
                }
            };
            if !listed {
                continue;
            }
            let tables: Vec<Result<serde_json::Value, Failure>> = futures::stream::iter(ids.iter().map(|t| {
                let url = format!("{base}/{t}");
                async move { self.get(&url, &[("fields", FIELDS.to_string())]).await }
            }))
            .buffer_unordered(16)
            .collect()
            .await;
            let mut sums = [0.0f64; 5];
            for t in tables {
                let t = t?;
                let n = |k: &str| t[k].as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| t[k].as_f64()).unwrap_or(0.0);
                let (total, long_term) = (n("numTotalPhysicalBytes"), n("numLongTermPhysicalBytes"));
                // As in TABLE_STORAGE, active physical bytes include time travel.
                let row = [
                    n("numActiveLogicalBytes"),
                    n("numLongTermLogicalBytes"),
                    (total - long_term).max(0.0),
                    long_term,
                    n("numTimeTravelPhysicalBytes"),
                ];
                for (s, v) in sums.iter_mut().zip(row) {
                    *s += v;
                }
            }
            let mut points = sums.to_vec();
            points.push(0.0);
            out.insert(d.clone(), Series { points, span_hours: (WINDOW_DAYS * 24) as f64 });
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

/// Columns of `INFORMATION_SCHEMA.TABLE_STORAGE` read for datasets.
const DATASET_COLUMNS: [&str; 6] = [
    "active_logical_bytes",
    "long_term_logical_bytes",
    "active_physical_bytes",
    "long_term_physical_bytes",
    "time_travel_physical_bytes",
    "fail_safe_physical_bytes",
];

/// One query: a kind of data for a chunk of names in a GCP project (and
/// location, for BigQuery).
#[derive(Clone)]
struct Query {
    kind: Kind,
    project: String,
    location: String,
    names: Vec<String>,
}

impl Query {
    fn key(&self) -> String {
        match self.location.as_str() {
            "" => format!("{}|{}|{}", self.kind.name(), self.project, self.names.join(",")),
            l => format!("{}|{}|{l}|{}", self.kind.name(), self.project, self.names.join(",")),
        }
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
            Kind::RunInstances => {
                let f = format!(
                    "metric.type=\"run.googleapis.com/container/instance_count\" AND resource.type=\"cloud_run_revision\" AND resource.label.service_name={}",
                    one_of(&self.names)
                );
                http.series(p, f, 86400, "ALIGN_MAX", "resource.label.service_name").await
            }
            Kind::BucketEgress => {
                let f = format!(
                    "metric.type=\"storage.googleapis.com/network/sent_bytes_count\" AND resource.type=\"gcs_bucket\" AND resource.label.bucket_name={}",
                    one_of(&self.names)
                );
                http.series(p, f, 86400, "ALIGN_SUM", "resource.label.bucket_name").await
            }
            Kind::RunCpu | Kind::RunMemory => {
                // Hourly 95th percentile of the distribution, the busiest revision.
                let metric = if self.kind == Kind::RunCpu { "cpu" } else { "memory" };
                let f = format!(
                    "metric.type=\"run.googleapis.com/container/{metric}/utilizations\" AND resource.type=\"cloud_run_revision\" AND resource.label.service_name={}",
                    one_of(&self.names)
                );
                http.series_reduced(p, f, 3600, "ALIGN_PERCENTILE_95", "REDUCE_MAX", "resource.label.service_name").await
            }
            Kind::Sql | Kind::SqlCpu | Kind::SqlMemory => {
                let ids: Vec<String> = self.names.iter().map(|n| format!("{p}:{n}")).collect();
                let (metric, align) = match self.kind {
                    Kind::Sql => ("disk/bytes_used", 86400),
                    Kind::SqlCpu => ("cpu/utilization", 3600),
                    _ => ("memory/utilization", 3600),
                };
                let mut f = format!("metric.type=\"cloudsql.googleapis.com/database/{metric}\" AND resource.type=\"cloudsql_database\"");
                if !ids.is_empty() {
                    f.push_str(&format!(" AND resource.label.database_id={}", one_of(&ids)));
                }
                let v = http.series(p, f, align, "ALIGN_MEAN", "resource.label.database_id").await?;
                Ok(v.into_iter().map(|(k, v)| (k.split_once(':').map(|(_, n)| n.to_string()).unwrap_or(k), v)).collect())
            }
            Kind::DatasetBytes => {
                let f = format!(
                    "metric.type=\"bigquery.googleapis.com/storage/stored_bytes\" AND resource.type=\"bigquery_dataset\" AND resource.label.dataset_id={}",
                    one_of(&self.names)
                );
                http.series(p, f, 86400, "ALIGN_MEAN", "resource.label.dataset_id").await
            }
            Kind::Dataset => http.bq_storage(p, &self.location, &self.names).await,
            Kind::DatasetTables => http.bq_tables(p, &self.names).await,
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

/// An API client, when credentials are available (getting a token can be
/// slow with gcloud: it counts in the deadline).
async fn connect<'a>(
    auth: Option<impl AsyncFnOnce() -> Option<Auth>>,
    quota: Option<&'a str>,
    until: std::time::Instant,
    api: &'a Endpoints,
) -> Option<Http<'a>> {
    let auth = tokio::time::timeout_at(tokio::time::Instant::from_std(until), auth?()).await.ok().flatten()?;
    let client = reqwest::Client::builder().user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION"))).gzip(true).build().ok()?;
    Some(Http { client, auth, quota, until: tokio::time::Instant::from_std(until), api })
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
    let mut wanted: BTreeMap<(Kind, String, String), BTreeSet<String>> = BTreeMap::new();
    let mut vm_projects: BTreeSet<String> = BTreeSet::new();
    for t in &targets.0 {
        let location = t.location.clone().unwrap_or_default();
        wanted.entry((t.kind, t.gcp_project.clone(), location)).or_default().insert(t.name.clone());
        if t.vm.is_some() {
            vm_projects.insert(t.gcp_project.clone());
        }
    }
    if wanted.is_empty() {
        return obs;
    }
    let mut queries = vec![];
    for ((kind, project, location), names) in &wanted {
        // An unnamed resource: one query for every instance of the project.
        if names.contains("") {
            queries.push(Query { kind: *kind, project: project.clone(), location: location.clone(), names: vec![] });
            continue;
        }
        let names: Vec<String> = names.iter().cloned().collect();
        for chunk in names.chunks(CHUNK) {
            queries.push(Query { kind: *kind, project: project.clone(), location: location.clone(), names: chunk.to_vec() });
        }
    }

    let cache: Cache = crate::util::read_json(&cache_path(cache_dir));
    let now = crate::pricing::sync::now();
    let fresh = |k: &str| cache.entries.get(k).filter(|(at, _)| now.saturating_sub(*at) < CACHE_TTL_SECS).map(|(_, v)| v.clone());
    let mut results: Vec<(Kind, String, Values)> = vec![];
    // INFORMATION_SCHEMA queries, and whether they were refused.
    let mut dataset_results: Vec<(Query, bool)> = vec![];
    let mut todo = vec![];
    for q in queries {
        match fresh(&q.key()) {
            Some(v) => {
                if q.kind == Kind::Dataset {
                    dataset_results.push((q.clone(), v.is_err()));
                }
                results.push((q.kind, q.project.clone(), v));
            }
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
    let mut auth = Some(auth);
    let mut http: Option<Http> = None;
    if !todo.is_empty() || !commit_todo.is_empty() {
        http = connect(auth.take(), quota, until, api).await;
        if let Some(http) = &http {
            let series = futures::future::join_all(todo.iter().map(|q| async move { (q, q.run(http).await) }));
            let commits = futures::future::join_all(commit_todo.iter().map(|p| async move { (p, http.commitments(p).await) }));
            let (series, commits) = futures::join!(series, commits);
            for (q, r) in series {
                let Some(v) = cacheable(r) else { continue };
                store.push((q.key(), (now, v.clone())));
                results.push((q.kind, q.project.clone(), v));
                if q.kind == Kind::Dataset {
                    dataset_results.push((q.clone(), results.last().is_some_and(|(_, _, v)| v.is_err())));
                }
            }
            for (p, r) in commits {
                let Some(v) = cacheable(r) else { continue };
                store.push((format!("{COMMITMENTS}|{p}"), (now, v.clone())));
                commitments.insert(p.clone(), v);
            }
        }
    }

    // Where INFORMATION_SCHEMA is refused, the tables API (dataset-level
    // permission) gives the same bytes.
    let mut fallback = vec![];
    for (q, _) in dataset_results.iter().filter(|(_, failed)| *failed) {
        let t = Query { kind: Kind::DatasetTables, ..q.clone() };
        match fresh(&t.key()) {
            Some(v) => results.push((t.kind, t.project.clone(), v)),
            None => fallback.push(t),
        }
    }
    if !fallback.is_empty() {
        if http.is_none() {
            http = connect(auth.take(), quota, until, api).await;
        }
        if let Some(http) = &http {
            for (q, r) in futures::future::join_all(fallback.iter().map(|q| async move { (q, q.run(http).await) })).await {
                let Some(v) = cacheable(r) else { continue };
                store.push((q.key(), (now, v.clone())));
                results.push((q.kind, q.project.clone(), v));
            }
        }
    }
    store_cache(cache_dir, store);

    let mut disabled: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut denied: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    // BigQuery storage read from the tables API: INFORMATION_SCHEMA is not needed there.
    let tables_ok: BTreeSet<&String> = results
        .iter()
        .filter(|(k, _, v)| *k == Kind::DatasetTables && v.as_ref().is_ok_and(|m| !m.is_empty()))
        .map(|(_, p, _)| p)
        .collect();
    let failures = results
        .iter()
        .filter(|(k, p, _)| !(*k == Kind::Dataset && tables_ok.contains(p)))
        .map(|(k, p, v)| (k.source(), p, v))
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
        let role = match *src {
            "Cloud Monitoring" => "roles/monitoring.viewer",
            "BigQuery storage" => {
                "roles/bigquery.metadataViewer and roles/bigquery.jobUser on the project, or roles/bigquery.metadataViewer on the datasets,"
            }
            _ => "roles/compute.viewer",
        };
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
    for (kind, _, _) in values.keys() {
        obs.sources.insert(kind.source().into());
    }
    if commitments.values().any(|v| v.as_ref().is_ok_and(|m| !m.is_empty())) {
        obs.sources.insert("Compute Engine commitments".into());
    }

    // Unnamed resources: the only one of the code in its project, matched with
    // the only instance there that no named resource of the code claims.
    let mut resolved: HashMap<(Kind, String), String> = HashMap::new();
    for t in targets.0.iter().filter(|t| t.name.is_empty()) {
        let unnamed = targets.0.iter().filter(|u| u.name.is_empty() && u.kind == t.kind && u.gcp_project == t.gcp_project).count();
        let named: BTreeSet<&str> =
            targets.0.iter().filter(|u| u.kind == t.kind && u.gcp_project == t.gcp_project).map(|u| u.name.as_str()).collect();
        let free: Vec<&String> = values
            .keys()
            .filter(|(k, p, n)| *k == t.kind && *p == t.gcp_project && !named.contains(n.as_str()))
            .map(|(_, _, n)| n)
            .collect();
        if let ([only], 1) = (free.as_slice(), unnamed) {
            resolved.insert((t.kind, t.gcp_project.clone()), (*only).clone());
        }
    }
    for t in &targets.0 {
        let name = match t.name.as_str() {
            "" => match resolved.get(&(t.kind, t.gcp_project.clone())) {
                Some(n) => n.clone(),
                None => continue,
            },
            n => n.to_string(),
        };
        // INFORMATION_SCHEMA, or the tables API where it was refused.
        let found = values.get(&(t.kind, t.gcp_project.clone(), name.clone())).map(|s| (t.kind, s)).or_else(|| {
            (t.kind == Kind::Dataset)
                .then(|| values.get(&(Kind::DatasetTables, t.gcp_project.clone(), name.clone())).map(|s| (Kind::DatasetTables, s)))
                .flatten()
        });
        let Some((kind, s)) = found else { continue };
        let Some(mut o) = observe(kind, s) else { continue };
        o.console = console_link(&t.rtype, &t.gcp_project, &name, t.place.as_deref());
        // Several kinds per resource: the first source of a value wins.
        let e = obs.resources.entry((t.project.clone(), t.address.clone())).or_default();
        for (k, v) in o.usage {
            e.usage.entry(k).or_insert(v);
        }
        e.cpu = e.cpu.or(o.cpu);
        e.console = e.console.take().or(o.console);
        e.memory = e.memory.or(o.memory);
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
    let stats = || {
        let mut sorted = points.to_vec();
        sorted.sort_by(f64::total_cmp);
        let p95 = sorted[((sorted.len() as f64 * 0.95).ceil() as usize).clamp(1, sorted.len()) - 1];
        CpuStats { mean, p95, max: sorted[sorted.len() - 1], days: span / 24.0 }
    };
    let mut o = ObservedResource::default();
    match kind {
        Kind::Vm => {
            // One point per hour the VM ran.
            let hours = points.len() as f64;
            o.cpu = Some(stats());
            o.usage.insert("monthly_hrs".into(), (hours / span * HOURS_PER_MONTH).min(HOURS_PER_MONTH).round());
        }
        // Daily bytes sent, scaled to a month: `sent_gb` (not a usage key of the
        // cost model; read by the cross-region data transfer rule).
        Kind::BucketEgress => {
            let total: f64 = points.iter().sum();
            o.usage.insert("sent_gb".into(), (total / span * HOURS_PER_MONTH / f64::from(1u32 << 30) * 100.0).round() / 100.0);
        }
        // Instances were running: a service without request data had no requests.
        Kind::RunInstances => {
            o.usage.insert("monthly_requests".into(), 0.0);
        }
        Kind::SqlCpu | Kind::RunCpu => o.cpu = Some(stats()),
        Kind::SqlMemory | Kind::RunMemory => o.memory = Some(stats()),
        Kind::Bucket | Kind::Sql | Kind::DatasetBytes => {
            o.usage.insert("storage_gb".into(), (mean / f64::from(1u32 << 30) * 100.0).round() / 100.0);
        }
        Kind::Run => {
            let total: f64 = points.iter().sum();
            o.usage.insert("monthly_requests".into(), (total / span * HOURS_PER_MONTH).round());
        }
        Kind::Dataset | Kind::DatasetTables => {
            let [active, long_term, active_physical, long_term_physical, time_travel, fail_safe] = points[..] else { return None };
            let gib = |b: f64| (b.max(0.0) / f64::from(1u32 << 30) * 100.0).round() / 100.0;
            o.usage.insert("storage_gb".into(), gib(active));
            o.usage.insert("long_term_storage_gb".into(), gib(long_term));
            // Active physical bytes include time travel bytes.
            o.usage.insert("physical_storage_gb".into(), gib(active_physical - time_travel));
            o.usage.insert("long_term_physical_storage_gb".into(), gib(long_term_physical));
            o.usage.insert("time_travel_storage_gb".into(), gib(time_travel));
            o.usage.insert("fail_safe_storage_gb".into(), gib(fail_safe));
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
            location: None,
            rtype: "google_compute_instance".into(),
            place: Some("europe-west1-b".into()),
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
            location: (kind == Kind::Dataset).then(|| "eu".into()),
            rtype: String::new(),
            place: None,
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
        let api = Endpoints { monitoring: server.url.clone(), compute: server.url.clone(), bigquery: server.url.clone() };
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
    fn reads_bigquery_dataset_storage() {
        let gib = 1u64 << 30;
        let server = fake_http::serve(move |req| {
            assert!(req.path.ends_with("/bigquery/v2/projects/proj/queries"), "{}", req.path);
            let q: serde_json::Value = serde_json::from_str(&req.body).unwrap();
            let sql = q["query"].as_str().unwrap();
            assert!(sql.contains("`region-eu`.INFORMATION_SCHEMA.TABLE_STORAGE") && sql.contains("IN ('events')"), "{sql}");
            let cell = |v: u64| json!({"v": v.to_string()});
            let row = json!({"f": [{"v": "events"}, cell(1000 * gib), cell(500 * gib), cell(120 * gib), cell(50 * gib), cell(20 * gib), cell(10 * gib)]});
            (200, json!({"jobComplete": true, "rows": [row]}).to_string())
        });
        let cache = tempfile::tempdir().unwrap();
        let targets = Targets(vec![target(Kind::Dataset, "events", None)]);
        let obs = run(&server, cache.path(), &targets, token());
        assert_eq!(obs.sources.iter().map(String::as_str).collect::<Vec<_>>(), ["BigQuery storage"]);
        let u = &obs.get("live/prod", "dataset.events").unwrap().usage;
        let want = [
            ("storage_gb", 1000.0),
            ("long_term_storage_gb", 500.0),
            ("physical_storage_gb", 100.0),
            ("long_term_physical_storage_gb", 50.0),
            ("time_travel_storage_gb", 20.0),
            ("fail_safe_storage_gb", 10.0),
        ];
        for (k, v) in want {
            assert_eq!(u[k], v, "{k}");
        }
    }

    #[test]
    fn bigquery_without_permission_is_a_note() {
        let server = fake_http::serve(|_| {
            (403, json!({"error": {"message": "Access Denied: User does not have bigquery.jobs.create permission", "status": "PERMISSION_DENIED"}}).to_string())
        });
        let cache = tempfile::tempdir().unwrap();
        let obs = run(&server, cache.path(), &Targets(vec![target(Kind::Dataset, "events", None)]), token());
        assert!(obs.resources.is_empty());
        let notes = obs.notes.join("\n");
        assert!(
            notes.contains("BigQuery storage: permission denied for proj (grant roles/bigquery.metadataViewer and roles/bigquery.jobUser"),
            "{notes}"
        );
    }

    #[test]
    fn reads_cloud_sql_cpu_and_memory_and_falls_back_to_stored_bytes() {
        let gib = f64::from(1u32 << 30);
        let server = fake_http::serve(move |req| {
            if req.path.ends_with("/queries") {
                return (
                    403,
                    json!({"error": {"message": "User does not have permission to query table", "status": "PERMISSION_DENIED"}})
                        .to_string(),
                );
            }
            let filter = req.param("filter").unwrap_or_default();
            let body = if filter.contains("cloudsql.googleapis.com/database/cpu/utilization") {
                json!({"timeSeries": [series_json("resource", "database_id", "proj:db", &vec![json!({"doubleValue": 0.1}); 200], 1)]})
            } else if filter.contains("cloudsql.googleapis.com/database/memory/utilization") {
                json!({"timeSeries": [series_json("resource", "database_id", "proj:db", &vec![json!({"doubleValue": 0.4}); 200], 1)]})
            } else if filter.contains("bigquery.googleapis.com/storage/stored_bytes") {
                assert!(filter.contains(r#"dataset_id=one_of("events")"#), "{filter}");
                json!({"timeSeries": [series_json("resource", "dataset_id", "events", &vec![json!({"doubleValue": 300.0 * gib}); 10], 24)]})
            } else {
                return (400, "{}".into());
            };
            (200, body.to_string())
        });
        let cache = tempfile::tempdir().unwrap();
        // Each kind of a resource is one target with the same address.
        let mut targets = vec![];
        for kind in [Kind::SqlCpu, Kind::SqlMemory] {
            targets.push(Target { address: "sql.db".into(), ..target(kind, "db", None) });
        }
        for kind in [Kind::Dataset, Kind::DatasetBytes] {
            targets.push(Target { address: "dataset.events".into(), ..target(kind, "events", None) });
        }
        let obs = run(&server, cache.path(), &Targets(targets), token());
        let db = obs.get("live/prod", "sql.db").unwrap();
        let (cpu, mem) = (db.cpu.unwrap(), db.memory.unwrap());
        assert!((cpu.mean - 0.1).abs() < 1e-9 && (mem.p95 - 0.4).abs() < 1e-9 && cpu.days > 8.0, "{db:?}");
        assert_eq!(obs.get("live/prod", "dataset.events").unwrap().usage["storage_gb"], 300.0);
        assert!(obs.notes.iter().any(|n| n.starts_with("BigQuery storage: permission denied")), "{:?}", obs.notes);
        assert_eq!(obs.sources.iter().map(String::as_str).collect::<Vec<_>>(), ["Cloud Monitoring"]);
    }

    #[test]
    fn cloud_run_instances_without_requests_mean_no_traffic() {
        let server = fake_http::serve(|req| {
            let filter = req.param("filter").unwrap_or_default();
            let body = if filter.contains("request_count") {
                json!({"timeSeries": [series_json("resource", "service_name", "busy", &vec![json!({"int64Value": "1000"}); 30], 24)]})
            } else if filter.contains("instance_count") {
                let alive = |n| series_json("resource", "service_name", n, &vec![json!({"int64Value": "1"}); 30], 24);
                json!({"timeSeries": [alive("busy"), alive("quiet")]})
            } else {
                return (400, "{}".into());
            };
            (200, body.to_string())
        });
        let cache = tempfile::tempdir().unwrap();
        let mut targets = vec![];
        for name in ["busy", "quiet", "absent"] {
            for kind in [Kind::Run, Kind::RunInstances] {
                targets.push(Target { address: format!("run.{name}"), ..target(kind, name, None) });
            }
        }
        let obs = run(&server, cache.path(), &Targets(targets), token());
        let requests = |a: &str| obs.get("live/prod", a).map(|o| o.usage["monthly_requests"]);
        assert!(requests("run.busy").is_some_and(|r| r > 30_000.0));
        assert_eq!(requests("run.quiet"), Some(0.0));
        // Neither requests nor instances: maybe not deployed, nothing is assumed.
        assert_eq!(requests("run.absent"), None);
    }

    #[test]
    fn unnamed_cloud_sql_instances_are_matched_by_elimination() {
        let serve = |deployed: &'static [&'static str]| {
            fake_http::serve(move |req| {
                let filter = req.param("filter").unwrap_or_default();
                // One query for the whole project: no name filter.
                assert!(!filter.contains("database_id="), "{filter}");
                let ts: Vec<_> = deployed
                    .iter()
                    .map(|d| series_json("resource", "database_id", &format!("proj:{d}"), &vec![json!({"doubleValue": 0.1}); 100], 1))
                    .collect();
                (200, json!({"timeSeries": ts}).to_string())
            })
        };
        let targets = || {
            Targets(vec![
                Target { address: "sql.known".into(), ..target(Kind::SqlCpu, "known", None) },
                Target { address: "sql.random".into(), ..target(Kind::SqlCpu, "", None) },
            ])
        };
        let server = serve(&["known", "pg-app-dev-akzo"]);
        let cache = tempfile::tempdir().unwrap();
        let obs = run(&server, cache.path(), &targets(), token());
        assert!(obs.get("live/prod", "sql.known").and_then(|o| o.cpu).is_some());
        assert!(obs.get("live/prod", "sql.random").and_then(|o| o.cpu).is_some(), "{:?}", obs.resources);
        // Two unclaimed instances: which one is ours is unknown.
        let server = serve(&["known", "pg-app-dev-akzo", "pg-other-x"]);
        let cache = tempfile::tempdir().unwrap();
        let obs = run(&server, cache.path(), &targets(), token());
        assert!(obs.get("live/prod", "sql.random").is_none());
    }

    #[test]
    fn bigquery_tables_api_when_information_schema_is_refused() {
        let gib = 1u64 << 30;
        let server = fake_http::serve(move |req| {
            let denied = (403, json!({"error": {"message": "Access Denied", "status": "PERMISSION_DENIED"}}).to_string());
            let b = |n: u64| (n * gib).to_string();
            let body = if req.path.ends_with("/queries") {
                return denied;
            } else if req.path.ends_with("/datasets/events/tables") {
                json!({"tables": [
                    {"type": "TABLE", "tableReference": {"tableId": "a"}},
                    {"type": "VIEW", "tableReference": {"tableId": "v"}},
                    {"type": "TABLE", "tableReference": {"tableId": "b"}}
                ]})
            } else if req.path.ends_with("/tables/a") {
                assert!(req.param("fields").is_some_and(|f| f.contains("numTotalPhysicalBytes")));
                json!({"numActiveLogicalBytes": b(600), "numLongTermLogicalBytes": b(400), "numTotalPhysicalBytes": b(130), "numLongTermPhysicalBytes": b(40), "numTimeTravelPhysicalBytes": b(10)})
            } else if req.path.ends_with("/tables/b") {
                json!({"numActiveLogicalBytes": b(400), "numLongTermLogicalBytes": b(100), "numTotalPhysicalBytes": b(70), "numLongTermPhysicalBytes": b(10), "numTimeTravelPhysicalBytes": b(10)})
            } else if req.path.ends_with("/datasets/locked/tables") {
                return denied;
            } else {
                return (404, json!({"error": {"message": "Not found"}}).to_string());
            };
            (200, body.to_string())
        });
        let cache = tempfile::tempdir().unwrap();
        let obs = run(&server, cache.path(), &Targets(vec![target(Kind::Dataset, "events", None)]), token());
        let u = &obs.get("live/prod", "dataset.events").unwrap().usage;
        for (k, v) in [
            ("storage_gb", 1000.0),
            ("long_term_storage_gb", 500.0),
            ("physical_storage_gb", 130.0),
            ("long_term_physical_storage_gb", 50.0),
            ("time_travel_storage_gb", 20.0),
            ("fail_safe_storage_gb", 0.0),
        ] {
            assert_eq!(u[k], v, "{k}");
        }
        // The tables API worked: no note about INFORMATION_SCHEMA.
        assert!(obs.notes.is_empty(), "{:?}", obs.notes);
        assert_eq!(obs.sources.iter().map(String::as_str).collect::<Vec<_>>(), ["BigQuery storage"]);
        // Cached: nothing is fetched again.
        let n = server.count();
        run(&server, cache.path(), &Targets(vec![target(Kind::Dataset, "events", None)]), token());
        assert_eq!(server.count(), n);

        // Both refused: one note with both ways to grant access.
        let cache = tempfile::tempdir().unwrap();
        let obs = run(&server, cache.path(), &Targets(vec![target(Kind::Dataset, "locked", None)]), token());
        assert!(obs.resources.is_empty());
        let notes = obs.notes.join("\n");
        assert!(
            notes.contains("BigQuery storage: permission denied for proj")
                && notes.contains("roles/bigquery.metadataViewer on the datasets"),
            "{notes}"
        );
        assert_eq!(obs.notes.len(), 1, "{notes}");
    }

    #[test]
    fn console_links() {
        let c = |t, n, place| console_link(t, "p-1", n, place);
        assert_eq!(
            c("google_compute_instance", "vm", Some("europe-west1-b")).as_deref(),
            Some("https://console.cloud.google.com/compute/instancesDetail/zones/europe-west1-b/instances/vm?project=p-1&tab=monitoring")
        );
        assert_eq!(
            c("google_compute_instance", "vm", None).as_deref(),
            Some("https://console.cloud.google.com/compute/instances?project=p-1")
        );
        assert_eq!(
            c("google_sql_database_instance", "db", None).as_deref(),
            Some("https://console.cloud.google.com/sql/instances/db/overview?project=p-1")
        );
        assert_eq!(
            c("google_cloud_run_v2_service", "api", Some("europe-west1")).as_deref(),
            Some("https://console.cloud.google.com/run/detail/europe-west1/api/metrics?project=p-1")
        );
        assert_eq!(
            c("google_cloudfunctions2_function", "f", Some("europe-west1")).as_deref(),
            Some("https://console.cloud.google.com/functions/details/europe-west1/f?env=gen2&project=p-1&tab=metrics")
        );
        assert!(c("google_bigquery_dataset", "ds", None).is_some_and(|u| u.contains("!1sp-1!2sds")));
        assert_eq!(c("google_cloud_run_v2_service", "api", None), None);
        assert_eq!(c("google_pubsub_topic", "t", None), None);
    }

    #[test]
    fn bytes_sent_by_buckets() {
        let gib = f64::from(1u32 << 30);
        let server = fake_http::serve(move |req| {
            let filter = req.param("filter").unwrap_or_default();
            if !filter.contains("network/sent_bytes_count") {
                return (400, "{}".into());
            }
            assert_eq!(req.param("aggregation.perSeriesAligner"), Some("ALIGN_SUM"));
            (
                200,
                json!({"timeSeries": [series_json("resource", "bucket_name", "logs", &vec![json!({"doubleValue": 10.0 * gib}); 30], 24)]})
                    .to_string(),
            )
        });
        let cache = tempfile::tempdir().unwrap();
        let t = Target { address: "bucket.logs".into(), ..target(Kind::BucketEgress, "logs", None) };
        let obs = run(&server, cache.path(), &Targets(vec![t]), token());
        // 10 GiB a day for 30 days.
        let sent = obs.get("live/prod", "bucket.logs").unwrap().usage["sent_gb"];
        assert!((sent - 304.17).abs() < 1.0, "{sent}");
    }

    #[test]
    fn several_kinds_per_resource_type() {
        assert_eq!(Kind::of("google_sql_database_instance"), &[Kind::Sql, Kind::SqlCpu, Kind::SqlMemory]);
        assert_eq!(Kind::of("google_storage_bucket"), &[Kind::Bucket, Kind::BucketEgress]);
        assert_eq!(Kind::of("google_bigquery_dataset"), &[Kind::Dataset, Kind::DatasetBytes]);
        assert_eq!(Kind::of("google_cloud_run_v2_service"), &[Kind::Run, Kind::RunInstances, Kind::RunCpu, Kind::RunMemory]);
        assert!(Kind::of("google_pubsub_topic").is_empty());
        assert_eq!(
            (Kind::SqlCpu.name(), Kind::DatasetBytes.name(), Kind::DatasetBytes.source()),
            ("sql-cpu", "dataset-bytes", "Cloud Monitoring")
        );
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
