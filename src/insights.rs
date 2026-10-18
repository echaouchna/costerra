//! External optimization insights from Google Cloud Recommender, based on
//! observed usage (rightsizing, idle resources, commitments…).
//!
//! Results are cached (6 h) so that runs stay instant, and matched to the
//! resources of the analysed code when possible.

use crate::cost::{Basis, Recommendation, Report, Severity};
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

/// Merge new entries into the cache file. Other costerra processes may update
/// it concurrently: the file is re-read under a lock and written atomically,
/// so no entry is lost.
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
    let auth = async || match resolve_auth(None).await {
        Ok(a @ Auth::Bearer(_)) => Some(a),
        _ => None,
    };
    fetch_gcp_from(cache_dir, queries, quota, until, "https://recommender.googleapis.com", auth).await
}

/// `fetch_gcp` with a given API base URL and credentials (resolved only when
/// something is not cached).
async fn fetch_gcp_from(
    cache_dir: &Path,
    queries: &[Query],
    quota: Option<&str>,
    until: std::time::Instant,
    api: &str,
    auth: impl AsyncFnOnce() -> Option<Auth>,
) -> Fetched {
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
    let Some(auth) = auth().await else { return Fetched { insights, notes } };
    let Ok(client) = reqwest::Client::builder().user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION"))).gzip(true).build() else {
        return Fetched { insights, notes };
    };
    let quota = quota.map(str::to_string);
    let until = tokio::time::Instant::from_std(until);
    let results: Vec<(Query, Result<Vec<Insight>, String>)> = futures::stream::iter(todo.into_iter().map(|q| {
        let (client, auth, quota) = (&client, &auth, quota.clone());
        async move {
            let url = format!("{api}/v1/{}/locations/{}/recommenders/{}/recommendations", q.parent, q.location, q.recommender);
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

fn severity(priority: Option<&str>) -> Severity {
    match priority.unwrap_or("") {
        "P1" => Severity::High,
        "P2" => Severity::Medium,
        "P3" => Severity::Low,
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
    // (project, address, recommender) of Google recommendations matched to the code.
    let mut superseded: Vec<(String, String, String)> = vec![];
    // Regions where Google recommends commitments for the billing account.
    let mut commitment_regions: BTreeSet<String> = BTreeSet::new();
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
                            severity: severity(ins.priority.as_deref()),
                            address: r.address.clone(),
                            title: ins.title.clone(),
                            detail: ins.detail.clone().unwrap_or_default(),
                            monthly_savings: ins.monthly_savings,
                            basis: Basis::Observed,
                        });
                    }
                    matched = true;
                    superseded.push((projects[pi].name.clone(), r.address.clone(), ins.kind.clone()));
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
                severity: severity(ins.priority.as_deref()),
                address,
                title: ins.title.clone(),
                detail: ins.detail.clone().unwrap_or_default(),
                monthly_savings: ins.monthly_savings,
                basis: Basis::Observed,
            });
            if ins.kind.contains("commitment")
                && let Some(l) = &ins.location
            {
                commitment_regions.insert(l.clone());
            }
        }
    }
    drop_superseded(report, &superseded, &commitment_regions);
    report.potential_savings =
        crate::cost::potential_savings(report.projects.iter().flat_map(|p| p.recommendations.iter().map(move |r| (p.name.as_str(), r))));
    report.external_recommendations.sort_by(|a, b| b.monthly_savings.unwrap_or(0.0).total_cmp(&a.monthly_savings.unwrap_or(0.0)));
    report.insight_notes.extend(notes);
}

/// costerra rules that a Google recommendation of this recommender replaces
/// on the same resource: Google's is based on observed usage.
fn supersedes(recommender: &str) -> &'static [&'static str] {
    match recommender {
        "compute-instance-machinetyperecommender" => &["compute-machine-family", "compute-rightsize"],
        "compute-instance-idleresourcerecommender" => {
            &["compute-machine-family", "compute-rightsize", "compute-idle", "compute-spot-nonprod", "compute-cud"]
        }
        "compute-instancegroupmanager-machinetyperecommender" => &["compute-machine-family"],
        "compute-disk-idleresourcerecommender" => &["disk-unattached", "disk-ssd-to-balanced"],
        "compute-address-idleresourcerecommender" => &["ip-unused"],
        "cloudsql-instance-idlerecommender" => &["sql-cud", "sql-ha-nonprod", "sql-replica-nonprod"],
        _ => &[],
    }
}

