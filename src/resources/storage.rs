//! Storage: Cloud Storage buckets and Rapid (Anywhere) Cache, Filestore,
//! Artifact Registry and Backup and DR.

use super::{Ctx, Spec, usage};
use crate::cost::{Component, HOURS_PER_MONTH};
use crate::hcl::value::{Value, fmt_number};
use crate::pricing::query::{Query, normalize_region};

const GCS: &str = "Cloud Storage";
const FILESTORE: &str = "Cloud Filestore";
const AR: &str = "Artifact Registry";
const BDR: &str = "Backup and DR Service";

/// Soft delete retention applied by GCS when `soft_delete_policy` is unset.
const DEFAULT_SOFT_DELETE_SECONDS: f64 = 604_800.0;

pub fn specs() -> Vec<Spec> {
    vec![
        Spec::new("google_storage_bucket", bucket)
            .usage(usage!(
                "storage_gb" => "Average data stored in GiB (live and noncurrent object versions)",
                "monthly_class_a_operations" => "Class A operations per month (uploads, overwrites, lists, rewrites)",
                "monthly_class_b_operations" => "Class B operations per month (object reads, metadata gets)",
                "monthly_data_retrieval_gb" => "GiB read per month from Nearline/Coldline/Archive objects",
                "monthly_egress_data_transfer_gb" => "Data transfer out in GiB per month: a number (internet, worldwide) or a map with keys same_continent, worldwide, asia, china, australia",
                "monthly_replicated_data_gb" => "GiB written per month to a dual/multi-region bucket (inter-region replication)",
                "soft_delete_storage_gb" => "Average soft-deleted data retained in GiB",
            ))
            .rules(rules::bucket),
        Spec::new("google_storage_anywhere_cache", anywhere_cache).usage(usage!(
            "cache_storage_gb" => "Average data held in the cache in GiB",
            "monthly_cache_ingest_gb" => "GiB ingested into the cache per month (cache misses)",
            "monthly_cache_data_transfer_out_gb" => "GiB served from the cache per month",
            "monthly_cache_operations" => "Read operations served from the cache per month",
        )),
        Spec::new("google_filestore_instance", filestore_instance)
            .usage(usage!("used_gb" => "Average used capacity in GiB (for right-sizing insights only)"))
            .rules(rules::filestore),
        Spec::new("google_filestore_backup", filestore_backup)
            .usage(usage!("storage_gb" => "Backup storage in GiB (compressed and incremental)")),
        Spec::new("google_artifact_registry_repository", ar_repository)
            .usage(usage!(
                "storage_gb" => "Average artifact storage in GiB",
                "monthly_egress_data_transfer_gb" => "Data transfer out in GiB per month: a number (internet, same continent) or a map with keys same_continent (other GCP regions), internet, intercontinental, china, oceania",
            ))
            .rules(rules::artifact_registry),
        Spec::new("google_backup_dr_backup_vault", backup_vault)
            .usage(usage!("storage_gb" => "Backup data stored in the vault in GiB"))
            .rules(rules::backup_vault),
        Spec::new("google_backup_dr_backup_plan_association", backup_plan_association)
            .usage(usage!("protected_gb" => "Size of the protected resource in GiB (defaults to its provisioned disk size)")),
    ]
}

// ---- Cloud Storage -----------------------------------------------------------

const PREDEFINED_DUAL: &[&str] = &["asia1", "eur4", "eur5", "eur7", "eur8", "nam4"];

#[derive(Debug, Clone, PartialEq)]
enum BucketLocation {
    Region(String),
    /// Billing region code of a multi-region: `us`, `europe`, `asia`.
    Multi(&'static str),
    /// Predefined dual-region, billed on its own SKUs (`eur4`, `nam4`, …).
    Dual(String),
    /// Configurable dual-region, billed on both regions' dual-region SKUs.
    CustomDual(Vec<String>),
}

impl BucketLocation {
    fn parse(location: &str, data_locations: &[String]) -> BucketLocation {
        let l = location.to_lowercase();
        match l.as_str() {
            "us" | "eu" | "asia" if data_locations.len() >= 2 => {
                BucketLocation::CustomDual(data_locations.iter().map(|d| normalize_region(&d.to_lowercase())).collect())
            }
            "us" => BucketLocation::Multi("us"),
            "eu" | "europe" => BucketLocation::Multi("europe"),
            "asia" => BucketLocation::Multi("asia"),
            d if PREDEFINED_DUAL.contains(&d) => BucketLocation::Dual(d.to_string()),
            r => BucketLocation::Region(normalize_region(r)),
        }
    }

    fn is_regional(&self) -> bool {
        matches!(self, BucketLocation::Region(_))
    }

    fn is_dual(&self) -> bool {
        matches!(self, BucketLocation::Dual(_) | BucketLocation::CustomDual(_))
    }

    /// Location prefix of operation SKUs.
    fn ops_scope(&self) -> &'static str {
        match self {
            BucketLocation::Region(_) => "Regional",
            BucketLocation::Multi(_) => "Multi-Region",
            _ => "Dual-Region",
        }
    }

    fn label(&self) -> String {
        match self {
            BucketLocation::Region(r) => r.clone(),
            BucketLocation::Multi(m) => format!("{} multi-region", if *m == "europe" { "EU".to_string() } else { m.to_uppercase() }),
            BucketLocation::Dual(d) => format!("{} dual-region", d.to_uppercase()),
            BucketLocation::CustomDual(v) => format!("dual-region {}", v.join("+")),
        }
    }

    /// Continent name as used in GCS network SKUs.
    fn continent(&self) -> Option<&'static str> {
        match self {
            BucketLocation::Region(r) => region_continent(r),
            BucketLocation::Multi("us") => Some("Northern America"),
            BucketLocation::Multi("europe") => Some("Europe"),
            BucketLocation::Multi(_) => Some("Asia"),
            BucketLocation::Dual(d) if d.starts_with("eur") => Some("Europe"),
            BucketLocation::Dual(d) if d.starts_with("nam") => Some("Northern America"),
            BucketLocation::Dual(_) => Some("Asia"),
            BucketLocation::CustomDual(v) => v.first().and_then(|r| region_continent(r)),
        }
    }

    /// A single region that could replace this location (for regional-bucket
    /// recommendations); prefers the provider region when it is on the same
    /// continent.
    fn regional_alternative(&self, provider_region: Option<&str>) -> String {
        if let (Some(p), Some(c)) = (provider_region, self.continent())
            && region_continent(p) == Some(c)
        {
            return p.to_string();
        }
        match self {
            BucketLocation::Region(r) => r.clone(),
            BucketLocation::Multi("us") => "us-central1".into(),
            BucketLocation::Multi("europe") => "europe-west1".into(),
            BucketLocation::Multi(_) => "asia-east1".into(),
            BucketLocation::Dual(d) => match d.as_str() {
                "eur4" => "europe-west4",
                "eur5" => "europe-west1",
                "eur7" | "eur8" => "europe-west3",
                "asia1" => "asia-northeast1",
                _ => "us-central1",
            }
            .into(),
            BucketLocation::CustomDual(v) => v.first().cloned().unwrap_or_else(|| "us-central1".into()),
        }
    }
}

