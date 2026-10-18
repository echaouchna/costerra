//! Managed databases: Cloud SQL, AlloyDB, Spanner, Bigtable, Memorystore
//! (Redis, Redis Cluster, Valkey, Memcached) and Firestore.

use super::machine::COMPUTE;
use super::{Ctx, Env, Spec, set_path, usage};
use crate::cost::{CodeChange, Component, CostedResource, HOURS_PER_MONTH, Recommendation, Severity};
use crate::hcl::value::{Value, fmt_number};
use crate::pricing::query::{Query, normalize_region};
use crate::tf::evaluate::ResourceInstance;

const SQL: &str = "Cloud SQL";
const ALLOYDB: &str = "AlloyDB";
const SPANNER: &str = "Cloud Spanner";
const BIGTABLE: &str = "Cloud Bigtable";
const REDIS: &str = "Cloud Memorystore for Redis";
const MEMCACHE: &str = "Cloud Memorystore for Memcached";
const MEMORYSTORE: &str = "Cloud Memorystore";
const FIRESTORE: &str = "Cloud Firestore";

pub fn specs() -> Vec<Spec> {
    vec![
        Spec::new("google_sql_database_instance", sql_instance)
            .usage(usage!(
                "storage_gb" => "Used storage in GiB when disk autoresize is enabled (billed size = max(disk_size, storage_gb))",
                "backup_storage_gb" => "Backup storage in GiB, including point-in-time recovery transaction logs",
                "monthly_egress_data_transfer_gb" => "Internet egress from the instance in GiB per month",
                "monthly_hrs" => "Hours per month the instance runs (default 730)",
            ))
            .rules(sql_rules),
        Spec::new("google_alloydb_cluster", alloydb_cluster).usage(usage!(
            "storage_gb" => "Database storage in GiB",
            "backup_storage_gb" => "Backup storage in GiB",
            "continuous_backup_storage_gb" => "Continuous backup (point-in-time recovery log) storage in GiB",
        )),
        Spec::new("google_alloydb_instance", alloydb_instance)
            .usage(usage!("monthly_hrs" => "Hours per month the instance runs (default 730)"))
            .rules(alloydb_rules),
        Spec::new("google_spanner_instance", spanner_instance)
            .usage(usage!(
                "storage_gb" => "SSD database storage in GiB (logical, per replica)",
                "hdd_storage_gb" => "HDD (tiered) database storage in GiB",
                "backup_storage_gb" => "Backup storage in GiB",
                "nodes" => "Average number of nodes (overrides num_nodes / processing_units / autoscaling minimum)",
            ))
            .rules(spanner_rules),
        Spec::new("google_bigtable_instance", bigtable_instance)
            .usage(usage!(
                "storage_gb" => "Data stored in GiB (replicated to every cluster)",
                "backup_storage_gb" => "Standard backup storage in GiB",
                "nodes" => "Average number of nodes per cluster (overrides num_nodes / autoscaling minimum)",
            ))
            .rules(bigtable_rules),
        Spec::new("google_redis_instance", redis_instance).rules(redis_rules),
        Spec::new("google_redis_cluster", redis_cluster)
            .usage(usage!("backup_storage_gb" => "Backup storage in GB"))
            .rules(memorystore_cluster_rules),
        Spec::new("google_memorystore_instance", valkey_instance)
            .usage(usage!("backup_storage_gb" => "Backup storage in GB"))
            .rules(memorystore_cluster_rules),
        Spec::new("google_memcache_instance", memcache_instance).rules(memcache_rules),
        Spec::new("google_firestore_database", firestore_database)
            .usage(usage!(
                "storage_gb" => "Stored data in GiB (documents + indexes)",
                "monthly_document_reads" => "Document reads per month (read units for Enterprise edition)",
                "monthly_document_writes" => "Document writes per month (write units for Enterprise edition)",
                "monthly_document_deletes" => "Document deletes per month (billed as write units for Enterprise edition)",
                "pitr_storage_gb" => "Point-in-time recovery storage in GiB (defaults to storage_gb when PITR is enabled)",
                "backup_storage_gb" => "Backup storage in GiB",
            ))
            .rules(firestore_rules),
    ]
}

// ---- shared helpers ---------------------------------------------------------

fn fmt(n: f64) -> String {
    fmt_number(n)
}

/// Component whose unit price is the SKU price multiplied by `factor`
/// (e.g. GiB-hour SKUs shown per GiB-month, per-operation SKUs per 100k).
fn scaled(ctx: &Ctx, name: impl Into<String>, unit: &str, qty: Option<f64>, factor: f64, q: Query, usage_based: bool) -> Component {
    let p = ctx.price(q);
    let mut c = ctx.fixed(name, unit, qty, p.as_ref().map(|p| p.unit_price() * factor));
    c.usage_based = usage_based;
    if let Some(p) = p {
        c.sku = Some(p.sku_id.clone());
        c.sku_description = Some(p.description.clone());
    }
    c
}

/// Storage priced per GiB-hour in the catalog, shown per GiB-month.
fn gib_month(ctx: &Ctx, name: impl Into<String>, gb: Option<f64>, q: Query, usage_based: bool) -> Component {
    scaled(ctx, name, "GiB-months", gb, HOURS_PER_MONTH, q, usage_based)
}

/// Usage-based component with a computed unit price.
fn usage_fixed(ctx: &Ctx, name: impl Into<String>, unit: &str, qty: Option<f64>, unit_price: Option<f64>) -> Component {
    let mut c = ctx.fixed(name, unit, qty, unit_price);
    c.usage_based = true;
    c
}

/// Sum of the monthly cost of components whose name starts with one of `prefixes`.
fn cost_of(cr: &CostedResource, prefixes: &[&str]) -> f64 {
    cost_where(cr, |c| prefixes.iter().any(|p| c.name.starts_with(p)))
}

fn cost_where(cr: &CostedResource, f: impl Fn(&Component) -> bool) -> f64 {
    cr.components.iter().filter(|c| f(c)).filter_map(|c| c.monthly_cost).sum()
}

/// Spend-based CUD discounts (1 year, 3 year) derived from the dollar-based
/// commitment SKUs, which are priced at `(1 - discount) * $0.01` per hour.
fn cud_discounts(ctx: &Ctx, service: &str, region: &str, one_year: &str, three_years: &str) -> Option<(f64, f64)> {
    let d = |pat: &str| ctx.price(Query::new(service, region, pat)).map(|p| 1.0 - p.unit_price() / 0.01).filter(|d| *d > 0.0 && *d < 1.0);
    Some((d(one_year)?, d(three_years)?))
}

fn cud_rec(ctx: &Ctx, id: &str, what: &str, eligible: f64, (d1, d3): (f64, f64)) -> Option<Recommendation> {
    if ctx.env == Env::NonProd || ctx.monthly_hours() < HOURS_PER_MONTH * 0.9 {
        return None;
    }
    let (s1, s3) = (eligible * d1, eligible * d3);
    (s3 > 5.0).then(|| {
        ctx.rec(
            id,
            Severity::Info,
            format!("Cover this always-on {what} with a committed use discount"),
            format!(
                "{:.0}% (1 year) or {:.0}% (3 years) off {}/mo of eligible compute: ~{}/mo or ~{}/mo saved. Commitments are spend-based and apply across the billing account, so size them on your steady-state baseline. Storage, backups and network are not covered.",
                d1 * 100.0,
                d3 * 100.0,
                ctx.money(eligible),
                ctx.money(s1),
                ctx.money(s3)
            ),
            Some(s1),
        )
    })
}

// ---- Cloud SQL --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum SqlSeries {
    Micro,
    Small,
    Custom,
    N4,
    N2,
    MemoryOptimized,
    C4a,
    C4,
}

#[derive(Debug, Clone, PartialEq)]
struct SqlTier {
    series: SqlSeries,
    vcpus: f64,
    memory_gb: f64,
    legacy: bool,
}

impl SqlTier {
    fn enterprise_plus(&self) -> bool {
        matches!(self.series, SqlSeries::N2 | SqlSeries::MemoryOptimized | SqlSeries::C4a | SqlSeries::C4)
    }
    fn shared(&self) -> bool {
        matches!(self.series, SqlSeries::Micro | SqlSeries::Small)
    }
    fn default_disk(&self) -> &'static str {
        match self.series {
            SqlSeries::N4 | SqlSeries::C4a | SqlSeries::C4 => "HYPERDISK_BALANCED",
            _ => "PD_SSD",
        }
    }
    /// Data cache size (GiB) when enabled on Enterprise Plus.
    fn data_cache_gb(&self) -> Option<f64> {
        let c = self.vcpus;
        match self.series {
            SqlSeries::N2 | SqlSeries::MemoryOptimized => Some(if c <= 8.0 {
                375.0
            } else if c <= 16.0 {
                750.0
            } else if c <= 32.0 {
                1500.0
            } else if c <= 48.0 {
                3000.0
            } else {
                6000.0
            }),
            SqlSeries::C4a => match c as u32 {
                0..=2 => None,
                3..=4 => Some(375.0),
                5..=8 => Some(750.0),
                9..=16 => Some(1500.0),
                17..=32 => Some(2250.0),
                33..=48 => Some(3750.0),
                49..=64 => Some(5250.0),
                _ => Some(6000.0),
            },
            _ => None,
        }
    }
    fn sku_suffixes(&self) -> (&'static str, &'static str) {
        match self.series {
            SqlSeries::Micro => ("Micro instance in", ""),
            SqlSeries::Small => ("Small instance in", ""),
            SqlSeries::Custom => ("vCPU in", "RAM in"),
            SqlSeries::N4 => ("Enterprise N4 vCPU in", "Enterprise N4 RAM in"),
            SqlSeries::N2 => ("Enterprise Plus N vCPU in", "Enterprise Plus N RAM in"),
            SqlSeries::MemoryOptimized => ("Enterprise Plus MoN vCPU in", "Enterprise Plus MoN RAM in"),
            SqlSeries::C4a => ("Enterprise Plus (Performance Optimized )?C4A vCPU in", "Enterprise Plus C4A RAM in"),
            SqlSeries::C4 => ("Enterprise Plus C4 vCPU in", "Enterprise Plus C4 RAM in"),
        }
    }
}