/// Whether a (possibly grouped) recommendation address designates a resource.
fn same_resource(rec_address: &str, address: &str) -> bool {
    let group = crate::engine::address_group(address);
    rec_address == address || rec_address == group || rec_address.starts_with(&format!("{group} ("))
}

/// Drop costerra recommendations that duplicate Google's for the same
/// resource, and costerra commitment advice where Google already recommends
/// commitments for the billing account (computed from actual usage).
fn drop_superseded(report: &mut Report, superseded: &[(String, String, String)], commitment_regions: &BTreeSet<String>) {
    for pr in report.projects.iter_mut() {
        let regions: std::collections::HashMap<&str, &str> =
            pr.resources.iter().filter_map(|r| r.region.as_deref().map(|rg| (r.address.as_str(), rg))).collect();
        pr.recommendations.retain(|rec| {
            if rec.source != "costerra" {
                return true;
            }
            let replaced = superseded
                .iter()
                .any(|(p, a, kind)| *p == pr.name && supersedes(kind).contains(&rec.id.as_str()) && same_resource(&rec.address, a));
            let covered =
                rec.id == "compute-cud" && regions.iter().any(|(a, rg)| commitment_regions.contains(*rg) && same_resource(&rec.address, a));
            !replaced && !covered
        });
    }
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

    fn rec(id: &str, source: &str, address: &str) -> Recommendation {
        Recommendation {
            id: id.into(),
            source: source.into(),
            source_ref: None,
            severity: Severity::Low,
            address: address.into(),
            title: id.into(),
            detail: String::new(),
            monthly_savings: Some(10.0),
            basis: Basis::Code,
        }
    }

    #[test]
    fn google_supersedes_costerra_on_the_same_resource() {
        let mut report: Report = serde_json::from_value(serde_json::json!({
            "version": "", "currency": "USD", "pricing_date": null, "generated_at": 0, "monthly_cost": 0.0,
            "potential_savings": 0.0, "free_tiers": [], "free_tier_included": false,
            "summary": {"priced": 0, "free": 0, "unsupported": 0, "unsupported_types": [], "usage_based_without_usage": 0},
            "projects": [{"name": "p", "path": "", "kind": "terraform", "monthly_cost": 0.0, "recommendations": [], "warnings": [],
                "resources": [{"address": "google_compute_instance.vm[0]", "type": "google_compute_instance", "region": "europe-west1",
                    "status": "priced", "monthly_cost": 0.0, "has_unpriced_usage": false, "components": [], "warnings": []}]}]
        }))
        .unwrap();
        report.projects[0].recommendations = vec![
            rec("compute-machine-family", "costerra", "google_compute_instance.vm[*] (2 instances)"),
            rec("compute-spot-nonprod", "costerra", "google_compute_instance.vm[*] (2 instances)"),
            rec("compute-cud", "costerra", "google_compute_instance.vm[0]"),
            rec("ip-unused", "costerra", "google_compute_address.ip"),
        ];
        let superseded =
            vec![("p".to_string(), "google_compute_instance.vm[0]".to_string(), "compute-instance-machinetyperecommender".to_string())];
        let regions: BTreeSet<String> = ["europe-west1".to_string()].into_iter().collect();
        drop_superseded(&mut report, &superseded, &regions);
        let ids: Vec<&str> = report.projects[0].recommendations.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["compute-spot-nonprod", "ip-unused"]);
    }

    // ---- Recommender API, against a local fake server ----------------------

    use crate::util::fake_http;
    use serde_json::json;

    fn fetch(server: &fake_http::Server, cache: &Path, queries: &[Query], auth: Option<Auth>) -> Fetched {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(fetch_gcp_from(
            cache,
            queries,
            Some("quota-proj"),
            until,
            &server.url,
            async || auth,
        ))
    }

    fn token() -> Option<Auth> {
        Some(Auth::Bearer("t0k3n".into()))
    }

    fn machine_type_query(project: &str) -> Query {
        Query {
            parent: format!("projects/{project}"),
            location: "europe-west1-b".into(),
            recommender: "google.compute.instance.MachineTypeRecommender",
        }
    }

    fn recommendation(id: &str, vm: &str) -> serde_json::Value {
        json!({
            "name": format!("projects/p/locations/europe-west1-b/recommenders/google.compute.instance.MachineTypeRecommender/recommendations/{id}"),
            "description": format!("Save cost by changing machine type of {vm}."),
            "priority": "P2",
            "stateInfo": {"state": "ACTIVE"},
            "primaryImpact": {"category": "COST", "costProjection": {"cost": {"units": "-30", "nanos": 0}, "duration": "2592000s"}},
            "content": {"operationGroups": [{"operations": [{"resource": format!("//compute.googleapis.com/projects/p/zones/europe-west1-b/instances/{vm}")}]}]}
        })
    }

    #[test]
    fn recommender_pages_are_read_and_cached() {
        let server = fake_http::serve(|req| {
            let body = match req.param("pageToken") {
                None => json!({"recommendations": [recommendation("1", "vm-a")], "nextPageToken": "p2"}),
                Some(_) => json!({"recommendations": [recommendation("2", "vm-b")]}),
            };
            (200, body.to_string())
        });
        let cache = tempfile::tempdir().unwrap();
        let q = [machine_type_query("p")];
        let f = fetch(&server, cache.path(), &q, token());
        assert!(f.notes.is_empty());
        assert_eq!(f.insights.len(), 2);
        assert_eq!(f.insights[0].source, "gcp");
        assert_eq!(f.insights[0].kind, "compute-instance-machinetyperecommender");
        assert!(f.insights[0].reference.as_deref().unwrap().contains("--billing-project=quota-proj"));
        let reqs = server.requests.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2);
        assert_eq!(
            reqs[0].path,
            "/v1/projects/p/locations/europe-west1-b/recommenders/google.compute.instance.MachineTypeRecommender/recommendations"
        );
        assert_eq!(reqs[0].header("authorization"), Some("Bearer t0k3n"));
        assert_eq!(reqs[0].header("x-goog-user-project"), Some("quota-proj"));
        // Cached for the next run.
        let again = fetch(&server, cache.path(), &q, None);
        assert_eq!(again.insights.len(), 2);
        assert_eq!(server.count(), 2);
    }

    #[test]
    fn disabled_api_is_a_note_and_denied_projects_are_cached_empty() {
        let server = fake_http::serve(|req| {
            if req.path.contains("/projects/disabled/") {
                (403, json!({"error": {"message": "Recommender API has not been used in project 1 before or it is disabled.", "status": "PERMISSION_DENIED"}}).to_string())
            } else {
                (403, json!({"error": {"message": "Permission denied", "status": "PERMISSION_DENIED"}}).to_string())
            }
        });
        let cache = tempfile::tempdir().unwrap();
        let q = [machine_type_query("disabled"), machine_type_query("denied")];
        let f = fetch(&server, cache.path(), &q, token());
        assert!(f.insights.is_empty());
        assert_eq!(f.notes.len(), 1);
        assert!(f.notes[0].contains("set a quota project with the Recommender API enabled"));
        // The denied project is not asked again; the disabled one is.
        fetch(&server, cache.path(), &q, token());
        assert_eq!(server.count(), 3);
    }

    #[test]
    fn recommender_needs_oauth() {
        let server = fake_http::serve(|_| (200, "{}".into()));
        let cache = tempfile::tempdir().unwrap();
        let f = fetch(&server, cache.path(), &[machine_type_query("p")], None);
        assert!(f.insights.is_empty() && f.notes.is_empty());
        assert_eq!(server.count(), 0);
    }

    // ---- End to end on testdata/terraform/basic ----------------------------

    fn basic() -> (Vec<Project>, Report) {
        use crate::pricing::catalog::{Catalog, CatalogFile};
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let opts =
            crate::project::LoadOptions { offline: true, cache_dir: std::env::temp_dir().join("costerra-insights"), ..Default::default() };
        let projects = crate::project::load(&root.join("terraform/basic"), &opts).unwrap();
        let f: CatalogFile = serde_json::from_str(&std::fs::read_to_string(root.join("pricing/golden-catalog.json")).unwrap()).unwrap();
        let pricer = crate::pricing::query::Pricer::new(Some(Catalog::from_file(f)), false);
        let (config, usage) = (crate::config::Config::default(), crate::usage::UsageFile::default());
        let engine = crate::engine::Engine {
            pricer: &pricer,
            list_pricer: None,
            usage_profile: None,
            examples: false,
            usage: &usage,
            config: &config,
            recommendations: true,
            observed: None,
        };
        let report = engine.report(&projects);
        (projects, report)
    }

    #[test]
    fn queries_follow_the_resources_of_the_code() {
        let (projects, _) = basic();
        let q = gcp_queries(&[&projects], Some("B-A-1"));
        let has = |parent: &str, location: &str, rec: &str| {
            q.iter().any(|x| x.parent == parent && x.location == location && x.recommender == rec)
        };
        assert!(has("projects/example-dev", "europe-west1-b", "google.compute.instance.MachineTypeRecommender"));
        assert!(has("projects/example-dev", "europe-west1-b", "google.compute.disk.IdleResourceRecommender"));
        assert!(has("projects/example-dev", "europe-west1", "google.compute.address.IdleResourceRecommender"));
        assert!(has("billingAccounts/B-A-1", "europe-west1", "google.compute.commitment.UsageCommitmentRecommender"));
        assert!(has("billingAccounts/B-A-1", "global", "google.cloudbilling.commitment.SpendBasedCommitmentRecommender"));
        assert!(!gcp_queries(&[&projects], None).iter().any(|x| x.parent.starts_with("billingAccounts/")));
        assert_eq!(quota_project(Some("explicit")).as_deref(), Some("explicit"));
    }

    fn insight(kind: &str, title: &str, resource: &str, location: Option<&str>, savings: f64) -> Insight {
        Insight {
            source: "gcp".into(),
            kind: kind.into(),
            title: title.into(),
            detail: None,
            monthly_savings: Some(savings),
            priority: Some("P2".into()),
            resources: vec![resource.into()],
            location: location.map(str::to_string),
            reference: None,
        }
    }

    #[test]
    fn attach_matches_merges_and_replaces() {
        let (projects, mut report) = basic();
        let api = "module.vm[\"api\"].google_compute_instance.this";
        let ids = |r: &Report| -> Vec<String> {
            r.projects[0].recommendations.iter().filter(|x| x.address == api).map(|x| format!("{}:{}", x.source, x.id)).collect()
        };
        assert!(ids(&report).contains(&"costerra:compute-spot-nonprod".to_string()), "{:?}", ids(&report));
        let idle = insight(
            "compute-instance-idleresourcerecommender",
            "Save cost by stopping idle VM 'api'",
            "//compute.googleapis.com/projects/example-dev/zones/europe-west1-b/instances/api",
            Some("europe-west1-b"),
            40.0,
        );
        let ba = "//cloudbilling.googleapis.com/billingAccounts/B-A-1";
        let insights = vec![
            idle.clone(),
            idle,
            insight(
                "compute-commitment-usagecommitmentrecommender",
                "Purchase a 1 year new commitment for N1 cores",
                ba,
                Some("europe-west1"),
                50.0,
            ),
            insight(
                "compute-commitment-usagecommitmentrecommender",
                "Purchase a 3 year new commitment for N1 cores",
                ba,
                Some("europe-west1"),
                90.0,
            ),
            insight(
                "compute-commitment-usagecommitmentrecommender",
                "Purchase a 3 year new commitment for N1 cores",
                ba,
                Some("europe-west1"),
                70.0,
            ),
        ];
        attach(&mut report, &projects, insights, vec!["a note".into()]);

        // Matched by GCP project, zone and name; the duplicate is dropped and
        // costerra's advice for that VM is replaced, except for its disk.
        let got = ids(&report);
        assert_eq!(got, vec!["costerra:disk-ssd-to-balanced", "gcp:gcp-compute-instance-idleresourcerecommender"]);
        let g = report.projects[0].recommendations.iter().find(|x| x.source == "gcp").unwrap();
        assert_eq!((g.basis, g.severity), (Basis::Observed, Severity::Medium));

        // Commitment offers for the billing account are merged into one.
        assert_eq!(report.external_recommendations.len(), 1);
        let c = &report.external_recommendations[0];
        assert_eq!(c.title, "Commit to N1 cores");
        assert_eq!(c.monthly_savings, Some(90.0));
        assert_eq!(c.detail, "resource-based committed use discount: 3-year saves ~$90/mo, 1-year saves ~$50/mo");
        assert!(c.address.starts_with("billing account B-A-1 (all projects)"), "{}", c.address);
        assert_eq!(report.insight_notes, vec!["a note".to_string()]);
        assert!(report.potential_savings > 0.0);
    }
}
