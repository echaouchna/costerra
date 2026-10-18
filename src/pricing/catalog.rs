//! On-disk pricing catalog (derived from the Cloud Billing Catalog API) and
//! its in-memory index.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const CATALOG_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sku {
    pub id: String,
    pub description: String,
    pub family: String,
    pub group: String,
    pub usage_type: String,
    pub regions: Vec<String>,
    /// Pricing unit, e.g. `h`, `GiBy.mo`, `GiBy`, `count`.
    pub unit: String,
    /// Tiered rates: (start usage amount, unit price). For billing-account
    /// catalogs these are the contract prices.
    pub tiers: Vec<(f64, f64)>,
    /// List prices, when they differ from `tiers` (billing-account catalogs).
    pub list_tiers: Option<Vec<(f64, f64)>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSkus {
    pub id: String,
    pub name: String,
    pub skus: Vec<Sku>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogFile {
    pub version: u32,
    pub currency: String,
    /// Unix timestamp (seconds) of the sync.
    pub fetched_at: u64,
    /// Billing account whose contract prices are applied, if any.
    pub billing_account: Option<String>,
    pub services: Vec<ServiceSkus>,
}

pub struct ServiceIndex {
    pub id: String,
    pub skus: Vec<Sku>,
    pub by_region: HashMap<String, Vec<u32>>,
}

impl ServiceIndex {
    fn new(id: String, skus: Vec<Sku>) -> ServiceIndex {
        let mut by_region: HashMap<String, Vec<u32>> = HashMap::new();
        for (i, sku) in skus.iter().enumerate() {
            for r in &sku.regions {
                by_region.entry(r.clone()).or_default().push(i as u32);
            }
        }
        ServiceIndex { id, skus, by_region }
    }
}

/// A service whose SKUs are decoded on first use.
struct LazyService {
    id: String,
    /// Compressed SKU list in `Catalog::bytes` (empty range when built in memory).
    blob: std::ops::Range<usize>,
    count: usize,
    cell: std::sync::OnceLock<ServiceIndex>,
}

pub struct Catalog {
    pub currency: String,
    pub fetched_at: u64,
    pub billing_account: Option<String>,
    /// Number of SKUs whose contract price differs from the list price.
    pub discounted_skus: usize,
    bytes: Vec<u8>,
    services: HashMap<String, LazyService>,
}

/// File layout: MAGIC, u32 header length, postcard(Header), then one
/// zstd(postcard(Vec<Sku>)) blob per service, so that only the services a
/// run needs are decoded.
const MAGIC: &[u8; 8] = b"COSTERRA";

#[derive(Serialize, Deserialize)]
struct Header {
    version: u32,
    currency: String,
    fetched_at: u64,
    billing_account: Option<String>,
    discounted_skus: usize,
    /// (id, name, blob length, sku count)
    services: Vec<(String, String, u64, u64)>,
}

fn decode_skus(blob: &[u8]) -> Vec<Sku> {
    zstd::decode_all(blob).ok().and_then(|raw| postcard::from_bytes(&raw).ok()).unwrap_or_default()
}

fn encode_skus(skus: &[Sku]) -> Result<Vec<u8>> {
    Ok(zstd::encode_all(&postcard::to_stdvec(skus)?[..], 3)?)
}

impl Catalog {
    pub fn from_file(f: CatalogFile) -> Catalog {
        let discounted_skus = f.services.iter().flat_map(|s| s.skus.iter()).filter(|s| s.list_tiers.is_some()).count();
        let services = f
            .services
            .into_iter()
            .map(|s| {
                let count = s.skus.len();
                let cell = std::sync::OnceLock::new();
                let _ = cell.set(ServiceIndex::new(s.id.clone(), s.skus));
                (s.name, LazyService { id: s.id, blob: 0..0, count, cell })
            })
            .collect();
        Catalog {
            currency: f.currency,
            fetched_at: f.fetched_at,
            billing_account: f.billing_account,
            discounted_skus,
            bytes: vec![],
            services,
        }
    }

    /// SKUs of a service (decoded on first access).
    pub fn service(&self, name: &str) -> Option<&ServiceIndex> {
        let s = self.services.get(name)?;
        Some(s.cell.get_or_init(|| ServiceIndex::new(s.id.clone(), decode_skus(&self.bytes[s.blob.clone()]))))
    }

    /// Decode every service (to update prices of arbitrary SKUs).
    pub fn load_all(&self) {
        let names: Vec<String> = self.services.keys().cloned().collect();
        for n in names {
            self.service(&n);
        }
    }

    pub fn service_names(&self) -> impl Iterator<Item = &str> {
        self.services.keys().map(String::as_str)
    }

    pub fn service_count(&self) -> usize {
        self.services.len()
    }

    pub fn sku_count(&self) -> usize {
        self.services.values().map(|s| s.count).sum()
    }

    /// Update SKU prices with fresh billing-account prices (only services
    /// already decoded can contain SKUs used by a run). Returns how many changed.
    pub fn apply_account_prices(&mut self, prices: &HashMap<String, crate::pricing::sync::AccountPrice>) -> usize {
        let same = |a: &[(f64, f64)], b: &[(f64, f64)]| {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x.0 - y.0).abs() < 1e-9 && (x.1 - y.1).abs() < 1e-12)
        };
        let mut changed = 0;
        for svc in self.services.values_mut() {
            let Some(idx) = svc.cell.get_mut() else { continue };
            for sku in idx.skus.iter_mut() {
                let Some(ap) = prices.get(&sku.id) else { continue };
                if ap.contract.is_empty() || (!ap.unit.is_empty() && ap.unit != sku.unit) {
                    continue;
                }
                let list = if ap.list.is_empty() { sku.list_tiers.clone().unwrap_or_else(|| sku.tiers.clone()) } else { ap.list.clone() };
                let new_list = if same(&ap.contract, &list) { None } else { Some(list) };
                if !same(&sku.tiers, &ap.contract) || sku.list_tiers != new_list {
                    if sku.list_tiers.is_some() != new_list.is_some() {
                        if new_list.is_some() {
                            self.discounted_skus += 1
                        } else {
                            self.discounted_skus = self.discounted_skus.saturating_sub(1)
                        }
                    }
                    changed += 1;
                }
                sku.tiers = ap.contract.clone();
                sku.list_tiers = new_list;
            }
        }
        changed
    }

    /// Write the catalog; services never decoded are copied as-is.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut names: Vec<&String> = self.services.keys().collect();
        names.sort();
        let mut blobs = Vec::with_capacity(names.len());
        let mut entries = Vec::with_capacity(names.len());
        for n in names {
            let s = &self.services[n];
            let blob = match s.cell.get() {
                Some(idx) => encode_skus(&idx.skus)?,
                None => self.bytes[s.blob.clone()].to_vec(),
            };
            entries.push((s.id.clone(), n.clone(), blob.len() as u64, s.count as u64));
            blobs.push(blob);
        }
        let header = Header {
            version: CATALOG_VERSION,
            currency: self.currency.clone(),
            fetched_at: self.fetched_at,
            billing_account: self.billing_account.clone(),
            discounted_skus: self.discounted_skus,
            services: entries,
        };
        write_file(path, &header, &blobs)
    }
}

