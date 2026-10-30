//! Actual billing of the resources of the code (the stack, not the whole GCP
//! projects), week by week, read from the detailed Cloud Billing export to
//! BigQuery (`--billing-history`).
//!
//! The export table is given (`--billing-export-table`,
//! `billing_export_table:`) or discovered: costerra looks for the
//! `gcp_billing_export_resource_v1_<billing account>` table in the
//! organization of the projects (Cloud Asset Inventory), then in the datasets
//! of the projects it can see in BigQuery. Only the detailed (resource-level)
//! export names the resource of each cost: rows are kept when their resource
//! is one of the code (by name, by the name prefix of the VMs of an instance
//! group, or by the GKE cluster label). One query sums the cost, net of
//! credits, per ISO week, service and resource of the code over the last
//! `WEEKS` weeks (the current one in progress); with `--billing-by-env`, the
//! same rows give one chart per environment when the code spans several. The query runs in the quota project when one is
//! configured, otherwise in the project of the service account of the
//! credentials, then in the project of the table. Results are cached for an
//! hour: the export is updated a few times a day.

use crate::output::money;
use crate::pricing::sync::{Auth, now, resolve_auth};
use crate::project::Project;
use futures::StreamExt;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Weeks shown: about three months, the current week included.
pub const WEEKS: usize = 13;
/// Services shown separately; the others are added up in `OTHER`.
const TOP_SERVICES: usize = 5;
pub const OTHER: &str = "Other services";
/// Weeks not over yet (or recently over) are read again after this.
const CACHE_TTL_SECS: u64 = 3600;
/// A week read this long after its end is final: the export is late by a day
/// or two at most, so it is never queried (and billed) again.
const SETTLED_DAYS: i64 = 5;
/// A query still running when costerra stopped waiting is read by a later
/// run within this time (BigQuery keeps the results of a query for a day).
const PENDING_TTL_SECS: u64 = 6 * 3600;
const DISCOVERY_TTL_SECS: u64 = 7 * 86400;
/// A search without result is not repeated before this (it can take seconds).
const MISS_TTL_SECS: u64 = 6 * 3600;
/// The detailed usage cost export: the standard one has no resource names.
const TABLE_PREFIX: &str = "gcp_billing_export_resource_v1_";
/// Resources of the code not found in the billing listed in the chart.
const MAX_LISTED: usize = 5;
/// Hours per month, as in the estimates.
const HOURS_PER_MONTH: f64 = 730.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Week {
    /// Monday of the week (`YYYY-MM-DD`).
    pub start: String,
    /// Cost net of credits.
    pub cost: f64,
    /// Cost per service, in the order of `BillingHistory::services`.
    pub services: IndexMap<String, f64>,
    /// The current week, not over yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_progress: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BillingHistory {
    /// Environment of the resources of this chart, when split by environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// Projects of the code of the environment (to compare with their estimate).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_projects: Vec<String>,
    /// The billing export table read.
    pub table: String,
    pub currency: String,
    /// GCP projects of the code.
    pub projects: Vec<String>,
    /// Resources of the code looked up in the billing.
    pub resources: usize,
    /// Resources of the code without a known name (or project), not looked up.
    #[serde(default)]
    pub unnamed: usize,
    /// Resources of the code (`project/name`) without cost in the billing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_found: Vec<String>,
    /// Services from the most to the least expensive (`OTHER` last, when any).
    pub services: Vec<String>,
    pub weeks: Vec<Week>,
    pub total: f64,
}

/// The billing of the resources of the code, and per environment when they
/// span several.
#[derive(Debug, Clone, PartialEq)]
pub struct Billing {
    pub total: BillingHistory,
    pub by_env: Vec<BillingHistory>,
}

impl Billing {
    /// The charts shown: one per environment, or the total.
    pub fn charts(&self) -> &[BillingHistory] {
        if self.by_env.is_empty() { std::slice::from_ref(&self.total) } else { &self.by_env }
    }
}

impl BillingHistory {
    /// Weeks that are over.
    fn full_weeks(&self) -> impl DoubleEndedIterator<Item = &Week> {
        self.weeks.iter().filter(|w| !w.in_progress)
    }
}

/// What to read.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// `project.dataset.table` of the export; discovered when `None`.
    pub table: Option<String>,
    /// Billing account of the projects (picks the export table when several are found).
    pub billing_account: Option<String>,
    /// Resources of the code.
    pub stack: Stack,
    /// Quota project, and project the query runs in (default: the project of
    /// the service account of the credentials, then the project of the table).
    pub quota: Option<String>,
    /// One history per environment when the resources span several.
    pub by_env: bool,
}

/// The resources of the code as the billing export names them, each as
/// `project/name`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stack {
    /// `resource.name` of the billed resources (VMs, disks, buckets, Cloud SQL
    /// instances, Cloud Run services…).
    pub names: BTreeSet<String>,
    /// Name prefixes of the VMs of managed instance groups (`project/base-`).
    pub prefixes: BTreeSet<String>,
    /// GKE clusters, matched by the `goog-k8s-cluster-name` label of their nodes.
    pub clusters: BTreeSet<String>,
    /// Cloud Workstations (`project/uid`), matched by the
    /// `goog-drz-workstation-uuid` label of their VMs and disks.
    pub workstations: BTreeSet<String>,
    /// Billable resources whose name or project is not known before apply.
    pub unnamed: usize,
    /// Environment of each resource (`names`, `prefixes`, `clusters`,
    /// `workstations` entry).
    pub envs: BTreeMap<String, String>,
    /// Resources without a known name, per environment.
    pub unnamed_by_env: BTreeMap<String, usize>,
    /// Projects of the code per environment.
    pub code_projects: BTreeMap<String, BTreeSet<String>>,
}

impl Stack {
    pub fn is_empty(&self) -> bool {
        self.keys().next().is_none()
    }

    fn len(&self) -> usize {
        self.keys().count()
    }

    /// All the resources: `names`, `prefixes`, then the ones matched by label.
    fn keys(&self) -> impl Iterator<Item = &String> {
        self.names.iter().chain(&self.prefixes).chain(self.labeled())
    }

    /// Resources matched by a label of their billed parts (clusters, workstations).
    fn labeled(&self) -> impl Iterator<Item = &String> {
        self.clusters.iter().chain(&self.workstations)
    }

    /// GCP projects of the resources, sorted.
    pub fn projects(&self) -> Vec<String> {
        let all = self.keys();
        let set: BTreeSet<&str> = all.filter_map(|k| k.split_once('/').map(|(p, _)| p)).collect();
        set.into_iter().map(str::to_string).collect()
    }

    /// The resource of the stack a billed key (`project/name`) belongs to.
    fn resource_of(&self, key: &str) -> Option<String> {
        if self.names.contains(key) || self.clusters.contains(key) || self.workstations.contains(key) {
            return Some(key.to_string());
        }
        self.prefixes.iter().find(|p| key.starts_with(p.as_str())).cloned()
    }

    /// The resources of each environment, when they span several.
    fn by_env(&self) -> Vec<(String, Stack)> {
        let envs: BTreeSet<&String> = self.envs.values().collect();
        if envs.len() < 2 {
            return vec![];
        }
        envs.into_iter()
            .map(|e| {
                let of = |set: &BTreeSet<String>| set.iter().filter(|k| self.envs.get(*k) == Some(e)).cloned().collect();
                let stack = Stack {
                    names: of(&self.names),
                    prefixes: of(&self.prefixes),
                    clusters: of(&self.clusters),
                    workstations: of(&self.workstations),
                    unnamed: self.unnamed_by_env.get(e).copied().unwrap_or(0),
                    code_projects: self.code_projects.get(e).map(|p| [(e.clone(), p.clone())].into()).unwrap_or_default(),
                    ..Stack::default()
                };
                (e.clone(), stack)
            })
            .collect()
    }
}

/// Environment of the resources of projects whose environment is not known.
pub const OTHER_ENV: &str = "other";

/// Environment of a project: the `env` of its `projects:` rule in
/// costerra.yml, else the last environment word (`environments:`) of its
/// name (`live/dev/app`, `network (prod)`), else of the GCP project ids of
/// its resources.
pub fn env_name(p: &Project, config: &crate::config::Config) -> Option<String> {
    if let Some(e) = config.project_env_name(&p.name) {
        return Some(e);
    }
    let words: BTreeSet<String> = config.nonprod_patterns().into_iter().chain(config.prod_patterns()).collect();
    let last = |text: &str| text.split(|c: char| !c.is_ascii_alphanumeric()).map(str::to_lowercase).rfind(|t| words.contains(t));
    last(&p.name).or_else(|| p.resources.iter().filter_map(crate::observed::gcp_project).find_map(last))
}

/// A name known before apply.
fn known(v: Option<&str>) -> Option<&str> {
    v.filter(|n| !n.is_empty() && !n.starts_with(crate::REF_PREFIX) && !n.contains("${"))
}

/// The billable resources of the code, as named in the billing export, with
/// the environment of their project (`env`, `OTHER_ENV` when not known).
pub fn stack_of(sets: &[&[Project]], env: impl Fn(&Project) -> Option<String>) -> Stack {
    let registry = crate::resources::registry();
    let mut stack = Stack::default();
    for p in sets.iter().flat_map(|s| s.iter()) {
        let e = env(p).unwrap_or_else(|| OTHER_ENV.into());
        stack.code_projects.entry(e.clone()).or_default().insert(p.name.clone());
        add_resources(&mut stack, registry, p, &e);
    }
    stack
}