/// A tier of the same series with half the vCPUs (and half the memory when
/// `halve_memory`), or None when there is no such tier.
fn smaller_sql_tier(name: &str, tier: &SqlTier, halve_memory: bool) -> Option<String> {
    if tier.shared() || tier.vcpus < 2.0 {
        return None;
    }
    let v = tier.vcpus / 2.0;
    let p: Vec<&str> = name.strip_prefix("db-")?.split('-').collect();
    match p.as_slice() {
        // Custom: 1 or an even number of vCPUs, 0.9 to 6.5 GiB per vCPU, in multiples of 256 MiB.
        ["custom", c, _] if c.parse::<f64>().is_ok() => {
            if v != 1.0 && v % 2.0 != 0.0 {
                return None;
            }
            let mem_gb = if halve_memory { tier.memory_gb / 2.0 } else { tier.memory_gb };
            let mem_mb = (mem_gb * 1024.0 / 256.0).round() * 256.0;
            (0.9..=6.5).contains(&(mem_mb / 1024.0 / v)).then(|| format!("db-custom-{}-{}", fmt(v), fmt(mem_mb)))
        }
        // Memory is proportional to the vCPUs.
        ["n1", kind, _] if halve_memory && [1.0, 2.0, 4.0, 8.0, 16.0, 32.0].contains(&v) => Some(format!("db-n1-{kind}-{}", fmt(v))),
        ["perf", "optimized", s @ ("N" | "n"), _] if halve_memory && [2.0, 4.0, 8.0, 16.0, 32.0, 48.0, 64.0].contains(&v) => {
            Some(format!("db-perf-optimized-{s}-{}", fmt(v)))
        }
        _ => None,
    }
}

/// A larger tier of the same series for an instance short of CPU (twice the
/// vCPUs) and/or memory (twice the memory), or None when there is none.
fn larger_sql_tier(name: &str, tier: &SqlTier, cpu: bool, memory: bool) -> Option<String> {
    if tier.shared() || !(cpu || memory) {
        return None;
    }
    let p: Vec<&str> = name.strip_prefix("db-")?.split('-').collect();
    match p.as_slice() {
        ["custom", c, _] if c.parse::<f64>().is_ok() => {
            let mut v = if cpu { tier.vcpus * 2.0 } else { tier.vcpus };
            let mut mem_gb = if memory { tier.memory_gb * 2.0 } else { tier.memory_gb };
            // 0.9 to 6.5 GiB per vCPU: more vCPUs when the memory needs them.
            while mem_gb / v > 6.5 {
                v = if v == 1.0 { 2.0 } else { v + 2.0 };
            }
            mem_gb = mem_gb.max(v * 0.9);
            let mem_mb = (mem_gb * 1024.0 / 256.0).ceil() * 256.0;
            (v <= 96.0).then(|| format!("db-custom-{}-{}", fmt(v), fmt(mem_mb)))
        }
        ["n1", "standard", _] if memory && !cpu => Some(format!("db-n1-highmem-{}", fmt(tier.vcpus))),
        ["n1", kind, _] if tier.vcpus * 2.0 <= 64.0 => Some(format!("db-n1-{kind}-{}", fmt(tier.vcpus * 2.0))),
        ["perf", "optimized", s @ ("N" | "n"), _] => {
            let v = tier.vcpus * 2.0;
            [4.0, 8.0, 16.0, 32.0, 64.0, 128.0].contains(&v).then(|| format!("db-perf-optimized-{s}-{}", fmt(v)))
        }
        _ => None,
    }
}

fn parse_sql_tier(tier: &str) -> Option<SqlTier> {
    let t = tier.strip_prefix("db-")?;
    let tier = |series, vcpus: f64, memory_gb: f64| Some(SqlTier { series, vcpus, memory_gb, legacy: false });
    match t {
        "f1-micro" => return tier(SqlSeries::Micro, 1.0, 0.6),
        "g1-small" => return tier(SqlSeries::Small, 1.0, 1.7),
        _ => {}
    }
    let p: Vec<&str> = t.split('-').collect();
    let num = |i: usize| p.get(i).and_then(|s| s.parse::<f64>().ok());
    match p.as_slice() {
        ["custom", "N4", ..] | ["custom", "n4", ..] => tier(SqlSeries::N4, num(2)?, num(3)? / 1024.0),
        ["custom", ..] => tier(SqlSeries::Custom, num(1)?, num(2)? / 1024.0),
        ["n1", kind, _] => {
            let c = num(2)?;
            let per = match *kind {
                "standard" => 3.75,
                "highmem" => 6.5,
                _ => return None,
            };
            Some(SqlTier { series: SqlSeries::Custom, vcpus: c, memory_gb: c * per, legacy: true })
        }
        ["perf", "optimized", "N" | "n", _] => {
            let c = num(3)?;
            tier(SqlSeries::N2, c, if c >= 128.0 { 864.0 } else { c * 8.0 })
        }
        ["perf", "optimized", "C4" | "c4", _] => {
            let c = num(3)?;
            tier(SqlSeries::C4, c, if c == 2.0 { 15.0 } else { c * 7.75 })
        }
        ["memory", "optimized", "N" | "n", _] => {
            let c = num(3)?;
            tier(SqlSeries::MemoryOptimized, c, c * 32.0)
        }
        ["c4a", "highmem", ..] => {
            let c = num(2)?;
            tier(SqlSeries::C4a, c, c * 8.0)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SqlEngine {
    MySql,
    Postgres,
    SqlServer,
}

impl SqlEngine {
    fn of(version: &str) -> Option<SqlEngine> {
        let v = version.to_uppercase();
        if v.starts_with("MYSQL") {
            Some(SqlEngine::MySql)
        } else if v.starts_with("POSTGRES") {
            Some(SqlEngine::Postgres)
        } else if v.starts_with("SQLSERVER") {
            Some(SqlEngine::SqlServer)
        } else {
            None
        }
    }
    fn sku_name(self) -> &'static str {
        match self {
            SqlEngine::MySql => "Cloud SQL for (MySQL|My SQL)",
            SqlEngine::Postgres => "Cloud SQL for (PostgreSQL|Postgres)",
            SqlEngine::SqlServer => "Cloud SQL for SQL Server",
        }
    }
}

/// Version tag used by extended support SKUs (`v13`, `v96`, `v57`).
fn sql_version_tag(version: &str) -> Option<String> {
    let v = version.to_uppercase();
    let parts: Vec<&str> = v.split('_').collect();
    match parts.as_slice() {
        ["POSTGRES", "9", minor, ..] => Some(format!("9{minor}")),
        ["POSTGRES", major, ..] => Some(major.to_string()),
        ["MYSQL", major, minor, ..] => Some(format!("{major}{minor}")),
        _ => None,
    }
}

/// Major version number (e.g. 16 for POSTGRES_16, 8.4 for MYSQL_8_4).
fn sql_major(version: &str) -> f64 {
    let parts: Vec<&str> = version.split('_').collect();
    let n = |i: usize| parts.get(i).and_then(|s| s.parse::<f64>().ok());
    match (n(1), n(2)) {
        (Some(a), Some(b)) if version.to_uppercase().starts_with("MYSQL") => a + b / 10.0,
        (Some(a), _) => a,
        _ => 0.0,
    }
}

/// SQL Server (year, edition) from SQLSERVER_2019_STANDARD.
fn sqlserver_license(version: &str) -> Option<(&str, String)> {
    let mut p = version.split('_').skip(1);
    let year = p.next()?;
    let ed = p.next()?;
    let ed = match ed.to_uppercase().as_str() {
        "STANDARD" => "Standard",
        "ENTERPRISE" => "Enterprise",
        "WEB" => "Web",
        "EXPRESS" => "Express",
        _ => return None,
    };
    Some((year, ed.to_string()))
}

struct SqlConf<'a> {
    engine: SqlEngine,
    version: &'a str,
    tier_name: &'a str,
    tier: SqlTier,
    edition: String,
    regional: bool,
}

impl SqlConf<'_> {
    fn plus(&self) -> bool {
        self.edition == "ENTERPRISE_PLUS"
    }
    fn prefix(&self) -> String {
        format!("^ ?{}: {} ?- ", self.engine.sku_name(), if self.regional { "Regional" } else { "Zonal" })
    }
}

fn sql_conf<'a>(ctx: &Ctx<'a>) -> Option<SqlConf<'a>> {
    let Some(version) = ctx.str("database_version") else {
        ctx.warn("database_version is not known statically");
        return None;
    };
    let Some(engine) = SqlEngine::of(version) else {
        ctx.warn(format!("unknown database_version '{version}'"));
        return None;
    };
    let Some(tier_name) = ctx.str("settings.0.tier") else {
        ctx.warn("settings.tier is not known statically");
        return None;
    };
    let Some(tier) = parse_sql_tier(tier_name) else {
        ctx.warn(format!("unknown Cloud SQL tier '{tier_name}'"));
        return None;
    };
    let inferred = if tier.enterprise_plus() { "ENTERPRISE_PLUS" } else { "ENTERPRISE" };
    let edition = match ctx.str("settings.0.edition") {
        Some(e) => {
            if e != inferred {
                ctx.warn(format!("tier {tier_name} is not available in the {e} edition; pricing it as {inferred}"));
            }
            inferred.to_string()
        }
        None => {
            let defaults_plus = match engine {
                SqlEngine::Postgres => sql_major(version) >= 16.0,
                SqlEngine::MySql => sql_major(version) >= 8.4,
                SqlEngine::SqlServer => false,
            };
            if defaults_plus && !tier.enterprise_plus() {
                ctx.warn(format!(
                    "settings.edition is not set: new {version} instances default to ENTERPRISE_PLUS, which does not support tier {tier_name}; set settings.edition = \"ENTERPRISE\" explicitly (priced as ENTERPRISE)"
                ));
            }
            inferred.to_string()
        }
    };
    if engine == SqlEngine::SqlServer && tier.shared() {
        ctx.warn("shared-core tiers are not available for SQL Server");
    }
    let regional = ctx.str_or("settings.0.availability_type", "ZONAL") == "REGIONAL";
    Some(SqlConf { engine, version, tier_name, tier, edition, regional })
}

fn sql_replica(ctx: &Ctx) -> bool {
    ctx.str("master_instance_name").is_some() || ctx.is_unknown("master_instance_name")
}

fn sql_q<'a>(region: &'a str, pat: &'a str) -> Query<'a> {
    Query::new(SQL, region, pat).ondemand()
}