fn write_file(path: &Path, header: &Header, blobs: &[Vec<u8>]) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let h = postcard::to_stdvec(header)?;
    let mut out = Vec::with_capacity(16 + h.len() + blobs.iter().map(Vec::len).sum::<usize>());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(h.len() as u32).to_le_bytes());
    out.extend_from_slice(&h);
    for b in blobs {
        out.extend_from_slice(b);
    }
    crate::util::write_atomic(path, &out)?;
    Ok(())
}

static CACHE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Set the cache directory (from `--cache-dir` or the config file). The first
/// call wins, so the command line takes precedence over the config.
pub fn set_cache_dir(p: PathBuf) {
    let _ = CACHE_DIR.set(p);
}

/// Cache directory: `--cache-dir` / `cache_dir:`, `COSTERRA_CACHE_DIR`,
/// `$XDG_CACHE_HOME/costerra`, otherwise `~/.cache/costerra`.
pub fn default_cache_dir() -> PathBuf {
    if let Some(p) = CACHE_DIR.get() {
        return p.clone();
    }
    if let Ok(d) = std::env::var("COSTERRA_CACHE_DIR") {
        return PathBuf::from(d);
    }
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME").filter(|x| !x.is_empty()) {
        return PathBuf::from(x).join("costerra");
    }
    dirs::home_dir().map(|h| h.join(".cache")).unwrap_or_else(std::env::temp_dir).join("costerra")
}

pub fn catalog_path(cache_dir: &Path, currency: &str) -> PathBuf {
    if let Ok(p) = std::env::var("COSTERRA_CATALOG") {
        return PathBuf::from(p);
    }
    cache_dir.join("pricing").join(format!("catalog-{currency}.bin.zst"))
}

