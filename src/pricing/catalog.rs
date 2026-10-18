//! On-disk pricing catalog (derived from the Cloud Billing Catalog API) and
//! its in-memory index.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const CATALOG_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sku {
    pub id: String,
    pub description: String,
    // Few distinct values: shared between the SKUs of a service (see `intern`).
    pub family: Arc<str>,
    pub group: Arc<str>,
    pub usage_type: Arc<str>,
    pub regions: Vec<Arc<str>>,
    /// Pricing unit, e.g. `h`, `GiBy.mo`, `GiBy`, `count`.
    pub unit: Arc<str>,
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
    pub by_region: HashMap<Arc<str>, Vec<u32>>,
}

impl ServiceIndex {
    fn new(id: String, mut skus: Vec<Sku>) -> ServiceIndex {
        intern(&mut skus);
        let mut by_region: HashMap<Arc<str>, Vec<u32>> = HashMap::new();
        for (i, sku) in skus.iter().enumerate() {
            for r in &sku.regions {
                by_region.entry(r.clone()).or_default().push(i as u32);
            }
        }
        for v in by_region.values_mut() {
            v.shrink_to_fit();
        }
        ServiceIndex { id, skus, by_region }
    }
}

/// Set the contract (and list) prices of a SKU. When they changed, returns
/// the change in the number of discounted SKUs (-1, 0 or 1).
fn update_sku(sku: &mut Sku, ap: &crate::pricing::sync::AccountPrice) -> Option<isize> {
    let same = |a: &[(f64, f64)], b: &[(f64, f64)]| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x.0 - y.0).abs() < 1e-9 && (x.1 - y.1).abs() < 1e-12)
    };
    if ap.contract.is_empty() || (!ap.unit.is_empty() && *ap.unit != *sku.unit) {
        return None;
    }
    let list = if ap.list.is_empty() { sku.list_tiers.clone().unwrap_or_else(|| sku.tiers.clone()) } else { ap.list.clone() };
    let new_list = if same(&ap.contract, &list) { None } else { Some(list) };
    let changed = !same(&sku.tiers, &ap.contract) || sku.list_tiers != new_list;
    let delta = match (sku.list_tiers.is_some(), new_list.is_some()) {
        (false, true) => 1,
        (true, false) => -1,
        _ => 0,
    };
    sku.tiers = ap.contract.clone();
    sku.list_tiers = new_list;
    changed.then_some(delta)
}

/// Share the repeated strings of SKUs (family, group, usage type, unit,
/// regions: a few hundred distinct values for tens of thousands of SKUs).
fn intern(skus: &mut [Sku]) {
    let mut seen: HashSet<Arc<str>> = HashSet::new();
    let mut one = |s: &mut Arc<str>| match seen.get(&**s) {
        Some(shared) => *s = shared.clone(),
        None => {
            seen.insert(s.clone());
        }
    };
    for sku in skus {
        one(&mut sku.family);
        one(&mut sku.group);
        one(&mut sku.usage_type);
        one(&mut sku.unit);
        sku.regions.iter_mut().for_each(&mut one);
        sku.regions.shrink_to_fit();
    }
}

/// A service whose SKUs are decoded on first use: all of them (`cell`), or
/// only those of the few regions prices are looked up in (`regions`), which
/// is what estimates usually need and keeps memory low.
struct LazyService {
    id: String,
    /// Compressed SKU list in `Catalog::bytes` (empty range when built in memory).
    blob: std::ops::Range<usize>,
    count: usize,
    cell: std::sync::OnceLock<ServiceIndex>,
    /// Decompressed bytes and region offsets (None: unexpected layout).
    scanned: std::sync::OnceLock<Option<Scanned>>,
    regions: parking_lot::RwLock<HashMap<String, Arc<Vec<Sku>>>>,
    /// One region decode at a time per service (pricing runs in parallel).
    decoding: parking_lot::Mutex<()>,
}

/// A decompressed service and the offsets of its SKUs per region: SKUs are
/// parsed only for the regions prices are looked up in.
struct Scanned {
    raw: Vec<u8>,
    by_region: HashMap<String, Vec<u32>>,
}

