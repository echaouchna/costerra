//! Downloading SKUs from the Cloud Billing Catalog API.

use super::catalog::{CATALOG_VERSION, CatalogFile, ServiceSkus, sku_from_api};
use anyhow::{Context, Result, bail};
use futures::StreamExt;
use std::path::Path;

/// Billing services priced by costerra (display names in the Billing Catalog).
pub const SERVICES: &[&str] = &[
    "Compute Engine",
    "Kubernetes Engine",
    "Backup for GKE",
    "Cloud SQL",
    "Cloud Storage",
    "Transfer Service",
    "Networking",
    "Network Security",
    "Cloud IDS",
    "Cloud Run",
    "Cloud Run Functions",
    "BigQuery",
    "BigQuery Reservation API",
    "BigQuery Storage API",
    "BigQuery BI Engine",
    "BigQuery Data Transfer Service",
    "Cloud Pub/Sub",
    "Cloud Memorystore for Redis",
    "Cloud Memorystore for Memcached",
    "Cloud Memorystore",
    "Cloud Spanner",
    "Artifact Registry",
    "Cloud Logging",
    "Cloud Monitoring",
    "Cloud Filestore",
    "Cloud Key Management Service (KMS)",
    "Secret Manager",
    "Cloud DNS",
    "Cloud Bigtable",
    "AlloyDB",
    "Cloud Composer",
    "Cloud Scheduler",
    "Cloud Tasks",
    "Cloud Firestore",
    "Cloud Dataflow",
    "Cloud Dataproc",
    "Dataproc",
    "Dataproc Metastore",
    "Workflows",
    "Vertex AI",
    "Cloud TPU",
    "Notebooks",
    "Cloud Build",
    "Eventarc",
    "API Gateway",
    "Google Service Control",
    "Certificate Manager",
    "Backup and DR Service",
    "Datastream",
    "Cloud Workstations",
    "Identity Platform",
    "App Engine",
    "Cloud Data Fusion",
    "Dataplex",
    "Cloud Deploy",
];

pub enum Auth {
    ApiKey(String),
    Bearer(String),
}

pub async fn resolve_auth(api_key: Option<String>) -> Result<Auth> {
    if let Some(k) = api_key.filter(|k| !k.is_empty()) {
        return Ok(Auth::ApiKey(k));
    }
    match gcp_auth::provider().await {
        Ok(p) => {
            let t = p.token(&["https://www.googleapis.com/auth/cloud-platform"]).await.context("obtaining access token")?;
            Ok(Auth::Bearer(t.as_str().to_string()))
        }
        Err(e) => {
            // Fall back to the gcloud CLI.
            let o = std::process::Command::new("gcloud").args(["auth", "print-access-token"]).output();
            match o {
                Ok(o) if o.status.success() => Ok(Auth::Bearer(String::from_utf8_lossy(&o.stdout).trim().to_string())),
                _ => bail!(
                    "no credentials for the Cloud Billing Catalog API ({e}). \
                     Set COSTERRA_API_KEY / --api-key, or configure Application Default Credentials"
                ),
            }
        }
    }
}

pub(crate) async fn get_json(client: &reqwest::Client, auth: &Auth, url: &str, query: &[(&str, &str)]) -> Result<serde_json::Value> {
    get_json_opts(client, auth, url, query, true).await
}

/// Error returned when the API throttles and `retry_throttled` is false.
#[derive(Debug)]
pub struct Throttled;

impl std::fmt::Display for Throttled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("rate limited by the Cloud Billing API")
    }
}

impl std::error::Error for Throttled {}

pub(crate) async fn get_json_opts(
    client: &reqwest::Client,
    auth: &Auth,
    url: &str,
    query: &[(&str, &str)],
    retry_throttled: bool,
) -> Result<serde_json::Value> {
    let mut rq = client.get(url).query(query);
    rq = match auth {
        Auth::ApiKey(k) => rq.query(&[("key", k.as_str())]),
        Auth::Bearer(t) => rq.bearer_auth(t),
    };
    let mut attempt: u32 = 0;
    loop {
        let resp = rq.try_clone().unwrap().send().await;
        let retry_after = match resp {
            Ok(r) if r.status().is_success() => return Ok(r.json().await?),
            Ok(r) if r.status().as_u16() == 429 && !retry_throttled => return Err(Throttled.into()),
            Ok(r) if (r.status().as_u16() == 429 || r.status().is_server_error()) && attempt < MAX_RETRIES => {
                tracing::debug!("{url}: HTTP {}, retrying (attempt {})", r.status(), attempt + 1);
                r.headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|s| std::time::Duration::from_secs(s.min(10)))
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                bail!("{url}: HTTP {status}: {}", body.chars().take(400).collect::<String>());
            }
            Err(e) if attempt < MAX_RETRIES => {
                tracing::debug!("{url}: {e}, retrying (attempt {})", attempt + 1);
                None
            }
            Err(e) => return Err(e.into()),
        };
        tokio::time::sleep(retry_after.unwrap_or_else(|| backoff(attempt))).await;
        attempt += 1;
    }
}