/// Catalog with the contract prices of a billing account.
pub fn account_catalog_path(cache_dir: &Path, currency: &str, billing_account: &str) -> PathBuf {
    cache_dir.join("pricing").join(format!("catalog-{currency}-{billing_account}.bin.zst"))
}

pub fn save(path: &Path, f: &CatalogFile) -> Result<()> {
    let mut blobs = Vec::with_capacity(f.services.len());
    let mut entries = Vec::with_capacity(f.services.len());
    for s in &f.services {
        let blob = encode_skus(&s.skus)?;
        entries.push((s.id.clone(), s.name.clone(), blob.len() as u64, s.skus.len() as u64));
        blobs.push(blob);
    }
    let header = Header {
        version: CATALOG_VERSION,
        currency: f.currency.clone(),
        fetched_at: f.fetched_at,
        billing_account: f.billing_account.clone(),
        discounted_skus: f.services.iter().flat_map(|s| s.skus.iter()).filter(|s| s.list_tiers.is_some()).count(),
        services: entries,
    };
    write_file(path, &header, &blobs)
}

/// Apply refreshed contract prices to the catalog file at `path`. The file is
/// re-read under a lock, so that concurrent refreshes (the foreground run and
/// the background `pricing refresh` process) merge instead of overwriting
/// each other. Returns the number of changed SKUs.
pub fn update_account_prices(path: &Path, prices: &HashMap<String, crate::pricing::sync::AccountPrice>) -> Result<usize> {
    if prices.is_empty() {
        return Ok(0);
    }
    crate::util::with_lock(path, || {
        let mut c = load(path)?;
        c.load_all();
        let n = c.apply_account_prices(prices);
        if n > 0 {
            c.save(path)?;
        }
        Ok(n)
    })
}

pub fn load(path: &Path) -> Result<Catalog> {
    let bytes = std::fs::read(path).with_context(|| format!("reading pricing catalog {}", path.display()))?;
    if bytes.len() < 12 || &bytes[..8] != MAGIC {
        anyhow::bail!("pricing catalog format is outdated; run `costerra pricing sync`");
    }
    let hlen = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let header: Header =
        postcard::from_bytes(bytes.get(12..12 + hlen).context("truncated pricing catalog")?).context("decoding pricing catalog")?;
    if header.version != CATALOG_VERSION {
        anyhow::bail!("pricing catalog format is outdated; run `costerra pricing sync`");
    }
    let mut offset = 12 + hlen;
    let mut services = HashMap::with_capacity(header.services.len());
    for (id, name, len, count) in header.services {
        let end = offset + len as usize;
        anyhow::ensure!(end <= bytes.len(), "truncated pricing catalog");
        services.insert(name, LazyService { id, blob: offset..end, count: count as usize, cell: std::sync::OnceLock::new() });
        offset = end;
    }
    Ok(Catalog {
        currency: header.currency,
        fetched_at: header.fetched_at,
        billing_account: header.billing_account,
        discounted_skus: header.discounted_skus,
        bytes,
        services,
    })
}

/// Convert a raw Billing Catalog API SKU JSON object.
pub fn sku_from_api(j: &serde_json::Value) -> Option<Sku> {
    let pi = j["pricingInfo"].get(0)?;
    let pe = &pi["pricingExpression"];
    let tiers = pe["tieredRates"]
        .as_array()?
        .iter()
        .map(|t| {
            let start = t["startUsageAmount"].as_f64().unwrap_or(0.0);
            let up = &t["unitPrice"];
            let units = up["units"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            let nanos = up["nanos"].as_f64().unwrap_or(0.0);
            (start, units + nanos / 1e9)
        })
        .collect();
    let cat = &j["category"];
    Some(Sku {
        id: j["skuId"].as_str()?.to_string(),
        description: j["description"].as_str().unwrap_or_default().to_string(),
        family: cat["resourceFamily"].as_str().unwrap_or_default().to_string(),
        group: cat["resourceGroup"].as_str().unwrap_or_default().to_string(),
        usage_type: cat["usageType"].as_str().unwrap_or_default().to_string(),
        regions: j["serviceRegions"]
            .as_array()
            .map(|a| a.iter().filter_map(|r| r.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        unit: pe["usageUnit"].as_str().unwrap_or_default().to_string(),
        tiers,
        list_tiers: None,
    })
}