/// The SKUs of a fully decoded service in one region.
fn in_region<'a>(idx: &'a ServiceIndex, region: &str) -> impl Iterator<Item = &'a Sku> {
    idx.by_region.get(region).into_iter().flatten().map(|i| &idx.skus[*i as usize])
}

/// Index a postcard-encoded `Vec<Sku>` by region without decoding it (no
/// allocation per SKU). None if the bytes do not have the expected layout.
fn scan(raw: &[u8]) -> Option<HashMap<String, Vec<u32>>> {
    fn varint(b: &[u8], pos: &mut usize) -> Option<usize> {
        let mut v: u64 = 0;
        for shift in (0..64).step_by(7) {
            let byte = *b.get(*pos)?;
            *pos += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return usize::try_from(v).ok();
            }
        }
        None
    }
    fn str<'a>(b: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
        let n = varint(b, pos)?;
        let s = b.get(*pos..*pos + n)?;
        *pos += n;
        Some(s)
    }
    let mut by_region: HashMap<String, Vec<u32>> = HashMap::new();
    let mut pos = 0;
    let count = varint(raw, &mut pos)?;
    for _ in 0..count {
        let start = u32::try_from(pos).ok()?;
        // id, description, family, group, usage_type
        for _ in 0..5 {
            str(raw, &mut pos)?;
        }
        for _ in 0..varint(raw, &mut pos)? {
            let r = std::str::from_utf8(str(raw, &mut pos)?).ok()?;
            match by_region.get_mut(r) {
                Some(v) => v.push(start),
                None => {
                    by_region.insert(r.to_string(), vec![start]);
                }
            }
        }
        str(raw, &mut pos)?; // unit
        pos += varint(raw, &mut pos)? * 16; // tiers: (f64, f64)
        match raw.get(pos)? {
            0 => pos += 1,
            1 => {
                pos += 1;
                pos += varint(raw, &mut pos)? * 16;
            }
            _ => return None,
        }
    }
    (pos == raw.len()).then_some(by_region)
}