fn region_continent(r: &str) -> Option<&'static str> {
    let prefix = r.split('-').next().unwrap_or(r);
    Some(match prefix {
        "us" | "northamerica" => "Northern America",
        "europe" => "Europe",
        "asia" => "Asia",
        "australia" => "Oceania",
        "southamerica" => "Latin America",
        "me" => "Middle East",
        "africa" => "Africa",
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum StorageClass {
    Standard,
    Nearline,
    Coldline,
    Archive,
    Dra,
}

impl StorageClass {
    fn parse(s: &str) -> Option<StorageClass> {
        Some(match s.to_uppercase().as_str() {
            "STANDARD" | "MULTI_REGIONAL" | "REGIONAL" => StorageClass::Standard,
            "NEARLINE" => StorageClass::Nearline,
            "COLDLINE" => StorageClass::Coldline,
            "ARCHIVE" => StorageClass::Archive,
            "DURABLE_REDUCED_AVAILABILITY" => StorageClass::Dra,
            _ => return None,
        })
    }

    /// Class name as it appears in SKU descriptions.
    fn sku_name(self) -> &'static str {
        match self {
            StorageClass::Standard => "Standard",
            StorageClass::Nearline => "Nearline",
            StorageClass::Coldline => "Coldline",
            StorageClass::Archive => "Archive",
            StorageClass::Dra => "Durable Reduced Availability",
        }
    }

    fn has_retrieval_fee(self) -> bool {
        matches!(self, StorageClass::Nearline | StorageClass::Coldline | StorageClass::Archive)
    }
}

/// One storage SKU lookup (configurable dual-regions bill two of them).
struct StorageSku {
    region: String,
    desc: String,
    exclude: Option<&'static str>,
    label: String,
}

impl StorageSku {
    fn query(&self) -> Query<'_> {
        let q = Query::new(GCS, &self.region, &self.desc).family("Storage").ondemand();
        match self.exclude {
            Some(e) => q.exclude(e),
            None => q,
        }
    }
}

fn storage_skus(loc: &BucketLocation, class: StorageClass) -> Vec<StorageSku> {
    let c = class.sku_name();
    match loc {
        BucketLocation::Region(r) => vec![StorageSku {
            region: r.clone(),
            desc: format!("^{c} Storage "),
            exclude: Some("Dual-region|Multi-region|Early Delete"),
            label: r.clone(),
        }],
        BucketLocation::Multi(m) => vec![StorageSku {
            region: m.to_string(),
            desc: format!("^{c} Storage "),
            exclude: Some("Dual-region|Early Delete"),
            label: loc.label(),
        }],
        BucketLocation::Dual(d) => {
            vec![StorageSku { region: d.clone(), desc: format!("^{c} Storage .*Dual-region$"), exclude: None, label: loc.label() }]
        }
        BucketLocation::CustomDual(v) => v
            .iter()
            .map(|r| StorageSku {
                region: r.clone(),
                desc: format!("^{c} Storage .*Dual-region$"),
                exclude: None,
                label: format!("dual-region, {r} copy"),
            })
            .collect(),
    }
}

/// Per GiB-month price of a class at a location (both copies for configurable
/// dual-regions).
fn storage_unit_price(ctx: &Ctx, loc: &BucketLocation, class: StorageClass) -> Option<f64> {
    storage_skus(loc, class).iter().map(|s| ctx.price(s.query()).map(|p| p.unit_price())).sum()
}

struct BucketConfig {
    loc: BucketLocation,
    class: StorageClass,
    autoclass: bool,
    hns: bool,
    turbo: bool,
    soft_delete_days: f64,
}

impl BucketConfig {
    fn from_ctx(ctx: &Ctx) -> BucketConfig {
        let data_locations: Vec<String> = ctx
            .get("custom_placement_config.0.data_locations")
            .and_then(|v| v.as_list())
            .map(|l| l.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let loc = match ctx.str("location") {
            Some(l) => BucketLocation::parse(l, &data_locations),
            None => {
                let r = ctx.region();
                ctx.warn(format!("bucket location unknown, assuming {r}"));
                BucketLocation::Region(r)
            }
        };
        let raw_class = ctx.str_or("storage_class", "STANDARD");
        let class = StorageClass::parse(raw_class).unwrap_or_else(|| {
            ctx.warn(format!("unknown storage class '{raw_class}', assuming STANDARD"));
            StorageClass::Standard
        });
        let turbo = ctx.str("rpo").is_some_and(|r| r.eq_ignore_ascii_case("ASYNC_TURBO"));
        if turbo && !loc.is_dual() {
            ctx.warn("turbo replication (rpo = ASYNC_TURBO) is only available for dual-region buckets; ignored");
        }
        BucketConfig {
            turbo: turbo && loc.is_dual(),
            loc,
            class,
            autoclass: ctx.bool_or("autoclass.0.enabled", false),
            hns: ctx.bool_or("hierarchical_namespace.0.enabled", false),
            soft_delete_days: ctx.num_or("soft_delete_policy.0.retention_duration_seconds", DEFAULT_SOFT_DELETE_SECONDS) / 86_400.0,
        }
    }

    fn ops_query_desc(&self, op: char) -> String {
        let class = if self.autoclass { "Standard" } else { self.class.sku_name() };
        format!(
            "^{}{}{} {class} Class {op} Operations$",
            self.loc.ops_scope(),
            if self.autoclass { " Autoclass" } else { "" },
            if self.hns { " HNS" } else { "" },
        )
    }
}

/// Usage-based operations component shown per 10,000 operations (SKUs are
/// priced per operation, which displays as a near-zero unit price).
fn ops_per_10k(ctx: &Ctx, name: &str, usage_key: &str, q: Query) -> Component {
    let mut c = ctx.usage(name, "10k operations", usage_key, q);
    c.quantity = c.quantity.map(|n| n / 10_000.0);
    c.unit_price = c.unit_price.map(|p| p * 10_000.0);
    c
}

/// Egress usage: a plain number or an Infracost-style map of destinations.
fn egress_usage(ctx: &Ctx, default_key: &'static str, keys: &[&'static str]) -> Option<Vec<(&'static str, f64)>> {
    match ctx.usage.get("monthly_egress_data_transfer_gb")? {
        Value::Map(m) => Some(keys.iter().filter_map(|k| Some((*k, m.get(*k)?.as_f64()?))).collect()),
        v => v.as_f64().map(|n| vec![(default_key, n)]),
    }
}