const MAX_RETRIES: u32 = 6;

/// Exponential backoff with full jitter: a random delay in [0, min(cap, base·2^n)].
pub fn backoff(attempt: u32) -> std::time::Duration {
    const BASE_MS: u64 = 200;
    const CAP_MS: u64 = 8_000;
    let ceiling = BASE_MS.saturating_mul(1 << attempt.min(16)).min(CAP_MS);
    std::time::Duration::from_millis(fastrand::u64(ceiling / 10..=ceiling))
}

const API: &str = "https://cloudbilling.googleapis.com/v1";

pub async fn sync(api_key: Option<String>, currency: &str, all_services: bool, progress: bool) -> Result<CatalogFile> {
    let auth = resolve_auth(api_key).await?;
    let client = reqwest::Client::builder().user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION"))).gzip(true).build()?;

    // List services.
    let mut services = vec![];
    let mut token = String::new();
    loop {
        let j = get_json(&client, &auth, &format!("{API}/services"), &[("pageSize", "5000"), ("pageToken", &token)]).await?;
        for s in j["services"].as_array().into_iter().flatten() {
            let name = s["displayName"].as_str().unwrap_or_default().to_string();
            let id = s["serviceId"].as_str().unwrap_or_default().to_string();
            let gcp = s["businessEntityName"].as_str() == Some("businessEntities/GCP");
            if gcp && (all_services || SERVICES.contains(&name.as_str())) {
                services.push((id, name));
            }
        }
        token = j["nextPageToken"].as_str().unwrap_or_default().to_string();
        if token.is_empty() {
            break;
        }
    }
    if services.is_empty() {
        bail!("no services returned by the Billing Catalog API");
    }

    let total = services.len();
    let fetched: Vec<Result<ServiceSkus>> = futures::stream::iter(services.into_iter().map(|(id, name)| {
        let client = &client;
        let auth = &auth;
        async move {
            let mut skus = vec![];
            let mut token = String::new();
            loop {
                let j = get_json(
                    client,
                    auth,
                    &format!("{API}/services/{id}/skus"),
                    &[("pageSize", "5000"), ("currencyCode", currency), ("pageToken", &token)],
                )
                .await
                .with_context(|| format!("fetching SKUs of {name}"))?;
                skus.extend(j["skus"].as_array().into_iter().flatten().filter_map(sku_from_api));
                token = j["nextPageToken"].as_str().unwrap_or_default().to_string();
                if token.is_empty() {
                    break;
                }
            }
            if progress {
                eprintln!("  {:<40} {:>6} SKUs", name, skus.len());
            }
            Ok(ServiceSkus { id, name, skus })
        }
    }))
    .buffer_unordered(12)
    .collect()
    .await;

    let mut out = vec![];
    for r in fetched {
        out.push(r?);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    if progress {
        eprintln!("  fetched {total} services");
    }
    Ok(CatalogFile { version: CATALOG_VERSION, currency: currency.to_string(), fetched_at: now(), billing_account: None, services: out })
}

fn money_value(m: &serde_json::Value) -> f64 {
    let units = m["units"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    units + m["nanos"].as_f64().unwrap_or(0.0) / 1e9
}

pub struct AccountPrice {
    pub unit: String,
    pub contract: Vec<(f64, f64)>,
    pub list: Vec<(f64, f64)>,
}

/// Parse a `BillingAccountPrice` object into (SKU id, prices).
pub fn parse_account_price(p: &serde_json::Value) -> Option<(String, AccountPrice)> {
    let id = p["name"].as_str()?.split("/skus/").nth(1)?.split('/').next()?.to_string();
    if p["valueType"].as_str() != Some("rate") {
        return None;
    }
    let rate = &p["rate"];
    let tiers = rate["tiers"].as_array().cloned().unwrap_or_default();
    let start = |t: &serde_json::Value| t["startAmount"]["value"].as_str().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let qty = rate["unitInfo"]["unitQuantity"]["value"].as_str().and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.0).max(1e-12);
    Some((
        id,
        AccountPrice {
            unit: rate["unitInfo"]["unit"].as_str().unwrap_or_default().to_string(),
            contract: tiers.iter().map(|t| (start(t), money_value(&t["contractPrice"]) / qty)).collect(),
            list: tiers.iter().map(|t| (start(t), money_value(&t["listPrice"]) / qty)).collect(),
        },
    ))
}

/// Download the prices of every SKU for a billing account (Cloud Billing
/// Pricing API), including contract / negotiated discounts.
pub async fn fetch_account_prices(
    api_key: Option<String>,
    billing_account: &str,
    currency: &str,
    progress: bool,
) -> Result<std::collections::HashMap<String, AccountPrice>> {
    let auth = resolve_auth(api_key).await?;
    if matches!(auth, Auth::ApiKey(_)) {
        bail!("billing account prices require OAuth credentials (Application Default Credentials or gcloud), not an API key");
    }
    let client = reqwest::Client::builder().user_agent(concat!("costerra/", env!("CARGO_PKG_VERSION"))).gzip(true).build()?;
    let url = format!("https://cloudbilling.googleapis.com/v1beta/billingAccounts/{billing_account}/skus/-/prices");
    let mut out = std::collections::HashMap::new();
    let mut token = String::new();
    let mut pages = 0;
    loop {
        let j = get_json(&client, &auth, &url, &[("pageSize", "5000"), ("currencyCode", currency), ("pageToken", &token)])
            .await
            .with_context(|| format!("fetching prices of billing account {billing_account} (requires billing.billingAccountPrice.get, e.g. Billing Account Viewer)"))?;
        for p in j["billingAccountPrices"].as_array().into_iter().flatten() {
            if let Some((id, ap)) = parse_account_price(p) {
                out.insert(id, ap);
            }
        }
        pages += 1;
        if progress {
            eprintln!("  billing account prices: {} SKUs ({pages} pages)", out.len());
        }
        token = j["nextPageToken"].as_str().unwrap_or_default().to_string();
        if token.is_empty() {
            break;
        }
    }
    Ok(out)
}

/// Apply billing-account prices to a list-price catalog. Returns the number
/// of SKUs whose price differs from the list price.
pub fn apply_account_prices(f: &mut CatalogFile, billing_account: &str, prices: &std::collections::HashMap<String, AccountPrice>) -> usize {
    let mut discounted = 0;
    for svc in &mut f.services {
        for sku in &mut svc.skus {
            let Some(ap) = prices.get(&sku.id) else { continue };
            if !ap.unit.is_empty() && ap.unit != sku.unit {
                tracing::debug!("{}: unit {} differs from catalog unit {}, keeping list price", sku.id, ap.unit, sku.unit);
                continue;
            }
            if ap.contract.is_empty() {
                continue;
            }
            let differs = ap.contract.len() != sku.tiers.len()
                || ap.contract.iter().zip(&sku.tiers).any(|(a, b)| (a.0 - b.0).abs() > 1e-9 || (a.1 - b.1).abs() > 1e-12);
            if differs {
                sku.list_tiers = Some(if ap.list.is_empty() { sku.tiers.clone() } else { ap.list.clone() });
                sku.tiers = ap.contract.clone();
                discounted += 1;
            }
        }
    }
    f.billing_account = Some(billing_account.to_string());
    discounted
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Build a catalog from raw API dumps: `services.jsonl` + `<serviceId>.jsonl`.
pub fn import_dir(dir: &Path, currency: &str) -> Result<CatalogFile> {
    let services_src = std::fs::read_to_string(dir.join("services.jsonl")).context("reading services.jsonl")?;
    let mut out = vec![];
    for line in services_src.lines() {
        let s: serde_json::Value = serde_json::from_str(line)?;
        let id = s["serviceId"].as_str().unwrap_or_default();
        let name = s["displayName"].as_str().unwrap_or_default();
        let path = dir.join(format!("{id}.jsonl"));
        let Ok(src) = std::fs::read_to_string(&path) else { continue };
        let skus = src.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).filter_map(|j| sku_from_api(&j)).collect();
        out.push(ServiceSkus { id: id.to_string(), name: name.to_string(), skus });
    }
    Ok(CatalogFile { version: CATALOG_VERSION, currency: currency.to_string(), fetched_at: now(), billing_account: None, services: out })
}