fn sql_instance(ctx: &Ctx) -> Vec<Component> {
    let Some(conf) = sql_conf(ctx) else { return vec![] };
    let region = ctx.region();
    let prefix = conf.prefix();
    let tier = &conf.tier;
    let av = if conf.regional { "regional" } else { "zonal" };
    let running = ctx.str_or("settings.0.activation_policy", "ALWAYS") != "NEVER";
    let replica = sql_replica(ctx);
    let mut out = vec![];

    if running {
        let (cpu_sfx, ram_sfx) = tier.sku_suffixes();
        if tier.shared() {
            out.push(ctx.hourly(format!("Instance ({}, {av})", conf.tier_name), 1.0, sql_q(&region, &format!("{prefix}{cpu_sfx}"))));
        } else {
            out.push(ctx.hourly_unit(
                format!("Instance vCPUs ({}, {av})", conf.tier_name),
                "vCPU-hours",
                tier.vcpus,
                sql_q(&region, &format!("{prefix}{cpu_sfx}")),
            ));
            out.push(ctx.hourly_unit(
                format!("Instance memory ({} GiB, {av})", fmt(tier.memory_gb)),
                "GiB-hours",
                tier.memory_gb,
                sql_q(&region, &format!("{prefix}{ram_sfx}")),
            ));
        }
        if replica && let Some(c) = out.first_mut() {
            c.note = Some("read replica: billed as a full instance".into());
        }
        if let Some(c) = sql_extended_support(ctx, &conf, &region) {
            out.push(c);
        }
        if let Some(c) = sql_license(ctx, &conf) {
            out.push(c);
        }
    }

    let disk_type = ctx.str_or("settings.0.disk_type", tier.default_disk());
    let provisioned = ctx.num_or("settings.0.disk_size", 10.0);
    let autoresize = ctx.bool_or("settings.0.disk_autoresize", true);
    let size = match ctx.usage_num("storage_gb") {
        Some(u) if autoresize => provisioned.max(u),
        _ => provisioned,
    };
    let plus = conf.plus();
    let label = |kind: &str| format!("Storage ({kind}, {} GiB{})", fmt(size), if conf.regional { ", regional" } else { "" });
    let mut storage = match disk_type {
        "PD_HDD" => {
            if plus {
                ctx.warn("Enterprise Plus does not support PD_HDD storage");
            }
            vec![ctx.monthly(label("HDD"), "GiB-months", size, sql_q(&region, &format!("{prefix}Low cost storage in")))]
        }
        "HYPERDISK_BALANCED" => {
            let stem = if plus { "Enterprise Plus Standard Storage HyperDisk" } else { "Enterprise Storage Hyperdisk Balanced" };
            let mut v =
                vec![ctx.monthly(label("Hyperdisk Balanced"), "GiB-months", size, sql_q(&region, &format!("{prefix}{stem} Capacity in")))];
            let iops = ctx.num("settings.0.data_disk_provisioned_iops").unwrap_or(3000.0);
            let tput = ctx.num("settings.0.data_disk_provisioned_throughput").unwrap_or(140.0);
            if iops > 3000.0 {
                v.push(ctx.monthly(
                    "Provisioned IOPS (above 3,000 baseline)",
                    "IOPS-months",
                    iops - 3000.0,
                    sql_q(&region, &format!("{prefix}{stem} IOPS in")),
                ));
            }
            if tput > 140.0 {
                v.push(ctx.monthly(
                    "Provisioned throughput (above 140 MiB/s baseline)",
                    "MiBps-months",
                    tput - 140.0,
                    sql_q(&region, &format!("{prefix}{stem} Throughput in")),
                ));
            }
            v
        }
        other => {
            if other != "PD_SSD" {
                ctx.warn(format!("unknown disk_type '{other}', priced as PD_SSD"));
            }
            let stem = if plus { "Enterprise Plus Standard Storage in" } else { "Standard storage in" };
            vec![ctx.monthly(label("SSD"), "GiB-months", size, sql_q(&region, &format!("{prefix}{stem}")))]
        }
    };
    if autoresize && let Some(c) = storage.first_mut() {
        c.note = Some("disk autoresize is enabled: the billed size grows with data (set usage storage_gb)".into());
    }
    out.append(&mut storage);

    if running && plus && ctx.bool_or("settings.0.data_cache_config.0.data_cache_enabled", false) {
        match tier.data_cache_gb() {
            Some(gb) => {
                let stem = if matches!(tier.series, SqlSeries::C4 | SqlSeries::C4a) {
                    "Enterprise Plus C4 Data Cache Storage in"
                } else {
                    "Enterprise Plus Data Cache Storage in"
                };
                out.push(ctx.monthly(
                    format!("Data cache ({} GiB)", fmt(gb)),
                    "GiB-months",
                    gb,
                    sql_q(&region, &format!("{prefix}{stem}")),
                ));
            }
            None => ctx.warn(format!("data cache is not available for tier {}", conf.tier_name)),
        }
    }

    if !running && ctx.bool_or("settings.0.ip_configuration.0.ipv4_enabled", true) {
        let mut c = ctx.hourly("Idle IPv4 address", 1.0, sql_q(&region, &format!("{prefix}IP address reservation in")));
        c.note = Some("instance is stopped (activation_policy = NEVER)".into());
        out.push(c);
    }

    if !replica {
        out.push(ctx.usage("Backups", "GiB-months", "backup_storage_gb", sql_q(&region, "^Cloud SQL: Backups in")));
    }
    out.push(ctx.usage(
        "Internet egress",
        "GiB",
        "monthly_egress_data_transfer_gb",
        sql_q(&region, "^Network Internet Data Transfer Out from "),
    ));
    out
}

fn sql_extended_support(ctx: &Ctx, conf: &SqlConf, region: &str) -> Option<Component> {
    let tag = sql_version_tag(conf.version)?;
    let prefix = conf.prefix();
    let find = |pat: String| ctx.price(Query::new(SQL, region, &pat).ondemand()).is_some().then_some(pat);
    let (count, pat) = match conf.tier.series {
        SqlSeries::Micro => (1.0, find(format!("{prefix}Extended support f1-micro v{tag} in"))?),
        SqlSeries::Small => (1.0, find(format!("{prefix}Extended support g1-small v{tag} in"))?),
        _ => {
            let plus = if conf.plus() { find(format!("{prefix}Enterprise Plus Extended support vCPU v{tag} in")) } else { None };
            (conf.tier.vcpus, plus.or_else(|| find(format!("{prefix}Extended support vCPU v{tag} in")))?)
        }
    };
    let unit = if conf.tier.shared() { "hours" } else { "vCPU-hours" };
    let mut c = ctx.hourly_unit(format!("Extended support ({})", conf.version), unit, count, Query::new(SQL, region, &pat).ondemand());
    c.note = Some("major version is past end of standard support".into());
    Some(c)
}

fn sql_license(ctx: &Ctx, conf: &SqlConf) -> Option<Component> {
    if conf.engine != SqlEngine::SqlServer {
        return None;
    }
    let Some((year, ed)) = sqlserver_license(conf.version) else {
        ctx.warn(format!("unknown SQL Server edition in '{}'", conf.version));
        return None;
    };
    if ed == "Express" {
        return None;
    }
    let vcpus = conf.tier.vcpus;
    let bucket = if vcpus <= 4.0 { "(up to|1 to) 4 VCPU" } else { "more than 4 VCPU" };
    let pat = |y: &str| format!("^Licensing Fee for (SQL Server {y} {ed} for Cloud SQL|CloudSQL SQL Server {y} {ed}) on VM with {bucket}");
    let (p_year, p_2022) = (pat(year), pat("2022"));
    let chosen = if ctx.price(Query::new(COMPUTE, "global", &p_year)).is_some() { &p_year } else { &p_2022 };
    let name = format!("SQL Server {ed} license ({} vCPU)", fmt(vcpus.max(4.0)));
    let mut c = if vcpus <= 4.0 {
        ctx.hourly(name, 1.0, Query::new(COMPUTE, "global", chosen))
    } else {
        ctx.hourly_unit(name, "vCPU-hours", vcpus, Query::new(COMPUTE, "global", chosen))
    };
    c.note = Some("4 vCPU minimum; HA instances pay a single license".into());
    Some(c)
}