pub struct Catalog {
    pub currency: String,
    pub fetched_at: u64,
    pub billing_account: Option<String>,
    /// Number of SKUs whose contract price differs from the list price.
    pub discounted_skus: usize,
    bytes: Vec<u8>,
    services: HashMap<String, LazyService>,
    /// Fresh billing-account prices, applied to SKUs decoded later too.
    fresh: parking_lot::RwLock<HashMap<String, crate::pricing::sync::AccountPrice>>,
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
    // Streamed: the decompressed bytes of a whole service are never held at once.
    let Ok(dec) = zstd::stream::read::Decoder::with_buffer(blob) else { return vec![] };
    let mut scratch = vec![0u8; 64 * 1024];
    postcard::from_io((dec, &mut scratch[..])).map(|(skus, _)| skus).unwrap_or_default()
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
                (
                    s.name,
                    LazyService {
                        id: s.id,
                        blob: 0..0,
                        count,
                        cell,
                        scanned: Default::default(),
                        regions: Default::default(),
                        decoding: Default::default(),
                    },
                )
            })
            .collect();
        Catalog {
            currency: f.currency,
            fetched_at: f.fetched_at,
            billing_account: f.billing_account,
            discounted_skus,
            bytes: vec![],
            services,
            fresh: Default::default(),
        }
    }

    /// SKUs of a service (all of them, decoded on first access).
    pub fn service(&self, name: &str) -> Option<&ServiceIndex> {
        let s = self.services.get(name)?;
        Some(s.cell.get_or_init(|| {
            tracing::debug!("decoding all the SKUs of {name}");
            let mut skus = match s.scanned.get() {
                Some(Some(sc)) => postcard::from_bytes(&sc.raw).unwrap_or_default(),
                _ => decode_skus(&self.bytes[s.blob.clone()]),
            };
            self.apply_fresh(&mut skus);
            ServiceIndex::new(s.id.clone(), skus)
        }))
    }

    /// Run `f` over the SKUs of a service in one region. Only the SKUs of that
    /// region are decoded, unless the whole service already is.
    pub fn with_region<R>(&self, name: &str, region: &str, f: impl FnOnce(&mut dyn Iterator<Item = &Sku>) -> R) -> Option<R> {
        let s = self.services.get(name)?;
        if let Some(idx) = s.cell.get() {
            return Some(f(&mut in_region(idx, region)));
        }
        let mut cached = s.regions.read().get(region).cloned();
        let _decoding = cached.is_none().then(|| s.decoding.lock());
        if cached.is_none() {
            // Another thread may have decoded it while we waited.
            if let Some(idx) = s.cell.get() {
                return Some(f(&mut in_region(idx, region)));
            }
            cached = s.regions.read().get(region).cloned();
        }
        let skus = match cached {
            Some(v) => v,
            None => {
                let scanned = s.scanned.get_or_init(|| {
                    let raw = zstd::decode_all(&self.bytes[s.blob.clone()]).ok()?;
                    let by_region = scan(&raw)?;
                    Some(Scanned { raw, by_region })
                });
                let Some(sc) = scanned else {
                    // Unexpected layout: decode the whole service.
                    return Some(f(&mut in_region(self.service(name)?, region)));
                };
                tracing::debug!("decoding the {region} SKUs of {name}");
                let mut skus: Vec<Sku> = sc
                    .by_region
                    .get(region)
                    .into_iter()
                    .flatten()
                    .filter_map(|off| postcard::take_from_bytes::<Sku>(sc.raw.get(*off as usize..)?).ok().map(|(sku, _)| sku))
                    .collect();
                self.apply_fresh(&mut skus);
                intern(&mut skus);
                skus.shrink_to_fit();
                let v = Arc::new(skus);
                s.regions.write().insert(region.to_string(), v.clone());
                v
            }
        };
        Some(f(&mut skus.iter()))
    }

    /// Apply the fresh billing-account prices to newly decoded SKUs.
    fn apply_fresh(&self, skus: &mut [Sku]) {
        let fresh = self.fresh.read();
        if fresh.is_empty() {
            return;
        }
        for sku in skus {
            if let Some(ap) = fresh.get(&sku.id) {
                let _ = update_sku(sku, ap);
            }
        }
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

    /// Update SKU prices with fresh billing-account prices: the decoded SKUs
    /// now, and those decoded later. Returns how many decoded SKUs changed.
    pub fn apply_account_prices(&mut self, prices: &HashMap<String, crate::pricing::sync::AccountPrice>) -> usize {
        self.fresh.get_mut().extend(prices.iter().map(|(k, v)| (k.clone(), v.clone())));
        let mut changed = 0;
        let mut discounted: isize = 0;
        for svc in self.services.values_mut() {
            for skus in svc.regions.get_mut().values_mut() {
                for sku in Arc::make_mut(skus).iter_mut() {
                    if let Some(ap) = prices.get(&sku.id) {
                        let _ = update_sku(sku, ap);
                    }
                }
            }
            let Some(idx) = svc.cell.get_mut() else { continue };
            for sku in idx.skus.iter_mut() {
                let Some(ap) = prices.get(&sku.id) else { continue };
                if let Some(d) = update_sku(sku, ap) {
                    changed += 1;
                    discounted += d;
                }
            }
        }
        self.discounted_skus = (self.discounted_skus as isize + discounted).max(0) as usize;
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
        services.insert(
            name,
            LazyService {
                id,
                blob: offset..end,
                count: count as usize,
                cell: std::sync::OnceLock::new(),
                scanned: Default::default(),
                regions: Default::default(),
                decoding: Default::default(),
            },
        );
        offset = end;
    }
    Ok(Catalog {
        currency: header.currency,
        fetched_at: header.fetched_at,
        billing_account: header.billing_account,
        discounted_skus: header.discounted_skus,
        bytes,
        services,
        fresh: Default::default(),
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
        family: cat["resourceFamily"].as_str().unwrap_or_default().into(),
        group: cat["resourceGroup"].as_str().unwrap_or_default().into(),
        usage_type: cat["usageType"].as_str().unwrap_or_default().into(),
        regions: j["serviceRegions"].as_array().map(|a| a.iter().filter_map(|r| r.as_str().map(Arc::from)).collect()).unwrap_or_default(),
        unit: pe["usageUnit"].as_str().unwrap_or_default().into(),
        tiers,
        list_tiers: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::sync::AccountPrice;

    fn sku(id: &str, regions: &[&str], price: f64) -> Sku {
        Sku {
            id: id.into(),
            description: format!("SKU {id}"),
            family: "Compute".into(),
            group: "CPU".into(),
            usage_type: "OnDemand".into(),
            regions: regions.iter().map(|r| Arc::from(*r)).collect(),
            unit: "h".into(),
            tiers: vec![(0.0, price)],
            list_tiers: None,
        }
    }

    fn saved() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.bin.zst");
        let skus = vec![
            sku("a", &["europe-west1"], 1.0),
            sku("b", &["us-central1"], 2.0),
            sku("c", &["europe-west1", "europe-west4"], 3.0),
            sku("g", &["global"], 4.0),
        ];
        let f = CatalogFile {
            version: CATALOG_VERSION,
            currency: "USD".into(),
            fetched_at: 0,
            billing_account: None,
            services: vec![ServiceSkus { id: "6F81-5844-456A".into(), name: "Compute Engine".into(), skus }],
        };
        save(&path, &f).unwrap();
        (dir, path)
    }

    fn ids(cat: &Catalog, region: &str) -> Vec<String> {
        cat.with_region("Compute Engine", region, |it| it.map(|s| s.id.clone()).collect()).unwrap()
    }

    #[test]
    fn regions_are_decoded_on_their_own() {
        let (_dir, path) = saved();
        let cat = load(&path).unwrap();
        assert_eq!(ids(&cat, "europe-west1"), ["a", "c"]);
        assert_eq!(ids(&cat, "europe-west4"), ["c"]);
        assert_eq!(ids(&cat, "global"), ["g"]);
        assert!(ids(&cat, "asia-east1").is_empty());
        // The whole service was never decoded, and gives the same SKUs.
        assert!(cat.services["Compute Engine"].cell.get().is_none());
        let full = cat.service("Compute Engine").unwrap();
        assert_eq!(full.skus.len(), 4);
        assert_eq!(ids(&cat, "europe-west1"), ["a", "c"]);
        assert!(cat.with_region("Cloud SQL", "europe-west1", |_| ()).is_none());
    }

    #[test]
    fn fresh_contract_prices_reach_regions_decoded_before_and_after() {
        let (_dir, path) = saved();
        let mut cat = load(&path).unwrap();
        let price = |cat: &Catalog, region: &str, id: &str| {
            cat.with_region("Compute Engine", region, |it| it.filter(|s| s.id == id).map(|s| s.tiers[0].1).next()).unwrap()
        };
        assert_eq!(price(&cat, "europe-west1", "a"), Some(1.0));
        let prices: HashMap<String, AccountPrice> = [
            ("a".to_string(), AccountPrice { unit: "h".into(), contract: vec![(0.0, 0.5)], list: vec![(0.0, 1.0)] }),
            ("b".to_string(), AccountPrice { unit: "h".into(), contract: vec![(0.0, 1.5)], list: vec![] }),
        ]
        .into_iter()
        .collect();
        cat.apply_account_prices(&prices);
        // Decoded before the refresh, and decoded after it.
        assert_eq!(price(&cat, "europe-west1", "a"), Some(0.5));
        assert_eq!(price(&cat, "us-central1", "b"), Some(1.5));
        let a = cat.service("Compute Engine").unwrap().skus.iter().find(|s| s.id == "a").unwrap().clone();
        assert_eq!((a.tiers[0].1, a.list_tiers.map(|l| l[0].1)), (0.5, Some(1.0)));
    }

    #[test]
    fn unexpected_bytes_are_not_indexed() {
        assert!(scan(&[]).is_none());
        assert!(scan(&[3, 1, 2]).is_none());
        let raw = postcard::to_stdvec(&vec![sku("x", &["europe-west1"], 1.0)]).unwrap();
        assert_eq!(scan(&raw).unwrap()["europe-west1"], [1]);
        // Trailing bytes: not a SKU list.
        let mut longer = raw.clone();
        longer.push(0);
        assert!(scan(&longer).is_none());
    }

    #[test]
    fn repeated_strings_are_shared() {
        let mut skus = vec![sku("a", &["europe-west1"], 1.0), sku("b", &["europe-west1"], 2.0)];
        intern(&mut skus);
        assert!(Arc::ptr_eq(&skus[0].family, &skus[1].family));
        assert!(Arc::ptr_eq(&skus[0].regions[0], &skus[1].regions[0]));
    }
}