fn bucket(ctx: &Ctx) -> Vec<Component> {
    let b = BucketConfig::from_ctx(ctx);
    let mut out = vec![];
    let class = b.class.sku_name();
    let storage_gb = ctx.usage_num("storage_gb");

    for sku in storage_skus(&b.loc, b.class) {
        let mut c = ctx.usage_qty(format!("Storage ({class}, {})", sku.label), "GiB-months", storage_gb, sku.query());
        if b.autoclass {
            c.note = Some(
                "Autoclass: priced as Standard (upper bound); objects move to colder classes automatically. Management fee (per 1,000 objects ≥128 KiB) not included".into(),
            );
        }
        out.push(c);
    }

    if b.soft_delete_days > 0.0 {
        let mut c = ctx.fixed(
            format!("Soft-deleted storage ({}-day retention)", fmt_number((b.soft_delete_days * 10.0).round() / 10.0)),
            "GiB-months",
            ctx.usage_num("soft_delete_storage_gb"),
            storage_unit_price(ctx, &b.loc, b.class),
        );
        c.usage_based = true;
        if ctx.get("soft_delete_policy").is_none() {
            c.note =
                Some("soft delete is on by default (7 days); set soft_delete_policy { retention_duration_seconds = 0 } to disable".into());
        }
        out.push(c);
    }

    let op_a = b.ops_query_desc('A');
    let op_b = b.ops_query_desc('B');
    out.push(ops_per_10k(ctx, "Class A operations", "monthly_class_a_operations", Query::new(GCS, "global", &op_a).ondemand()));
    out.push(ops_per_10k(ctx, "Class B operations", "monthly_class_b_operations", Query::new(GCS, "global", &op_b).ondemand()));

    if b.class.has_retrieval_fee() && !b.autoclass {
        let pat = format!("^{class} Data Retrieval$");
        out.push(ctx.usage(
            format!("Data retrieval ({class})"),
            "GiB",
            "monthly_data_retrieval_gb",
            Query::new(GCS, "global", &pat).ondemand(),
        ));
    }

    let continent = b.loc.continent();
    if !b.loc.is_regional()
        && let Some(cont) = continent
    {
        let (name, pat) = if b.turbo {
            ("Inter-region replication (turbo)", format!("^Networking Traffic GCP Turbo Replication within {cont}$"))
        } else {
            ("Inter-region replication", format!("^Network Data Transfer GCP Replication within {cont}$"))
        };
        out.push(ctx.usage(name, "GiB", "monthly_replicated_data_gb", Query::new(GCS, "global", &pat).ondemand()));
    }

    let egress = egress_usage(ctx, "worldwide", &["same_continent", "worldwide", "asia", "china", "australia"]);
    let lines = egress.clone().unwrap_or_else(|| vec![("worldwide", f64::NAN)]);
    for (dest, gb) in lines {
        let qty = (!gb.is_nan()).then_some(gb);
        let (name, pat) = match dest {
            "same_continent" => {
                let Some(cont) = continent else { continue };
                let scope = if b.loc.is_regional() { "Inter Region" } else { "Multi-region" };
                let multi = format!("^Network Data Transfer GCP {scope} within {cont}$");
                let pat = if ctx.price(Query::new(GCS, "global", &multi).ondemand()).is_some() {
                    multi
                } else {
                    format!("^Network Data Transfer GCP Inter Region within {cont}$")
                };
                ("Data transfer to GCP (same continent)", pat)
            }
            "asia" => ("Internet egress (Asia)", "^Download APAC$".to_string()),
            "china" => ("Internet egress (China)", "^Download China$".to_string()),
            "australia" => ("Internet egress (Australia)", "^Download Australia$".to_string()),
            _ => ("Internet egress (worldwide)", "^Download Worldwide Destinations \\(excluding Asia & Australia\\)$".to_string()),
        };
        out.push(ctx.usage_qty(name, "GiB", qty, Query::new(GCS, "global", &pat).ondemand()));
    }
    out
}

fn anywhere_cache(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let hours = ctx.monthly_hours();
    vec![
        ctx.usage_qty(
            "Cache storage",
            "GiB-hours",
            ctx.usage_num("cache_storage_gb").map(|g| g * hours),
            Query::new(GCS, &region, "^Rapid Cache Storage ").ondemand(),
        ),
        ctx.usage("Cache ingest", "GiB", "monthly_cache_ingest_gb", Query::new(GCS, "global", "^Rapid Cache Ingest$").ondemand()),
        ctx.usage(
            "Cache data transfer out",
            "GiB",
            "monthly_cache_data_transfer_out_gb",
            Query::new(GCS, "global", "^Rapid Cache Data Transfer Out$").ondemand(),
        ),
        ops_per_10k(
            ctx,
            "Cache operations",
            "monthly_cache_operations",
            Query::new(GCS, "global", "^Rapid Cache Data Transfer Out Operations$").ondemand(),
        ),
    ]
}

// ---- Filestore -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum FsTier {
    BasicHdd,
    BasicSsd,
    Zonal,
    Regional,
    Enterprise,
    HighScale,
}

impl FsTier {
    fn parse(s: &str) -> Option<FsTier> {
        Some(match s.to_uppercase().as_str() {
            "BASIC_HDD" | "STANDARD" => FsTier::BasicHdd,
            "BASIC_SSD" | "PREMIUM" => FsTier::BasicSsd,
            "ZONAL" => FsTier::Zonal,
            "REGIONAL" => FsTier::Regional,
            "ENTERPRISE" => FsTier::Enterprise,
            "HIGH_SCALE_SSD" => FsTier::HighScale,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            FsTier::BasicHdd => "BASIC_HDD",
            FsTier::BasicSsd => "BASIC_SSD",
            FsTier::Zonal => "ZONAL",
            FsTier::Regional => "REGIONAL",
            FsTier::Enterprise => "ENTERPRISE",
            FsTier::HighScale => "HIGH_SCALE_SSD",
        }
    }

    /// (min, max) provisionable capacity in GiB.
    fn capacity_range(self) -> (f64, f64) {
        match self {
            FsTier::BasicHdd => (1024.0, 65_434.0),
            FsTier::BasicSsd => (2560.0, 65_434.0),
            FsTier::Enterprise => (1024.0, 10_240.0),
            FsTier::HighScale => (10_240.0, 102_400.0),
            FsTier::Zonal | FsTier::Regional => (1024.0, 102_400.0),
        }
    }

    fn capacity_pattern(self) -> &'static str {
        match self {
            FsTier::BasicHdd => "^Filestore Capacity Basic HDD ",
            FsTier::BasicSsd => "^Filestore Capacity Basic SSD ",
            FsTier::Zonal | FsTier::HighScale => "^Filestore Capacity Zonal and High Scale ",
            FsTier::Regional | FsTier::Enterprise => "^Filestore Capacity Regional and Enterprise ",
        }
    }