fn sql_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let Some(conf) = sql_conf(ctx) else { return out };
    let tier = &conf.tier;
    let nonprod = ctx.env == Env::NonProd;
    let running = ctx.str_or("settings.0.activation_policy", "ALWAYS") != "NEVER";
    let replica = sql_replica(ctx);

    if nonprod && conf.regional {
        let new = ctx.reprice(|a| set_path(a, "settings.0.availability_type", "ZONAL".into()));
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "sql-ha-nonprod",
                Severity::Medium,
                "Disable high availability on this non-production Cloud SQL instance",
                "Regional (HA) instances pay for a standby: vCPU, memory and storage are billed twice. Non-production databases rarely need automatic zonal failover. Set settings.availability_type = \"ZONAL\" (causes a restart).",
                Some(s),
            ));
        }
    }

    if nonprod && conf.plus() && running {
        let vcpus = tier.vcpus.clamp(1.0, 96.0);
        let vcpus = if vcpus > 1.0 && vcpus % 2.0 != 0.0 { vcpus + 1.0 } else { vcpus };
        let mem_mb = ((tier.memory_gb.min(vcpus * 6.5) * 1024.0) / 256.0).floor() * 256.0;
        let new_tier = format!("db-custom-{}-{}", fmt(vcpus), fmt(mem_mb.max(3840.0)));
        let hyperdisk = ctx.str("settings.0.disk_type") == Some("HYPERDISK_BALANCED") || tier.default_disk() == "HYPERDISK_BALANCED";
        let new = ctx.reprice(|a| {
            set_path(a, "settings.0.edition", "ENTERPRISE".into());
            set_path(a, "settings.0.tier", Value::from(new_tier.as_str()));
            set_path(a, "settings.0.data_cache_config", Value::Null);
            if hyperdisk {
                set_path(a, "settings.0.disk_type", "PD_SSD".into());
            }
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "sql-enterprise-plus-nonprod",
                Severity::Medium,
                format!("Use the Enterprise edition ({new_tier}) for this non-production instance"),
                "Enterprise Plus adds a 99.99% SLA, data cache and faster maintenance at a ~30% compute premium, which non-production workloads rarely need. Set settings.edition = \"ENTERPRISE\" with a db-custom tier (edition changes require a restart; data cache and C4A/Hyperdisk options are Enterprise Plus only).",
                Some(s),
            ));
        }
    }

    if nonprod
        && running
        && conf.edition == "ENTERPRISE"
        && conf.engine != SqlEngine::SqlServer
        && !tier.shared()
        && tier.vcpus <= 2.0
        && tier.memory_gb <= 8.0
    {
        let new = ctx.reprice(|a| set_path(a, "settings.0.tier", "db-g1-small".into()));
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "sql-shared-core-dev",
                Severity::Low,
                format!("Use a shared-core tier instead of {}", conf.tier_name),
                "Small dev/test databases often fit a shared-core tier (db-g1-small: 1.7 GB RAM, or db-f1-micro). Shared-core instances have no SLA and limited connections, so validate the workload first. Change settings.tier.",
                Some(s),
            ));
        }
    }

    if tier.legacy {
        out.push(ctx.rec(
            "sql-legacy-tier",
            Severity::Low,
            format!("Replace the legacy tier {}", conf.tier_name),
            format!(
                "db-n1-* tiers are legacy MySQL machine types. Move to the equivalent db-custom-{}-{} (same price, right-sizable memory) or to the N4 series (db-custom-N4-*) for better price-performance.",
                fmt(tier.vcpus),
                fmt((tier.memory_gb * 1024.0 / 256.0).round() * 256.0)
            ),
            None,
        ));
    }

    let ext = cost_of(cr, &["Extended support"]);
    if ext > 1.0 {
        out.push(ctx.rec(
            "sql-extended-support",
            Severity::Medium,
            format!("Upgrade {} to a supported major version", conf.version),
            format!("This version is past the end of standard support and incurs an extended support fee of ~{}/mo. Plan a major version upgrade (in-place upgrade via database_version) after testing application compatibility.", ctx.money(ext)),
            Some(ext),
        ));
    }

    if nonprod && replica && cr.monthly_cost > 5.0 {
        out.push(ctx.rec(
            "sql-replica-nonprod",
            Severity::Low,
            "Remove this read replica from non-production",
            "Read replicas are billed as full instances. Non-production environments can usually read from the primary instance.",
            Some(cr.monthly_cost),
        ));
    }

    let disk_type = ctx.str_or("settings.0.disk_type", tier.default_disk());
    let size = ctx.num_or("settings.0.disk_size", 10.0);
    if nonprod && disk_type == "PD_SSD" && !conf.plus() && size >= 100.0 {
        let new = ctx.reprice(|a| set_path(a, "settings.0.disk_type", "PD_HDD".into()));
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "sql-hdd-vs-ssd",
                Severity::Info,
                "HDD storage would be cheaper for this non-production instance",
                "PD_HDD costs ~47% less per GiB than PD_SSD but has much lower IOPS and higher latency. Only suitable for large, rarely queried datasets. disk_type cannot be changed in place: it requires a new instance.",
                Some(s),
            ));
        }
    }

    if size >= 1000.0 && !ctx.bool_or("settings.0.disk_autoresize", true) {
        let storage = cost_of(cr, &["Storage"]);
        out.push(ctx.rec(
            "sql-oversized-disk",
            Severity::Info,
            format!("Check the utilization of this {} GiB disk", fmt(size)),
            format!(
                "The disk is provisioned at {} GiB with autoresize off ({}/mo). Storage can't be reduced by editing disk_size (shrinking requires a dedicated operation with downtime): if actual usage is much lower, plan a shrink and enable disk_autoresize with a disk_autoresize_limit instead of over-provisioning.",
                fmt(size),
                ctx.money(storage)
            ),
            None,
        ));
    }

    // Upsize: an instance short of CPU or memory.
    let cpu_obs = ctx.observed_cpu().filter(|c| c.days >= 7.0);
    let mem_obs = ctx.observed_memory().filter(|m| m.days >= 7.0);
    let cpu_bound = cpu_obs.is_some_and(|c| c.p95 >= 0.85);
    let mem_bound = mem_obs.is_some_and(|m| m.p95 >= 0.9);
    if running
        && (cpu_bound || mem_bound)
        && let Some(new_tier) = larger_sql_tier(conf.tier_name, tier, cpu_bound, mem_bound)
    {
        let extra = ctx.reprice(|a| set_path(a, "settings.0.tier", Value::from(new_tier.as_str()))).map(|n| n - cr.monthly_cost);
        let short = match (cpu_bound, mem_bound) {
            (true, true) => "CPU and memory",
            (true, false) => "CPU",
            _ => "memory",
        };
        let stat = |label: &str, s: Option<crate::observed::CpuStats>| {
            s.map(|s| format!("{label} mean {:.1}%, 95th percentile {:.1}%", s.mean * 100.0, s.p95 * 100.0))
        };
        let usage: Vec<String> = [stat("CPU", cpu_obs), stat("memory", mem_obs)].into_iter().flatten().collect();
        let mut rec = ctx.rec(
            "sql-upsize",
            if mem_bound { Severity::High } else { Severity::Medium },
            format!("Upsize {} to {new_tier}: it is short of {short}", conf.tier_name),
            format!(
                "Use over {:.0} days: {} (suggested from a CPU 95th percentile of 85% or a memory 95th percentile of 90%). A saturated database slows every query (and runs out of memory under load). The larger tier costs more. Changing the tier restarts the instance.",
                cpu_obs.or(mem_obs).map_or(0.0, |s| s.days),
                usage.join(", ")
            ),
            extra.map(|e| -e),
        );
        rec.change = Some(CodeChange { attribute: "settings.0.tier".into(), from: conf.tier_name.to_string(), to: new_tier, unset: false });
        out.push(rec);
    }

    // Rightsize: half the vCPUs of an instance that uses little of its CPU
    // (and half the memory when its memory use is low too).
    if running
        && !(cpu_bound || mem_bound)
        && let Some(c) = cpu_obs.filter(|c| c.p95 < 0.35 && c.mean < 0.2)
    {
        let mem = mem_obs;
        let halve_memory = mem.is_some_and(|m| m.p95 < 0.5);
        if let Some(new_tier) = smaller_sql_tier(conf.tier_name, tier, halve_memory)
            && let Some(s) =
                ctx.savings_vs(cr.monthly_cost, ctx.reprice(|a| set_path(a, "settings.0.tier", Value::from(new_tier.as_str()))))
        {
            let memory = match mem {
                Some(m) => format!(", memory mean {:.1}%, 95th percentile {:.1}%", m.mean * 100.0, m.p95 * 100.0),
                None => String::new(),
            };
            let mut rec = ctx.rec(
                "sql-rightsize",
                if s > 100.0 { Severity::Medium } else { Severity::Low },
                format!("Downsize {} to {new_tier}", conf.tier_name),
                format!(
                    "Use over {:.0} days: CPU mean {:.1}%, 95th percentile {:.1}%{memory} (suggested below a CPU mean of 20% and 95th percentile of 35%; memory halved too below a 95th percentile of 50%). Half the vCPUs would still leave headroom{}. Changing the tier restarts the instance.",
                    c.days,
                    c.mean * 100.0,
                    c.p95 * 100.0,
                    if halve_memory { " (and half the memory, which is little used)" } else { "; the memory is kept" }
                ),
                Some(s),
            );
            rec.change =
                Some(CodeChange { attribute: "settings.0.tier".into(), from: conf.tier_name.to_string(), to: new_tier, unset: false });
            out.push(rec);
        }
    }

    if running {
        let region = ctx.region();
        let pat = |y: &str| format!("^Commitment - dollar based v1: Cloud SQL database {region} for {y}");
        if let Some(d) = cud_discounts(ctx, SQL, &region, &pat("1 year"), &pat("3 years")) {
            out.extend(cud_rec(ctx, "sql-cud", "Cloud SQL instance", cost_of(cr, &["Instance"]), d));
        }
    }
    out
}

// ---- AlloyDB ----------------------------------------------------------------

fn alloydb_cluster_of<'a>(ctx: &Ctx<'a>) -> Option<&'a ResourceInstance> {
    ctx.str("cluster").and_then(|c| ctx.resolve_ref(c, "google_alloydb_cluster"))
}

fn alloydb_trial(cluster: &ResourceInstance) -> bool {
    cluster.attrs.get("subscription_type").and_then(|v| v.as_str()) == Some("TRIAL")
}

fn alloydb_cluster(ctx: &Ctx) -> Vec<Component> {
    if ctx.str("subscription_type") == Some("TRIAL") {
        ctx.warn("AlloyDB free trial cluster: no charge during the trial");
        return vec![];
    }
    let region = ctx.region();
    let q = |pat: &'static str| Query::new(ALLOYDB, &region, pat).ondemand();
    let mut out = vec![
        gib_month(ctx, "Database storage", ctx.usage_num("storage_gb"), q("^AlloyDB Cluster Database Storage in"), true),
        gib_month(ctx, "Backup storage", ctx.usage_num("backup_storage_gb"), q("^AlloyDB Backup Storage in"), true),
    ];
    if ctx.bool_or("continuous_backup_config.0.enabled", true) {
        out.push(gib_month(
            ctx,
            "Continuous backup storage (PITR logs)",
            ctx.usage_num("continuous_backup_storage_gb"),
            q("^AlloyDB backup storage for point-in-time recovery logs in"),
            true,
        ));
    }
    out
}

fn alloydb_instance(ctx: &Ctx) -> Vec<Component> {
    let cluster = alloydb_cluster_of(ctx);
    if cluster.is_some_and(alloydb_trial) {
        ctx.warn("instance of an AlloyDB free trial cluster: no charge during the trial");
        return vec![];
    }
    let region = cluster
        .and_then(|c| c.attrs.get("location").and_then(|v| v.as_str()).map(str::to_string))
        .or_else(|| {
            let c = ctx.str("cluster")?;
            let (_, rest) = c.split_once("/locations/")?;
            rest.split('/').next().map(str::to_string)
        })
        .unwrap_or_else(|| ctx.region());
    ctx.set_location(region.clone());
    let itype = ctx.str_or("instance_type", "PRIMARY");
    let machine = ctx.str("machine_config.0.machine_type");
    let mt = machine.and_then(super::machine::parse);
    let vcpus = ctx.num("machine_config.0.cpu_count").or(mt.as_ref().map(|m| m.vcpus)).unwrap_or_else(|| {
        ctx.warn("machine_config.cpu_count not set, assuming 2 vCPUs");
        2.0
    });
    let memory = mt.as_ref().filter(|m| m.vcpus == vcpus).map(|m| m.memory_gb).unwrap_or(vcpus * 8.0);
    let (nodes, label) = match itype {
        "READ_POOL" => {
            let n = ctx.num_or("read_pool_config.0.node_count", 1.0);
            (n, format!("read pool, {} node{}", fmt(n), if n == 1.0 { "" } else { "s" }))
        }
        _ => {
            if ctx.str_or("availability_type", "REGIONAL") == "REGIONAL" {
                (2.0, "HA: active + standby".to_string())
            } else {
                (1.0, "zonal".to_string())
            }
        }
    };
    let (cpu_pat, ram_pat) = match machine {
        Some(m) if m.starts_with("c4-") => {
            ("^AlloyDB Instance vCPU Cores for C4 LSSD instances in", "^AlloyDB Instance RAM for C4 LSSD instances in")
        }
        Some(m) if m.starts_with("z3-") && m.contains("highlssd") => {
            ("^AlloyDB Instance vCPU Cores for Z3 HighLSSD instances in", "^AlloyDB Instance RAM for Z3 Highmem High LSSD instances in")
        }
        Some(m) if m.starts_with("z3-") => (
            "^AlloyDB Instance vCPU Cores for Z3 StandardLSSD instances in",
            "^AlloyDB Instance RAM for Z3 Highmem Standard LSSD instances in",
        ),
        _ => ("^AlloyDB Instance vCPU Cores in", "^AlloyDB Instance RAM Usage in"),
    };
    vec![
        ctx.hourly_unit(
            format!("vCPUs ({} vCPU, {label})", fmt(vcpus)),
            "vCPU-hours",
            vcpus * nodes,
            Query::new(ALLOYDB, &region, cpu_pat).ondemand(),
        ),
        ctx.hourly_unit(
            format!("Memory ({} GiB, {label})", fmt(memory)),
            "GiB-hours",
            memory * nodes,
            Query::new(ALLOYDB, &region, ram_pat).ondemand(),
        ),
    ]
}