fn add_resources(stack: &mut Stack, registry: &HashMap<&'static str, crate::resources::Spec>, p: &Project, env: &str) {
    for r in &p.resources {
        if !registry.contains_key(r.rtype.as_str()) {
            continue;
        }
        let attr = |k: &str| known(r.attrs.get(k).and_then(|v| v.as_str()));
        let Some(project) = crate::observed::gcp_project(r) else {
            stack.unnamed += 1;
            *stack.unnamed_by_env.entry(env.into()).or_default() += 1;
            continue;
        };
        let (set, name) = match r.rtype.as_str() {
            "google_container_cluster" => (&mut stack.clusters, attr("name").map(str::to_string)),
            // `cluster` may be a full id: projects/p/locations/l/clusters/c.
            "google_container_node_pool" => (&mut stack.clusters, attr("cluster").and_then(|c| c.rsplit('/').next()).map(str::to_string)),
            "google_compute_instance_group_manager" | "google_compute_region_instance_group_manager" => {
                (&mut stack.prefixes, attr("base_instance_name").map(|b| format!("{b}-")))
            }
            "google_bigquery_dataset" => (&mut stack.names, attr("dataset_id").map(str::to_string)),
            // Its VMs and disks are named after ids that are not in Terraform,
            // but labeled with its `uid` (known from the state once created).
            "google_workstations_workstation" => (&mut stack.workstations, attr("uid").map(str::to_string)),
            _ => (&mut stack.names, attr("name").map(str::to_string)),
        };
        match name {
            Some(n) => {
                let key = format!("{project}/{n}");
                set.insert(key.clone());
                stack.envs.entry(key).or_insert_with(|| env.into());
            }
            // A node pool of a cluster of the code: billed with its cluster.
            None if r.rtype == "google_container_node_pool" => {}
            None => {
                stack.unnamed += 1;
                *stack.unnamed_by_env.entry(env.into()).or_default() += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Dates (days since 1970-01-01)
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// (year, month, day) of a day number (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn iso_date(days: i64) -> String {
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.get(..10)?.split('-');
    let (y, m, d) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    Some(days_from_civil(y, m, d))
}

/// Monday of the week of `days` (1970-01-01 was a Thursday).
fn monday(days: i64) -> i64 {
    days - (days + 3).rem_euclid(7)
}

fn today() -> i64 {
    (now() / 86400) as i64
}

/// `Oct 13`
fn short_date(s: &str) -> String {
    match parse_date(s).map(civil) {
        Some((_, m, d)) => format!("{} {d}", MONTHS[m as usize - 1]),
        None => s.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    /// billing account (or "") -> (discovered table, found at).
    #[serde(default)]
    tables: HashMap<String, (String, u64)>,
    /// billing account (or "") -> (why no table was found, searched at).
    #[serde(default)]
    misses: HashMap<String, (String, u64)>,
    /// table -> Monday of the week -> what was read of that week.
    #[serde(default)]
    weeks: HashMap<String, BTreeMap<String, CachedWeek>>,
    /// table -> the query still running when the last run stopped waiting.
    #[serde(default)]
    pending: HashMap<String, PendingJob>,
}

/// A BigQuery query job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Job {
    project: String,
    id: String,
    location: String,
}

/// A query that was still running when costerra stopped waiting. It is
/// billed whether or not its result is read: the next run reads it (for
/// free) instead of running the query again.
#[derive(Clone, Serialize, Deserialize)]
struct PendingJob {
    job: Job,
    /// Monday of the first week read.
    start: String,
    /// The resources looked for.
    resources: BTreeSet<String>,
    /// When it was started.
    at: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct CachedWeek {
    /// When it was read.
    at: u64,
    /// The resources looked for (`names`, `prefixes` and `clusters` entries).
    resources: BTreeSet<String>,
    rows: Vec<Row>,
}

/// The rows of the weeks from `first` that are cached for the resources of
/// `stack`, up to the first week to read again, returned with it.
fn cached_rows(cache: &Cache, table: &str, stack: &Stack, first: i64) -> (Vec<Row>, Option<i64>) {
    let keys: BTreeSet<&String> = stack.keys().collect();
    let weeks = cache.weeks.get(table);
    let usable = |day: i64| {
        let w = weeks?.get(&iso_date(day))?;
        let settled = w.at as i64 >= (day + 7 + SETTLED_DAYS) * 86400;
        let fresh = now().saturating_sub(w.at) < CACHE_TTL_SECS;
        (keys.iter().all(|k| w.resources.contains(*k)) && (settled || fresh)).then_some(w)
    };
    let mut rows = vec![];
    for day in (0..WEEKS as i64).map(|i| first + 7 * i) {
        match usable(day) {
            Some(w) => rows.extend(w.rows.iter().cloned()),
            None => return (rows, Some(day)),
        }
    }
    (rows, None)
}

fn cache_path(dir: &Path) -> PathBuf {
    dir.join(crate::pricing::sync::identity_file("billing-history"))
}

/// `project.dataset.table` -> its parts (the project may contain dots and a
/// colon: `example.com:proj`). Only the characters of Google identifiers.
fn parse_table(t: &str) -> Option<(&str, &str, &str)> {
    let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_-.:".contains(c));
    let mut it = t.rsplitn(3, '.');
    let (table, dataset, project) = (it.next()?, it.next()?, it.next()?);
    (ok(project) && ok(dataset) && !dataset.contains(['.', ':']) && ok(table) && !table.contains(['.', ':']))
        .then_some((project, dataset, table))
}

#[derive(Debug)]
enum ApiError {
    Status(u16, String),
    Network(String),
    Deadline,
    /// The deadline passed while the query was running in BigQuery.
    Running(Job),
}

impl ApiError {
    /// A short reason; `role` is what to grant when access is denied.
    fn reason(&self, role: &str) -> String {
        match self {
            ApiError::Status(_, b) if b.contains("SERVICE_DISABLED") || b.contains("has not been used") => {
                "API disabled in the quota project".into()
            }
            ApiError::Status(403 | 401, _) => format!("permission denied: grant {role}"),
            ApiError::Status(s, b) => format!("HTTP {s}: {}", message(b)),
            ApiError::Network(e) => e.clone(),
            ApiError::Deadline | ApiError::Running(_) => "no answer in time".into(),
        }
    }

    /// A readable reason, `what` being read.
    fn describe(&self, what: &str) -> String {
        match self {
            ApiError::Status(_, b) if b.contains("SERVICE_DISABLED") || b.contains("has not been used") => {
                format!("{what}: the BigQuery API is disabled in this project (or set a quota project with --quota-project)")
            }
            ApiError::Status(403 | 429, b) if b.to_lowercase().contains("quota exceeded") || b.contains("quotaExceeded") => format!(
                "{what}: BigQuery quota exceeded ({}); try again later (a daily quota resets the next day), \
                 or run the query in another project with --quota-project",
                message(b)
            ),
            ApiError::Status(400, b) if b.contains("_PARTITIONTIME") => {
                format!("{what}: the table is not partitioned by day, as the Cloud Billing exports are ({})", message(b))
            }
            ApiError::Status(403 | 401, b) => {
                let reason = message(b);
                format!(
                    "{what}: permission denied ({reason}); grant roles/bigquery.dataViewer on the billing export dataset and roles/bigquery.jobUser on the project the query runs in"
                )
            }
            ApiError::Status(404, b) => format!("{what}: not found ({})", message(b)),
            ApiError::Status(400, b) if b.contains("Unrecognized name: resource") || b.contains("Name resource not found") => format!(
                "{what}: not a detailed usage cost export ({TABLE_PREFIX}…): the standard export has no resource names, \
                 so the costs of the resources of the code cannot be told apart"
            ),
            ApiError::Status(s, b) => format!("{what}: HTTP {s} ({})", message(b)),
            ApiError::Network(e) => format!("{what}: {e}"),
            ApiError::Deadline => format!("{what}: no answer in time (try again: the result is cached once read)"),
            ApiError::Running(_) => {
                format!("{what}: the query is still running in BigQuery; the next run reads its result instead of running it again")
            }
        }
    }
}

/// Project of a service account by its email: `name@PROJECT.iam.gserviceaccount.com`
/// or `PROJECT@appspot.gserviceaccount.com` (the default Compute Engine one
/// names a project number, not an id).
fn project_of_service_account(email: &str) -> Option<String> {
    let (name, domain) = email.split_once('@')?;
    let project = match domain.strip_suffix(".iam.gserviceaccount.com") {
        Some(p) => p,
        None if domain == "appspot.gserviceaccount.com" => name,
        None => return None,
    };
    (!project.is_empty()).then(|| project.to_string())
}

/// The `error.message` of a Google API error body.
fn message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|j| j["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect())
}

/// Base URLs of the APIs (a local server in tests).
pub struct Endpoints {
    bigquery: String,
    asset: String,
    resource_manager: String,
    oauth2: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            bigquery: "https://bigquery.googleapis.com".into(),
            asset: "https://cloudasset.googleapis.com".into(),
            resource_manager: "https://cloudresourcemanager.googleapis.com".into(),
            oauth2: "https://oauth2.googleapis.com".into(),
        }
    }
}

/// What the discovery of the export table looked at, to explain a miss.
#[derive(Default)]
struct Search {
    /// Organization of the projects of the code.
    org: Option<String>,
    /// Why the Cloud Asset Inventory search failed.
    asset_error: Option<String>,
    /// BigQuery projects whose datasets were listed, and refused.
    scanned: usize,
    refused: usize,
    /// An export table of another billing account (used when none matches).
    other: Option<String>,
}

/// Words of the projects that usually hold the billing export: searched first.
const BILLING_HINTS: [&str; 5] = ["billing", "finops", "cost", "finance", "invoice"];
/// Most BigQuery projects searched for the export table.
const MAX_SCANNED: usize = 500;
/// Share of the deadline given to the discovery of the table (the query needs the rest).
const DISCOVERY_SECS: u64 = 20;

struct Http<'a> {
    client: reqwest::Client,
    auth: Auth,
    quota: Option<&'a str>,
    until: tokio::time::Instant,
    api: &'a Endpoints,
}

impl Http<'_> {
    async fn send(&self, mut rq: reqwest::RequestBuilder) -> Result<serde_json::Value, ApiError> {
        if let Auth::Bearer(t) = &self.auth {
            rq = rq.bearer_auth(t);
        }
        if let Some(q) = self.quota {
            rq = rq.header("x-goog-user-project", q);
        }
        match tokio::time::timeout_at(self.until, rq.send()).await {
            Ok(Ok(r)) if r.status().is_success() => r.json().await.map_err(|e| ApiError::Network(e.to_string())),
            Ok(Ok(r)) => {
                let status = r.status().as_u16();
                let body = r.text().await.unwrap_or_default();
                tracing::debug!("billing export: HTTP {status}: {}", body.chars().take(300).collect::<String>());
                Err(ApiError::Status(status, body))
            }
            Ok(Err(e)) => Err(ApiError::Network(e.to_string())),
            Err(_) => Err(ApiError::Deadline),
        }
    }

    async fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<serde_json::Value, ApiError> {
        self.send(self.client.get(url).query(query)).await
    }

    /// Project of the service account of the credentials, from the email of
    /// the token (`name@PROJECT.iam.gserviceaccount.com`); `None` for a user.
    async fn service_account_project(&self) -> Option<String> {
        let Auth::Bearer(t) = &self.auth else { return None };
        let rq = self.client.get(format!("{}/tokeninfo", self.api.oauth2)).query(&[("access_token", t.as_str())]);
        let j: serde_json::Value =
            tokio::time::timeout_at(self.until, async { rq.send().await.ok()?.json().await.ok() }).await.ok().flatten()?;
        project_of_service_account(j["email"].as_str()?)
    }

    /// The organization of the first of these projects that can be read.
    async fn organization(&self, projects: &[String]) -> Option<String> {
        for p in projects.iter().take(4) {
            let url = format!("{}/v1/projects/{p}:getAncestry", self.api.resource_manager);
            let Ok(j) = self.send(self.client.post(url).json(&serde_json::json!({}))).await else { continue };
            let org = j["ancestor"].as_array().into_iter().flatten().find(|a| a["resourceId"]["type"] == "organization");
            if let Some(id) = org.and_then(|a| a["resourceId"]["id"].as_str()) {
                return Some(id.to_string());
            }
        }
        None
    }

    /// Billing export tables of the organization, from Cloud Asset Inventory
    /// (one search over every project): the table of `account` by its exact
    /// name, otherwise every table named like an export.
    async fn asset_search(&self, org: &str, account: Option<&str>) -> Result<Vec<String>, ApiError> {
        let url = format!("{}/v1/organizations/{org}:searchAllResources", self.api.asset);
        let query = match account {
            Some(a) => format!("displayName={TABLE_PREFIX}{}", a.replace('-', "_").to_uppercase()),
            None => format!("displayName:{TABLE_PREFIX}*"),
        };
        let mut found = vec![];
        let mut token = String::new();
        for _ in 0..5 {
            let mut q = vec![("assetTypes", "bigquery.googleapis.com/Table"), ("query", query.as_str()), ("pageSize", "500")];
            if !token.is_empty() {
                q.push(("pageToken", token.as_str()));
            }
            let j = self.get(&url, &q).await?;
            for r in j["results"].as_array().into_iter().flatten() {
                // //bigquery.googleapis.com/projects/P/datasets/D/tables/T
                let parts: Vec<&str> = r["name"].as_str().unwrap_or_default().split('/').collect();
                if let [.., "projects", p, "datasets", d, "tables", t] = parts.as_slice()
                    && t.starts_with(TABLE_PREFIX)
                {
                    found.push(format!("{p}.{d}.{t}"));
                }
            }
            match j["nextPageToken"].as_str().filter(|t| !t.is_empty()) {
                Some(t) => token = t.to_string(),
                None => break,
            }
        }
        found.sort();
        Ok(found)
    }

    /// Projects the caller can see in BigQuery.
    async fn bigquery_projects(&self) -> Vec<String> {
        let url = format!("{}/bigquery/v2/projects", self.api.bigquery);
        let mut out = vec![];
        let mut token = String::new();
        for _ in 0..5 {
            let mut q = vec![("maxResults", "1000")];
            if !token.is_empty() {
                q.push(("pageToken", token.as_str()));
            }
            let Ok(j) = self.get(&url, &q).await else { break };
            out.extend(
                j["projects"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p["projectReference"]["projectId"].as_str().map(str::to_string)),
            );
            match j["nextPageToken"].as_str().filter(|t| !t.is_empty()) {
                Some(t) => token = t.to_string(),
                None => break,
            }
        }
        out
    }

    /// Billing export tables in the datasets of `project`.
    async fn export_tables(&self, project: &str) -> Result<Vec<String>, ApiError> {
        let j = self.get(&format!("{}/bigquery/v2/projects/{project}/datasets", self.api.bigquery), &[("maxResults", "1000")]).await?;
        let mut datasets: Vec<String> = j["datasets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["datasetReference"]["datasetId"].as_str().map(str::to_string))
            .collect();
        // Billing datasets first: they are usually named so.
        datasets.sort_by_key(|d| !d.to_lowercase().contains("billing"));
        let lists = futures::stream::iter(datasets.into_iter().map(|d| async move {
            let url = format!("{}/bigquery/v2/projects/{project}/datasets/{d}/tables", self.api.bigquery);
            let tables = self.get(&url, &[("maxResults", "1000")]).await.unwrap_or_default();
            tables["tables"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["tableReference"]["tableId"].as_str())
                .filter(|t| t.starts_with(TABLE_PREFIX))
                .map(|t| format!("{project}.{d}.{t}"))
                .collect::<Vec<_>>()
        }))
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await;
        let mut found: Vec<String> = lists.into_iter().flatten().collect();
        found.sort();
        Ok(found)
    }

    /// The export table of `account` (or, without a match, another export
    /// table): first with Cloud Asset Inventory over the organization of the
    /// projects, then in the datasets of the projects the caller can see in
    /// BigQuery (`known` first, then the ones named like billing projects).
    async fn discover(&self, known: &[String], account: Option<&str>, search: &mut Search) -> Option<String> {
        let suffix = account.map(|a| a.replace('-', "_").to_uppercase());
        let matches = |t: &String| suffix.as_ref().is_none_or(|s| t.to_uppercase().ends_with(s.as_str()));
        search.org = self.organization(known).await;
        if let Some(org) = &search.org {
            match self.asset_search(org, account).await {
                Ok(found) => {
                    if let Some(t) = found.iter().find(|t| matches(t)) {
                        return Some(t.clone());
                    }
                    search.other = found.into_iter().next();
                }
                Err(e) => search.asset_error = Some(e.reason("roles/cloudasset.viewer on the organization")),
            }
        }
        let mut candidates: Vec<String> = known.to_vec();
        let mut others = self.bigquery_projects().await;
        others.sort_by_key(|p| !BILLING_HINTS.iter().any(|h| p.to_lowercase().contains(h)));
        for p in others {
            if !candidates.contains(&p) {
                candidates.push(p);
            }
        }
        candidates.truncate(MAX_SCANNED);
        let mut scans = futures::stream::iter(candidates.iter().map(|p| self.export_tables(p))).buffer_unordered(8);
        while let Some(r) = scans.next().await {
            search.scanned += 1;
            match r {
                Ok(found) => {
                    if let Some(t) = found.iter().find(|t| matches(t)) {
                        return Some(t.clone());
                    }
                    search.other = search.other.take().or(found.into_iter().next());
                }
                Err(_) => search.refused += 1,
            }
        }
        None
    }

    /// Rows since `start` for the resources of `stack`.
    /// Project the query runs in: the quota project when set, otherwise the
    /// project of the service account, otherwise the project of the table.
    /// It runs once: no other project is tried, each try being billed.
    async fn job_project(&self, table: &str) -> String {
        if let Some(q) = self.quota {
            return q.to_string();
        }
        let table_project = parse_table(table).map(|(p, ..)| p.to_string()).unwrap_or_default();
        self.service_account_project().await.unwrap_or(table_project)
    }

    /// Rows since `start` (a Monday) for the resources of `stack`, the query run in `job_project`.
    async fn weekly(&self, job_project: &str, table: &str, stack: &Stack, start: &str) -> Result<Vec<Row>, ApiError> {
        let sql = {
            // The export is partitioned by export day, never before the usage
            // day: only the partitions since `start` are scanned (and billed).
            let pruning = " AND DATE(_PARTITIONTIME) >= DATE_SUB(@start, INTERVAL 1 DAY)";
            // The labels are only read (and billed) when the code has
            // resources matched by label.
            let label_keys: Vec<&str> = [
                (!stack.clusters.is_empty()).then_some("'goog-k8s-cluster-name'"),
                (!stack.workstations.is_empty()).then_some("'goog-drz-workstation-uuid'"),
            ]
            .into_iter()
            .flatten()
            .collect();
            let cluster = if label_keys.is_empty() {
                "CAST(NULL AS STRING)".to_string()
            } else {
                format!(
                    "(SELECT CONCAT(project.id, '/', l.value) FROM UNNEST(labels) AS l WHERE l.key IN ({}) LIMIT 1)",
                    label_keys.join(", ")
                )
            };
            format!(
                "WITH r AS (SELECT usage_start_time, service.description AS service, cost, credits, currency, \
                 CONCAT(project.id, '/', IFNULL(resource.name, '')) AS rkey, {cluster} AS ckey \
                 FROM `{table}` WHERE usage_start_time >= TIMESTAMP(@start) AND project.id IN UNNEST(@projects){pruning}), \
                 k AS (SELECT *, IF(ckey IN UNNEST(@clusters), ckey, IF(rkey IN UNNEST(@names), rkey, \
                 (SELECT p FROM UNNEST(@prefixes) AS p WHERE STARTS_WITH(rkey, p) LIMIT 1))) AS resource FROM r) \
                 SELECT FORMAT_DATE('%F', DATE_TRUNC(DATE(usage_start_time), ISOWEEK)) AS week, service, \
                 SUM(cost) + SUM(IFNULL((SELECT SUM(c.amount) FROM UNNEST(credits) AS c), 0)) AS net, ANY_VALUE(currency) AS currency, resource \
                 FROM k WHERE resource IS NOT NULL GROUP BY week, service, resource"
            )
        };
        let array = |name: &str, values: Vec<&str>| {
            serde_json::json!({"name": name, "parameterType": {"type": "ARRAY", "arrayType": {"type": "STRING"}},
                "parameterValue": {"arrayValues": values.into_iter().map(|v| serde_json::json!({"value": v})).collect::<Vec<_>>()}})
        };
        let projects = stack.projects();
        let params = serde_json::json!([
            {"name": "start", "parameterType": {"type": "DATE"}, "parameterValue": {"value": start}},
            array("projects", projects.iter().map(String::as_str).collect()),
            array("names", stack.names.iter().map(String::as_str).collect()),
            array("prefixes", stack.prefixes.iter().map(String::as_str).collect()),
            array("clusters", stack.labeled().map(String::as_str).collect()),
        ]);
        let url = format!("{}/bigquery/v2/projects/{job_project}/queries", self.api.bigquery);
        let body = serde_json::json!({
            "query": sql, "useLegacySql": false, "parameterMode": "NAMED",
            "queryParameters": params, "timeoutMs": 10000, "maxResults": 10000,
        });
        let j = self.send(self.client.post(&url).json(&body)).await?;
        self.rows(job_project, j).await
    }

    /// The rows of a query started by an earlier run.
    async fn resume(&self, job: &Job) -> Result<Vec<Row>, ApiError> {
        let url = format!("{}/bigquery/v2/projects/{}/queries/{}", self.api.bigquery, job.project, job.id);
        let j = self.get(&url, &[("location", job.location.as_str()), ("timeoutMs", "5000"), ("maxResults", "10000")]).await?;
        self.rows(&job.project, j).await
    }

    /// The rows of a query, from its first response: until the job is
    /// complete, then page after page.
    async fn rows(&self, job_project: &str, mut j: serde_json::Value) -> Result<Vec<Row>, ApiError> {
        let mut rows = vec![];
        loop {
            let complete = j["jobComplete"].as_bool() == Some(true);
            if complete {
                for row in j["rows"].as_array().into_iter().flatten() {
                    let cells: Vec<&serde_json::Value> = row["f"].as_array().into_iter().flatten().map(|c| &c["v"]).collect();
                    let s = |i: usize| cells.get(i).and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    rows.push(Row { week: s(0), service: s(1), cost: s(2).parse().unwrap_or(0.0), currency: s(3), resource: s(4) });
                }
            }
            let page = j["pageToken"].as_str().map(str::to_string);
            if complete && page.is_none() {
                return Ok(rows);
            }
            let job = &j["jobReference"];
            let (Some(id), location) = (job["jobId"].as_str(), job["location"].as_str().unwrap_or_default()) else {
                return Err(ApiError::Network("query without a job reference".into()));
            };
            let url = format!("{}/bigquery/v2/projects/{job_project}/queries/{id}", self.api.bigquery);
            let mut query = vec![("location", location), ("timeoutMs", "5000"), ("maxResults", "10000")];
            if let Some(p) = page.as_deref().filter(|_| complete) {
                query.push(("pageToken", p));
            }
            j = match self.get(&url, &query).await {
                Err(ApiError::Deadline) => {
                    let job = Job { project: job_project.to_string(), id: id.to_string(), location: location.to_string() };
                    return Err(ApiError::Running(job));
                }
                r => r?,
            };
        }
    }
}

/// One row of the query: the cost of a resource of the stack for a week and a service.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct Row {
    week: String,
    service: String,
    cost: f64,
    currency: String,
    /// The resource of the stack (`names`, `prefixes` or `clusters` entry).
    resource: String,
}