    /// SKU family for custom performance pricing (None if unsupported).
    fn custom_perf_family(self) -> Option<&'static str> {
        match self {
            FsTier::Zonal => Some("Zonal"),
            FsTier::Regional | FsTier::Enterprise => Some("Regional"),
            _ => None,
        }
    }
}

fn filestore_capacity(ctx: &Ctx, tier: FsTier) -> f64 {
    let total: f64 = ctx.blocks("file_shares").iter().filter_map(|s| s.get("capacity_gb").and_then(|c| c.as_f64())).sum();
    if total > 0.0 {
        return total;
    }
    let min = tier.capacity_range().0;
    ctx.warn(format!("file_shares capacity unknown, assuming the {} minimum of {} GiB", tier.name(), fmt_number(min)));
    min
}

/// Provisioned IOPS when custom performance is configured.
fn filestore_custom_iops(ctx: &Ctx, capacity_gb: f64) -> Option<f64> {
    ctx.num("performance_config.0.fixed_iops.0.max_iops")
        .or_else(|| ctx.num("performance_config.0.iops_per_tb.0.max_iops_per_tb").map(|n| n * capacity_gb / 1024.0))
}

fn filestore_instance(ctx: &Ctx) -> Vec<Component> {
    let raw = ctx.str("tier").unwrap_or_else(|| {
        ctx.warn("tier not set, assuming BASIC_HDD");
        "BASIC_HDD"
    });
    let Some(tier) = FsTier::parse(raw) else {
        ctx.warn(format!("unknown Filestore tier '{raw}'"));
        return vec![];
    };
    let region = ctx.region();
    let cap = filestore_capacity(ctx, tier);
    let q = |pat: &'static str| Query::new(FILESTORE, &region, pat).ondemand();
    let mut out = vec![];

    if let (Some(fam), Some(iops)) = (tier.custom_perf_family(), filestore_custom_iops(ctx, cap)) {
        let (count, capacity, iops_pat) = match fam {
            "Zonal" => ("^Filestore Instance Count Zonal ", "^Filestore Instance Capacity Zonal ", "^Filestore Instance IOPS Zonal "),
            _ => ("^Filestore Instance Count Regional ", "^Filestore Instance Capacity Regional ", "^Filestore Instance IOPS Regional "),
        };
        out.push(ctx.monthly(
            format!("Instance ({}, custom performance)", tier.name()),
            "months",
            ctx.monthly_hours() / HOURS_PER_MONTH,
            q(count),
        ));
        out.push(ctx.monthly(format!("Capacity ({}, {} GiB)", tier.name(), fmt_number(cap)), "GiB-months", cap, q(capacity)));
        out.push(ctx.monthly(format!("Provisioned IOPS ({})", fmt_number(iops.round())), "IOPS-months", iops, q(iops_pat)));
        return out;
    }

    out.push(ctx.monthly(format!("Capacity ({}, {} GiB)", tier.name(), fmt_number(cap)), "GiB-months", cap, q(tier.capacity_pattern())));
    if tier == FsTier::BasicHdd && cap < 1024.0 {
        let mut c = ctx.monthly("Small instance provisioning fee", "months", 1.0, q("^Filestore Small Instance Provisioning Fee "));
        c.note = Some("charged for Basic HDD instances below 1 TiB".into());
        out.push(c);
    }
    out
}

fn filestore_backup(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let mut c =
        ctx.usage("Backup storage", "GiB-months", "storage_gb", Query::new(FILESTORE, &region, "^Filestore Backup Usage ").ondemand());
    let src_region = ctx
        .str("source_instance")
        .and_then(|s| ctx.resolve_ref(s, "google_filestore_instance"))
        .and_then(|i| i.attrs.get("location").and_then(|l| l.as_str()))
        .map(|l| normalize_region(&l.to_lowercase()));
    if let Some(src) = src_region.filter(|s| *s != region) {
        c.note = Some(format!("source instance is in {src}: cross-region backup transfer is charged per GiB"));
    }
    vec![c]
}

// ---- Artifact Registry -------------------------------------------------------

/// Continent label used in Artifact Registry network SKUs, and a region in
/// which those SKUs are listed.
fn ar_continent(region: &str) -> Option<(&'static str, &'static str)> {
    let prefix = region.split('-').next().unwrap_or(region);
    Some(match prefix {
        "us" | "northamerica" => ("North America", "us-central1"),
        "europe" | "eu" => ("Europe", "europe-west1"),
        "asia" => ("AsiaPacfic", "asia-east1"),
        "australia" => ("Oceania", "australia-southeast1"),
        "southamerica" => ("South America", "southamerica-east1"),
        "me" => ("MiddleEast", "me-central1"),
        "africa" => ("Africa", "africa-south1"),
        _ => return None,
    })
}

fn ar_repository(ctx: &Ctx) -> Vec<Component> {
    let mode = ctx.str_or("mode", "STANDARD_REPOSITORY");
    if mode == "VIRTUAL_REPOSITORY" {
        return vec![];
    }
    let mut out = vec![];
    let mut storage = ctx.usage("Storage", "GiB-months", "storage_gb", Query::new(AR, "global", "^Artifact Registry Storage$").ondemand());
    storage.note = Some(if mode == "REMOTE_REPOSITORY" {
        "cached upstream artifacts; first 0.5 GiB per billing account is free".into()
    } else {
        "first 0.5 GiB per billing account is free".into()
    });
    out.push(storage);

    let region = ctx.region();
    let Some((cont, fallback)) = ar_continent(&region) else { return out };
    let internet_pat = format!("^Artifact Registry Network Internet Egress {cont} to {cont}$");
    // Continental SKUs are only listed for some regions (none for multi-regions).
    let sku_region =
        if ctx.price(Query::new(AR, &region, &internet_pat).ondemand()).is_some() { region.clone() } else { fallback.to_string() };
    let inter_pat = format!("^Artifact Registry Network Inter Region Egress {cont} to {cont}$");
    let (inter_pat, inter_region) = if ctx.price(Query::new(AR, &sku_region, &inter_pat).ondemand()).is_some() {
        (inter_pat, sku_region.as_str())
    } else {
        ("^Artifact Registry Network Inter Region Egress Intercontinental \\(Excl Oceania\\)$".to_string(), "global")
    };
    let egress = egress_usage(ctx, "internet", &["same_continent", "internet", "intercontinental", "china", "oceania"]);
    let lines = egress.unwrap_or_else(|| vec![("internet", f64::NAN)]);
    for (dest, gb) in lines {
        let qty = (!gb.is_nan()).then_some(gb);
        let (name, pat, reg) = match dest {
            "same_continent" => ("Data transfer to GCP (other regions, same continent)", inter_pat.clone(), inter_region),
            "intercontinental" => (
                "Internet egress (intercontinental)",
                "^Artifact Registry Network Internet Egress Intercontinental \\(Excl Oceania, Africa and China\\)$".to_string(),
                "global",
            ),
            "china" => {
                ("Internet egress (China)", "^Artifact Registry Network Internet Egress Intercontinental to China$".to_string(), "global")
            }
            "oceania" => (
                "Internet egress (Oceania)",
                "^Artifact Registry Network Internet Egress Intercontinental from/to Oceania$".to_string(),
                "global",
            ),
            _ => ("Internet egress (same continent)", internet_pat.clone(), sku_region.as_str()),
        };
        out.push(ctx.usage_qty(name, "GiB", qty, Query::new(AR, reg, &pat).ondemand()));
    }
    out
}