fn alloydb_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let itype = ctx.str_or("instance_type", "PRIMARY");
    if ctx.env == Env::NonProd && itype != "READ_POOL" && ctx.str_or("availability_type", "REGIONAL") == "REGIONAL" {
        let new = ctx.reprice(|a| {
            a.insert("availability_type".into(), "ZONAL".into());
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "alloydb-ha-nonprod",
                Severity::Medium,
                "Use a zonal (basic) AlloyDB instance in non-production",
                "Regional AlloyDB primaries run an active and a standby node, both billed. Set availability_type = \"ZONAL\" for dev/test clusters that can tolerate a zonal outage.",
                Some(s),
            ));
        }
    }
    if ctx.env == Env::NonProd && itype == "READ_POOL" && cr.monthly_cost > 5.0 {
        out.push(ctx.rec(
            "alloydb-read-pool-nonprod",
            Severity::Low,
            "Remove or shrink this non-production read pool",
            "Each read pool node is billed like a primary node. Non-production workloads can usually read from the primary instance, or use a single node.",
            Some(cr.monthly_cost),
        ));
    }
    let region = ctx.region();
    if let Some(d) = cud_discounts(
        ctx,
        ALLOYDB,
        &region,
        "^Commitment - dollar based v1: AlloyDB for 1 year",
        "^Commitment - dollar based v1: AlloyDB for 3 years",
    ) {
        out.extend(cud_rec(ctx, "alloydb-cud", "AlloyDB instance", cr.monthly_cost, d));
    }
    out
}

// ---- Spanner ----------------------------------------------------------------

/// Replicas of a base instance configuration: (region, read-write, read-only, witness).
type Replicas = &'static [(&'static str, u8, u8, u8)];

const SPANNER_CONFIGS: &[(&str, Replicas)] = &[
    ("asia1", &[("asia-northeast1", 2, 0, 0), ("asia-northeast2", 2, 0, 0), ("asia-northeast3", 0, 0, 1)]),
    ("asia2", &[("asia-south1", 2, 0, 0), ("asia-south2", 2, 0, 0), ("asia-southeast1", 1, 0, 0)]),
    ("eur3", &[("europe-west1", 2, 0, 0), ("europe-west4", 2, 0, 0), ("europe-north1", 0, 0, 1)]),
    ("eur5", &[("europe-west2", 2, 0, 0), ("europe-west1", 2, 0, 0), ("europe-west4", 0, 0, 1)]),
    ("eur6", &[("europe-west4", 2, 0, 0), ("europe-west3", 2, 0, 0), ("europe-west6", 0, 0, 1)]),
    ("eur7", &[("europe-west8", 2, 0, 0), ("europe-west3", 2, 0, 0), ("europe-west12", 0, 0, 1)]),
    ("nam3", &[("us-east4", 2, 0, 0), ("us-east1", 2, 0, 0), ("us-central1", 0, 0, 1)]),
    ("nam6", &[("us-central1", 2, 0, 0), ("us-east1", 2, 0, 0), ("us-west1", 0, 1, 0), ("us-west2", 0, 1, 0), ("us-central2", 0, 0, 1)]),
    ("nam7", &[("us-central1", 2, 0, 0), ("us-east4", 2, 0, 0), ("us-central2", 0, 0, 1)]),
    ("nam8", &[("us-west2", 2, 0, 0), ("us-west1", 2, 0, 0), ("us-west3", 0, 0, 1)]),
    ("nam9", &[("us-east4", 2, 0, 0), ("us-central1", 2, 0, 0), ("us-west1", 0, 2, 0), ("us-east1", 0, 0, 1)]),
    ("nam10", &[("us-central1", 2, 0, 0), ("us-west3", 2, 0, 0), ("us-central2", 0, 0, 1)]),
    ("nam11", &[("us-central1", 2, 0, 0), ("us-east1", 2, 0, 0), ("us-central2", 0, 0, 1)]),
    ("nam12", &[("us-central1", 2, 0, 0), ("us-east4", 2, 0, 0), ("us-west1", 0, 2, 0), ("us-central2", 0, 0, 1)]),
    ("nam13", &[("us-central2", 2, 0, 0), ("us-central1", 2, 0, 0), ("us-west3", 0, 0, 1)]),
    ("nam14", &[("us-east4", 2, 0, 0), ("northamerica-northeast1", 2, 0, 0), ("us-east1", 0, 0, 1)]),
    ("nam15", &[("us-south1", 2, 0, 0), ("us-east4", 2, 0, 0), ("us-central1", 0, 0, 1)]),
    ("nam16", &[("us-central1", 2, 0, 0), ("us-east4", 2, 0, 0), ("us-east5", 0, 0, 1)]),
    ("nam22", &[("us-east5", 2, 0, 0), ("us-east1", 2, 0, 0), ("us-east4", 0, 0, 1)]),
    ("nam23", &[("us-central1", 2, 0, 0), ("us-west4", 2, 0, 0), ("us-south1", 0, 0, 1)]),
    (
        "nam-eur-asia1",
        &[("us-central1", 2, 0, 0), ("us-central2", 2, 0, 0), ("europe-west1", 0, 2, 0), ("asia-east1", 0, 2, 0), ("us-east1", 0, 0, 1)],
    ),
    (
        "nam-eur-asia3",
        &[
            ("us-central1", 2, 0, 0),
            ("us-east1", 2, 0, 0),
            ("europe-west1", 0, 1, 0),
            ("europe-west4", 0, 1, 0),
            ("asia-east1", 0, 2, 0),
            ("us-central2", 0, 0, 1),
        ],
    ),
    ("dual-region-australia1", &[("australia-southeast1", 2, 0, 1), ("australia-southeast2", 2, 0, 1)]),
    ("dual-region-canada1", &[("northamerica-northeast2", 2, 0, 1), ("northamerica-northeast1", 2, 0, 1)]),
    ("dual-region-germany1", &[("europe-west10", 2, 0, 1), ("europe-west3", 2, 0, 1)]),
    ("dual-region-india1", &[("asia-south1", 2, 0, 1), ("asia-south2", 2, 0, 1)]),
    ("dual-region-japan1", &[("asia-northeast1", 2, 0, 1), ("asia-northeast2", 2, 0, 1)]),
];

/// Replica layout of an instance configuration name (regional or base multi-region).
fn spanner_layout(config: &str) -> Option<Vec<(String, u8, u8, u8)>> {
    if let Some(r) = config.strip_prefix("regional-") {
        return Some(vec![(r.to_string(), 3, 0, 0)]);
    }
    SPANNER_CONFIGS
        .iter()
        .find(|(n, _)| *n == config)
        .map(|(_, reps)| reps.iter().map(|(r, rw, ro, w)| (r.to_string(), *rw, *ro, *w)).collect())
}

fn spanner_edition_label(e: &str) -> &'static str {
    match e {
        "ENTERPRISE" => "Enterprise",
        "ENTERPRISE_PLUS" => "Enterprise Plus",
        _ => "Standard",
    }
}

/// Compute capacity in nodes (1 node = 1,000 processing units).
fn spanner_nodes(ctx: &Ctx) -> f64 {
    if let Some(n) = ctx.usage_num("nodes") {
        return n;
    }
    let limits = "autoscaling_config.0.autoscaling_limits.0";
    if let Some(min) =
        ctx.num(&format!("{limits}.min_nodes")).or_else(|| ctx.num(&format!("{limits}.min_processing_units")).map(|p| p / 1000.0))
    {
        let max =
            ctx.num(&format!("{limits}.max_nodes")).or_else(|| ctx.num(&format!("{limits}.max_processing_units")).map(|p| p / 1000.0));
        if max.is_some_and(|m| m > min) {
            ctx.warn(format!(
                "autoscaled up to {} nodes; estimating with the minimum ({}) (set usage 'nodes' to override)",
                fmt(max.unwrap_or(min)),
                fmt(min)
            ));
        }
        return min;
    }
    if let Some(n) = ctx.num("num_nodes").filter(|n| *n > 0.0) {
        return n;
    }
    if let Some(pu) = ctx.num("processing_units").filter(|n| *n > 0.0) {
        return pu / 1000.0;
    }
    ctx.warn("compute capacity not set, assuming 1 node");
    1.0
}

fn spanner_config(ctx: &Ctx) -> String {
    let raw = ctx.str_or("config", "");
    let name = raw.rsplit('/').next().unwrap_or(raw);
    if name.is_empty() {
        let r = ctx.region();
        ctx.warn(format!("instance config unknown, assuming regional-{r}"));
        return format!("regional-{r}");
    }
    name.to_string()
}

