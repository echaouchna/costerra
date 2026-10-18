//! `costerra pricing …`

use super::args::PricingCmd;
use super::common::rt;
use super::ui::notice;
use anyhow::Result;
use costerra::pricing::catalog;
use costerra::pricing::query::{Pricer, Query};
use costerra::pricing::sync;
use std::time::{Duration, Instant};

pub fn run(p: PricingCmd) -> Result<i32> {
    let cache = catalog::default_cache_dir();
    match p {
        PricingCmd::Sync { api_key, currency, all_services, billing_account, if_older_than } => {
            let path = catalog::catalog_path(&cache, &currency);
            if let Some(max_age) = if_older_than
                && let Ok(c) = catalog::load(&path)
            {
                let age = sync::now().saturating_sub(c.fetched_at);
                let account_fresh = billing_account.as_deref().is_none_or(|ba| {
                    catalog::load(&catalog::account_catalog_path(&cache, &currency, ba))
                        .is_ok_and(|a| sync::now().saturating_sub(a.fetched_at) <= max_age)
                });
                if age <= max_age && account_fresh {
                    notice("info", format!("pricing catalog is {} days old, not refreshed (--if-older-than)", age / 86400));
                    return Ok(0);
                }
            }
            let t = Instant::now();
            notice("info", format!("downloading SKUs from the Cloud Billing Catalog API ({currency})…"));
            let rt = rt()?;
            let mut f = rt.block_on(sync::sync(api_key.clone(), &currency, all_services, true))?;
            let n: usize = f.services.iter().map(|s| s.skus.len()).sum();
            catalog::save(&path, &f)?;
            notice("info", format!("saved {n} SKUs (list prices) to {} in {:.1?}", path.display(), t.elapsed()));
            if let Some(ba) = billing_account {
                let t = Instant::now();
                notice("info", format!("downloading contract prices of billing account {ba}…"));
                let prices = rt.block_on(sync::fetch_account_prices(api_key, &ba, &currency, true))?;
                let discounted = sync::apply_account_prices(&mut f, &ba, &prices);
                let apath = catalog::account_catalog_path(&cache, &currency, &ba);
                catalog::save(&apath, &f)?;
                notice(
                    "info",
                    format!(
                        "saved contract prices to {} in {:.1?}: {discounted} of {n} SKUs differ from list price",
                        apath.display(),
                        t.elapsed()
                    ),
                );
                notice(
                    "info",
                    format!("use them with `billing_account: {ba}` in costerra.yml, --billing-account or COSTERRA_BILLING_ACCOUNT"),
                );
            }
        }
        PricingCmd::Import { dir, currency } => {
            let f = sync::import_dir(&dir, &currency)?;
            let n: usize = f.services.iter().map(|s| s.skus.len()).sum();
            let path = catalog::catalog_path(&cache, &currency);
            catalog::save(&path, &f)?;
            notice("info", format!("imported {n} SKUs into {}", path.display()));
        }
        PricingCmd::Refresh { billing_account, currency, skus } => {
            // One background refresh at a time: others give up (their SKUs are
            // picked up by later runs).
            let Some(_lock) = costerra::util::try_lock(&super::prices::refresh_lock_path()) else {
                return Ok(0);
            };
            let ids: Vec<String> = skus.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect();
            let path = catalog::account_catalog_path(&cache, &currency, &billing_account);
            let r = rt()?.block_on(costerra::pricing::account::refresh(
                &billing_account,
                &currency,
                &ids,
                Instant::now() + Duration::from_secs(120),
                &|| {},
            ))?;
            if r.throttled {
                costerra::pricing::account::mark_throttled(&cache);
            }
            costerra::pricing::account::mark_refreshed(&cache, &billing_account, &currency, r.prices.keys().cloned());
            catalog::update_account_prices(&path, &r.prices)?;
        }
        PricingCmd::Status { currency, billing_account } => {
            let path = match &billing_account {
                Some(ba) => catalog::account_catalog_path(&cache, &currency, ba),
                None => catalog::catalog_path(&cache, &currency),
            };
            let t = Instant::now();
            let c = catalog::load(&path)?;
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("catalog:   {}", path.display());
            println!("size:      {:.1} MiB", size as f64 / 1048576.0);
            println!("currency:  {}", c.currency);
            match &c.billing_account {
                Some(ba) => println!("prices:    contract prices of billing account {ba} ({} SKUs differ from list)", c.discounted_skus),
                None => println!("prices:    list prices"),
            }
            println!("synced:    {} days ago", sync::now().saturating_sub(c.fetched_at) / 86400);
            println!("services:  {}", c.service_count());
            println!("SKUs:      {}", c.sku_count());
            println!("load time: {:?}", t.elapsed());
        }
        PricingCmd::Search { query, service, region, limit, currency } => {
            let c = catalog::load(&catalog::catalog_path(&cache, &currency))?;
            let pricer = Pricer::new(Some(c), true);
            let q = Query { service: &service, region: &region, desc: &query, ..Default::default() };
            let res = pricer.search(q);
            for s in res.iter().take(limit) {
                let tiers: Vec<String> = s.tiers.iter().map(|(st, p)| format!("{p}@{st}")).collect();
                println!(
                    "{}  {:<70} {:<10} {:<12} {:>8}  {}  [{}]",
                    s.id,
                    s.description,
                    s.usage_type,
                    s.group,
                    s.unit,
                    tiers.join(","),
                    s.regions.iter().take(4).cloned().collect::<Vec<_>>().join(",")
                );
            }
            println!("{} matches", res.len());
        }
    }
    Ok(0)
}