// ---- Backup and DR -------------------------------------------------------------

fn backup_vault(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    vec![ctx.usage(
        "Backup storage",
        "GiB-months",
        "storage_gb",
        Query::new(BDR, &region, "^BackupDR: Storage - In GCP - Long-Term - Standard - in ").ondemand(),
    )]
}

/// Provisioned size in GiB of a resource protected by a backup plan.
fn protected_size(ctx: &Ctx, resource: &str) -> Option<f64> {
    let r = ctx.resolve_ref(resource, "google_")?;
    let a = &r.attrs;
    let num = |v: &Value, p: &str| v.path(p).and_then(|x| x.as_f64());
    let disk_size =
        |src: &str| ctx.resolve_ref(src, "google_compute").and_then(|d| d.attrs.get("size").and_then(|s| s.as_f64())).unwrap_or(10.0);
    match r.rtype.as_str() {
        "google_compute_instance" => {
            let boot = match a.path("boot_disk.0.source").and_then(|s| s.as_str()) {
                Some(src) => disk_size(src),
                None => num(a, "boot_disk.0.initialize_params.0.size").unwrap_or(10.0),
            };
            let attached: f64 = a
                .get("attached_disk")
                .and_then(|l| l.as_list())
                .map(|l| l.iter().filter_map(|d| d.get("source").and_then(|s| s.as_str())).map(disk_size).sum())
                .unwrap_or(0.0);
            Some(boot + attached)
        }
        "google_compute_disk" | "google_compute_region_disk" => Some(num(a, "size").unwrap_or(10.0)),
        "google_sql_database_instance" => Some(num(a, "settings.0.disk_size").unwrap_or(10.0)),
        "google_filestore_instance" => a
            .get("file_shares")
            .and_then(|l| l.as_list())
            .map(|l| l.iter().filter_map(|s| s.get("capacity_gb").and_then(|c| c.as_f64())).sum()),
        _ => None,
    }
}

fn backup_plan_association(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let rtype = ctx.str_or("resource_type", "compute.googleapis.com/Instance");
    let (label, pat, reg) = match rtype {
        t if t.starts_with("compute.") && t.ends_with("/Disk") => ("Persistent Disk", "^BackupDR: Management - In GCP - DISK$", "global"),
        t if t.starts_with("compute.") => ("Compute Engine VM", "^BackupDR: Management - In GCP - GCE VM$", "global"),
        t if t.starts_with("sqladmin.") => ("Cloud SQL", "^BackupDR: Management - In GCP - Cloud SQL - ", region.as_str()),
        t if t.starts_with("file.") => ("Filestore", "^BackupDR: Management - In GCP - Filestore - ", region.as_str()),
        t if t.starts_with("alloydb.") => ("AlloyDB", "^BackupDR: Management - In GCP - AlloyDB - ", region.as_str()),
        other => {
            ctx.warn(format!("unsupported Backup and DR resource type '{other}'"));
            return vec![];
        }
    };
    let size = ctx.usage_num("protected_gb").or_else(|| ctx.str("resource").and_then(|r| protected_size(ctx, r)));
    let mut c = ctx.usage_qty(format!("Backup management ({label})"), "GiB-months", size, Query::new(BDR, reg, pat).ondemand());
    c.note = Some("billed on the protected resource size; vault storage is priced on the backup vault".into());
    vec![c]
}

/// Parse a duration like "2592000s" into seconds.
fn parse_duration_secs(s: &str) -> Option<f64> {
    s.trim().strip_suffix('s').unwrap_or(s).parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations() {
        assert_eq!(BucketLocation::parse("EU", &[]), BucketLocation::Multi("europe"));
        assert_eq!(BucketLocation::parse("eu", &[]), BucketLocation::Multi("europe"));
        assert_eq!(BucketLocation::parse("US", &[]), BucketLocation::Multi("us"));
        assert_eq!(BucketLocation::parse("EUR4", &[]), BucketLocation::Dual("eur4".into()));
        assert_eq!(BucketLocation::parse("EUROPE-WEST1", &[]), BucketLocation::Region("europe-west1".into()));
        assert_eq!(
            BucketLocation::parse("EU", &["EUROPE-WEST1".into(), "EUROPE-WEST4".into()]),
            BucketLocation::CustomDual(vec!["europe-west1".into(), "europe-west4".into()])
        );
        assert_eq!(BucketLocation::parse("nam4", &[]).ops_scope(), "Dual-Region");
        assert_eq!(BucketLocation::parse("asia1", &[]).continent(), Some("Asia"));
        assert_eq!(BucketLocation::Multi("europe").regional_alternative(Some("us-east1")), "europe-west1");
        assert_eq!(BucketLocation::Multi("europe").regional_alternative(Some("europe-west4")), "europe-west4");
    }

    #[test]
    fn continents() {
        assert_eq!(region_continent("northamerica-northeast1"), Some("Northern America"));
        assert_eq!(region_continent("australia-southeast1"), Some("Oceania"));
        assert_eq!(region_continent("me-central2"), Some("Middle East"));
        assert_eq!(ar_continent("asia-southeast1").map(|c| c.0), Some("AsiaPacfic"));
    }

    #[test]
    fn classes_and_tiers() {
        assert_eq!(StorageClass::parse("regional"), Some(StorageClass::Standard));
        assert_eq!(StorageClass::parse("COLDLINE"), Some(StorageClass::Coldline));
        assert_eq!(StorageClass::parse("RAPID"), None);
        assert_eq!(FsTier::parse("PREMIUM"), Some(FsTier::BasicSsd));
        assert_eq!(FsTier::parse("enterprise").and_then(|t| t.custom_perf_family()), Some("Regional"));
        assert_eq!(parse_duration_secs("2592000s"), Some(2_592_000.0));
    }
}