fn spanner_instance(ctx: &Ctx) -> Vec<Component> {
    if ctx.str("instance_type") == Some("FREE_INSTANCE") {
        ctx.warn("Spanner free trial instance: no charge during the trial");
        return vec![];
    }
    let config = spanner_config(ctx);
    ctx.set_location(config.strip_prefix("regional-").unwrap_or(&config).to_string());
    let multi = !config.starts_with("regional-");
    let edition = match ctx.str("edition") {
        Some(e) => e.to_string(),
        None if multi => {
            ctx.warn("edition not set: dual- and multi-region configurations require ENTERPRISE_PLUS");
            "ENTERPRISE_PLUS".into()
        }
        None => "STANDARD".into(),
    };
    let ed = spanner_edition_label(&edition);
    let nodes = spanner_nodes(ctx);
    let capacity = if nodes < 1.0 {
        format!("{} PU", fmt(nodes * 1000.0))
    } else {
        format!("{} node{}", fmt(nodes), if nodes == 1.0 { "" } else { "s" })
    };

    let Some(layout) = spanner_layout(&config) else {
        ctx.warn(format!("unknown or custom instance config '{config}': priced with the legacy per-configuration SKU"));
        let pat = format!("^Server Node for Multi-Region Configuration \\({}\\)", regex::escape(&config));
        let spat = format!("^Storage for Multi-Region Configuration \\({}\\)", regex::escape(&config));
        return vec![
            ctx.hourly_unit(format!("Compute capacity ({capacity}, {config})"), "node-hours", nodes, Query::new(SPANNER, "global", &pat)),
            ctx.usage("Storage (SSD)", "GiB-months", "storage_gb", Query::new(SPANNER, "global", &spat)),
        ];
    };

    let price = |region: &str, pat: String| ctx.price(Query::new(SPANNER, region, &pat).ondemand()).map(|p| p.unit_price());
    let mut compute = Some(0.0);
    let mut ssd = Some(0.0);
    let mut hdd = Some(0.0);
    let mut desc = vec![];
    for (region, rw, ro, w) in &layout {
        let add = |acc: &mut Option<f64>, n: u8, p: Option<f64>| {
            if n > 0 {
                *acc = acc.zip(p).map(|(a, p)| a + p * f64::from(n));
            }
        };
        add(&mut compute, *rw, price(region, format!("^Read-Write Replica for {ed} Edition in")));
        add(&mut compute, *ro, price(region, format!("^Read-Only Replica for {ed} Edition in")));
        add(&mut compute, *w, price(region, format!("^Witness Replica for {ed} Edition in")));
        add(&mut ssd, rw + ro, price(region, "^SSD Storage for Replica in".into()));
        add(&mut hdd, rw + ro, price(region, "^HDD Storage for Replica in".into()));
        for (n, kind) in [(*rw, "read-write"), (*ro, "read-only"), (*w, "witness")] {
            if n > 0 {
                desc.push(format!("{n} {kind} in {region}"));
            }
        }
    }
    if multi && edition != "ENTERPRISE_PLUS" {
        ctx.warn(format!("{config} requires the ENTERPRISE_PLUS edition"));
    }
    let hours = ctx.monthly_hours();
    let mut c = ctx.fixed(format!("Compute capacity ({capacity}, {ed}, {config})"), "node-hours", Some(nodes * hours), compute);
    c.note = Some(format!("per node: {}", desc.join(", ")));
    let data_replicas: u8 = layout.iter().map(|r| r.1 + r.2).sum();
    let replicated = |mut c: Component| {
        c.note = Some(format!("price covers the {data_replicas} replicas storing data"));
        c
    };
    let mut out = vec![c, replicated(usage_fixed(ctx, "Storage (SSD)", "GiB-months", ctx.usage_num("storage_gb"), ssd))];
    if ctx.usage_num("hdd_storage_gb").is_some() {
        out.push(replicated(usage_fixed(ctx, "Storage (HDD)", "GiB-months", ctx.usage_num("hdd_storage_gb"), hdd)));
    }
    let backup = if multi {
        Query::new(SPANNER, "global", "^Backup storage for Multi-Region Configurations$")
    } else {
        Query::new(SPANNER, &layout[0].0, "^Backup storage for Regional Configuration \\(")
    };
    out.push(ctx.usage("Backup storage", "GiB-months", "backup_storage_gb", backup));
    out
}

fn spanner_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let config = spanner_config(ctx);
    ctx.set_location(config.strip_prefix("regional-").unwrap_or(&config).to_string());
    let multi = !config.starts_with("regional-");
    let nonprod = ctx.env == Env::NonProd;
    let autoscaled = ctx.block("autoscaling_config").is_some();

    if nonprod && multi {
        let leader = spanner_layout(&config).and_then(|l| l.first().map(|r| r.0.clone())).unwrap_or_else(|| ctx.region());
        let regional = format!("regional-{leader}");
        let new = ctx.reprice(|a| {
            a.insert("config".into(), Value::from(regional.as_str()));
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "spanner-multiregion-nonprod",
                Severity::Medium,
                format!("Use a regional configuration ({regional}) in non-production"),
                "Dual- and multi-region configurations bill 5+ replicas per node (vs 3 for regional) plus inter-region replication. Non-production instances rarely need 99.999% availability. Changing config moves the instance (online, but billed at both configurations during the move); you can also drop to the Standard edition once regional.",
                Some(s),
            ));
        }
    }

    let nodes = spanner_nodes(ctx);
    if nonprod && !autoscaled && nodes >= 1.0 && ctx.usage_num("nodes").is_none() {
        let new = ctx.reprice(|a| {
            a.insert("num_nodes".into(), Value::Null);
            a.insert("processing_units".into(), Value::from(100.0));
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "spanner-processing-units",
                Severity::Medium,
                "Use fractional compute capacity (processing units) in non-production",
                format!(
                    "This instance has {} node(s) (= {} processing units). Spanner instances can be as small as 100 PU (1/10 node), which supports up to 1 TiB of data and is usually plenty for dev/test. Set processing_units = 100 (or an autoscaling_config with a low minimum) instead of num_nodes.",
                    fmt(nodes),
                    fmt(nodes * 1000.0)
                ),
                Some(s),
            ));
        }
    }

    let edition = ctx.str_or("edition", if multi { "ENTERPRISE_PLUS" } else { "STANDARD" });
    if nonprod && !multi && edition != "STANDARD" {
        let new = ctx.reprice(|a| {
            a.insert("edition".into(), "STANDARD".into());
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "spanner-edition-nonprod",
                Severity::Low,
                "Use the Standard edition for this non-production instance",
                "Enterprise / Enterprise Plus add features (search, graph, vector, managed autoscaler, geo-partitioning...) at a 37-90% compute premium. If the non-production workload does not rely on them, set edition = \"STANDARD\".",
                Some(s),
            ));
        }
    }

    if !autoscaled && !nonprod && nodes >= 3.0 {
        out.push(ctx.rec(
            "spanner-autoscaling",
            Severity::Info,
            "Consider autoscaling this Spanner instance",
            format!("{} nodes are provisioned permanently. With autoscaling_config (min/max nodes and a high-priority CPU target) capacity follows the daily load, which typically reduces compute cost when traffic is uneven.", fmt(nodes)),
            None,
        ));
    }

    if let Some(d) = cud_discounts(
        ctx,
        SPANNER,
        "global",
        "^Commitment - dollar based v1: Cloud Spanner for 1 year",
        "^Commitment - dollar based v1: Cloud Spanner for 3 years",
    ) {
        out.extend(cud_rec(ctx, "spanner-cud", "Spanner instance", cost_of(cr, &["Compute capacity"]), d));
    }
    out
}

// ---- Bigtable ---------------------------------------------------------------

struct BigtableCluster {
    id: String,
    region: String,
    nodes: f64,
    hdd: bool,
}

fn bigtable_clusters(ctx: &Ctx) -> Vec<BigtableCluster> {
    let dev = ctx.str("instance_type") == Some("DEVELOPMENT");
    if dev {
        ctx.warn("DEVELOPMENT instances are deprecated and billed as a 1-node production cluster");
    }
    ctx.blocks("cluster")
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let id = c.get("cluster_id").and_then(|v| v.as_str()).map(str::to_string).unwrap_or_else(|| format!("cluster-{}", i + 1));
            let region = c.get("zone").and_then(|v| v.as_str()).map(normalize_region).unwrap_or_else(|| ctx.region());
            let min = c.path("autoscaling_config.0.min_nodes").and_then(|v| v.as_f64());
            let nodes = if let Some(n) = ctx.usage_num("nodes") {
                n
            } else if dev {
                1.0
            } else if let Some(min) = min {
                let max = c.path("autoscaling_config.0.max_nodes").and_then(|v| v.as_f64()).unwrap_or(min);
                if max > min {
                    ctx.warn(format!(
                        "cluster {id} autoscales between {} and {} nodes; estimating with the minimum (set usage 'nodes' to override)",
                        fmt(min),
                        fmt(max)
                    ));
                }
                min
            } else {
                c.get("num_nodes").and_then(|v| v.as_f64()).unwrap_or_else(|| {
                    ctx.warn(format!("cluster {id}: num_nodes not set, assuming 1"));
                    1.0
                })
            };
            let hdd = c.get("storage_type").and_then(|v| v.as_str()) == Some("HDD");
            BigtableCluster { id, region, nodes, hdd }
        })
        .collect()
}

fn bigtable_instance(ctx: &Ctx) -> Vec<Component> {
    let clusters = bigtable_clusters(ctx);
    let mut regions: Vec<String> = clusters.iter().map(|c| c.region.clone()).collect();
    regions.dedup();
    if !regions.is_empty() {
        ctx.set_location(regions.join(","));
    }
    if clusters.is_empty() {
        ctx.warn("no cluster block found");
        return vec![];
    }
    let plus = ctx.str("edition").is_some_and(|e| e.eq_ignore_ascii_case("ENTERPRISE_PLUS"));
    let node_pat = if plus { "^Enterprise Plus Server Node in" } else { "^Server Node( in|$)" };
    let storage = ctx.usage_num("storage_gb");
    let mut out = vec![];
    for c in &clusters {
        let q = |pat: &'static str| Query::new(BIGTABLE, &c.region, pat).ondemand().exclude("trial");
        out.push(ctx.hourly_unit(
            format!("Cluster {} nodes ({}, {} node{})", c.id, c.region, fmt(c.nodes), if c.nodes == 1.0 { "" } else { "s" }),
            "node-hours",
            c.nodes,
            q(node_pat),
        ));
        let (kind, pat) = if c.hdd { ("HDD", "^HDD storage( in|$)") } else { ("SSD", "^SSD storage( in|$)") };
        out.push(ctx.usage_qty(format!("Cluster {} storage ({kind})", c.id), "GiB-months", storage, q(pat)));
    }
    let first = &clusters[0].region;
    out.push(ctx.usage(
        "Backup storage",
        "GiB-months",
        "backup_storage_gb",
        Query::new(BIGTABLE, first, "^Backup storage( in|$)").ondemand().exclude("trial"),
    ));
    out
}

