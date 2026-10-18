//! SKU lookup with memoization.

use super::catalog::{Catalog, Sku};
use parking_lot::RwLock;
use regex::Regex;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Price {
    pub sku_id: String,
    pub description: String,
    pub unit: String,
    pub tiers: Vec<(f64, f64)>,
}

impl Price {
    /// Total cost of `qty` units, applying tiered rates.
    pub fn cost(&self, qty: f64) -> f64 {
        if self.tiers.is_empty() {
            return 0.0;
        }
        let mut total = 0.0;
        for (i, (start, price)) in self.tiers.iter().enumerate() {
            if qty <= *start {
                break;
            }
            let end = self.tiers.get(i + 1).map(|t| t.0).unwrap_or(f64::INFINITY);
            total += (qty.min(end) - start) * price;
        }
        total
    }

    /// The marginal unit price beyond any free tier.
    pub fn unit_price(&self) -> f64 {
        self.tiers.iter().map(|t| t.1).find(|p| *p > 0.0).unwrap_or(0.0)
    }

    /// Size of the free tier (usage covered at price 0 before paid tiers).
    pub fn free_units(&self) -> f64 {
        match self.tiers.as_slice() {
            [(0.0, p), (next, _), ..] if *p == 0.0 => *next,
            _ => 0.0,
        }
    }
}

/// A SKU search. `desc` is a case-insensitive regex matched against the SKU
/// description; other fields filter on the SKU category.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<'a> {
    pub service: &'a str,
    pub region: &'a str,
    pub desc: &'a str,
    pub exclude: Option<&'a str>,
    pub family: Option<&'a str>,
    pub group: Option<&'a str>,
    pub usage_type: Option<&'a str>,
    pub unit: Option<&'a str>,
}

impl<'a> Query<'a> {
    pub fn new(service: &'a str, region: &'a str, desc: &'a str) -> Self {
        Query { service, region, desc, ..Default::default() }
    }
    pub fn group(mut self, g: &'a str) -> Self {
        self.group = Some(g);
        self
    }
    pub fn family(mut self, f: &'a str) -> Self {
        self.family = Some(f);
        self
    }
    pub fn usage(mut self, u: &'a str) -> Self {
        self.usage_type = Some(u);
        self
    }
    pub fn ondemand(self) -> Self {
        self.usage("OnDemand")
    }
    pub fn exclude(mut self, e: &'a str) -> Self {
        self.exclude = Some(e);
        self
    }
    pub fn unit(mut self, u: &'a str) -> Self {
        self.unit = Some(u);
        self
    }
    fn key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}",
            self.service,
            self.region,
            self.desc,
            self.exclude.unwrap_or(""),
            self.family.unwrap_or(""),
            self.group.unwrap_or(""),
            self.usage_type.unwrap_or(""),
            self.unit.unwrap_or("")
        )
    }
}

pub struct Pricer {
    pub catalog: Option<Arc<Catalog>>,
    /// Free tiers are granted per billing account, not per resource, so by
    /// default they are removed from per-resource estimates.
    pub include_free_tier: bool,
    /// Use list prices even when the catalog holds contract prices.
    pub use_list: bool,
    regexes: RwLock<HashMap<String, Arc<Regex>>>,
    memo: RwLock<HashMap<String, Option<Arc<Price>>>>,
    /// SKU ids returned by lookups (to refresh account prices of used SKUs).
    used: parking_lot::Mutex<std::collections::HashSet<String>>,
}

impl Pricer {
    pub fn new(catalog: Option<Catalog>, include_free_tier: bool) -> Self {
        Pricer {
            catalog: catalog.map(Arc::new),
            include_free_tier,
            use_list: false,
            regexes: RwLock::new(HashMap::new()),
            memo: RwLock::new(HashMap::new()),
            used: Default::default(),
        }
    }

    /// A pricer over the same catalog that uses list prices, when the catalog
    /// holds the contract prices of a billing account.
    pub fn list_view(&self) -> Option<Pricer> {
        let cat = self.catalog.as_ref()?;
        cat.billing_account.as_ref()?;
        Some(Pricer {
            catalog: Some(cat.clone()),
            include_free_tier: self.include_free_tier,
            use_list: true,
            regexes: RwLock::new(HashMap::new()),
            memo: RwLock::new(HashMap::new()),
            used: Default::default(),
        })
    }

    /// SKU ids resolved so far.
    pub fn used_skus(&self) -> Vec<String> {
        let mut v: Vec<String> = self.used.lock().iter().cloned().collect();
        v.sort();
        v
    }

    /// Take back the catalog (only possible when no other pricer shares it).
    pub fn into_catalog(self) -> Option<Catalog> {
        Arc::try_unwrap(self.catalog?).ok()
    }

    pub fn billing_account(&self) -> Option<&str> {
        self.catalog.as_ref()?.billing_account.as_deref()
    }

    pub fn currency(&self) -> &str {
        self.catalog.as_ref().map(|c| c.currency.as_str()).unwrap_or("USD")
    }

    fn regex(&self, pat: &str) -> Option<Arc<Regex>> {
        if let Some(r) = self.regexes.read().get(pat) {
            return Some(r.clone());
        }
        let re = match Regex::new(&format!("(?i){pat}")) {
            Ok(r) => Arc::new(r),
            Err(e) => {
                tracing::error!("invalid SKU regex {pat:?}: {e}");
                return None;
            }
        };
        self.regexes.write().insert(pat.to_string(), re.clone());
        Some(re)
    }