pub(crate) mod rules {
    use super::*;
    use crate::cost::{CostedResource, Recommendation, Severity};
    use crate::hcl::value::Map;
    use crate::resources::{Env, set_path};

    fn price_str(ctx: &Ctx, p: f64) -> String {
        ctx.unit_price(p)
    }

    fn component_cost(cr: &CostedResource, prefix: &str) -> Option<f64> {
        cr.components.iter().filter(|c| c.name.starts_with(prefix)).filter_map(|c| c.monthly_cost).reduce(|a, b| a + b)
    }

    fn lifecycle_rules<'a>(ctx: &Ctx<'a>) -> Vec<&'a Value> {
        ctx.blocks("lifecycle_rule")
    }

    fn deletes_noncurrent(rule: &Value) -> bool {
        let is_delete = rule.path("action.0.type").and_then(|t| t.as_str()) == Some("Delete");
        let Some(c) = rule.path("condition.0") else { return false };
        let set = |k: &str| c.get(k).is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
        is_delete
            && (c.get("num_newer_versions").and_then(|v| v.as_f64()).is_some_and(|n| n > 0.0)
                || set("days_since_noncurrent_time")
                || set("noncurrent_time_before")
                || c.get("with_state").and_then(|v| v.as_str()) == Some("ARCHIVED"))
    }

    /// Shortest `age` (days) of a Delete lifecycle rule on live objects.
    fn min_delete_age(ctx: &Ctx) -> Option<f64> {
        lifecycle_rules(ctx)
            .into_iter()
            .filter(|r| r.path("action.0.type").and_then(|t| t.as_str()) == Some("Delete"))
            .filter_map(|r| r.path("condition.0.age").and_then(|a| a.as_f64()))
            .reduce(f64::min)
    }

    /// Regions a bucket location serves without inter-region data transfer:
    /// its region(s), or its continent for a multi-region.
    fn serves(loc: &BucketLocation, region: &str) -> bool {
        let pair = |a: &str, b: &str| region == a || region == b;
        match loc {
            BucketLocation::Region(r) => r == region,
            BucketLocation::CustomDual(v) => v.iter().any(|r| r == region),
            BucketLocation::Dual(d) => match d.as_str() {
                "eur4" => pair("europe-north1", "europe-west4"),
                "eur5" => pair("europe-west1", "europe-west2"),
                "eur7" => pair("europe-west2", "europe-west3"),
                "eur8" => pair("europe-west3", "europe-west6"),
                "nam4" => pair("us-central1", "us-east1"),
                "asia1" => pair("asia-northeast1", "asia-northeast2"),
                _ => loc.continent() == region_continent(region),
            },
            BucketLocation::Multi(_) => loc.continent() == region_continent(region),
        }
    }

    /// The region a resource runs in, when it has one (`europe-west1-b` → `europe-west1`).
    fn region_of(r: &crate::tf::evaluate::ResourceInstance) -> Option<String> {
        let s = |k: &str| r.attrs.get(k).and_then(|v| v.as_str()).filter(|v| !v.starts_with(crate::REF_PREFIX));
        let raw = s("zone").or(s("region")).or(s("location")).map(str::to_string).or_else(|| r.provider.region.clone())?;
        let region = crate::pricing::query::normalize_region(&raw.to_lowercase());
        // Real regions only (not `global`, nor multi-regions such as `EU`).
        (region.contains('-') && region.chars().any(|c| c.is_ascii_digit())).then_some(region)
    }

    /// Whether a configuration value mentions the bucket (`my-bucket`, `gs://my-bucket/…`).
    fn mentions(v: &Value, bucket: &str) -> bool {
        match v {
            Value::String(s) => s == bucket || s.strip_prefix("gs://").is_some_and(|p| p == bucket || p.starts_with(&format!("{bucket}/"))),
            Value::List(l) => l.iter().any(|x| mentions(x, bucket)),
            Value::Map(m) => m.values().any(|x| mentions(x, bucket)),
            _ => false,
        }
    }

    /// Resources of the project that use the bucket from a region its location
    /// does not serve: their reads are billed as inter-region data transfer.
    fn cross_region(ctx: &Ctx, loc: &BucketLocation) -> Option<Recommendation> {
        let bucket = ctx.str("name").filter(|n| !n.is_empty() && !n.starts_with(crate::REF_PREFIX))?;
        let mut readers: Vec<(String, String)> = ctx
            .siblings
            .iter()
            .filter(|r| !r.rtype.starts_with("google_storage_") && r.address != ctx.r.address && mentions(&r.attrs, bucket))
            .filter_map(|r| Some((r.address.clone(), region_of(r)?)))
            .filter(|(_, region)| !serves(loc, region))
            .collect();
        if readers.is_empty() {
            return None;
        }
        readers.sort();
        let mut regions: Vec<&str> = readers.iter().map(|(_, r)| r.as_str()).collect();
        regions.dedup();
        let shown: Vec<String> = readers.iter().take(3).map(|(a, r)| format!("{a} ({r})")).collect();
        let more = if readers.len() > 3 { format!(" and {} more", readers.len() - 3) } else { String::new() };
        // With the bytes the bucket sent, an upper bound of the transfer cost.
        let sent = ctx.observed_value("sent_gb");
        let estimate = match (loc, sent) {
            (BucketLocation::Region(src), Some(gb)) if gb > 0.0 => {
                crate::resources::network::inter_region_egress_component(ctx, src, regions[0], Some(gb)).monthly_cost
            }
            _ => None,
        };
        let observed = match (sent, estimate) {
            (Some(gb), Some(cost)) => format!(
                " The bucket sent {} GiB a month over the last 30 days: if those reads come from {}, they cost up to ~{}/mo.",
                crate::hcl::value::fmt_number(gb.round()),
                regions[0],
                ctx.money(cost)
            ),
            (Some(gb), None) => {
                format!(" The bucket sent {} GiB a month over the last 30 days.", crate::hcl::value::fmt_number(gb.round()))
            }
            _ => String::new(),
        };
        Some(ctx.rec(
            "gcs-cross-region",
            if estimate.is_some_and(|c| c > 50.0) { Severity::Medium } else { Severity::Low },
            format!("Used from another region than the bucket ({})", loc.label()),
            format!(
                "{}{more} use{} this bucket from {}, outside {}: every read is billed as inter-region data transfer (and is slower).{observed} Keep the bucket and its readers in the same region: a regional bucket next to them, or the readers in the bucket's region.",
                shown.join(", "),
                if readers.len() == 1 { "s" } else { "" },
                regions.join(", "),
                loc.label()
            ),
            estimate.filter(|c| *c > 1.0),
        ))
    }

    pub fn bucket(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let b = BucketConfig::from_ctx(ctx);
        out.extend(cross_region(ctx, &b.loc));
        let storage_gb = ctx.usage_num("storage_gb");
        let lifecycle = lifecycle_rules(ctx);
        let cur_price = storage_unit_price(ctx, &b.loc, b.class);

        // Lifecycle management / Autoclass.
        if lifecycle.is_empty() && !b.autoclass && matches!(b.class, StorageClass::Standard | StorageClass::Dra) {
            let near = storage_unit_price(ctx, &b.loc, StorageClass::Nearline);
            let cold = storage_unit_price(ctx, &b.loc, StorageClass::Coldline);
            let arch = storage_unit_price(ctx, &b.loc, StorageClass::Archive);
            match (storage_gb, cur_price, near) {
                (Some(gb), Some(cur), Some(near)) => {
                    let s = gb * 0.5 * (cur - near);
                    if s > 1.0 {
                        out.push(ctx.rec(
                            "gcs-no-lifecycle",
                            Severity::Low,
                            "Add lifecycle rules to move or delete cold data",
                            format!(
                                "This bucket has no lifecycle_rule and Autoclass is disabled, so all {} GiB stay in {} storage ({}/GiB-month). If half of it is rarely read, moving it to Nearline after 30 days ({}/GiB-month) saves the amount shown; Coldline/Archive or deletion save more. Add lifecycle_rule {{ action {{ type = \"SetStorageClass\", storage_class = \"NEARLINE\" }} condition {{ age = 30 }} }}, or enable autoclass {{ enabled = true }} if access patterns are unknown. Caveats: Nearline/Coldline/Archive have 30/90/365-day minimum durations and per-GiB retrieval fees.",
                                fmt_number(gb),
                                b.class.sku_name(),
                                price_str(ctx, cur),
                                price_str(ctx, near)
                            ),
                            Some(s),
                        ));
                    }
                }
                (None, Some(cur), Some(near)) if b.class == StorageClass::Standard => {
                    let mut prices = format!("Standard {} vs Nearline {}", price_str(ctx, cur), price_str(ctx, near));
                    if let (Some(c), Some(a)) = (cold, arch) {
                        prices.push_str(&format!(", Coldline {}, Archive {}", price_str(ctx, c), price_str(ctx, a)));
                    }
                    out.push(ctx.rec(
                        "gcs-autoclass-suggest",
                        Severity::Info,
                        "Consider Autoclass or lifecycle rules for this bucket",
                        format!(
                            "No lifecycle_rule is defined and Autoclass is disabled. Per GiB-month: {prices}. Autoclass (autoclass {{ enabled = true }}) moves objects that are not accessed to colder classes automatically, with no retrieval or early-deletion fees (management fee per 1,000 objects). Set storage_gb in the usage file to quantify savings."
                        ),
                        None,
                    ));
                }
                _ => {}
            }
        }

        // Multi/dual-region location in non-production.
        if ctx.env == Env::NonProd && !b.loc.is_regional() {
            let target = b.loc.regional_alternative(ctx.r.provider.region.as_deref());
            let new_loc = BucketLocation::Region(target.clone());
            let new_price = storage_unit_price(ctx, &new_loc, b.class);
            if let (Some(cur), Some(np)) = (cur_price, new_price)
                && np < cur
            {
                let t = target.clone();
                let new = ctx.reprice(|a: &mut Map| {
                    a.insert("location".into(), Value::from(t.to_uppercase()));
                    a.shift_remove("custom_placement_config");
                    a.shift_remove("rpo");
                });
                let savings = ctx.savings_vs(cr.monthly_cost, new);
                if savings.is_some() || storage_gb.is_none() {
                    out.push(ctx.rec(
                        "gcs-multiregion-nonprod",
                        Severity::Low,
                        format!("Use a regional bucket ({target}) for this non-production data"),
                        format!(
                            "{} storage costs {}/GiB-month vs {} in {target} ({:.0}% less), and regional buckets have cheaper Class A operations and no inter-region replication charges. Geo-redundancy is rarely needed outside production. The location of an existing bucket cannot be changed: create a new bucket with location = \"{}\" and copy the data (Storage Transfer Service), or use bucket relocation.",
                            b.loc.label(),
                            price_str(ctx, cur),
                            price_str(ctx, np),
                            (1.0 - np / cur) * 100.0,
                            target.to_uppercase()
                        ),
                        savings,
                    ));
                }
            }
        }

        // Turbo replication in non-production.
        if ctx.env == Env::NonProd && b.turbo {
            let new = ctx.reprice(|a: &mut Map| {
                a.insert("rpo".into(), Value::from("DEFAULT"));
            });
            let savings = ctx.savings_vs(cr.monthly_cost, new);
            out.push(ctx.rec(
                "gcs-turbo-replication-nonprod",
                Severity::Low,
                "Disable turbo replication on this non-production bucket",
                format!(
                    "Turbo replication (rpo = \"ASYNC_TURBO\") doubles the inter-region replication price per GiB written in exchange for a 15-minute RPO, which non-production data rarely needs. Set rpo = \"DEFAULT\".{}",
                    if ctx.usage_num("monthly_replicated_data_gb").is_none() { " Set monthly_replicated_data_gb in the usage file to quantify savings." } else { "" }
                ),
                savings,
            ));
        }

        // Object versioning without noncurrent-version cleanup.
        if ctx.bool_or("versioning.0.enabled", false) && !lifecycle.iter().any(|r| deletes_noncurrent(r)) {
            out.push(ctx.rec(
                "gcs-versioning-no-lifecycle",
                Severity::Medium,
                "Versioned bucket has no rule to delete noncurrent versions",
                "Object versioning is enabled but no lifecycle rule deletes noncurrent versions, so every overwrite or delete keeps a billed copy forever and storage grows unbounded. Add lifecycle_rule { action { type = \"Delete\" } condition { num_newer_versions = 3 } } and/or condition { days_since_noncurrent_time = 30 }. Soft delete already offers short-term recovery.",
                None,
            ));
        }

        // Soft delete retention.
        if b.soft_delete_days > 0.0 {
            let sd_cost = component_cost(cr, "Soft-deleted storage");
            let churny = min_delete_age(ctx).is_some_and(|age| age <= 30.0)
                || ctx.usage_num("soft_delete_storage_gb").zip(storage_gb).is_some_and(|(sd, gb)| gb > 0.0 && sd / gb > 0.1);
            if b.soft_delete_days > 7.0 {
                let savings = sd_cost.map(|c| c * (1.0 - 7.0 / b.soft_delete_days)).filter(|s| *s > 1.0);
                out.push(ctx.rec(
                    "gcs-soft-delete",
                    Severity::Info,
                    format!("Soft delete keeps deleted objects for {} days", fmt_number(b.soft_delete_days.round())),
                    format!(
                        "Soft-deleted objects are billed at the storage class rate for the whole retention period.{} Reduce soft_delete_policy.retention_duration_seconds (default 604800 = 7 days) unless compliance requires longer recovery windows; set it to 0 for scratch/temporary data.",
                        if churny { " This bucket deletes or overwrites data frequently, so the soft-deleted volume can be significant." } else { "" }
                    ),
                    savings,
                ));
            } else if ctx.env == Env::NonProd && churny {
                out.push(ctx.rec(
                    "gcs-soft-delete",
                    Severity::Info,
                    "Disable soft delete on this high-churn non-production bucket",
                    "Objects are deleted or overwritten frequently, and soft delete (on by default for 7 days) bills every deleted byte for the retention period. For temporary non-production data set soft_delete_policy { retention_duration_seconds = 0 }.",
                    sd_cost.filter(|s| *s > 1.0),
                ));
            }
        }
        out
    }

    pub fn filestore(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let Some(tier) = ctx.str("tier").and_then(FsTier::parse) else { return out };
        let cap: f64 = ctx.blocks("file_shares").iter().filter_map(|s| s.get("capacity_gb").and_then(|c| c.as_f64())).sum();
        if cap <= 0.0 {
            return out;
        }

        if ctx.env == Env::NonProd {
            let target = match tier {
                FsTier::Regional | FsTier::Enterprise => Some(FsTier::Zonal),
                FsTier::Zonal | FsTier::HighScale | FsTier::BasicSsd => Some(FsTier::BasicHdd),
                FsTier::BasicHdd => None,
            };
            if let Some(t) = target {
                let (min, max) = t.capacity_range();
                if cap >= min && cap <= max {
                    let new = ctx.reprice(|a: &mut Map| {
                        a.insert("tier".into(), Value::from(t.name()));
                        if t.custom_perf_family().is_none() {
                            a.shift_remove("performance_config");
                        }
                    });
                    if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                        let (sev, why) = if t == FsTier::Zonal {
                            (
                                Severity::Medium,
                                "Regional/Enterprise instances replicate synchronously across zones for high availability, which non-production environments rarely need. ZONAL offers the same SSD performance in a single zone.",
                            )
                        } else {
                            (
                                Severity::Low,
                                "BASIC_HDD is the cheapest tier and is usually enough for dev/test shares; it has lower throughput/IOPS and no snapshots or capacity shrinking.",
                            )
                        };
                        out.push(ctx.rec(
                            "filestore-tier-nonprod",
                            sev,
                            format!("Use the {} tier for this non-production Filestore instance", t.name()),
                            format!("{why} The tier cannot be changed in place: set tier = \"{}\" (instance is recreated) and restore data from a backup.", t.name()),
                            Some(s),
                        ));
                    }
                }
            }
        }

        let (min, _) = tier.capacity_range();
        let shrinkable = matches!(tier, FsTier::Zonal | FsTier::Regional | FsTier::Enterprise | FsTier::HighScale);
        let how = if shrinkable {
            "Reduce file_shares.capacity_gb (these tiers support shrinking in place)."
        } else {
            "Basic tiers cannot shrink in place: create a smaller instance and copy the data (or restore a backup)."
        };
        match ctx.usage_num("used_gb") {
            Some(used) if used < cap * 0.5 && cap > min => {
                let step = if matches!(tier, FsTier::HighScale) { 2560.0 } else { 256.0 };
                let target = ((used * 1.25 / step).ceil() * step).max(min);
                if target < cap {
                    let new = ctx.reprice(|a: &mut Map| set_path(a, "file_shares.0.capacity_gb", Value::from(target)));
                    if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                        out.push(ctx.rec(
                            "filestore-oversized",
                            Severity::Info,
                            format!("Filestore instance is {:.0}% used", used / cap * 100.0),
                            format!(
                                "Filestore bills provisioned capacity, not used capacity: {} GiB used out of {} GiB. Sizing to {} GiB keeps 25% headroom. {how}",
                                fmt_number(used.round()),
                                fmt_number(cap),
                                fmt_number(target)
                            ),
                            Some(s),
                        ));
                    }
                }
            }
            None if cap >= min * 4.0 => out.push(ctx.rec(
                "filestore-oversized",
                Severity::Info,
                "Check the utilization of this large Filestore instance",
                format!(
                    "{} GiB are provisioned ({}/month) and billed whether used or not. Compare with actual usage (metric file.googleapis.com/nfs/server/used_bytes_percent) and set used_gb in the usage file to quantify right-sizing. {how}",
                    fmt_number(cap),
                    ctx.money(cr.monthly_cost)
                ),
                None,
            )),
            _ => {}
        }
        out
    }

    pub fn artifact_registry(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        if ctx.str_or("mode", "STANDARD_REPOSITORY") == "VIRTUAL_REPOSITORY" {
            return vec![];
        }
        let no_policy = ctx.blocks("cleanup_policies").is_empty();
        let dry_run = ctx.bool_or("cleanup_policy_dry_run", false);
        if !no_policy && !dry_run {
            return vec![];
        }
        let savings = component_cost(cr, "Storage").map(|c| c * 0.5).filter(|s| *s > 1.0);
        let (title, what) = if no_policy {
            ("Repository has no cleanup policy", "No cleanup_policies are defined")
        } else {
            ("Cleanup policies run in dry-run mode only", "cleanup_policy_dry_run = true, so cleanup policies never delete anything")
        };
        vec![ctx.rec(
            "ar-no-cleanup-policy",
            Severity::Low,
            title,
            format!(
                "{what}: every pushed image/package version is kept and billed per GiB-month forever. Add cleanup_policies to keep the N most recent versions (action = \"KEEP\", most_recent_versions {{ keep_count = 10 }}) and delete untagged or old versions (action = \"DELETE\", condition {{ tag_state = \"UNTAGGED\", older_than = \"2592000s\" }}).{}",
                if savings.is_some() { " Savings assume cleanup removes half of the stored data." } else { "" }
            ),
            savings,
        )]
    }

    pub fn backup_vault(ctx: &Ctx, _cr: &CostedResource) -> Vec<Recommendation> {
        let Some(secs) = ctx.str("backup_minimum_enforced_retention_duration").and_then(parse_duration_secs) else { return vec![] };
        let days = secs / 86_400.0;
        if ctx.env != Env::NonProd || days <= 30.0 {
            return vec![];
        }
        vec![ctx.rec(
            "bdr-vault-retention-nonprod",
            Severity::Info,
            format!("Non-production backup vault enforces {} days of retention", fmt_number(days.round())),
            "Backups in this vault cannot be deleted before the enforced minimum retention, so storage costs are locked in even when the data is no longer needed. For non-production workloads a shorter backup_minimum_enforced_retention_duration (e.g. \"604800s\") is usually sufficient; note that it can only be reduced while the vault is unlocked.",
            None,
        )]
    }
}