fn bigtable_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let clusters = bigtable_clusters(ctx);
    let nonprod = ctx.env == Env::NonProd;
    if nonprod && clusters.len() > 1 {
        let new = ctx.reprice(|a| {
            if let Some(Value::List(l)) = a.get_mut("cluster") {
                l.truncate(1);
            }
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "bigtable-replication-nonprod",
                Severity::Medium,
                format!("Use a single cluster instead of {} replicated clusters in non-production", clusters.len()),
                "Every cluster bills its own nodes and a full copy of the data, plus replication traffic between regions. Non-production instances rarely need replication: keep one cluster block.",
                Some(s),
            ));
        }
    }
    if nonprod && ctx.usage_num("nodes").is_none() && clusters.iter().any(|c| c.nodes > 1.0) {
        let new = ctx.reprice(|a| {
            if let Some(Value::List(l)) = a.get_mut("cluster") {
                for c in l.iter_mut() {
                    if let Value::Map(m) = c {
                        m.insert("num_nodes".into(), Value::from(1.0));
                        m.shift_remove("autoscaling_config");
                    }
                }
            }
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "bigtable-dev-instance",
                Severity::Low,
                "Run this non-production Bigtable instance with 1 node per cluster",
                "A single node serves ~10k QPS and up to 5 TB (SSD) / 16 TB (HDD). Set num_nodes = 1, or autoscaling_config with min_nodes = 1.",
                Some(s),
            ));
        }
    }
    if let Some(gb) = ctx.usage_num("storage_gb").filter(|g| *g >= 1024.0)
        && clusters.iter().any(|c| !c.hdd)
    {
        let new = ctx.reprice(|a| {
            if let Some(Value::List(l)) = a.get_mut("cluster") {
                for c in l.iter_mut() {
                    if let Value::Map(m) = c {
                        m.insert("storage_type".into(), "HDD".into());
                    }
                }
            }
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "bigtable-ssd-to-hdd",
                Severity::Info,
                format!("HDD storage would be much cheaper for {} GiB of data", fmt(gb)),
                "HDD storage costs ~85% less than SSD but has far higher read latency (only suitable for large batch/analytical workloads with few random reads). storage_type cannot be changed in place: it requires a new cluster.",
                Some(s),
            ));
        }
    }
    if let Some(d) = cud_discounts(
        ctx,
        BIGTABLE,
        "global",
        "^Commitment - dollar based v1: Cloud Bigtable for 1 year",
        "^Commitment - dollar based v1: Cloud Bigtable for 3 years",
    ) {
        out.extend(cud_rec(ctx, "bigtable-cud", "Bigtable instance", cost_where(cr, |c| c.unit == "node-hours"), d));
    }
    out
}

// ---- Memorystore ------------------------------------------------------------

/// Memorystore for Redis capacity tier for a provisioned size.
fn redis_capacity_tier(gb: f64) -> &'static str {
    if gb <= 4.0 {
        "M1"
    } else if gb <= 10.0 {
        "M2"
    } else if gb <= 35.0 {
        "M3"
    } else if gb <= 100.0 {
        "M4"
    } else {
        "M5"
    }
}

fn redis_instance(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let tier = ctx.str_or("tier", "BASIC");
    let Some(gb) = ctx.num("memory_size_gb") else {
        ctx.warn("memory_size_gb is not known statically");
        return vec![];
    };
    let m = redis_capacity_tier(gb);
    let replicas = ctx.str("read_replicas_mode") == Some("READ_REPLICAS_ENABLED");
    let (name, count, pat) = match (tier, replicas) {
        ("STANDARD_HA", true) => {
            let r = ctx.num_or("replica_count", 2.0);
            (
                format!("Redis Standard ({} GiB, {m}, primary + {} read replica{})", fmt(gb), fmt(r), if r == 1.0 { "" } else { "s" }),
                1.0 + r,
                format!("^Redis Standard Node Capacity {m} "),
            )
        }
        ("STANDARD_HA", false) => (format!("Redis Standard HA ({} GiB, {m})", fmt(gb)), 1.0, format!("^Redis Capacity Standard {m} ")),
        _ => (format!("Redis Basic ({} GiB, {m})", fmt(gb)), 1.0, format!("^Redis Capacity Basic {m} ")),
    };
    vec![ctx.hourly_unit(name, "GiB-hours", gb * count, Query::new(REDIS, &region, &pat).ondemand())]
}

fn memorystore_cud(ctx: &Ctx, id: &str, what: &str, cr: &CostedResource) -> Option<Recommendation> {
    let d = cud_discounts(
        ctx,
        REDIS,
        "global",
        "^Commitment - dollar based v1: Memorystore for 1 year",
        "^Commitment - dollar based v1: Memorystore for 3 years",
    )?;
    cud_rec(ctx, id, what, cost_where(cr, |c| c.unit.ends_with("-hours")), d)
}

fn redis_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    if ctx.env == Env::NonProd && ctx.str("tier") == Some("STANDARD_HA") {
        let new = ctx.reprice(|a| {
            a.insert("tier".into(), "BASIC".into());
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "redis-standard-ha-nonprod",
                Severity::Medium,
                "Use the Basic tier for this non-production Redis instance",
                "Standard tier bills a replica (and any read replicas) for automatic failover. A non-production cache can usually be a Basic instance (no replication, data lost on failure). The tier cannot be changed in place: set tier = \"BASIC\" (recreates the instance).",
                Some(s),
            ));
        }
    }
    out.extend(memorystore_cud(ctx, "memorystore-cud", "Memorystore instance", cr));
    out
}

/// Node type SKU stem and capacity (GB) for Redis Cluster / Valkey node types.
fn memorystore_node(node_type: &str, redis_cluster: bool) -> (String, Option<f64>) {
    let t = node_type.trim_start_matches("REDIS_").to_uppercase();
    let capacity = match t.as_str() {
        "SHARED_CORE_NANO" => Some(1.4),
        "STANDARD_SMALL" => Some(6.5),
        "HIGHMEM_MEDIUM" | "HIGHCPU_MEDIUM" => Some(13.0),
        "STANDARD_LARGE" => Some(26.0),
        "HIGHMEM_XLARGE" => Some(58.0),
        "HIGHMEM_2XLARGE" => Some(110.0),
        _ => None,
    };
    let stem = if redis_cluster && t == "HIGHMEM_MEDIUM" { "Default".to_string() } else { t.replace('_', " ") };
    (stem, capacity)
}

fn memorystore_cluster_components(ctx: &Ctx, service: &str, node_type: &str, shards: f64, replicas: f64, aof: bool) -> Vec<Component> {
    let region = ctx.region();
    let redis_cluster = service == REDIS;
    let (stem, capacity) = memorystore_node(node_type, redis_cluster);
    let nodes = shards * (1.0 + replicas);
    let esc = regex::escape(&stem);
    let (node_pat, aof_pat, backup_pat) = if redis_cluster {
        (format!("^Redis Cluster Node {esc} "), "^Memorystore for Redis Cluster: AOF Storage", "^Memorystore for Redis Cluster: Backups in")
    } else {
        (format!("^{esc} Node "), "^Memorystore for Valkey: AOF Storage", "^Memorystore for Valkey: Backups in")
    };
    let mut out = vec![ctx.hourly_unit(
        format!(
            "Nodes ({}, {} shard{} × {} node{})",
            node_type.to_lowercase().replace('_', "-"),
            fmt(shards),
            if shards == 1.0 { "" } else { "s" },
            fmt(1.0 + replicas),
            if replicas == 0.0 { "" } else { "s" }
        ),
        "node-hours",
        nodes,
        Query::new(service, &region, &node_pat).ondemand(),
    )];
    if aof {
        match capacity {
            Some(cap) => out.push(scaled(
                ctx,
                format!("AOF persistence ({} GB provisioned)", fmt(cap * nodes)),
                "GB-months",
                Some(cap * nodes),
                HOURS_PER_MONTH,
                Query::new(service, &region, aof_pat).ondemand(),
                false,
            )),
            None => ctx.warn(format!("AOF persistence: capacity of node type {node_type} unknown, not priced")),
        }
    }
    out.push(scaled(
        ctx,
        "Backups",
        "GB-months",
        ctx.usage_num("backup_storage_gb"),
        HOURS_PER_MONTH,
        Query::new(service, &region, backup_pat).ondemand(),
        true,
    ));
    out
}

fn redis_cluster(ctx: &Ctx) -> Vec<Component> {
    let shards = ctx.num("shard_count").unwrap_or_else(|| {
        ctx.warn("shard_count is not known statically, assuming 3");
        3.0
    });
    let aof = ctx.str("persistence_config.0.mode") == Some("AOF");
    memorystore_cluster_components(
        ctx,
        REDIS,
        ctx.str_or("node_type", "REDIS_HIGHMEM_MEDIUM"),
        shards,
        ctx.num_or("replica_count", 0.0),
        aof,
    )
}

fn valkey_instance(ctx: &Ctx) -> Vec<Component> {
    let shards = if ctx.str("mode") == Some("CLUSTER_DISABLED") {
        1.0
    } else {
        ctx.num("shard_count").unwrap_or_else(|| {
            ctx.warn("shard_count is not known statically, assuming 1");
            1.0
        })
    };
    let aof = ctx.str("persistence_config.0.mode") == Some("AOF");
    memorystore_cluster_components(
        ctx,
        MEMORYSTORE,
        ctx.str_or("node_type", "HIGHMEM_MEDIUM"),
        shards,
        ctx.num_or("replica_count", 0.0),
        aof,
    )
}

fn memorystore_cluster_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    if ctx.env == Env::NonProd && ctx.num_or("replica_count", 0.0) > 0.0 {
        let new = ctx.reprice(|a| {
            a.insert("replica_count".into(), Value::from(0.0));
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "memorystore-replicas-nonprod",
                Severity::Medium,
                "Remove replicas from this non-production cluster",
                "Each replica is billed as a full node. Non-production caches rarely need replica-based high availability: set replica_count = 0.",
                Some(s),
            ));
        }
    }
    out.extend(memorystore_cud(ctx, "memorystore-cud", "Memorystore cluster", cr));
    out
}

fn memcache_instance(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let nodes = ctx.num_or("node_count", 1.0);
    let cpus = ctx.num_or("node_config.0.cpu_count", 1.0);
    let Some(mem_mb) = ctx.num("node_config.0.memory_size_mb") else {
        ctx.warn("node_config.memory_size_mb is not known statically");
        return vec![];
    };
    let mem = mem_mb / 1024.0;
    let m = if mem <= 4.0 { "M1" } else { "M2" };
    vec![
        ctx.hourly_unit(
            format!("vCPUs ({} × {} vCPU)", fmt(nodes), fmt(cpus)),
            "vCPU-hours",
            nodes * cpus,
            Query::new(MEMCACHE, &region, &format!("^Custom Core {m} ")).ondemand(),
        ),
        ctx.hourly_unit(
            format!("Memory ({} × {} GiB, {m})", fmt(nodes), fmt(mem)),
            "GiB-hours",
            nodes * mem,
            Query::new(MEMCACHE, &region, &format!("^Custom RAM {m} ")).ondemand(),
        ),
    ]
}

fn memcache_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    memorystore_cud(ctx, "memorystore-cud", "Memcached instance", cr).into_iter().collect()
}

// ---- Firestore --------------------------------------------------------------