/// Read the weekly billing of the resources of `req`, or say why it cannot be read.
pub async fn fetch(cache_dir: &Path, req: &Request, until: std::time::Instant) -> Result<Billing, String> {
    let auth = async || match resolve_auth(None).await {
        Ok(a @ Auth::Bearer(_)) => Some(a),
        _ => None,
    };
    let mut req = req.clone();
    if req.billing_account.is_none() && req.table.is_none() {
        // The account names the export table: found even when its prices are not readable.
        req.billing_account = crate::pricing::account::detect(cache_dir, &req.stack.projects(), false).await;
    }
    fetch_from(cache_dir, &req, until, &Endpoints::default(), auth, today()).await
}

async fn fetch_from(
    cache_dir: &Path,
    req: &Request,
    until: std::time::Instant,
    api: &Endpoints,
    auth: impl AsyncFnOnce() -> Option<Auth>,
    today: i64,
) -> Result<Billing, String> {
    if req.stack.is_empty() {
        return Err("no resource of the code has a name and a GCP project known before apply (set `project` on the provider)".into());
    }
    if let Some(t) = &req.table
        && parse_table(t).is_none()
    {
        return Err(format!("invalid billing export table '{t}' (expected PROJECT.DATASET.TABLE)"));
    }
    let first = monday(today) - 7 * (WEEKS as i64 - 1);
    let account_key = req.billing_account.clone().unwrap_or_default();
    let cache: Cache = crate::util::read_json(&cache_path(cache_dir));
    if req.table.is_none()
        && let Some((why, at)) = cache.misses.get(&account_key)
        && now().saturating_sub(*at) < MISS_TTL_SECS
    {
        return Err(why.clone());
    }
    // Tables found by earlier versions may be standard exports, without resource names.
    let discovered = cache
        .tables
        .get(&account_key)
        .filter(|(t, at)| t.contains(TABLE_PREFIX) && now().saturating_sub(*at) < DISCOVERY_TTL_SECS)
        .map(|(t, _)| t.clone());
    let projects = req.stack.projects();
    if let Some(t) = req.table.clone().or(discovered.clone())
        && let (rows, None) = cached_rows(&cache, &t, &req.stack, first)
    {
        return Ok(billing(&t, &req.stack, req.by_env, first, today, &rows));
    }

    let until = tokio::time::Instant::from_std(until);
    let auth = tokio::time::timeout_at(until, auth())
        .await
        .ok()
        .flatten()
        .ok_or("no Google credentials (run `gcloud auth application-default login`)")?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION")))
        .gzip(true)
        .build()
        .map_err(|e| e.to_string())?;
    let http = Http { client, auth, quota: req.quota.as_deref(), until, api };

    let table = match req.table.clone().or(discovered) {
        Some(t) => t,
        None => {
            let mut known: Vec<String> = req.quota.iter().cloned().collect();
            known.extend(projects.iter().filter(|p| !known.contains(p)).cloned().collect::<Vec<_>>());
            let mut search = Search::default();
            let deadline = until.min(tokio::time::Instant::now() + std::time::Duration::from_secs(DISCOVERY_SECS));
            let found = tokio::time::timeout_at(deadline, http.discover(&known, req.billing_account.as_deref(), &mut search)).await;
            let timed_out = found.is_err();
            let Some(found) = found.ok().flatten().or(search.other.take()) else {
                let why = not_found(&search, timed_out);
                if !timed_out {
                    crate::util::update_json(&cache_path(cache_dir), |c: &mut Cache| {
                        c.misses.insert(account_key.clone(), (why.clone(), now()));
                    });
                }
                return Err(why);
            };
            tracing::info!("billing export table: {found}");
            crate::util::update_json(&cache_path(cache_dir), |c: &mut Cache| {
                c.tables.insert(account_key.clone(), (found.clone(), now()));
                c.misses.remove(&account_key);
            });
            found
        }
    };
    // Only the weeks not cached yet (or not final) are read: the cost of the
    // query is the partitions it scans.
    let (mut rows, from) = cached_rows(&cache, &table, &req.stack, first);
    // All cached, for a table just found again.
    let Some(from) = from else { return Ok(billing(&table, &req.stack, req.by_env, first, today, &rows)) };
    let start = iso_date(from);
    let wanted: BTreeSet<String> = req.stack.keys().cloned().collect();
    // A query of an earlier run, for these weeks and resources, that was
    // still running: its result is read instead of running it again.
    let pending = cache
        .pending
        .get(&table)
        .filter(|p| p.start == start && wanted.is_subset(&p.resources) && now().saturating_sub(p.at) < PENDING_TTL_SECS)
        .cloned();
    let resumed = match &pending {
        Some(p) => match http.resume(&p.job).await {
            Ok(rows) => Some((rows, p.at, p.resources.clone())),
            Err(ApiError::Running(_)) => {
                return Err(ApiError::Running(p.job.clone()).describe(&format!("billing export {table} (query run in {})", p.job.project)));
            }
            Err(e) => {
                tracing::debug!("billing export: the query of an earlier run cannot be read: {}", e.reason(""));
                None
            }
        },
        None => None,
    };
    let (read, read_at, resources) = match resumed {
        Some(r) => r,
        None => {
            let job = http.job_project(&table).await;
            tracing::info!("reading the billing of {} resources from {table} since {start}, in {job}", req.stack.len());
            let at = now();
            match http.weekly(&job, &table, &req.stack, &start).await {
                Ok(rows) => (rows, at, wanted),
                Err(e) => {
                    if let ApiError::Running(j) = &e {
                        let p = PendingJob { job: j.clone(), start: start.clone(), resources: wanted, at };
                        crate::util::update_json(&cache_path(cache_dir), |c: &mut Cache| {
                            c.pending.insert(table.clone(), p);
                        });
                    }
                    return Err(e.describe(&format!("billing export {table} (query run in {job})")));
                }
            }
        }
    };
    crate::util::update_json(&cache_path(cache_dir), |c: &mut Cache| {
        c.pending.remove(&table);
        let weeks = c.weeks.entry(table.clone()).or_default();
        for day in (from..=monday(today)).step_by(7) {
            let week = iso_date(day);
            let rows = read.iter().filter(|r| r.week == week).cloned().collect();
            weeks.insert(week, CachedWeek { at: read_at, resources: resources.clone(), rows });
        }
        weeks.retain(|w, _| parse_date(w).is_some_and(|d| d >= first));
    });
    rows.extend(read);
    Ok(billing(&table, &req.stack, req.by_env, first, today, &rows))
}