    pub fn find(&self, q: Query) -> Option<Arc<Price>> {
        let r = self.find_memo(q);
        if let Some(p) = &r
            && !self.used.lock().contains(&p.sku_id)
        {
            self.used.lock().insert(p.sku_id.clone());
        }
        r
    }

    fn find_memo(&self, q: Query) -> Option<Arc<Price>> {
        let key = q.key();
        if let Some(r) = self.memo.read().get(&key) {
            return r.clone();
        }
        let res = self.find_uncached(&q).map(|s| self.price_of(s));
        if res.is_none() {
            tracing::debug!("no SKU for {key}");
        }
        self.memo.write().insert(key, res.clone());
        res
    }

    /// Price of a SKU as seen by this pricer (contract or list tiers, free tier
    /// handling), recording its use.
    pub fn price_of(&self, s: &Sku) -> Arc<Price> {
        let mut tiers = if self.use_list { s.list_tiers.clone().unwrap_or_else(|| s.tiers.clone()) } else { s.tiers.clone() };
        if !self.include_free_tier && tiers.len() > 1 && tiers[0].1 == 0.0 {
            tiers.remove(0);
            tiers[0].0 = 0.0;
        }
        self.used.lock().insert(s.id.clone());
        Arc::new(Price { sku_id: s.id.clone(), description: s.description.clone(), unit: s.unit.clone(), tiers })
    }

    /// All SKUs matching a query (for diagnostics / `pricing search`).
    pub fn search(&self, q: Query) -> Vec<Sku> {
        let Some(cat) = &self.catalog else { return vec![] };
        let Some(re) = self.regex(q.desc) else { return vec![] };
        let ex = q.exclude.and_then(|e| self.regex(e));
        let services: Vec<&super::catalog::ServiceIndex> = if q.service.is_empty() {
            cat.service_names().filter_map(|n| cat.service(n)).collect()
        } else {
            cat.service(q.service).into_iter().collect()
        };
        let mut out = vec![];
        for svc in services {
            let iter: Box<dyn Iterator<Item = &Sku>> = if q.region.is_empty() {
                Box::new(svc.skus.iter())
            } else {
                Box::new(svc.by_region.get(q.region).into_iter().flatten().map(|i| &svc.skus[*i as usize]))
            };
            for s in iter {
                if self.matches(s, &q, &re, ex.as_deref()) {
                    out.push(s.clone());
                }
            }
        }
        out
    }

    fn matches(&self, s: &Sku, q: &Query, re: &Regex, ex: Option<&Regex>) -> bool {
        q.family.is_none_or(|f| s.family == f)
            && q.group.is_none_or(|g| s.group == g)
            && q.usage_type.is_none_or(|u| s.usage_type == u)
            && q.unit.is_none_or(|u| s.unit == u)
            && re.is_match(&s.description)
            && ex.is_none_or(|e| !e.is_match(&s.description))
    }

    fn find_uncached(&self, q: &Query) -> Option<&Sku> {
        let cat = self.catalog.as_ref()?;
        let svc = cat.service(q.service)?;
        let re = self.regex(q.desc)?;
        let ex = q.exclude.and_then(|e| self.regex(e));
        let region = normalize_region(q.region);
        for r in [region.as_str(), "global"] {
            let Some(idx) = svc.by_region.get(r) else { continue };
            let mut best: Option<&Sku> = None;
            for i in idx {
                let s = &svc.skus[*i as usize];
                if !self.matches(s, q, &re, ex.as_deref()) {
                    continue;
                }
                // A $0 global SKU reached through the fallback is almost always an
                // unrelated match (attribution / trial SKUs), not the wanted price.
                if r == "global" && region != "global" && s.tiers.iter().all(|t| t.1 == 0.0) {
                    continue;
                }
                best = Some(match best {
                    None => s,
                    Some(b) => {
                        let ks = (s.regions.len(), s.description.len(), &s.id);
                        let kb = (b.regions.len(), b.description.len(), &b.id);
                        if ks < kb { s } else { b }
                    }
                });
            }
            if best.is_some() {
                return best;
            }
        }
        None
    }
}

/// `europe-west1-b` -> `europe-west1`; other values unchanged.
pub fn normalize_region(r: &str) -> String {
    let parts: Vec<&str> = r.split('-').collect();
    if parts.len() == 3 && parts[2].len() == 1 && parts[1].chars().last().is_some_and(|c| c.is_ascii_digit()) {
        format!("{}-{}", parts[0], parts[1])
    } else {
        r.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiered_cost() {
        let p =
            Price { sku_id: "x".into(), description: "".into(), unit: "GiBy".into(), tiers: vec![(0.0, 0.0), (1.0, 0.12), (1024.0, 0.11)] };
        assert!((p.cost(0.5) - 0.0).abs() < 1e-9);
        assert!((p.cost(11.0) - 1.2).abs() < 1e-9);
        assert!((p.cost(2024.0) - (1023.0 * 0.12 + 1000.0 * 0.11)).abs() < 1e-6);
        assert_eq!(p.free_units(), 1.0);
    }

    #[test]
    fn zone_to_region() {
        assert_eq!(normalize_region("europe-west1-b"), "europe-west1");
        assert_eq!(normalize_region("us-central1"), "us-central1");
        assert_eq!(normalize_region("europe"), "europe");
    }
}