/// Location name used in Firestore SKU descriptions.
const FIRESTORE_LOCATIONS: &[(&str, &str)] = &[
    ("africa-south1", "Johannesburg"),
    ("asia-east1", "Taiwan"),
    ("asia-east2", "Hong Kong"),
    ("asia-northeast1", "Tokyo"),
    ("asia-northeast2", "Osaka"),
    ("asia-northeast3", "Seoul"),
    ("asia-south1", "Mumbai"),
    ("asia-south2", "Delhi"),
    ("asia-southeast1", "Singapore"),
    ("asia-southeast2", "Jakarta"),
    ("asia-southeast3", "Bangkok"),
    ("australia-southeast1", "Sydney"),
    ("australia-southeast2", "Melbourne"),
    ("europe-central2", "Warsaw"),
    ("europe-north1", "Finland"),
    ("europe-north2", "Stockholm"),
    ("europe-southwest1", "Madrid"),
    ("europe-west1", "Belgium"),
    ("europe-west2", "London"),
    ("europe-west3", "Frankfurt"),
    ("europe-west4", "Netherlands"),
    ("europe-west6", "Zurich"),
    ("europe-west8", "Milan"),
    ("europe-west9", "Paris"),
    ("europe-west10", "Berlin"),
    ("europe-west12", "Turin"),
    ("me-central1", "Doha"),
    ("me-central2", "Dammam"),
    ("me-west1", "Tel ?Aviv"),
    ("northamerica-northeast1", "Montreal"),
    ("northamerica-northeast2", "Toronto"),
    ("northamerica-south1", "Mexico"),
    ("southamerica-east1", "Sao Paulo"),
    ("southamerica-west1", "Santiago"),
    ("us-central1", "Iowa"),
    ("us-east1", "South Carolina"),
    ("us-east4", "Northern Virginia"),
    ("us-east5", "Columbus"),
    ("us-south1", "Dallas"),
    ("us-west1", "Oregon"),
    ("us-west2", "Los Angeles"),
    ("us-west3", "Salt Lake City"),
    ("us-west4", "Las Vegas"),
    ("us-west8", "Phoenix"),
    ("eur3", "Europe 3"),
    ("nam5", "North America 5"),
    ("nam7", "(Nam7|North America 7)"),
];

fn firestore_database(ctx: &Ctx) -> Vec<Component> {
    let location = ctx.str("location_id").map(str::to_lowercase).unwrap_or_else(|| {
        let r = ctx.region();
        ctx.warn(format!("location_id unknown, assuming {r}"));
        r
    });
    ctx.set_location(location.clone());
    let Some((_, city)) = FIRESTORE_LOCATIONS.iter().find(|(l, _)| *l == location) else {
        ctx.warn(format!("unsupported Firestore location '{location}'"));
        return vec![];
    };
    let enterprise = ctx.str("database_edition") == Some("ENTERPRISE");
    let pitr = ctx.str("point_in_time_recovery_enablement") == Some("POINT_IN_TIME_RECOVERY_ENABLED");
    let pat = |std: &str, ent: &str| {
        if enterprise { format!("^Cloud Firestore Enterprise {ent}\\s+{city}$") } else { format!("^Cloud Firestore {std}\\s+{city}$") }
    };
    let mk = |name: &str, unit: &str, qty: Option<f64>, per: f64, std: &str, ent: &str| {
        let p = pat(std, ent);
        scaled(ctx, name, unit, qty.map(|q| q / per), per, Query::new(FIRESTORE, &location, &p).ondemand(), true)
    };
    let ops = |key: &str| ctx.usage_num(key);
    let deletes = if enterprise { "Write Units" } else { "Entity Deletes" };
    let mut out = vec![
        mk("Stored data", "GiB-months", ctx.usage_num("storage_gb"), 1.0, "Storage", "Storage"),
        mk(
            if enterprise { "Read units" } else { "Document reads" },
            "100k ops",
            ops("monthly_document_reads"),
            1e5,
            "Read Ops",
            "Read Units",
        ),
        mk(
            if enterprise { "Write units" } else { "Document writes" },
            "100k ops",
            ops("monthly_document_writes"),
            1e5,
            "Entity Writes",
            "Write Units",
        ),
        mk("Document deletes", "100k ops", ops("monthly_document_deletes"), 1e5, deletes, deletes),
    ];
    if pitr {
        let gb = ctx.usage_num("pitr_storage_gb").or_else(|| ctx.usage_num("storage_gb"));
        out.push(mk("Point-in-time recovery storage", "GiB-months", gb, 1.0, "Point-in-time Recovery Storage", "PITR Storage"));
    }
    out.push(mk("Backup storage", "GiB-months", ctx.usage_num("backup_storage_gb"), 1.0, "Zonal Backup Storage", "Zonal Backup Storage"));
    if let Some(c) = out.first_mut() {
        c.note = Some("the daily free quota applies to a single database per project and is not deducted".into());
    }
    out
}

fn firestore_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    if ctx.env == Env::NonProd && ctx.str("point_in_time_recovery_enablement") == Some("POINT_IN_TIME_RECOVERY_ENABLED") {
        let pitr = cost_of(cr, &["Point-in-time"]);
        if pitr > 1.0 {
            out.push(ctx.rec(
                "firestore-pitr-nonprod",
                Severity::Low,
                "Disable point-in-time recovery on this non-production database",
                "PITR keeps 7 days of versions, billed like stored data. Non-production databases rarely need it: set point_in_time_recovery_enablement = \"POINT_IN_TIME_RECOVERY_DISABLED\".",
                Some(pitr),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn larger_sql_tiers() {
        let larger = |name: &str, cpu, memory| larger_sql_tier(name, &parse_sql_tier(name).unwrap(), cpu, memory);
        assert_eq!(larger("db-custom-2-8192", true, false).as_deref(), Some("db-custom-4-8192"));
        assert_eq!(larger("db-custom-2-8192", false, true).as_deref(), Some("db-custom-4-16384"), "16 GiB needs more vCPUs");
        assert_eq!(larger("db-custom-4-8192", false, true).as_deref(), Some("db-custom-4-16384"));
        assert_eq!(larger("db-custom-1-3840", true, true).as_deref(), Some("db-custom-2-7680"));
        assert_eq!(larger("db-n1-standard-2", false, true).as_deref(), Some("db-n1-highmem-2"));
        assert_eq!(larger("db-n1-standard-2", true, false).as_deref(), Some("db-n1-standard-4"));
        assert_eq!(larger("db-perf-optimized-N-8", true, false).as_deref(), Some("db-perf-optimized-N-16"));
        assert_eq!(larger("db-custom-96-393216", true, false), None);
        assert_eq!(larger("db-f1-micro", true, false), None);
        assert_eq!(larger("db-custom-2-8192", false, false), None);
    }

    #[test]
    fn smaller_sql_tiers() {
        let smaller = |name: &str, halve_memory| smaller_sql_tier(name, &parse_sql_tier(name).unwrap(), halve_memory);
        assert_eq!(smaller("db-custom-4-16384", true).as_deref(), Some("db-custom-2-8192"));
        assert_eq!(smaller("db-custom-4-16384", false).as_deref(), None, "8 GiB per vCPU is above the limit");
        assert_eq!(smaller("db-custom-4-8192", false).as_deref(), Some("db-custom-2-8192"));
        assert_eq!(smaller("db-custom-2-3840", true).as_deref(), Some("db-custom-1-2048"));
        assert_eq!(smaller("db-custom-6-12288", true), None, "3 vCPUs is not a valid custom shape");
        assert_eq!(smaller("db-n1-standard-4", true).as_deref(), Some("db-n1-standard-2"));
        assert_eq!(smaller("db-n1-standard-4", false), None);
        assert_eq!(smaller("db-perf-optimized-N-8", true).as_deref(), Some("db-perf-optimized-N-4"));
        assert_eq!(smaller("db-g1-small", true), None);
        assert_eq!(smaller("db-custom-1-3840", true), None);
    }

    #[test]
    fn sql_tiers() {
        let t = parse_sql_tier("db-custom-2-7680").unwrap();
        assert_eq!((t.series, t.vcpus, t.memory_gb), (SqlSeries::Custom, 2.0, 7.5));
        let t = parse_sql_tier("db-n1-standard-4").unwrap();
        assert!(t.legacy && t.vcpus == 4.0 && t.memory_gb == 15.0);
        let t = parse_sql_tier("db-custom-N4-4-16384").unwrap();
        assert_eq!((t.series, t.vcpus, t.memory_gb), (SqlSeries::N4, 4.0, 16.0));
        let t = parse_sql_tier("db-perf-optimized-N-8").unwrap();
        assert!(t.enterprise_plus() && t.memory_gb == 64.0);
        assert_eq!(parse_sql_tier("db-perf-optimized-N-128").unwrap().memory_gb, 864.0);
        assert_eq!(parse_sql_tier("db-c4a-highmem-4").unwrap().series, SqlSeries::C4a);
        assert_eq!(parse_sql_tier("db-perf-optimized-C4-8").unwrap().series, SqlSeries::C4);
        assert!(parse_sql_tier("db-f1-micro").unwrap().shared());
        assert!(parse_sql_tier("n2-standard-4").is_none());
        assert_eq!(parse_sql_tier("db-perf-optimized-N-16").unwrap().data_cache_gb(), Some(750.0));
        assert_eq!(parse_sql_tier("db-c4a-highmem-2").unwrap().data_cache_gb(), None);
    }

    #[test]
    fn sql_versions() {
        assert_eq!(sql_version_tag("POSTGRES_9_6").as_deref(), Some("96"));
        assert_eq!(sql_version_tag("POSTGRES_13").as_deref(), Some("13"));
        assert_eq!(sql_version_tag("MYSQL_5_7").as_deref(), Some("57"));
        assert_eq!(sql_version_tag("MYSQL_8_0_31").as_deref(), Some("80"));
        assert_eq!(sql_major("POSTGRES_16"), 16.0);
        assert_eq!(sql_major("MYSQL_8_4"), 8.4);
        assert_eq!(sqlserver_license("SQLSERVER_2019_STANDARD"), Some(("2019", "Standard".to_string())));
    }

    #[test]
    fn redis_tiers() {
        assert_eq!(redis_capacity_tier(1.0), "M1");
        assert_eq!(redis_capacity_tier(5.0), "M2");
        assert_eq!(redis_capacity_tier(35.0), "M3");
        assert_eq!(redis_capacity_tier(100.0), "M4");
        assert_eq!(redis_capacity_tier(300.0), "M5");
        assert_eq!(memorystore_node("REDIS_HIGHMEM_MEDIUM", true).0, "Default");
        assert_eq!(memorystore_node("HIGHMEM_XLARGE", false), ("HIGHMEM XLARGE".to_string(), Some(58.0)));
    }

    #[test]
    fn spanner_layouts() {
        assert_eq!(spanner_layout("regional-europe-west1"), Some(vec![("europe-west1".to_string(), 3, 0, 0)]));
        let eur3 = spanner_layout("eur3").unwrap();
        assert_eq!(eur3.iter().map(|r| r.1 + r.2 + r.3).sum::<u8>(), 5);
        assert!(spanner_layout("custom-foo").is_none());
    }
}