/// The total, and one history per environment when asked for and the stack
/// spans several.
fn billing(table: &str, stack: &Stack, by_env: bool, first: i64, today: i64, rows: &[Row]) -> Billing {
    let envs = if by_env { stack.by_env() } else { vec![] };
    let by_env = envs
        .into_iter()
        .map(|(env, sub)| {
            let mine: Vec<&Row> = rows.iter().filter(|r| sub.resource_of(&r.resource).is_some()).collect();
            let mut h = aggregate(table, &sub, first, today, mine);
            h.code_projects = sub.code_projects.get(&env).map(|p| p.iter().cloned().collect()).unwrap_or_default();
            h.env = Some(env);
            h
        })
        .collect();
    Billing { total: aggregate(table, stack, first, today, rows.iter().collect()), by_env }
}

/// Why no export table was found: where costerra looked, and what was refused.
fn not_found(search: &Search, timed_out: bool) -> String {
    let mut looked = vec![];
    match (&search.org, &search.asset_error) {
        (Some(org), None) => looked.push(format!("organization {org} (Cloud Asset Inventory)")),
        (Some(org), Some(e)) => looked.push(format!("organization {org} could not be searched with Cloud Asset Inventory ({e})")),
        (None, _) => looked.push("the organization of the projects is not readable (resourcemanager.projects.get)".into()),
    }
    let refused = if search.refused > 0 { format!(", {} refused", search.refused) } else { String::new() };
    looked.push(format!("the datasets of {} BigQuery projects{refused}", search.scanned));
    let stopped = if timed_out { " (search stopped by the time limit)" } else { "" };
    format!(
        "no detailed Cloud Billing export table ({TABLE_PREFIX}…) found: {}{stopped}. It is enabled in the console under Billing > \
         Billing export > BigQuery export > Detailed usage cost (the standard export has no resource names); \
         or set --billing-export-table PROJECT.DATASET.TABLE",
        looked.join("; ")
    )
}

/// Weeks from the query rows: the `TOP_SERVICES` most expensive services,
/// the others in `OTHER`, and a zero for weeks without cost.
fn aggregate(table: &str, stack: &Stack, first: i64, today: i64, rows: Vec<&Row>) -> BillingHistory {
    let currency = rows.iter().map(|r| r.currency.clone()).find(|c| !c.is_empty()).unwrap_or_else(|| "USD".into());
    let mut per_week: BTreeMap<i64, HashMap<String, f64>> = BTreeMap::new();
    let mut totals: HashMap<String, f64> = HashMap::new();
    let mut billed: BTreeSet<String> = BTreeSet::new();
    for Row { week, service, cost, resource, .. } in rows {
        // Cached weeks can hold the rows of more resources (read for a larger
        // stack): only the ones of this stack count.
        let Some(res) = stack.resource_of(resource) else { continue };
        billed.insert(res);
        let Some(day) = parse_date(week).filter(|d| *d >= first) else { continue };
        *per_week.entry(day).or_default().entry(service.clone()).or_default() += cost;
        *totals.entry(service.clone()).or_default() += cost;
    }
    let mut ranked: Vec<(String, f64)> = totals.into_iter().filter(|(_, t)| t.abs() >= 0.005).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let top: Vec<String> = ranked.iter().take(TOP_SERVICES).map(|(s, _)| s.clone()).collect();
    let mut services = top.clone();
    if ranked.len() > TOP_SERVICES {
        services.push(OTHER.into());
    }
    let weeks: Vec<Week> = (0..WEEKS as i64)
        .map(|i| {
            let day = first + 7 * i;
            let costs = per_week.remove(&day).unwrap_or_default();
            let mut by: IndexMap<String, f64> = services.iter().map(|s| (s.clone(), 0.0)).collect();
            for (s, c) in costs {
                let key = if top.contains(&s) { s } else { OTHER.into() };
                if let Some(v) = by.get_mut(&key) {
                    *v += c;
                }
            }
            Week { start: iso_date(day), cost: by.values().sum(), services: by, in_progress: today < day + 7 }
        })
        .collect();
    let total = weeks.iter().map(|w| w.cost).sum();
    let not_found = stack.keys().filter(|k| !billed.contains(*k)).cloned().collect();
    BillingHistory {
        env: None,
        code_projects: vec![],
        table: table.to_string(),
        currency,
        projects: stack.projects(),
        resources: stack.len(),
        unnamed: stack.unnamed,
        not_found,
        services,
        weeks,
        total,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// 256-color palette of the services (terracotta, sky, sage, sand, lilac,
/// teal), and grey for the other services.
const PALETTE: [u8; 6] = [209, 74, 114, 179, 140, 73];
const OTHER_COLOR: u8 = 245;
const ESTIMATE_COLOR: u8 = 117;
/// Lower eighth blocks: index = eighths filled.
const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const HEIGHT: usize = 10;

fn color_of(h: &BillingHistory, i: usize) -> u8 {
    if h.services[i] == OTHER { OTHER_COLOR } else { PALETTE[i % PALETTE.len()] }
}

/// Weekly cost equivalent of a monthly estimate.
pub fn weekly(monthly: f64) -> f64 {
    monthly * 168.0 / HOURS_PER_MONTH
}

/// `950`, `1.2k`, `12k`, `1.5M`
fn compact(v: f64) -> String {
    let a = v.abs();
    let s = if a < 999.5 {
        format!("{a:.0}")
    } else if a < 9_950.0 {
        format!("{:.1}k", a / 1e3)
    } else if a < 999_500.0 {
        format!("{:.0}k", a / 1e3)
    } else {
        format!("{:.1}M", a / 1e6)
    };
    if v < 0.0 && s != "0" { format!("-{s}") } else { s }
}

fn symbol(currency: &str) -> &'static str {
    match currency {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        _ => "",
    }
}

/// A round axis maximum at or above `v`.
fn nice_ceil(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let e = 10f64.powf(v.log10().floor());
    [1.0, 1.2, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0].iter().map(|m| m * e).find(|x| *x >= v * 0.999_999).unwrap_or(10.0 * e)
}

fn fg(on: bool, c: u8, s: &str) -> String {
    if on { format!("\x1b[38;5;{c}m{s}\x1b[0m") } else { s.to_string() }
}

fn styled(on: bool, code: &str, s: &str) -> String {
    if on { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

fn pct_change(new: f64, old: f64) -> Option<f64> {
    (old.abs() >= 0.005).then(|| (new - old) / old.abs() * 100.0)
}

/// `▲ 4.1%` (red) or `▼ 2.0%` (green).
fn trend(on: bool, p: f64) -> String {
    if p >= 0.05 {
        styled(on, "1;31", &format!("▲ {p:.1}%"))
    } else if p <= -0.05 {
        styled(on, "1;32", &format!("▼ {:.1}%", -p))
    } else {
        styled(on, "2", "= 0.0%")
    }
}

/// One cell of a stacked column: `ends` are the cumulative heights (in
/// eighths of a row) of the segments, bottom first.
fn cell(ends: &[(u8, usize)], row: usize, color: bool) -> Option<String> {
    let (lo, hi) = (row * 8, row * 8 + 8);
    let (i, &(a, end_a)) = ends.iter().enumerate().find(|(_, (_, e))| *e > lo)?;
    let fill = end_a.min(hi) - lo;
    if !color {
        let top = ends.last().map_or(0, |e| e.1);
        return Some(BLOCKS[top.min(hi) - lo].to_string());
    }
    if fill == 8 {
        return Some(format!("\x1b[38;5;{a}m█\x1b[0m"));
    }
    // The segment above fills the rest of the cell (its background).
    match ends[i + 1..].iter().find(|(_, e)| *e > end_a) {
        Some(&(b, _)) => Some(format!("\x1b[38;5;{a};48;5;{b}m{}\x1b[0m", BLOCKS[fill])),
        None => Some(format!("\x1b[38;5;{a}m{}\x1b[0m", BLOCKS[fill])),
    }
}

/// `23 of 31 resources found in the billing (4 without a known name)`
/// ` of <env>` for a chart of one environment.
fn of_env(h: &BillingHistory) -> String {
    h.env.as_deref().map(|e| format!(" of {e}")).unwrap_or_default()
}

fn scope(h: &BillingHistory) -> String {
    let unnamed = if h.unnamed > 0 { format!(" ({} more without a name known before apply)", h.unnamed) } else { String::new() };
    format!("{} of {} resources found in the billing{unnamed}", h.resources - h.not_found.len(), h.resources)
}

/// The weekly billing as a stacked bar chart for the terminal, with the
/// weekly equivalent of the estimate (`estimate_monthly`, same currency) as
/// a dashed line.
pub fn table(h: &BillingHistory, estimate_monthly: Option<f64>, color: bool, width: usize) -> String {
    let cur = h.currency.as_str();
    let sym = symbol(cur);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n{} {}",
        styled(color, "1", &format!("📊 Actual billing{}, last {} weeks", of_env(h), h.weeks.len())),
        styled(color, "2", &format!("(net of credits, {cur})"))
    );
    let _ = writeln!(out, "{}", styled(color, "2", &format!("   resources of the code only: {}", scope(h))));
    if !h.not_found.is_empty() {
        let mut listed: Vec<&str> = h.not_found.iter().take(MAX_LISTED).map(String::as_str).collect();
        let more = format!("… {} more", h.not_found.len() - listed.len());
        if h.not_found.len() > MAX_LISTED {
            listed.push(&more);
        }
        let _ = writeln!(out, "{}", styled(color, "2", &format!("   no cost found for: {}", listed.join(", "))));
    }
    let _ = writeln!(out, "{}\n", styled(color, "2", &format!("   from {}", h.table)));
    if h.weeks.iter().all(|w| w.cost.abs() < 0.005) {
        let _ = writeln!(out, "   No cost billed to the resources of the code over the period.");
        return out;
    }

    let estimate = estimate_monthly.map(weekly);
    let peak = h.weeks.iter().map(|w| w.cost).chain(estimate).fold(0.0, f64::max);
    let top = nice_ceil(peak);
    let to_eighths = |v: f64| ((v.max(0.0) / top) * (HEIGHT * 8) as f64).round() as usize;
    let cw = if width >= 80 { 4 } else { 3 };
    let labels = [(HEIGHT - 1, top), (HEIGHT / 2 - 1, top / 2.0)];
    let label = |v: f64| format!("{sym}{}", compact(v));
    let lw = labels.iter().map(|(_, v)| label(*v).chars().count()).max().unwrap_or(1).max(label(0.0).chars().count()) + 1;

    let columns: Vec<Vec<(u8, usize)>> = h
        .weeks
        .iter()
        .map(|w| {
            let mut cum = 0.0;
            w.services
                .values()
                .enumerate()
                .map(|(i, v)| {
                    cum += v.max(0.0);
                    (color_of(h, i), to_eighths(cum))
                })
                .collect()
        })
        .collect();
    let estimate_row = estimate.map(|e| (to_eighths(e) / 8).min(HEIGHT - 1));
    let dash = |n: usize| fg(color, ESTIMATE_COLOR, &"╌".repeat(n));

    for row in (0..=HEIGHT).rev() {
        let y = labels.iter().find(|(r, _)| *r == row).map(|(_, v)| label(*v)).unwrap_or_default();
        let axis = if row == HEIGHT {
            ' '
        } else if y.is_empty() {
            '│'
        } else {
            '┤'
        };
        let _ = write!(out, "{}{y} {}", " ".repeat(lw - y.chars().count()), styled(color, "2", &axis.to_string()));
        let on_estimate = estimate_row == Some(row);
        for (w, ends) in h.weeks.iter().zip(&columns) {
            out.push_str(&if on_estimate { dash(1) } else { " ".into() });
            let height = ends.last().map_or(0, |e| e.1);
            match cell(ends, row, color) {
                // The week in progress is dimmed: its cost is not final.
                Some(c) if w.in_progress => out.push_str(&c.replace("\x1b[38;5;", "\x1b[2;38;5;").repeat(cw)),
                Some(c) => out.push_str(&c.repeat(cw)),
                None if row == height.div_ceil(8) && cw >= 4 && w.cost.abs() >= 0.005 => {
                    let v = compact(w.cost);
                    let pad = cw.saturating_sub(v.chars().count());
                    let text = format!("{}{v}{}", " ".repeat(pad / 2), " ".repeat(pad - pad / 2));
                    out.push_str(&styled(color, "2", &text));
                }
                None if on_estimate => out.push_str(&dash(cw)),
                None => out.push_str(&" ".repeat(cw)),
            }
        }
        out.push('\n');
    }
    let span = h.weeks.len() * (cw + 1);
    let zero = label(0.0);
    let _ = writeln!(out, "{}{zero} {}", " ".repeat(lw - zero.chars().count()), styled(color, "2", &format!("└{}", "─".repeat(span))));
    // Day of each Monday ("now" for the current week), then the month where it changes.
    let mut days = String::new();
    let mut months = String::new();
    let mut last_month = 0;
    for w in &h.weeks {
        let (_, m, d) = parse_date(&w.start).map(civil).unwrap_or((0, 1, 1));
        let day = if w.in_progress { "now".to_string() } else { d.to_string() };
        let pad = cw.saturating_sub(day.chars().count());
        let _ = write!(days, " {}{day}{}", " ".repeat(pad / 2), " ".repeat(pad - pad / 2));
        let month = if m != last_month { MONTHS[m as usize - 1] } else { "" };
        last_month = m;
        let _ = write!(months, " {month:<cw$}");
    }
    let indent = " ".repeat(lw + 1);
    let _ = writeln!(out, "{indent}{}", styled(color, "2", &days));
    let _ = writeln!(out, "{indent}{}\n", styled(color, "2", months.trim_end()));

    // Legend: services with their total over the period.
    let totals: Vec<f64> = (0..h.services.len()).map(|i| h.weeks.iter().map(|w| w.services[i]).sum()).collect();
    let name_w = h.services.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let amounts: Vec<String> = totals.iter().map(|t| money(cur, *t)).collect();
    let amount_w = amounts.iter().map(|a| a.chars().count()).max().unwrap_or(0);
    for (i, s) in h.services.iter().enumerate() {
        let swatch = if color { fg(true, color_of(h, i), "██") } else { "  ".into() };
        let share = if h.total.abs() >= 0.005 { totals[i] / h.total * 100.0 } else { 0.0 };
        let _ = writeln!(out, "   {swatch} {s:<name_w$}  {:>amount_w$}  {}", amounts[i], styled(color, "2", &format!("{share:>5.1}%")));
    }
    if let (Some(m), Some(e)) = (estimate_monthly, estimate) {
        let _ = writeln!(
            out,
            "   {} {}",
            dash(2),
            styled(color, "2", &format!("estimate of the code: {}/month ≈ {}/week", money(cur, m), money(cur, e)))
        );
    }
    out.push('\n');

    // Summary.
    let full: Vec<&Week> = h.full_weeks().collect();
    let _ = writeln!(out, "   Total over {} weeks: {}", h.weeks.len(), styled(color, "1", &money(cur, h.total)));
    if !full.is_empty() {
        let avg = full.iter().map(|w| w.cost).sum::<f64>() / full.len() as f64;
        let _ = writeln!(out, "   Average of the {} full weeks: {}/week", full.len(), money(cur, avg));
    }
    if let [.., prev, last] = full.as_slice() {
        let change = pct_change(last.cost, prev.cost).map(|p| format!(", {} vs the week before", trend(color, p))).unwrap_or_default();
        let _ = writeln!(out, "   Last full week ({}): {}{change}", short_date(&last.start), money(cur, last.cost));
    }
    if full.len() >= 8 {
        let n = full.len();
        let recent: f64 = full[n - 4..].iter().map(|w| w.cost).sum();
        let before: f64 = full[n - 8..n - 4].iter().map(|w| w.cost).sum();
        if let Some(p) = pct_change(recent, before) {
            let _ = writeln!(out, "   Last 4 weeks vs the 4 before: {}", trend(color, p));
        }
    }
    if let (Some(e), Some(last)) = (estimate, full.last())
        && let Some(p) = pct_change(e, last.cost)
    {
        let side = if p >= 0.0 { "above" } else { "below" };
        let _ = writeln!(out, "   The estimate of the code is {:.1}% {side} the last full week", p.abs());
    }
    out
}

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('<', "&lt;").replace('>', "&gt;")
}

/// The weekly billing for a merge request comment: a Mermaid bar chart
/// (rendered by GitHub and GitLab) and the weeks per service.
pub fn markdown(h: &BillingHistory, estimate_monthly: Option<f64>) -> String {
    let cur = h.currency.as_str();
    let mut out = String::new();
    let _ = writeln!(out, "\n#### 📊 Actual billing{}, last {} weeks\n", of_env(h), h.weeks.len());
    let _ = writeln!(
        out,
        "<sub>Resources of the code only, net of credits, from the Cloud Billing export `{}`: {}.</sub>\n",
        md_escape(&h.table),
        scope(h)
    );
    let estimate = estimate_monthly.map(weekly);
    let peak = h.weeks.iter().map(|w| w.cost).chain(estimate).fold(0.0, f64::max);
    let labels: Vec<String> =
        h.weeks.iter().map(|w| if w.in_progress { "now".to_string() } else { short_date(&w.start) }).map(|l| format!("\"{l}\"")).collect();
    let values: Vec<String> = h.weeks.iter().map(|w| format!("{:.2}", w.cost.max(0.0))).collect();
    let _ = writeln!(out, "```mermaid\nxychart-beta\n    title \"Weekly cost ({cur})\"");
    let _ = writeln!(out, "    x-axis [{}]", labels.join(", "));
    let _ = writeln!(out, "    y-axis \"{cur}\" 0 --> {:.0}", nice_ceil(peak));
    let _ = writeln!(out, "    bar [{}]", values.join(", "));
    if let Some(e) = estimate {
        let _ = writeln!(out, "    line [{}]", vec![format!("{e:.2}"); h.weeks.len()].join(", "));
    }
    let _ = writeln!(out, "```\n");
    let full: Vec<&Week> = h.full_weeks().collect();
    let mut facts = vec![format!("Total **{}**", money(cur, h.total))];
    if let [.., prev, last] = full.as_slice() {
        let change = pct_change(last.cost, prev.cost).map(|p| format!(" ({p:+.1}% vs the week before)")).unwrap_or_default();
        facts.push(format!("last full week {}{change}", money(cur, last.cost)));
    }
    if let (Some(m), Some(e)) = (estimate_monthly, estimate) {
        facts.push(format!("estimate of the code {}/month ≈ {}/week (line)", money(cur, m), money(cur, e)));
    }
    let _ = writeln!(out, "{}.\n", facts.join(" · "));
    let _ = writeln!(out, "<details><summary>Weeks by service</summary>\n");
    let headers: Vec<String> = h.services.iter().map(|s| md_escape(s)).collect();
    let _ = writeln!(out, "| Week | Total | {} |", headers.join(" | "));
    let _ = writeln!(out, "|---|---:|{}", "---:|".repeat(h.services.len()));
    for w in &h.weeks {
        let cells: Vec<String> = w.services.values().map(|v| money(cur, *v)).collect();
        let name = if w.in_progress { format!("{} (in progress)", w.start) } else { w.start.clone() };
        let _ = writeln!(out, "| {name} | {} | {} |", money(cur, w.cost), cells.join(" | "));
    }
    let _ = writeln!(out, "\n</details>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::fake_http::{self, Request as HttpRequest};
    use serde_json::json;

    #[test]
    fn dates() {
        let d = parse_date("2026-10-19").unwrap();
        assert_eq!(iso_date(d), "2026-10-19");
        // 2026-10-19 is a Monday, 2026-10-25 a Sunday.
        assert_eq!(monday(d), d);
        assert_eq!(iso_date(monday(d + 6)), "2026-10-19");
        assert_eq!(iso_date(monday(d - 1)), "2026-10-12");
        assert_eq!(iso_date(days_from_civil(2024, 2, 29)), "2024-02-29");
        assert_eq!(short_date("2026-07-20"), "Jul 20");
    }

    #[test]
    fn table_names() {
        assert_eq!(parse_table("p.billing.gcp_billing_export_v1_X"), Some(("p", "billing", "gcp_billing_export_v1_X")));
        assert_eq!(parse_table("example.com:p.d.t"), Some(("example.com:p", "d", "t")));
        assert_eq!(parse_table("d.t"), None);
        assert_eq!(parse_table("p.d.t`; DROP"), None);
    }

    #[test]
    fn amounts() {
        assert_eq!(compact(950.0), "950");
        assert_eq!(compact(1234.0), "1.2k");
        assert_eq!(compact(12_345.0), "12k");
        assert_eq!(compact(1_500_000.0), "1.5M");
        assert_eq!(nice_ceil(1234.0), 1500.0);
        assert_eq!(nice_ceil(800.0), 800.0);
        assert!((weekly(730.0) - 168.0).abs() < 1e-9);
    }

    fn rows() -> Vec<Row> {
        let mut v = vec![];
        let services = ["Compute Engine", "Cloud SQL", "Cloud Storage", "Networking", "BigQuery", "Cloud Run", "Cloud Logging"];
        for (i, week) in ["2026-07-27", "2026-08-03", "2026-10-12", "2026-10-19"].iter().enumerate() {
            for (j, s) in services.iter().enumerate() {
                v.push(Row {
                    week: week.to_string(),
                    service: s.to_string(),
                    cost: 100.0 * (7 - j) as f64 + i as f64,
                    currency: "EUR".to_string(),
                    resource: "a/vm".into(),
                });
            }
        }
        v
    }

    #[test]
    fn weeks_are_aggregated() {
        let today = parse_date("2026-10-21").unwrap();
        let first = monday(today) - 7 * 12;
        assert_eq!(iso_date(first), "2026-07-27");
        let h = aggregate("p.d.t", &stack(), first, today, rows().iter().collect());
        assert_eq!(h.weeks.len(), WEEKS);
        assert_eq!(h.currency, "EUR");
        assert_eq!(h.services, ["Compute Engine", "Cloud SQL", "Cloud Storage", "Networking", "BigQuery", OTHER]);
        assert_eq!(h.weeks[0].start, "2026-07-27");
        assert_eq!(h.weeks[0].services[OTHER], 300.0);
        assert_eq!(h.weeks[2].cost, 0.0);
        assert!(h.weeks[12].in_progress && !h.weeks[11].in_progress);
        assert!((h.total - (2800.0 * 4.0 + 7.0 * 6.0)).abs() < 1e-6);
    }

    #[test]
    fn chart_renders_with_and_without_colors() {
        let today = parse_date("2026-10-21").unwrap();
        let h = aggregate("p.d.t", &stack(), monday(today) - 84, today, rows().iter().collect());
        let plain = table(&h, Some(10_000.0), false, 100);
        assert!(plain.contains("Actual billing, last 13 weeks"), "{plain}");
        assert!(plain.contains("now") && plain.contains("Jul") && plain.contains("Oct"), "{plain}");
        assert!(plain.contains("╌") && plain.contains("estimate of the code: €10,000.00/month"), "{plain}");
        assert!(plain.contains("Compute Engine") && plain.contains("Other services"), "{plain}");
        assert!(plain.contains("2.8k"), "{plain}");
        assert!(!plain.contains('\x1b'));
        let colored = table(&h, None, true, 100);
        assert!(colored.contains("\x1b[38;5;209m█"), "{colored}");
        let md = markdown(&h, Some(10_000.0));
        assert!(md.contains("```mermaid\nxychart-beta") && md.contains("bar [2800.00, 2807.00,") && md.contains("line ["), "{md}");
        assert!(md.contains("| 2026-10-19 (in progress) |"), "{md}");
    }

    #[test]
    fn stacked_cells() {
        // Two segments: 0..12 eighths and 12..20.
        let ends = [(1u8, 12usize), (2, 20)];
        assert_eq!(cell(&ends, 0, true).unwrap(), "\x1b[38;5;1m█\x1b[0m");
        // Row 1: segment 1 fills 4 eighths, segment 2 the rest.
        assert_eq!(cell(&ends, 1, true).unwrap(), "\x1b[38;5;1;48;5;2m▄\x1b[0m");
        assert_eq!(cell(&ends, 2, true).unwrap(), "\x1b[38;5;2m▄\x1b[0m");
        assert_eq!(cell(&ends, 3, true), None);
        assert_eq!(cell(&ends, 2, false).unwrap(), "▄");
    }

    fn bigquery(req: &HttpRequest) -> (u16, String) {
        let body = if req.path.ends_with("/projects/quota/datasets") {
            json!({"datasets": [{"datasetReference": {"datasetId": "app"}}, {"datasetReference": {"datasetId": "billing"}}]})
        } else if req.path.ends_with("/datasets/billing/tables") {
            json!({"tables": [
                {"tableReference": {"tableId": "gcp_billing_export_resource_v1_000000_000000_000000"}},
                {"tableReference": {"tableId": "gcp_billing_export_v1_012345_6789AB_CDEF01"}},
                {"tableReference": {"tableId": "gcp_billing_export_resource_v1_012345_6789AB_CDEF01"}},
            ]})
        } else if req.path.ends_with("/tables") {
            json!({"tables": [{"tableReference": {"tableId": "events"}}]})
        } else if req.path.ends_with("/projects/quota/queries") {
            let q: serde_json::Value = serde_json::from_str(&req.body).unwrap();
            assert!(q["query"].as_str().unwrap().contains("FROM `quota.billing.gcp_billing_export_resource_v1_012345_6789AB_CDEF01`"));
            assert_eq!(q["queryParameters"][1]["parameterValue"]["arrayValues"][0]["value"], "app-prod");
            assert_eq!(q["queryParameters"][2]["parameterValue"]["arrayValues"][0]["value"], "app-prod/vm-a");
            assert_eq!(q["queryParameters"][3]["parameterValue"]["arrayValues"][0]["value"], "app-prod/web-");
            assert_eq!(q["queryParameters"][4]["parameterValue"]["arrayValues"][0]["value"], "app-prod/gke");
            assert!(q["query"].as_str().unwrap().contains("goog-k8s-cluster-name"));
            assert!(q["query"].as_str().unwrap().contains("GROUP BY week, service, resource"));
            // Only the partitions of the weeks read are scanned.
            assert!(q["query"].as_str().unwrap().contains("DATE(_PARTITIONTIME) >= DATE_SUB(@start, INTERVAL 1 DAY)"));
            assert_eq!(q["queryParameters"][0]["parameterValue"]["value"], "2026-07-27");
            json!({"jobComplete": false, "jobReference": {"jobId": "j1", "location": "EU"}})
        } else if req.path.ends_with("/projects/quota/queries/j1") {
            assert_eq!(req.param("location"), Some("EU"));
            // Two pages.
            match req.param("pageToken") {
                None => json!({"jobComplete": true, "pageToken": "p2", "jobReference": {"jobId": "j1", "location": "EU"}, "rows": [
                    {"f": [{"v": "2026-10-12"}, {"v": "Compute Engine"}, {"v": "100"}, {"v": "USD"}, {"v": "app-prod/vm-a"}]},
                ]}),
                Some("p2") => json!({"jobComplete": true, "rows": [
                    {"f": [{"v": "2026-10-12"}, {"v": "Compute Engine"}, {"v": "23.5"}, {"v": "USD"}, {"v": "app-prod/web-"}]},
                    {"f": [{"v": "2026-10-19"}, {"v": "Cloud SQL"}, {"v": "40"}, {"v": "USD"}, {"v": "app-prod/web-"}]},
                ]}),
                Some(t) => panic!("unexpected page {t}"),
            }
        } else {
            return (404, json!({"error": {"message": format!("unexpected {}", req.path)}}).to_string());
        };
        (200, body.to_string())
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn request() -> Request {
        Request {
            table: None,
            billing_account: Some("012345-6789AB-CDEF01".into()),
            stack: Stack {
                names: ["app-prod/vm-a".to_string()].into(),
                prefixes: ["app-prod/web-".to_string()].into(),
                clusters: ["app-prod/gke".to_string()].into(),
                unnamed: 1,
                ..Default::default()
            },
            quota: Some("quota".into()),
            by_env: false,
        }
    }

    #[test]
    fn discovers_the_export_and_reads_weeks() {
        let server = fake_http::serve(bigquery);
        let cache = tempfile::tempdir().unwrap();
        let today = parse_date("2026-10-21").unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let token = async || Some(Auth::Bearer("t0k3n".into()));
        let h = block_on(fetch_from(cache.path(), &request(), until, &api(&server.url), token, today)).unwrap().total;
        assert_eq!(h.table, "quota.billing.gcp_billing_export_resource_v1_012345_6789AB_CDEF01");
        assert_eq!(h.services, ["Compute Engine", "Cloud SQL"]);
        assert_eq!((h.resources, h.unnamed, h.not_found.as_slice()), (3, 1, ["app-prod/gke".to_string()].as_slice()));
        assert_eq!(h.weeks[11].cost, 123.5);
        assert!(h.weeks[12].in_progress && h.weeks[12].cost == 40.0);
        let calls = server.count();
        // Cached: no new request, not even for credentials.
        let again = block_on(fetch_from(cache.path(), &request(), until, &api(&server.url), async || None, today)).unwrap().total;
        assert_eq!(again, h);
        assert_eq!(server.count(), calls);
    }

    #[test]
    fn only_the_weeks_not_final_are_read_again() {
        let starts = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let log = starts.clone();
        let server = fake_http::serve(move |r| {
            let q: serde_json::Value = serde_json::from_str(&r.body).unwrap();
            let start = q["queryParameters"][0]["parameterValue"]["value"].as_str().unwrap().to_string();
            log.lock().unwrap().push(start.clone());
            // vm-b is billed too, but only some runs look for it.
            (
                200,
                json!({"jobComplete": true, "rows": [
                    {"f": [{"v": start.clone()}, {"v": "Compute Engine"}, {"v": "5"}, {"v": "USD"}, {"v": "app-prod/vm-a"}]},
                    {"f": [{"v": start}, {"v": "Compute Engine"}, {"v": "1000"}, {"v": "USD"}, {"v": "app-prod/vm-b"}]}
                ]})
                .to_string(),
            )
        });
        let cache = tempfile::tempdir().unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let token = async || Some(Auth::Bearer("t0k3n".into()));
        let req = Request { table: Some("p.d.t".into()), ..request() };
        // Weeks long over: read now, they are final.
        let today = parse_date("2026-03-04").unwrap();
        let b = block_on(fetch_from(cache.path(), &req, until, &api(&server.url), token, today)).unwrap();
        assert_eq!(b.total.weeks[0].cost, 5.0);
        // A week later: only the new week is read, the others come from the cache.
        let b = block_on(fetch_from(cache.path(), &req, until, &api(&server.url), token, today + 7)).unwrap();
        assert_eq!(*starts.lock().unwrap(), [iso_date(monday(today) - 84), iso_date(monday(today + 7))]);
        assert_eq!((b.total.weeks[12].cost, b.total.total), (5.0, 5.0));
        // Another resource: not in the cached weeks, all read again.
        let more = Request {
            stack: Stack { names: ["app-prod/vm-a".to_string(), "app-prod/vm-b".to_string()].into(), ..req.stack.clone() },
            ..req.clone()
        };
        block_on(fetch_from(cache.path(), &more, until, &api(&server.url), token, today + 7)).unwrap();
        assert_eq!(starts.lock().unwrap().last(), Some(&iso_date(monday(today + 7) - 84)));
        // The resources of the code of the previous run are among them: no
        // query, and the costs of the other resource are left out.
        let b = block_on(fetch_from(cache.path(), &req, until, &api(&server.url), async || None, today + 7)).unwrap();
        assert_eq!(starts.lock().unwrap().len(), 3);
        assert_eq!(b.total.total, 5.0);
    }

    #[test]
    fn project_of_a_service_account() {
        assert_eq!(project_of_service_account("ci@ci-prj.iam.gserviceaccount.com").as_deref(), Some("ci-prj"));
        assert_eq!(project_of_service_account("my-app@appspot.gserviceaccount.com").as_deref(), Some("my-app"));
        assert_eq!(project_of_service_account("123-compute@developer.gserviceaccount.com"), None);
        assert_eq!(project_of_service_account("me@example.com"), None);
    }

    #[test]
    fn the_query_runs_in_the_project_of_the_service_account() {
        let table = "bill-prj.billing.gcp_billing_export_resource_v1_012345_6789AB_CDEF01";
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let token = async || Some(Auth::Bearer("t0k3n".into()));
        let done = json!({"jobComplete": true, "rows": [{"f": [{"v": "2026-10-12"}, {"v": "Compute Engine"}, {"v": "7"}, {"v": "USD"}, {"v": "app-prod/vm-a"}]}]});
        let req = Request { table: Some(table.into()), quota: None, ..request() };
        // Accepted in the project of the service account, refused in it (not
        // tried again elsewhere: each try is billed), or a user.
        for (email, sa_accepts, ran_in) in [
            ("ci@ci-prj.iam.gserviceaccount.com", true, "ci-prj"),
            ("ci@ci-prj.iam.gserviceaccount.com", false, "ci-prj"),
            ("me@example.com", true, "bill-prj"),
        ] {
            let done = done.clone();
            let ran = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
            let log = ran.clone();
            let server = fake_http::serve(move |r| {
                if r.path.ends_with("/tokeninfo") {
                    assert_eq!(r.param("access_token"), Some("t0k3n"));
                    return (200, json!({"email": email}).to_string());
                }
                let project = r.path.split('/').nth_back(1).unwrap_or_default().to_string();
                log.lock().unwrap().push(project.clone());
                if project == "ci-prj" && !sa_accepts {
                    return (403, json!({"error": {"message": "User does not have bigquery.jobs.create permission"}}).to_string());
                }
                (200, done.to_string())
            });
            let cache = tempfile::tempdir().unwrap();
            let r = block_on(fetch_from(cache.path(), &req, until, &api(&server.url), token, parse_date("2026-10-21").unwrap()));
            match r {
                Ok(b) => assert_eq!(b.total.weeks[11].cost, 7.0),
                Err(e) => assert!(!sa_accepts && e.contains("(query run in ci-prj): permission denied"), "{e}"),
            }
            assert_eq!(*ran.lock().unwrap(), [ran_in], "{email} {sa_accepts}");
        }
    }

    #[test]
    fn problems_are_explained() {
        let server = fake_http::serve(|_| (403, json!({"error": {"message": "Access Denied: Table p:d.t"}}).to_string()));
        let cache = tempfile::tempdir().unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let token = async || Some(Auth::Bearer("t0k3n".into()));
        let req = Request { table: Some("p.d.t".into()), ..request() };
        let e = block_on(fetch_from(cache.path(), &req, until, &api(&server.url), token, 20_000)).unwrap_err();
        assert!(
            e.contains("billing export p.d.t (query run in quota): permission denied (Access Denied: Table p:d.t)")
                && e.contains("roles/bigquery.jobUser"),
            "{e}"
        );
        let quota = fake_http::serve(|_| {
            let m = "Custom quota exceeded: Your usage exceeded the custom quota for QueryUsagePerUserPerDay";
            (403, json!({"error": {"message": m, "errors": [{"reason": "quotaExceeded"}]}}).to_string())
        });
        let e = block_on(fetch_from(cache.path(), &req, until, &api(&quota.url), token, 20_000)).unwrap_err();
        assert!(e.contains("(query run in quota): BigQuery quota exceeded (Custom quota exceeded") && e.contains("--quota-project"), "{e}");
        assert_eq!(quota.count(), 1, "not tried again");
        let none = Request { stack: Stack::default(), ..request() };
        assert!(
            block_on(fetch_from(cache.path(), &none, until, &api(&server.url), token, 20_000))
                .unwrap_err()
                .contains("no resource of the code")
        );
        let bad = Request { table: Some("nope".into()), ..request() };
        assert!(
            block_on(fetch_from(cache.path(), &bad, until, &api(&server.url), token, 20_000))
                .unwrap_err()
                .contains("invalid billing export table")
        );
        let empty = fake_http::serve(|req| {
            let body = if req.path.ends_with(":getAncestry") {
                json!({"ancestor": [{"resourceId": {"type": "project", "id": "app-prod"}}, {"resourceId": {"type": "organization", "id": "42"}}]})
            } else if req.path.ends_with(":searchAllResources") {
                return (403, json!({"error": {"message": "denied"}}).to_string());
            } else if req.path.ends_with("/bigquery/v2/projects") {
                json!({"projects": [{"projectReference": {"projectId": "secret"}}]})
            } else if req.path.ends_with("/projects/secret/datasets") {
                return (403, json!({"error": {"message": "denied"}}).to_string());
            } else if req.path.ends_with("/datasets") {
                json!({"datasets": []})
            } else {
                json!({})
            };
            (200, body.to_string())
        });
        let e = block_on(fetch_from(cache.path(), &request(), until, &api(&empty.url), token, 20_000)).unwrap_err();
        assert!(
            e.contains("no detailed Cloud Billing export table (gcp_billing_export_resource_v1_…) found: organization 42 could not be searched with Cloud Asset Inventory (permission denied: grant roles/cloudasset.viewer on the organization); the datasets of 3 BigQuery projects, 1 refused"),
            "{e}"
        );
        // The miss is remembered: no new search.
        let calls = empty.count();
        let again = block_on(fetch_from(cache.path(), &request(), until, &api(&empty.url), token, 20_000)).unwrap_err();
        assert_eq!((again, empty.count()), (e, calls));
    }

    #[test]
    fn finds_the_export_in_the_organization() {
        let server = fake_http::serve(|req| {
            let body = if req.path.ends_with("/v1/projects/app-prod:getAncestry") {
                json!({"ancestor": [{"resourceId": {"type": "folder", "id": "7"}}, {"resourceId": {"type": "organization", "id": "42"}}]})
            } else if req.path.ends_with("/v1/organizations/42:searchAllResources") {
                assert_eq!(req.param("query"), Some("displayName=gcp_billing_export_resource_v1_012345_6789AB_CDEF01"));
                assert_eq!(req.param("assetTypes"), Some("bigquery.googleapis.com/Table"));
                json!({"results": [{"name": "//bigquery.googleapis.com/projects/acme-finops/datasets/billing/tables/gcp_billing_export_resource_v1_012345_6789AB_CDEF01"}]})
            } else if req.path.ends_with("/queries") {
                assert!(
                    req.body.contains("FROM `acme-finops.billing.gcp_billing_export_resource_v1_012345_6789AB_CDEF01`"),
                    "{}",
                    req.body
                );
                json!({"jobComplete": true, "rows": [{"f": [{"v": "2026-10-12"}, {"v": "Cloud SQL"}, {"v": "10"}, {"v": "EUR"}, {"v": "app-prod/vm-a"}]}]})
            } else {
                return (404, json!({"error": {"message": format!("unexpected {}", req.path)}}).to_string());
            };
            (200, body.to_string())
        });
        let cache = tempfile::tempdir().unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let token = async || Some(Auth::Bearer("t0k3n".into()));
        let h = block_on(fetch_from(cache.path(), &request(), until, &api(&server.url), token, parse_date("2026-10-21").unwrap()))
            .unwrap()
            .total;
        assert_eq!(h.table, "acme-finops.billing.gcp_billing_export_resource_v1_012345_6789AB_CDEF01");
        assert_eq!(h.currency, "EUR");
    }

    #[test]
    fn scans_the_bigquery_projects_named_like_billing_first() {
        let server = fake_http::serve(|req| {
            let body = if req.path.ends_with("/bigquery/v2/projects") {
                json!({"projects": [{"projectReference": {"projectId": "web"}}, {"projectReference": {"projectId": "acme-finops"}}]})
            } else if req.path.ends_with("/projects/acme-finops/datasets") {
                json!({"datasets": [{"datasetReference": {"datasetId": "costs"}}]})
            } else if req.path.ends_with("/projects/acme-finops/datasets/costs/tables") {
                json!({"tables": [{"tableReference": {"tableId": "gcp_billing_export_resource_v1_012345_6789AB_CDEF01"}}]})
            } else if req.path.ends_with("/datasets") {
                json!({"datasets": []})
            } else if req.path.ends_with("/queries") {
                json!({"jobComplete": true, "rows": []})
            } else {
                return (404, json!({"error": {"message": "not found"}}).to_string());
            };
            (200, body.to_string())
        });
        let cache = tempfile::tempdir().unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let token = async || Some(Auth::Bearer("t0k3n".into()));
        let h = block_on(fetch_from(cache.path(), &request(), until, &api(&server.url), token, 20_000)).unwrap().total;
        assert_eq!(h.table, "acme-finops.costs.gcp_billing_export_resource_v1_012345_6789AB_CDEF01");
    }

    fn stack() -> Stack {
        Stack { names: ["a/vm".to_string()].into(), ..Default::default() }
    }

    #[test]
    fn one_history_per_environment() {
        let today = parse_date("2026-10-21").unwrap();
        let first = monday(today) - 84;
        let row = |resource: &str, cost: f64| Row {
            week: "2026-10-12".into(),
            service: "Compute Engine".into(),
            cost,
            currency: "EUR".into(),
            resource: resource.into(),
        };
        let rows = [row("dev-prj/vm", 10.0), row("prd-prj/web-", 90.0)];
        let mut s = Stack {
            names: ["dev-prj/vm".to_string()].into(),
            prefixes: ["prd-prj/web-".to_string()].into(),
            envs: [("dev-prj/vm".to_string(), "dev".to_string()), ("prd-prj/web-".to_string(), "prod".to_string())].into(),
            unnamed: 1,
            unnamed_by_env: [("prod".to_string(), 1)].into(),
            code_projects: [
                ("dev".to_string(), ["live/dev/app".to_string()].into()),
                ("prod".to_string(), ["live/prod/app".to_string()].into()),
            ]
            .into(),
            ..Default::default()
        };
        let b = billing("p.d.t", &s, true, first, today, &rows);
        assert_eq!(b.total.weeks[11].cost, 100.0);
        let envs: Vec<_> = b
            .charts()
            .iter()
            .map(|h| (h.env.as_deref(), h.weeks[11].cost, h.unnamed, h.code_projects.clone(), h.not_found.len()))
            .collect();
        assert_eq!(
            envs,
            [(Some("dev"), 10.0, 0, vec!["live/dev/app".to_string()], 0), (Some("prod"), 90.0, 1, vec!["live/prod/app".to_string()], 0)]
        );
        assert!(table(&b.by_env[1], None, false, 100).contains("Actual billing of prod, last 13 weeks"));
        // Not asked for: the total only.
        assert!(billing("p.d.t", &s, false, first, today, &rows).by_env.is_empty());
        // One environment: the total only.
        s.envs.insert("dev-prj/vm".into(), "prod".into());
        let b = billing("p.d.t", &s, true, first, today, &rows);
        assert!(b.by_env.is_empty());
        assert_eq!(b.charts(), std::slice::from_ref(&b.total));
    }

    #[test]
    fn environment_of_a_project() {
        let config = crate::config::Config::default();
        let p = |name: &str| Project {
            name: name.into(),
            path: PathBuf::new(),
            kind: crate::project::ProjectKind::Terraform,
            resources: vec![],
            prior: None,
            external: vec![],
            warnings: vec![],
        };
        assert_eq!(env_name(&p("live/dev/app"), &config).as_deref(), Some("dev"));
        assert_eq!(env_name(&p("network (staging)"), &config).as_deref(), Some("staging"));
        assert_eq!(env_name(&p("live/prod/app"), &config).as_deref(), Some("prod"));
        assert_eq!(env_name(&p("app"), &config), None);
        let rules = crate::config::Config {
            projects: vec![crate::config::ProjectRule { pattern: "app".into(), env: Some("qa".into()) }],
            ..Default::default()
        };
        assert_eq!(env_name(&p("app"), &rules).as_deref(), Some("qa"));
    }

    #[test]
    fn the_stack_is_named_as_in_the_billing() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
provider "google" { project = "app-prod" }
variable "suffix" {}
resource "google_compute_instance" "vm" {
  name         = "vm-a"
  machine_type = "e2-small"
  zone         = "europe-west1-b"
}
resource "google_storage_bucket" "logs" {
  name     = "acme-logs"
  location = "EU"
}
resource "google_compute_region_instance_group_manager" "web" {
  name               = "web"
  base_instance_name = "web"
  region             = "europe-west1"
}
resource "google_container_cluster" "gke" {
  name     = "gke"
  location = "europe-west1"
}
resource "google_container_node_pool" "pool" {
  name    = "pool"
  cluster = google_container_cluster.gke.id
}
resource "google_bigquery_dataset" "events" {
  dataset_id = "events"
}
resource "google_storage_bucket" "tmp" {
  name     = "tmp-${var.suffix}"
  location = "EU"
}
resource "google_project_iam_member" "x" {
  project = "app-prod"
  role    = "roles/viewer"
  member  = "user:a@example.com"
}
"#;
        std::fs::write(dir.path().join("main.tf"), src).unwrap();
        let opts = crate::project::LoadOptions { offline: true, cache_dir: dir.path().join(".cache"), ..Default::default() };
        let projects = crate::project::load(dir.path(), &opts).unwrap();
        let s = stack_of(&[&projects], |_| None);
        let set = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<BTreeSet<_>>();
        assert_eq!(s.names, set(&["app-prod/acme-logs", "app-prod/events", "app-prod/vm-a"]));
        assert_eq!(s.prefixes, set(&["app-prod/web-"]));
        assert_eq!(s.clusters.iter().next().map(String::as_str), Some("app-prod/gke"), "{s:?}");
        assert_eq!(s.unnamed, 1, "{s:?}");
        assert_eq!(s.projects(), ["app-prod"]);
        assert_eq!(s.resource_of("app-prod/web-x1z2").as_deref(), Some("app-prod/web-"));
        assert_eq!(s.resource_of("app-prod/other"), None);
    }

    /// Workstations are found by the `goog-drz-workstation-uuid` label of
    /// their VMs and disks (named after other ids), with their `uid`.
    #[test]
    fn workstations_are_matched_by_their_uid_label() {
        let server = fake_http::serve(|r| {
            let q: serde_json::Value = serde_json::from_str(&r.body).unwrap();
            let sql = q["query"].as_str().unwrap();
            assert!(sql.contains("'goog-drz-workstation-uuid'") && !sql.contains("goog-k8s-cluster-name"), "{sql}");
            assert_eq!(q["queryParameters"][4]["parameterValue"]["arrayValues"][0]["value"], "ws-prod/1e87c094");
            let start = q["queryParameters"][0]["parameterValue"]["value"].as_str().unwrap().to_string();
            (
                200,
                json!({"jobComplete": true, "rows": [
                    {"f": [{"v": start}, {"v": "Compute Engine"}, {"v": "28"}, {"v": "USD"}, {"v": "ws-prod/1e87c094"}]}
                ]})
                .to_string(),
            )
        });
        let cache = tempfile::tempdir().unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let req = Request {
            table: Some("p.d.t".into()),
            stack: Stack { workstations: ["ws-prod/1e87c094".to_string()].into(), ..Default::default() },
            ..request()
        };
        let token = async || Some(Auth::Bearer("t0k3n".into()));
        let h = block_on(fetch_from(cache.path(), &req, until, &api(&server.url), token, parse_date("2026-10-21").unwrap())).unwrap().total;
        assert_eq!((h.resources, h.not_found.len(), h.total), (1, 0, 28.0));
    }

    /// A query still running when costerra stops waiting is billed anyway:
    /// the next run reads its result instead of running it again.
    #[test]
    fn a_query_still_running_is_read_by_the_next_run() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let posts = std::sync::Arc::new(AtomicUsize::new(0));
        let (d, n) = (done.clone(), posts.clone());
        let server = fake_http::serve(move |r| {
            let reference = json!({"jobId": "slow", "location": "EU"});
            if r.path.ends_with("/queries") {
                n.fetch_add(1, Ordering::SeqCst);
                return (200, json!({"jobComplete": false, "jobReference": reference}).to_string());
            }
            assert!(r.path.ends_with("/projects/quota/queries/slow"), "{}", r.path);
            if !d.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(50));
                return (200, json!({"jobComplete": false, "jobReference": reference}).to_string());
            }
            (
                200,
                json!({"jobComplete": true, "jobReference": reference, "rows": [
                    {"f": [{"v": "2026-10-19"}, {"v": "Compute Engine"}, {"v": "7"}, {"v": "USD"}, {"v": "app-prod/vm-a"}]}
                ]})
                .to_string(),
            )
        });
        let cache = tempfile::tempdir().unwrap();
        let today = parse_date("2026-10-21").unwrap();
        let req = Request { table: Some("p.d.t".into()), ..request() };
        let token = async || Some(Auth::Bearer("t0k3n".into()));
        let soon = std::time::Instant::now() + std::time::Duration::from_millis(400);
        let e = block_on(fetch_from(cache.path(), &req, soon, &api(&server.url), token, today)).unwrap_err();
        assert!(e.contains("still running") && e.contains("next run"), "{e}");
        // Done meanwhile: read without a new query.
        done.store(true, Ordering::SeqCst);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let b = block_on(fetch_from(cache.path(), &req, until, &api(&server.url), token, today)).unwrap();
        assert_eq!((posts.load(Ordering::SeqCst), b.total.total), (1, 7.0));
        // Then from the cache of weeks.
        let again = block_on(fetch_from(cache.path(), &req, until, &api(&server.url), token, today)).unwrap();
        assert_eq!((posts.load(Ordering::SeqCst), again.total.total), (1, 7.0));
    }

    fn api(url: &str) -> Endpoints {
        Endpoints { bigquery: url.into(), asset: url.into(), resource_manager: url.into(), oauth2: url.into() }
    }
}
