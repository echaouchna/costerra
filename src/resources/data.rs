//! Data & analytics: BigQuery (storage, queries, reservations, commitments,
//! BI Engine, transfers), Dataflow, Dataproc (clusters, serverless batches),
//! Cloud Composer, Data Fusion, Datastream and Dataplex.

use super::compute::{VmSpec, vm_components};
use super::machine::{self, COMPUTE, Pricing};
use super::{Ctx, Spec, usage};
use crate::cost::{Component, HOURS_PER_MONTH};
use crate::hcl::value::{Value, fmt_number};
use crate::pricing::query::Query;
use crate::tf::evaluate::ResourceInstance;

const BQ: &str = "BigQuery";
const BQ_RES: &str = "BigQuery Reservation API";
const BQ_STORAGE_API: &str = "BigQuery Storage API";
const BQ_BI: &str = "BigQuery BI Engine";
const DATAFLOW: &str = "Cloud Dataflow";
const DATAPROC: &str = "Dataproc";
const COMPOSER: &str = "Cloud Composer";
const DATA_FUSION: &str = "Cloud Data Fusion";
const DATASTREAM: &str = "Datastream";
const DATAPLEX: &str = "Dataplex";

pub fn specs() -> Vec<Spec> {
    const DATASET: &[super::UsageParam] = usage!(
        "storage_gb" => "Active logical storage in GiB",
        "long_term_storage_gb" => "Long-term logical storage in GiB (tables not modified for 90 days)",
        "physical_storage_gb" => "Active physical (compressed) storage in GiB, excluding time travel and fail-safe",
        "long_term_physical_storage_gb" => "Long-term physical (compressed) storage in GiB",
        "time_travel_storage_gb" => "Time travel physical storage in GiB (physical billing only)",
        "fail_safe_storage_gb" => "Fail-safe physical storage in GiB (physical billing only)",
        "compression_ratio" => "Logical/physical size ratio, used to derive physical storage when physical sizes are not given",
        "monthly_queries_tib" => "On-demand query bytes scanned per month in TiB",
        "monthly_streaming_inserts_mb" => "Legacy streaming inserts (tabledata.insertAll) per month in MiB",
        "monthly_storage_write_api_gb" => "Storage Write API ingestion per month in GiB",
        "monthly_storage_read_api_tib" => "Storage Read API bytes scanned per month in TiB",
    );
    const TABLE: &[super::UsageParam] = usage!(
        "storage_gb" => "Active logical storage in GiB",
        "long_term_storage_gb" => "Long-term logical storage in GiB (not modified for 90 days)",
        "physical_storage_gb" => "Active physical (compressed) storage in GiB, excluding time travel and fail-safe",
        "long_term_physical_storage_gb" => "Long-term physical (compressed) storage in GiB",
        "time_travel_storage_gb" => "Time travel physical storage in GiB (physical billing only)",
        "fail_safe_storage_gb" => "Fail-safe physical storage in GiB (physical billing only)",
        "compression_ratio" => "Logical/physical size ratio, used to derive physical storage when physical sizes are not given",
        "monthly_streaming_inserts_mb" => "Legacy streaming inserts (tabledata.insertAll) per month in MiB",
        "monthly_storage_write_api_gb" => "Storage Write API ingestion per month in GiB",
        "monthly_storage_read_api_tib" => "Storage Read API bytes scanned per month in TiB",
    );
    const DATAFLOW_USAGE: &[super::UsageParam] = usage!(
        "monthly_hrs" => "Hours per month the job runs (default 730 for streaming jobs; required for batch jobs)",
        "workers" => "Average number of workers (default num_workers, or 1)",
        "streaming" => "1 for a streaming job, 0 for batch (auto-detected otherwise)",
        "monthly_streaming_engine_compute_units" => "Streaming Engine Compute Units consumed per month (streaming jobs with Streaming Engine)",
        "monthly_shuffle_data_gb" => "Dataflow Shuffle data processed per month in GiB (batch jobs)",
        "monthly_dcus" => "Data Compute Units consumed per month (Dataflow Prime jobs)",
    );
    vec![
        Spec::new("google_bigquery_dataset", bq_dataset).usage(DATASET).rules(rules::bq_dataset),
        Spec::new("google_bigquery_table", bq_table).usage(TABLE).rules(rules::bq_dataset),
        Spec::new("google_bigquery_reservation", bq_reservation)
            .usage(usage!(
                "average_autoscale_slots" => "Average number of autoscaled slots in use over the month",
                "monthly_hrs" => "Hours per month the baseline slots are provisioned (default 730)",
            ))
            .rules(rules::bq_reservation),
        Spec::new("google_bigquery_capacity_commitment", bq_commitment).rules(rules::bq_commitment),
        Spec::new("google_bigquery_bi_reservation", bq_bi_reservation),
        Spec::new("google_bigquery_data_transfer_config", bq_transfer)
            .usage(usage!("monthly_queries_tib" => "Bytes scanned per month by a scheduled query in TiB (on-demand pricing)")),
        Spec::new("google_dataflow_job", dataflow_job).usage(DATAFLOW_USAGE).rules(rules::dataflow),
        Spec::new("google_dataflow_flex_template_job", dataflow_job).usage(DATAFLOW_USAGE).rules(rules::dataflow),
        Spec::new("google_dataproc_cluster", dataproc_cluster)
            .usage(usage!(
                "monthly_hrs" => "Hours per month the cluster runs (default 730)",
                "workers" => "Average number of primary workers (overrides num_instances)",
                "secondary_workers" => "Average number of secondary workers (overrides num_instances)",
            ))
            .rules(rules::dataproc),
        Spec::new("google_dataproc_batch", dataproc_batch).usage(usage!(
            "monthly_hrs" => "Hours per month the batch workload runs (used with the configured Spark resources)",
            "monthly_dcu_hours" => "DCU-hours consumed per month (overrides the estimate from Spark properties)",
            "monthly_shuffle_storage_gb" => "Average shuffle storage in GiB-months",
        )),
        Spec::new("google_composer_environment", composer)
            .usage(usage!(
                "workers" => "Average number of Airflow workers (default workloads_config worker min_count)",
                "database_storage_gb" => "Airflow database storage in GiB (default 10)",
                "monthly_hrs" => "Hours per month the environment runs (default 730)",
            ))
            .rules(rules::composer),
        Spec::new("google_data_fusion_instance", data_fusion)
            .usage(usage!("monthly_hrs" => "Hours per month the instance runs (default 730)"))
            .rules(rules::data_fusion),
        Spec::new("google_datastream_stream", datastream).usage(usage!(
            "monthly_cdc_data_gb" => "Change data capture volume processed per month in GiB",
            "monthly_backfill_data_gb" => "Backfill volume processed per month in GiB",
        )),
        Spec::new("google_dataplex_datascan", dataplex_datascan)
            .usage(usage!("monthly_dcu_hours" => "Data Compute Unit hours consumed by scan runs per month")),
        Spec::new("google_dataplex_task", dataplex_task)
            .usage(usage!("monthly_dcu_hours" => "Data Compute Unit hours consumed by task runs per month")),
    ]
}

// ---- shared helpers --------------------------------------------------------

fn vnum(v: &Value, path: &str) -> Option<f64> {
    v.path(path).and_then(Value::as_f64)
}

fn vstr<'v>(v: &'v Value, path: &str) -> Option<&'v str> {
    v.path(path).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn opt_sum(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        _ => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

/// Express a component priced in a sub-unit (milli DCU-hours, mCPU-hours)
/// in whole units: the quantity is divided and the unit price multiplied by
/// `factor`, leaving the cost unchanged.
fn rescale(mut c: Component, factor: f64, unit: &str) -> Component {
    c.quantity = c.quantity.map(|q| q / factor);
    c.unit_price = c.unit_price.map(|p| p * factor);
    c.unit = unit.to_string();
    c
}

fn with_note(mut c: Component, note: impl Into<String>) -> Component {
    c.note = Some(note.into());
    c
}

fn fmt(n: f64) -> String {
    fmt_number((n * 1000.0).round() / 1000.0)
}

// ---- BigQuery ---------------------------------------------------------------

/// BigQuery location of a resource (`US` when unset, the provider default).
fn bq_location(ctx: &Ctx) -> String {
    ctx.str("location").map(str::to_lowercase).unwrap_or_else(|| "us".into())
}

fn bq_location_of(r: &ResourceInstance) -> String {
    norm_location(r.attrs.get("location").and_then(Value::as_str).unwrap_or("US"))
}

fn norm_location(l: &str) -> String {
    let l = l.to_lowercase();
    if l == "europe" { "eu".into() } else { l }
}

/// Billing Catalog regions of a BigQuery location, in lookup order. SKUs of
/// the EU multi-region are split between the `eu` and `europe` regions.
fn bq_regions(loc: &str) -> Vec<String> {
    match loc {
        "eu" | "europe" => vec!["eu".into(), "europe".into()],
        other => vec![other.to_string()],
    }
}

/// The first query (over `regions`) that resolves to a SKU.
fn bq_query<'a>(ctx: &Ctx, regions: &'a [String], build: impl Fn(&'a str) -> Query<'a>) -> Query<'a> {
    regions.iter().map(|r| build(r)).find(|q| ctx.price(*q).is_some()).unwrap_or_else(|| build(&regions[0]))
}

fn bq_storage(ctx: &Ctx, regions: &[String], physical: bool, time_travel_hours: f64) -> Vec<Component> {
    let q = |pat: &'static str, group: &'static str| bq_query(ctx, regions, |r| Query::new(BQ, r, pat).group(group).ondemand());
    if !physical {
        return vec![
            ctx.usage("Active logical storage", "GiB-months", "storage_gb", q("^Active Logical Storage", "ActiveStorage")),
            ctx.usage(
                "Long-term logical storage",
                "GiB-months",
                "long_term_storage_gb",
                q("^Long Term Logical Storage", "LongTermStorage"),
            ),
        ];
    }
    let ratio = ctx.usage_num("compression_ratio").filter(|r| *r > 0.0);
    let physical_of = |key: &str, logical: &str| ctx.usage_num(key).or_else(|| Some(ctx.usage_num(logical)? / ratio?));
    let active = q("^Active Physical Storage", "PhysicalStorage");
    let travel = opt_sum(ctx.usage_num("time_travel_storage_gb"), ctx.usage_num("fail_safe_storage_gb"));
    vec![
        ctx.usage_qty("Active physical storage", "GiB-months", physical_of("physical_storage_gb", "storage_gb"), active),
        with_note(
            ctx.usage_qty("Time travel & fail-safe storage", "GiB-months", travel, active),
            format!("{} h time travel window + 7 days fail-safe, billed at the active physical rate", fmt(time_travel_hours)),
        ),
        ctx.usage_qty(
            "Long-term physical storage",
            "GiB-months",
            physical_of("long_term_physical_storage_gb", "long_term_storage_gb"),
            q("^Long-Term Physical Storage", "PhysicalStorage"),
        ),
    ]
}

fn bq_ingest(ctx: &Ctx, regions: &[String]) -> Vec<Component> {
    vec![
        ctx.usage(
            "Streaming inserts",
            "MiB",
            "monthly_streaming_inserts_mb",
            bq_query(ctx, regions, |r| Query::new(BQ, r, "^Streaming Insert").group("Streaming").ondemand()),
        ),
        ctx.usage(
            "Storage Write API",
            "GiB",
            "monthly_storage_write_api_gb",
            bq_query(ctx, regions, |r| Query::new(BQ_STORAGE_API, r, "^BigQuery Storage API - Write").ondemand()),
        ),
        ctx.usage(
            "Storage Read API",
            "TiB",
            "monthly_storage_read_api_tib",
            bq_query(ctx, regions, |r| Query::new(BQ_STORAGE_API, r, "^BigQuery Storage API - Read Bytes Scanned").ondemand()),
        ),
    ]
}

/// True if a reservation assignment exists for this location, meaning
/// queries run on reserved slots instead of on-demand pricing.
fn uses_reservation(ctx: &Ctx, loc: &str) -> bool {
    let loc = norm_location(loc);
    ctx.siblings.iter().any(|r| {
        r.rtype == "google_bigquery_reservation_assignment"
            && r.attrs.get("location").and_then(Value::as_str).is_none_or(|l| norm_location(l) == loc)
    })
}

fn bq_queries(ctx: &Ctx, regions: &[String], key: &str) -> Component {
    ctx.usage(
        "Queries (on-demand)",
        "TiB",
        key,
        bq_query(ctx, regions, |r| Query::new(BQ, r, "^Analysis( \\(|$)").group("OnDemandAnalysis").ondemand()),
    )
}

fn bq_dataset(ctx: &Ctx) -> Vec<Component> {
    let loc = bq_location(ctx);
    let regions = bq_regions(&loc);
    let physical = ctx.str("storage_billing_model") == Some("PHYSICAL");
    let mut out = bq_storage(ctx, &regions, physical, ctx.num_or("max_time_travel_hours", 168.0));
    if !uses_reservation(ctx, &loc) {
        out.push(bq_queries(ctx, &regions, "monthly_queries_tib"));
    }
    out.extend(bq_ingest(ctx, &regions));
    out
}

/// The dataset resource a table belongs to.
fn table_dataset<'a>(ctx: &Ctx<'a>) -> Option<&'a ResourceInstance> {
    let ds = ctx.str("dataset_id")?;
    ctx.resolve_ref(ds, "google_bigquery_dataset").filter(|r| r.rtype == "google_bigquery_dataset").or_else(|| {
        ctx.siblings.iter().find(|r| r.rtype == "google_bigquery_dataset" && r.attrs.get("dataset_id").and_then(Value::as_str) == Some(ds))
    })
}

fn bq_table(ctx: &Ctx) -> Vec<Component> {
    let materialized = ctx.block("materialized_view").is_some();
    if !materialized && (ctx.block("view").is_some() || ctx.block("external_data_configuration").is_some()) {
        return vec![];
    }
    let ds = table_dataset(ctx);
    let loc = ds.map(bq_location_of).unwrap_or_else(|| "us".into());
    let physical = ds.and_then(|d| d.attrs.get("storage_billing_model")).and_then(Value::as_str) == Some("PHYSICAL");
    let tt = ds.and_then(|d| d.attrs.get("max_time_travel_hours")).and_then(Value::as_f64).unwrap_or(168.0);
    let regions = bq_regions(&loc);
    let mut out = bq_storage(ctx, &regions, physical, tt);
    if !materialized {
        out.extend(bq_ingest(ctx, &regions));
    }
    out
}

fn edition_of(attrs: &Value) -> String {
    attrs.get("edition").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("ENTERPRISE").to_uppercase()
}

fn edition_label(e: &str) -> &'static str {
    match e {
        "STANDARD" => "Standard",
        "ENTERPRISE_PLUS" => "Enterprise Plus",
        _ => "Enterprise",
    }
}

/// SKU regex for edition slots, pay-as-you-go (`years` = 0) or committed.
fn edition_pattern(edition: &str, years: u8) -> String {
    let label = edition_label(edition);
    match years {
        1 => format!("^BigQuery {label} Edition 1 Year for "),
        3 => format!("^BigQuery {label} Edition 3 Years for "),
        _ => format!("^BigQuery {label} Edition for "),
    }
}

fn edition_usage(years: u8) -> &'static str {
    match years {
        1 => "Commit1Yr",
        3 => "Commit3Yr",
        _ => "OnDemand",
    }
}

/// Hourly price of one slot of `edition` in `loc`.
fn slot_price(ctx: &Ctx, loc: &str, edition: &str, years: u8) -> Option<f64> {
    let regions = bq_regions(loc);
    let pat = edition_pattern(edition, years);
    let q = bq_query(ctx, &regions, |r| Query::new(BQ_RES, r, &pat).usage(edition_usage(years)));
    ctx.price(q).map(|p| p.unit_price())
}

/// Baseline slots of this reservation covered by capacity commitments of the
/// same location and edition. Commitments are allocated to reservations in
/// address order.
fn committed_coverage(ctx: &Ctx, loc: &str, edition: &str, baseline: f64) -> f64 {
    let loc = norm_location(loc);
    let matches = |r: &ResourceInstance| bq_location_of(r) == loc && edition_of(&r.attrs) == edition;
    let committed: f64 = ctx
        .siblings
        .iter()
        .filter(|r| r.rtype == "google_bigquery_capacity_commitment" && matches(r))
        .filter_map(|r| r.attrs.get("slot_count").and_then(Value::as_f64))
        .sum();
    if committed <= 0.0 {
        return 0.0;
    }
    let mut reservations: Vec<(&str, f64)> = ctx
        .siblings
        .iter()
        .filter(|r| r.rtype == "google_bigquery_reservation" && r.address != ctx.r.address && matches(r))
        .map(|r| (r.address.as_str(), r.attrs.get("slot_capacity").and_then(Value::as_f64).unwrap_or(0.0)))
        .collect();
    reservations.push((ctx.r.address.as_str(), baseline));
    reservations.sort_by(|a, b| a.0.cmp(b.0));
    let mut remaining = committed;
    for (addr, slots) in reservations {
        let take = slots.min(remaining);
        if addr == ctx.r.address {
            return take;
        }
        remaining -= take;
    }
    0.0
}

struct Reservation {
    loc: String,
    edition: String,
    baseline: f64,
    max_slots: f64,
    covered: f64,
}

fn reservation_of(ctx: &Ctx) -> Reservation {
    let loc = bq_location(ctx);
    let edition = edition_of(ctx.attrs);
    let baseline = ctx.num_or("slot_capacity", 0.0);
    let covered = committed_coverage(ctx, &loc, &edition, baseline);
    Reservation { max_slots: ctx.num_or("autoscale.0.max_slots", 0.0), loc, edition, baseline, covered }
}

fn bq_reservation(ctx: &Ctx) -> Vec<Component> {
    if ctx.str("edition").is_none() {
        ctx.warn("edition not set, assuming ENTERPRISE");
    }
    let r = reservation_of(ctx);
    let regions = bq_regions(&r.loc);
    let pat = edition_pattern(&r.edition, 0);
    let q = bq_query(ctx, &regions, |reg| Query::new(BQ_RES, reg, &pat).usage("OnDemand"));
    let label = edition_label(&r.edition);
    let mut out = vec![];
    if r.baseline > 0.0 {
        let mut c = ctx.hourly_unit(format!("Baseline slots ({label}, {})", fmt(r.baseline)), "slot-hours", r.baseline - r.covered, q);
        if r.covered > 0.0 {
            c.note = Some(format!("{} slots covered by capacity commitments (billed on the commitment)", fmt(r.covered)));
        }
        out.push(c);
    }
    if r.max_slots > 0.0 {
        let avg = ctx.usage_num("average_autoscale_slots").map(|s| s.min(r.max_slots));
        out.push(ctx.usage_qty(
            format!("Autoscale slots ({label}, up to {})", fmt(r.max_slots)),
            "slot-hours",
            avg.map(|s| s * ctx.monthly_hours()),
            q,
        ));
    }
    out
}

fn bq_commitment(ctx: &Ctx) -> Vec<Component> {
    let loc = bq_location(ctx);
    let regions = bq_regions(&loc);
    let slots = ctx.num_or("slot_count", 0.0);
    let plan = ctx.str_or("plan", "ANNUAL").to_uppercase();
    let hours = slots * HOURS_PER_MONTH;
    match (ctx.str("edition").map(str::to_uppercase), plan.as_str()) {
        (Some(e), "FLEX" | "MONTHLY" | "TRIAL") => {
            ctx.warn(format!("{plan} commitments are not available for BigQuery editions ({e})"));
            vec![]
        }
        (None, "FLEX") => vec![ctx.monthly(
            format!("Flex slots (legacy flat-rate, {})", fmt(slots)),
            "slot-hours",
            hours,
            bq_query(ctx, &regions, |r| Query::new(BQ_RES, r, "^BigQuery Flat Rate Flex for ").unit("h")),
        )],
        (None, "MONTHLY") => vec![ctx.monthly(
            format!("Monthly slots (legacy flat-rate, {})", fmt(slots)),
            "slot-months",
            slots,
            bq_query(ctx, &regions, |r| Query::new(BQ_RES, r, "^BigQuery Flat Rate Monthly for ")),
        )],
        (None, "TRIAL") => vec![],
        (edition, _) => {
            let edition = edition.unwrap_or_else(|| {
                ctx.warn("edition not set, assuming ENTERPRISE");
                "ENTERPRISE".into()
            });
            if edition == "STANDARD" {
                ctx.warn("capacity commitments are not available for the Standard edition");
                return vec![];
            }
            let years = if plan == "THREE_YEAR" { 3 } else { 1 };
            let pat = edition_pattern(&edition, years);
            vec![ctx.monthly(
                format!("Committed slots ({}, {years}y, {})", edition_label(&edition), fmt(slots)),
                "slot-hours",
                hours,
                bq_query(ctx, &regions, |r| Query::new(BQ_RES, r, &pat).usage(edition_usage(years))),
            )]
        }
    }
}

fn bq_bi_reservation(ctx: &Ctx) -> Vec<Component> {
    let loc = bq_location(ctx);
    let regions = bq_regions(&loc);
    let gib = ctx.num_or("size", 0.0) / (1u64 << 30) as f64;
    if gib <= 0.0 {
        return vec![];
    }
    vec![ctx.hourly_unit(
        format!("BI Engine capacity ({} GiB)", fmt(gib)),
        "GiB-hours",
        gib,
        bq_query(ctx, &regions, |r| Query::new(BQ_BI, r, "^BigQuery BI Engine for ").ondemand()),
    )]
}

fn bq_transfer(ctx: &Ctx) -> Vec<Component> {
    let source = ctx.str_or("data_source_id", "");
    match source {
        "scheduled_query" => {
            let loc = bq_location(ctx);
            if uses_reservation(ctx, &loc) {
                return vec![];
            }
            vec![bq_queries(ctx, &bq_regions(&loc), "monthly_queries_tib")]
        }
        "google_cloud_storage" | "amazon_s3" | "azure_blob_storage" | "cross_region_copy" | "redshift" | "on_premises" | "" => vec![],
        other => {
            ctx.warn(format!("BigQuery Data Transfer Service connector '{other}' is billed per source (not in the pricing catalog)"));
            vec![]
        }
    }
}

// ---- Dataflow ---------------------------------------------------------------

fn norm_key(k: &str) -> String {
    k.chars().filter(|c| !matches!(c, '_' | '-')).collect::<String>().to_lowercase()
}

fn df_param<'a>(ctx: &Ctx<'a>, names: &[&str]) -> Option<&'a Value> {
    let m = ctx.get("parameters")?.as_map()?;
    m.iter().find(|(k, _)| names.contains(&norm_key(k).as_str())).map(|(_, v)| v)
}

fn df_param_str<'a>(ctx: &Ctx<'a>, names: &[&str]) -> Option<&'a str> {
    df_param(ctx, names).and_then(Value::as_str)
}

fn df_experiments(ctx: &Ctx) -> Vec<String> {
    let mut out: Vec<String> = ctx
        .get("additional_experiments")
        .and_then(Value::as_list)
        .map(|l| l.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    for key in ["experiments", "dataflowserviceoptions"] {
        if let Some(s) = df_param_str(ctx, &[key]) {
            out.extend(s.split(',').map(|x| x.trim().to_string()));
        }
    }
    out
}

/// Parse `worker_accelerator=type:nvidia-tesla-t4;count:1;install-nvidia-driver`.
fn parse_worker_accelerator(s: &str) -> Option<(String, f64)> {
    let spec = s.strip_prefix("worker_accelerator=")?;
    let mut t = None;
    let mut count = 1.0;
    for part in spec.split(';') {
        if let Some(v) = part.strip_prefix("type:") {
            t = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("count:") {
            count = v.parse().unwrap_or(1.0);
        }
    }
    Some((t?, count))
}

/// Dataflow SKU stem of a GPU type, with a description exclusion.
fn df_gpu_stem(t: &str) -> Option<(&'static str, Option<&'static str>)> {
    Some(match t {
        "nvidia-tesla-t4" => ("T4", None),
        "nvidia-l4" => ("L4", None),
        "nvidia-tesla-p4" => ("P4", None),
        "nvidia-tesla-p100" => ("P100", None),
        "nvidia-tesla-v100" => ("V100", None),
        "nvidia-tesla-a100" => ("A100", Some("80GB")),
        "nvidia-a100-80gb" => ("A100 80GB", None),
        "nvidia-h100-80gb" => ("H100", Some("Mega")),
        "nvidia-h100-mega-80gb" => ("H100 Mega", None),
        _ => return None,
    })
}

struct DfJob {
    streaming: bool,
    streaming_engine: bool,
    flexrs: bool,
    prime: bool,
    machine: String,
    workers: f64,
    max_workers: Option<f64>,
    disk_gb: f64,
    disk_ssd: bool,
    gpu: Option<(String, f64)>,
}

fn df_job(ctx: &Ctx) -> DfJob {
    let exps = df_experiments(ctx);
    let has_exp = |name: &str| exps.iter().any(|e| e == name || e.starts_with(&format!("{name}=")));
    let template = ctx.str("template_gcs_path").or_else(|| ctx.str("container_spec_gcs_path")).unwrap_or("").to_lowercase();
    let template_name = template.rsplit('/').next().unwrap_or("");
    let param_true = |names: &[&str]| df_param(ctx, names).and_then(Value::as_bool) == Some(true);
    let prime = has_exp("enable_prime");
    let explicit_engine =
        ctx.bool("enable_streaming_engine") == Some(true) || has_exp("enable_streaming_engine") || param_true(&["enablestreamingengine"]);
    let streaming = match ctx.usage_num("streaming") {
        Some(v) => v > 0.0,
        None => {
            explicit_engine
                || param_true(&["streaming"])
                || ctx
                    .get("parameters")
                    .and_then(Value::as_map)
                    .is_some_and(|m| m.keys().map(|k| norm_key(k)).any(|k| k.contains("subscription") || k == "inputtopic" || k == "topic"))
                || ["pubsub", "stream", "kafka", "cdc"].iter().any(|s| template_name.contains(s))
        }
    };
    // Dataflow Prime streaming jobs always use Streaming Engine.
    let streaming_engine = explicit_engine || (prime && streaming);
    let flexrs = !streaming
        && (df_param_str(ctx, &["flexrsgoal"]).is_some_and(|g| g.eq_ignore_ascii_case("COST_OPTIMIZED"))
            || exps.iter().any(|e| e.eq_ignore_ascii_case("flexrs_goal=COST_OPTIMIZED")));
    let default_machine = match (streaming, streaming_engine) {
        (false, _) => "n1-standard-1",
        (true, true) => "n1-standard-2",
        (true, false) => "n1-standard-4",
    };
    let machine =
        ctx.str("machine_type").or_else(|| df_param_str(ctx, &["workermachinetype", "machinetype"])).unwrap_or(default_machine).to_string();
    let num_workers = ctx.num("num_workers").or_else(|| df_param(ctx, &["numworkers"]).and_then(Value::as_f64));
    let default_disk = match (streaming, streaming_engine) {
        (false, _) => 25.0,
        (true, true) => 30.0,
        (true, false) => 400.0,
    };
    DfJob {
        streaming,
        streaming_engine,
        flexrs,
        prime,
        machine,
        workers: ctx.usage_num("workers").or(num_workers).unwrap_or(1.0),
        max_workers: ctx.num("max_workers").or_else(|| df_param(ctx, &["maxnumworkers", "maxworkers"]).and_then(Value::as_f64)),
        disk_gb: df_param(ctx, &["disksizegb"]).and_then(Value::as_f64).filter(|d| *d > 0.0).unwrap_or(default_disk),
        disk_ssd: df_param_str(ctx, &["workerdisktype"]).is_some_and(|t| t.contains("pd-ssd")),
        gpu: exps.iter().find_map(|e| parse_worker_accelerator(e)),
    }
}

fn dataflow_job(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let job = df_job(ctx);
    let Some(mt) = machine::parse(&job.machine) else {
        ctx.warn(format!("unknown machine type '{}'", job.machine));
        return vec![];
    };
    let hours = if job.streaming { Some(ctx.monthly_hours()) } else { ctx.usage_num("monthly_hrs") };
    if let (None, Some(max)) = (ctx.usage_num("workers"), job.max_workers.filter(|m| *m > job.workers)) {
        ctx.warn(format!("autoscaled up to {} workers; estimating with {} (set usage 'workers' to override)", fmt(max), fmt(job.workers)));
    }
    let worker_hours = hours.map(|h| h * job.workers);
    let arm = matches!(mt.family.as_str(), "t2a" | "c4a");
    let mode = match (job.streaming, job.flexrs) {
        (true, _) => "streaming",
        (false, true) => "FlexRS batch",
        (false, false) => "batch",
    };
    let workers_label = format!("{}, {mode}, {} worker{}", mt.name, fmt(job.workers), if job.workers == 1.0 { "" } else { "s" });
    let mut out = vec![];
    if job.prime {
        let pat = if job.streaming { "^Dataflow Compute Units Streaming " } else { "^Dataflow Compute Units Batch " };
        out.push(ctx.usage("Data Compute Units (Dataflow Prime)", "DCUs", "monthly_dcus", Query::new(DATAFLOW, &region, pat).ondemand()));
    } else {
        let (cpu_pat, ram_pat, ram_ex) = match (job.streaming, job.flexrs, arm) {
            (true, _, true) => ("^VCPU Time Arm Streaming ", "^RAM Time Arm Streaming ", None),
            (true, _, false) => ("^vCPU Time Streaming ", "^RAM Time Streaming ", None),
            (false, true, _) => ("^vCPU Time FlexRS Batch ", "^RAM Time FlexRS ", None),
            (false, false, true) => ("^VCPU Time Arm Batch ", "^RAM Time Arm Batch ", None),
            (false, false, false) => ("^vCPU Time Batch ", "^RAM Time ", Some("Streaming|FlexRS|Arm|Confidential")),
        };
        let mut ram_q = Query::new(DATAFLOW, &region, ram_pat).ondemand();
        if let Some(ex) = ram_ex {
            ram_q = ram_q.exclude(ex);
        }
        out.push(ctx.usage_qty(
            format!("Worker vCPU ({workers_label})"),
            "vCPU-hours",
            worker_hours.map(|h| h * mt.vcpus),
            Query::new(DATAFLOW, &region, cpu_pat).ondemand(),
        ));
        out.push(ctx.usage_qty(
            format!("Worker memory ({} GiB)", fmt(mt.memory_gb)),
            "GiB-hours",
            worker_hours.map(|h| h * mt.memory_gb),
            ram_q,
        ));
    }
    let (disk_label, disk_pat) =
        if job.disk_ssd { ("pd-ssd", "^Local Disk Time PD SSD") } else { ("pd-standard", "^Local Disk Time PD Standard") };
    out.push(ctx.usage_qty(
        format!("Worker disk ({disk_label}, {} GiB)", fmt(job.disk_gb)),
        "GiB-hours",
        worker_hours.map(|h| h * job.disk_gb),
        Query::new(DATAFLOW, &region, disk_pat).ondemand(),
    ));
    if let Some((t, n)) = &job.gpu {
        match df_gpu_stem(t) {
            Some((stem, ex)) => {
                let pat = format!("^GPU Time {stem} ");
                let mut q = Query::new(DATAFLOW, &region, &pat).ondemand();
                if let Some(ex) = ex {
                    q = q.exclude(ex);
                }
                out.push(ctx.usage_qty(format!("Worker GPU ({t} x{})", fmt(*n)), "GPU-hours", worker_hours.map(|h| h * n), q));
            }
            None => ctx.warn(format!("unknown Dataflow accelerator type '{t}'")),
        }
    }
    if !job.prime {
        if job.streaming && job.streaming_engine {
            out.push(ctx.usage(
                "Streaming Engine",
                "compute units",
                "monthly_streaming_engine_compute_units",
                Query::new(DATAFLOW, &region, "^Streaming Engine Compute Unit ").ondemand(),
            ));
        } else if !job.streaming {
            out.push(ctx.usage(
                "Dataflow Shuffle data processed",
                "GiB",
                "monthly_shuffle_data_gb",
                Query::new(DATAFLOW, &region, "^Shuffle chargeable data processed for ").ondemand(),
            ));
        }
    }
    out
}

// ---- Dataproc ---------------------------------------------------------------

const DATAPROC_DEFAULT_MACHINE: &str = "n2-standard-4";

struct DpRole {
    label: &'static str,
    machine: String,
    count: f64,
    pricing: Pricing,
    disk_type: String,
    disk_gb: f64,
    local_ssd_gb: f64,
    gpus: Vec<(String, f64)>,
}

fn dp_role(cfg: Option<&Value>, label: &'static str, count: f64, machine: &str, pricing: Pricing) -> DpRole {
    let disk = cfg.and_then(|c| c.path("disk_config.0"));
    let gpus = cfg
        .and_then(|c| c.get("accelerators"))
        .and_then(Value::as_list)
        .map(|l| {
            l.iter().filter_map(|a| Some((vstr(a, "accelerator_type")?.to_string(), vnum(a, "accelerator_count").unwrap_or(1.0)))).collect()
        })
        .unwrap_or_default();
    DpRole {
        label,
        machine: cfg.and_then(|c| vstr(c, "machine_type")).unwrap_or(machine).to_string(),
        count,
        pricing,
        disk_type: disk.and_then(|d| vstr(d, "boot_disk_type")).unwrap_or("pd-standard").to_string(),
        disk_gb: disk.and_then(|d| vnum(d, "boot_disk_size_gb")).unwrap_or(500.0),
        local_ssd_gb: disk.and_then(|d| vnum(d, "num_local_ssds")).unwrap_or(0.0) * 375.0,
        gpus,
    }
}

const DP_POLICY_URI: &str = "cluster_config.0.autoscaling_config.0.policy_uri";

fn dp_has_policy(ctx: &Ctx) -> bool {
    ctx.get(DP_POLICY_URI).is_some() || ctx.is_unknown(DP_POLICY_URI)
}

/// The autoscaling policy of a cluster: by URI / policy id, or through a
/// reference to a computed attribute (e.g. `google_dataproc_autoscaling_policy.x.name`).
fn dp_policy<'a>(ctx: &Ctx<'a>) -> Option<&'a ResourceInstance> {
    if !dp_has_policy(ctx) {
        return None;
    }
    let is_policy = |r: &&ResourceInstance| r.rtype == "google_dataproc_autoscaling_policy";
    let by_uri = ctx.str(DP_POLICY_URI).and_then(|uri| {
        ctx.resolve_ref(uri, "google_dataproc_autoscaling_policy").filter(is_policy).or_else(|| {
            let last = uri.rsplit('/').next()?;
            ctx.siblings.iter().filter(is_policy).find(|r| r.attrs.get("policy_id").and_then(Value::as_str) == Some(last))
        })
    });
    by_uri.or_else(|| ctx.siblings.iter().filter(is_policy).find(|r| ctx.r.refs.contains(&format!("{}.{}", r.rtype, r.name))))
}

fn dp_roles(ctx: &Ctx) -> Vec<DpRole> {
    let cc = ctx.block("cluster_config");
    let cfg = |k: &str| cc.and_then(|c| c.path(&format!("{k}.0")));
    let (master, worker, secondary) = (cfg("master_config"), cfg("worker_config"), cfg("preemptible_worker_config"));
    let zero_workers = cc
        .and_then(|c| c.path("software_config.0.override_properties"))
        .and_then(|p| p.get("dataproc:dataproc.allow.zero.workers"))
        .and_then(Value::as_bool)
        == Some(true);
    let policy = dp_policy(ctx);
    let bounds = |key: &str, dmin: f64| {
        policy.and_then(|p| p.attrs.path(&format!("{key}.0"))).map(|c| {
            let min = vnum(c, "min_instances").unwrap_or(dmin);
            (min, vnum(c, "max_instances").unwrap_or(min))
        })
    };
    let clamp = |n: f64, b: Option<(f64, f64)>| b.map(|(lo, hi)| n.clamp(lo, hi.max(lo))).unwrap_or(n);
    let configured_workers = worker.and_then(|w| vnum(w, "num_instances")).unwrap_or(if zero_workers { 0.0 } else { 2.0 });
    let workers = ctx.usage_num("workers").unwrap_or_else(|| clamp(configured_workers, bounds("worker_config", 2.0)));
    let configured_secondary = secondary.and_then(|s| vnum(s, "num_instances")).unwrap_or(0.0);
    let secondaries =
        ctx.usage_num("secondary_workers").unwrap_or_else(|| clamp(configured_secondary, bounds("secondary_worker_config", 0.0)));
    let max_total =
        bounds("worker_config", 2.0).map_or(workers, |b| b.1) + bounds("secondary_worker_config", 0.0).map_or(secondaries, |b| b.1);
    if policy.is_some() && max_total > workers + secondaries && ctx.usage_num("workers").is_none() {
        ctx.warn(format!(
            "autoscaled up to {} workers; estimating with {} (set usage 'workers' / 'secondary_workers' to override)",
            fmt(max_total),
            fmt(workers + secondaries)
        ));
    }

    let worker_machine = worker.and_then(|w| vstr(w, "machine_type")).unwrap_or(DATAPROC_DEFAULT_MACHINE);
    let secondary_machine = secondary
        .and_then(|s| vstr(s, "instance_flexibility_policy.0.instance_selection_list.0.machine_types.0"))
        .unwrap_or(worker_machine);
    let secondary_pricing = match secondary.and_then(|s| vstr(s, "preemptibility")) {
        Some("NON_PREEMPTIBLE") => Pricing::OnDemand,
        _ => Pricing::Spot,
    };
    let masters = master.and_then(|m| vnum(m, "num_instances")).unwrap_or(1.0);
    vec![
        dp_role(master, "Master", masters, DATAPROC_DEFAULT_MACHINE, Pricing::OnDemand),
        dp_role(worker, "Workers", workers, DATAPROC_DEFAULT_MACHINE, Pricing::OnDemand),
        dp_role(secondary, "Secondary workers", secondaries, secondary_machine, secondary_pricing),
    ]
}

fn dataproc_cluster(ctx: &Ctx) -> Vec<Component> {
    if ctx.block("virtual_cluster_config").is_some() && ctx.block("cluster_config").is_none() {
        ctx.warn("Dataproc on GKE: nodes are billed on the GKE node pools");
        return vec![];
    }
    let region = ctx.region();
    let cc = ctx.block("cluster_config");
    let external_ips = cc.and_then(|c| c.path("gce_cluster_config.0.internal_ip_only")).and_then(Value::as_bool) == Some(false);
    let mut out = vec![];
    let mut vcpus = 0.0;
    for (role, key) in dp_roles(ctx).into_iter().zip(["master_config", "worker_config", "preemptible_worker_config"]) {
        if role.count <= 0.0 {
            continue;
        }
        let explicit = |k: &str| {
            ["machine_type", "instance_flexibility_policy.0.instance_selection_list.0.machine_types.0"]
                .iter()
                .any(|p| cc.and_then(|c| c.path(&format!("{k}.0.{p}"))).and_then(Value::as_str).is_some())
        };
        if !explicit(key) && (key == "master_config" || !explicit("worker_config")) {
            ctx.warn(format!("machine_type not set, assuming the Dataproc default {DATAPROC_DEFAULT_MACHINE}"));
        }
        if let Some(mt) = machine::parse(&role.machine) {
            vcpus += mt.vcpus * role.count;
        }
        let spec = VmSpec {
            machine: role.machine.clone(),
            pricing: role.pricing,
            count: role.count,
            disks: vec![(role.disk_type.clone(), role.disk_gb, None, None)],
            local_ssd_gb: role.local_ssd_gb,
            gpus: role.gpus.clone(),
            ephemeral_ips: if external_ips { 1.0 } else { 0.0 },
            image: None,
        };
        for mut c in vm_components(ctx, &region, &spec) {
            c.name = format!("{}: {}", role.label, c.name.replacen("Boot disk", "boot disk", 1));
            out.push(c);
        }
    }
    if vcpus > 0.0 {
        out.push(ctx.hourly_unit(
            format!("Dataproc fee ({} vCPUs)", fmt(vcpus)),
            "vCPU-hours",
            vcpus,
            Query::new(COMPUTE, "global", "^Licensing Fee for Google Cloud Dataproc \\(CPU cost\\)").ondemand(),
        ));
    }
    out
}

/// Parse a Spark memory size (`512m`, `16g`, `12200m`) into GiB.
fn spark_mem_gb(s: &str) -> Option<f64> {
    let s = s.trim().to_lowercase();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.parse().ok()?;
    Some(match unit.trim_end_matches('b') {
        "k" => n / (1024.0 * 1024.0),
        "m" | "" => n / 1024.0,
        "g" => n,
        "t" => n * 1024.0,
        _ => return None,
    })
}

/// DCUs of a Serverless Spark batch from its Spark properties: 0.6 DCU per
/// vCPU and 0.1 DCU per GiB (memory + 10% overhead) for the driver and
/// executors.
fn spark_dcus(props: Option<&Value>) -> f64 {
    let p = |k: &str| props.and_then(|m| m.get(k));
    let num = |k: &str, d: f64| p(k).and_then(Value::as_f64).unwrap_or(d);
    let mem = |k: &str| p(k).and_then(Value::as_str).and_then(spark_mem_gb).unwrap_or(12200.0 / 1024.0) * 1.1;
    let executors = num("spark.executor.instances", 2.0);
    let driver = num("spark.driver.cores", 4.0) * 0.6 + mem("spark.driver.memory") * 0.1;
    let executor = num("spark.executor.cores", 4.0) * 0.6 + mem("spark.executor.memory") * 0.1;
    driver + executors * executor
}

fn dataproc_batch(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let props = ctx.get("runtime_config.0.properties");
    let premium = ["dataproc.tier", "spark.dataproc.tier"]
        .iter()
        .any(|k| props.and_then(|m| m.get(k)).and_then(Value::as_str).is_some_and(|t| t.eq_ignore_ascii_case("premium")));
    let dcus = spark_dcus(props);
    let dcu_hours = ctx.usage_num("monthly_dcu_hours").or_else(|| ctx.usage_num("monthly_hrs").map(|h| h * dcus));
    let (tier, dcu_pat, shuffle_pat) = if premium {
        (
            "premium",
            "^Data Compute Unit Premium \\(milli\\) Hours - Dataproc Serverless Batch",
            "^Dataproc Serverless Shuffle Storage Premium GB-Months",
        )
    } else {
        ("standard", "^Data Compute Unit \\(milli\\) Hours - Dataproc Serverless Batch", "^Dataproc Serverless Shuffle Storage GB-Months")
    };
    let dcu = ctx.usage_qty(
        format!("Data Compute Units ({tier}, ~{} DCU)", fmt((dcus * 10.0).round() / 10.0)),
        "milli DCU-hours",
        dcu_hours.map(|h| h * 1000.0),
        Query::new(DATAPROC, &region, dcu_pat).ondemand(),
    );
    vec![
        rescale(dcu, 1000.0, "DCU-hours"),
        ctx.usage("Shuffle storage", "GiB-months", "monthly_shuffle_storage_gb", Query::new(DATAPROC, &region, shuffle_pat).ondemand()),
    ]
}

// ---- Cloud Composer ---------------------------------------------------------

/// Major Composer version from an image version like `composer-2.9.7-airflow-2.9.3`.
fn composer_major(image: &str) -> Option<u8> {
    let rest = image.strip_prefix("composer-")?;
    match rest.split(['.', '-']).next()? {
        "1" => Some(1),
        "2" => Some(2),
        "3" => Some(3),
        "latest" => Some(3),
        _ => None,
    }
}

fn composer_version(ctx: &Ctx) -> u8 {
    if let Some(v) = ctx.str("config.0.software_config.0.image_version").and_then(composer_major) {
        return v;
    }
    if ctx.get("config.0.node_count").is_some() || ctx.get("config.0.node_config.0.machine_type").is_some() {
        return 1;
    }
    if ctx.get("config.0.workloads_config.0.dag_processor").is_some() {
        return 3;
    }
    ctx.warn("image_version not set, assuming Cloud Composer 3");
    3
}

#[derive(Clone, Copy, Default)]
struct Workload {
    cpu: f64,
    mem: f64,
    storage: f64,
    count: f64,
}

impl Workload {
    const fn new(count: f64, cpu: f64, mem: f64, storage: f64) -> Self {
        Workload { cpu, mem, storage, count }
    }
}

struct Workloads {
    scheduler: Workload,
    web_server: Workload,
    worker: Workload,
    worker_max: f64,
    triggerer: Workload,
    dag_processor: Workload,
}

fn size_key(size: &str) -> &'static str {
    match size {
        "ENVIRONMENT_SIZE_MEDIUM" => "medium",
        "ENVIRONMENT_SIZE_LARGE" => "large",
        _ => "small",
    }
}

/// Default workloads of the environment presets (used for unset values).
fn composer_presets(version: u8, size: &str) -> Workloads {
    let w = Workload::new;
    let none = Workload::default();
    match (version, size) {
        (3, "medium") => Workloads {
            scheduler: w(2.0, 1.0, 4.0, 1.0),
            web_server: w(1.0, 1.0, 4.0, 1.0),
            worker: w(2.0, 2.0, 7.5, 5.0),
            worker_max: 6.0,
            triggerer: w(1.0, 0.5, 1.0, 0.0),
            dag_processor: w(2.0, 1.0, 4.0, 1.0),
        },
        (3, "large") => Workloads {
            scheduler: w(2.0, 2.0, 7.5, 5.0),
            web_server: w(1.0, 2.0, 7.5, 10.0),
            worker: w(3.0, 4.0, 15.0, 10.0),
            worker_max: 12.0,
            triggerer: w(1.0, 1.0, 1.0, 0.0),
            dag_processor: w(2.0, 2.0, 7.5, 5.0),
        },
        (3, _) => Workloads {
            scheduler: w(1.0, 0.5, 2.0, 1.0),
            web_server: w(1.0, 0.5, 2.0, 1.0),
            worker: w(1.0, 0.5, 2.0, 1.0),
            worker_max: 3.0,
            triggerer: w(1.0, 0.5, 1.0, 0.0),
            dag_processor: w(1.0, 1.0, 2.0, 1.0),
        },
        (_, "medium") => Workloads {
            scheduler: w(2.0, 2.0, 7.5, 5.0),
            web_server: w(1.0, 2.0, 7.5, 5.0),
            worker: w(2.0, 2.0, 7.5, 5.0),
            worker_max: 6.0,
            triggerer: none,
            dag_processor: none,
        },
        (_, "large") => Workloads {
            scheduler: w(2.0, 4.0, 15.0, 10.0),
            web_server: w(1.0, 2.0, 7.5, 10.0),
            worker: w(3.0, 4.0, 15.0, 10.0),
            worker_max: 12.0,
            triggerer: none,
            dag_processor: none,
        },
        _ => Workloads {
            scheduler: w(1.0, 0.5, 1.875, 1.0),
            web_server: w(1.0, 0.5, 1.875, 1.0),
            worker: w(1.0, 0.5, 1.875, 1.0),
            worker_max: 3.0,
            triggerer: none,
            dag_processor: none,
        },
    }
}

fn composer_workloads(ctx: &Ctx, version: u8, size: &str) -> Workloads {
    let mut w = composer_presets(version, size);
    let Some(cfg) = ctx.get("config.0.workloads_config.0") else { return w };
    let apply = |base: &mut Workload, key: &str, count_key: Option<&str>| {
        let Some(b) = cfg.path(&format!("{key}.0")) else { return };
        if base.count == 0.0 {
            base.count = 1.0;
        }
        base.cpu = vnum(b, "cpu").unwrap_or(base.cpu);
        base.mem = vnum(b, "memory_gb").unwrap_or(base.mem);
        base.storage = vnum(b, "storage_gb").unwrap_or(base.storage);
        if let Some(k) = count_key {
            base.count = vnum(b, k).unwrap_or(base.count);
        }
    };
    apply(&mut w.scheduler, "scheduler", Some("count"));
    apply(&mut w.web_server, "web_server", None);
    apply(&mut w.worker, "worker", Some("min_count"));
    apply(&mut w.triggerer, "triggerer", Some("count"));
    apply(&mut w.dag_processor, "dag_processor", Some("count"));
    if let Some(max) = cfg.path("worker.0.max_count").and_then(Value::as_f64) {
        w.worker_max = max;
    }
    w
}

/// Total (vCPU, memory GiB, storage GiB) of the Airflow components.
fn workload_totals(w: &Workloads, workers: f64) -> (f64, f64, f64) {
    let mut worker = w.worker;
    worker.count = workers;
    [w.scheduler, w.web_server, worker, w.triggerer, w.dag_processor]
        .iter()
        .fold((0.0, 0.0, 0.0), |acc, x| (acc.0 + x.count * x.cpu, acc.1 + x.count * x.mem, acc.2 + x.count * x.storage))
}

/// Approximate Composer 3 DCUs per vCPU / GiB of memory / GiB of storage,
/// and of the environment infrastructure per size and resilience, calibrated
/// on the equivalent Composer 2 rates (us-central1: $0.045/vCPU-h,
/// $0.005/GiB-h, $0.0002/GiB-h, fees $0.35/0.55/0.85 and $0.49/0.77/1.19 per
/// hour) at $0.06 per DCU-hour.
const C3_DCU_PER_VCPU: f64 = 0.75;
const C3_DCU_PER_GIB: f64 = 0.005 / 0.06;
const C3_DCU_PER_STORAGE_GIB: f64 = 0.0002 / 0.06;

fn c3_infra_dcus(size: &str, ha: bool) -> f64 {
    let fee = match (size, ha) {
        ("medium", false) => 0.55,
        ("large", false) => 0.85,
        ("small", true) => 0.49,
        ("medium", true) => 0.77,
        ("large", true) => 1.19,
        _ => 0.35,
    };
    fee / 0.06
}

/// vCPUs of a Composer 1 web server / Cloud SQL machine type (trailing number).
fn trailing_vcpus(machine: &str) -> Option<f64> {
    machine.rsplit('-').next()?.parse().ok()
}

fn composer(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let version = composer_version(ctx);
    if version == 1 {
        return composer1(ctx, &region);
    }
    let size = size_key(ctx.str_or("config.0.environment_size", "ENVIRONMENT_SIZE_SMALL"));
    let ha = ctx.str("config.0.resilience_mode") == Some("HIGH_RESILIENCE");
    let w = composer_workloads(ctx, version, size);
    let workers = ctx.usage_num("workers").unwrap_or(w.worker.count);
    if ctx.usage_num("workers").is_none() && w.worker_max > workers {
        ctx.warn(format!(
            "workers autoscale between {} and {}; estimating with {} (set usage 'workers' to override)",
            fmt(w.worker.count),
            fmt(w.worker_max),
            fmt(workers)
        ));
    }
    let (cpu, mem, storage) = workload_totals(&w, workers);
    let hours = ctx.monthly_hours();
    let db_gb = ctx.usage_num("database_storage_gb").unwrap_or(10.0);
    let resilience = if ha { ", highly resilient" } else { "" };
    let mut out = vec![];
    if version == 3 {
        let q = Query::new(COMPOSER, &region, "^Cloud Composer 3 standard milli DCU-hours").ondemand();
        let infra = c3_infra_dcus(size, ha);
        let wl = cpu * C3_DCU_PER_VCPU + mem * C3_DCU_PER_GIB + storage * C3_DCU_PER_STORAGE_GIB;
        let note = "DCU estimate calibrated on Composer 2 equivalent rates; actual DCU usage depends on autoscaling";
        out.push(with_note(
            rescale(
                ctx.monthly(format!("Environment infrastructure ({size}{resilience})"), "milli DCU-hours", infra * hours * 1000.0, q),
                1000.0,
                "DCU-hours",
            ),
            note,
        ));
        out.push(with_note(
            rescale(
                ctx.monthly(
                    format!(
                        "Airflow workloads ({} vCPU, {} GiB, {} worker{})",
                        fmt(cpu),
                        fmt(mem),
                        fmt(workers),
                        if workers == 1.0 { "" } else { "s" }
                    ),
                    "milli DCU-hours",
                    wl * hours * 1000.0,
                    q,
                ),
                1000.0,
                "DCU-hours",
            ),
            note,
        ));
        let db = db_gb * if ha { 2.0 } else { 1.0 };
        out.push(ctx.monthly(
            format!("Database storage ({} GiB{})", fmt(db_gb), if ha { " x2 regional" } else { "" }),
            "GiB-months",
            db * hours / HOURS_PER_MONTH,
            Query::new(COMPOSER, &region, "^Cloud Composer 3 database storage").ondemand(),
        ));
        return out;
    }
    let size_label = format!("{}{} ", size[..1].to_uppercase(), &size[1..]);
    let fee_pat = format!("^{size_label}{}Cloud Composer Environment Fee", if ha { "Highly Resilient " } else { "" });
    out.push(ctx.hourly(format!("Environment fee ({size}{resilience})"), 1.0, Query::new(COMPOSER, &region, &fee_pat).ondemand()));
    out.push(rescale(
        ctx.monthly(
            format!("Compute vCPU ({} vCPU, {} worker{})", fmt(cpu), fmt(workers), if workers == 1.0 { "" } else { "s" }),
            "mCPU-hours",
            cpu * hours * 1000.0,
            Query::new(COMPOSER, &region, "^Cloud Composer Compute mCPUs").ondemand(),
        ),
        1000.0,
        "vCPU-hours",
    ));
    out.push(ctx.monthly(
        format!("Compute memory ({} GiB)", fmt(mem)),
        "GiB-hours",
        mem * hours,
        Query::new(COMPOSER, &region, "^Cloud Composer Compute Memory").ondemand(),
    ));
    out.push(ctx.monthly(
        format!("Compute storage ({} GiB)", fmt(storage)),
        "GiB-hours",
        storage * hours,
        Query::new(COMPOSER, &region, "^Cloud Composer Compute Storage").ondemand(),
    ));
    out.push(ctx.monthly(
        format!("Database storage ({} GiB)", fmt(db_gb)),
        "GiB-months",
        db_gb * hours / HOURS_PER_MONTH,
        Query::new(COMPOSER, &region, "^Cloud Composer Database Storage").ondemand(),
    ));
    out
}

fn composer1(ctx: &Ctx, region: &str) -> Vec<Component> {
    let nodes = ctx.num_or("config.0.node_count", 3.0);
    let machine = ctx.str_or("config.0.node_config.0.machine_type", "n1-standard-1");
    let disk_gb = ctx.num_or("config.0.node_config.0.disk_size_gb", 100.0);
    let spec = VmSpec {
        machine: machine.to_string(),
        pricing: Pricing::OnDemand,
        count: nodes,
        disks: vec![("pd-standard".into(), disk_gb, None, None)],
        local_ssd_gb: 0.0,
        gpus: vec![],
        ephemeral_ips: 0.0,
        image: None,
    };
    let mut out: Vec<Component> = vm_components(ctx, region, &spec)
        .into_iter()
        .map(|mut c| {
            c.name = format!("GKE nodes: {}", c.name.replacen("Boot disk", "disk", 1));
            c
        })
        .collect();
    let web = ctx.str_or("config.0.web_server_config.0.machine_type", "composer-n1-webserver-2");
    let private = ctx.block("config.0.private_environment_config").is_some();
    let web_vcpus = trailing_vcpus(web).unwrap_or(2.0) * if private { 2.0 } else { 1.0 };
    out.push(ctx.hourly_unit(
        format!("Web server ({web}{})", if private { " x2, private IP" } else { "" }),
        "vCPU-hours",
        web_vcpus,
        Query::new(COMPOSER, region, "^Cloud Composer vCPU time in").ondemand(),
    ));
    let db = ctx.str_or("config.0.database_config.0.machine_type", "db-n1-standard-2");
    out.push(ctx.hourly_unit(
        format!("Cloud SQL ({db})"),
        "vCPU-hours",
        trailing_vcpus(db).unwrap_or(2.0),
        Query::new(COMPOSER, region, "^Cloud Composer SQL vCPU time in").ondemand(),
    ));
    let db_gb = ctx.usage_num("database_storage_gb").unwrap_or(10.0);
    out.push(ctx.monthly(
        format!("Data storage (database {} GiB + web server 20 GiB)", fmt(db_gb)),
        "GiB-months",
        (db_gb + 20.0) * ctx.monthly_hours() / HOURS_PER_MONTH,
        Query::new(COMPOSER, region, "^Cloud Composer Data storage in").ondemand(),
    ));
    out
}

// ---- Data Fusion, Datastream, Dataplex -------------------------------------

fn data_fusion(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let t = ctx.str_or("type", "BASIC").to_uppercase();
    let (label, pat) = match t.as_str() {
        "DEVELOPER" => ("Developer", "^Data Fusion Developer$"),
        "ENTERPRISE" => ("Enterprise", "^Data Fusion Enterprise$"),
        _ => ("Basic", "^Data Fusion Basic$"),
    };
    vec![with_note(
        ctx.hourly(format!("Instance ({label} edition)"), 1.0, Query::new(DATA_FUSION, &region, pat).ondemand()),
        "pipeline runs are billed separately on the Dataproc clusters they use",
    )]
}

fn datastream(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let source_profile =
        ctx.str("source_config.0.source_connection_profile").and_then(|p| ctx.resolve_ref(p, "google_datastream_connection_profile"));
    let gcp_source = ctx.block("source_config.0.spanner_source_config").is_some()
        || source_profile.is_some_and(|p| p.refs.iter().any(|r| r.starts_with("google_alloydb_") || r.starts_with("google_spanner_")));
    let cdc_pat = if gcp_source { "^GCP Source CDC Bytes Processed" } else { "^CDC Bytes Processed" };
    let mut out = vec![ctx.usage("CDC data processed", "GiB", "monthly_cdc_data_gb", Query::new(DATASTREAM, &region, cdc_pat).ondemand())];
    if ctx.block("backfill_none").is_none() {
        out.push(ctx.usage(
            "Backfill data processed",
            "GiB",
            "monthly_backfill_data_gb",
            Query::new(DATASTREAM, &region, "^Backfill Bytes Processed").ondemand(),
        ));
    }
    out
}

fn dataplex_dcu(ctx: &Ctx, name: String, premium: bool) -> Component {
    let region = ctx.region();
    let pat = if premium { "^Dataplex Premium Processing \\(milli DCU-hr\\)" } else { "^Dataplex Processing \\(milli DCU-hr\\)" };
    let c = ctx.usage_qty(
        name,
        "milli DCU-hours",
        ctx.usage_num("monthly_dcu_hours").map(|h| h * 1000.0),
        Query::new(DATAPLEX, &region, pat).ondemand(),
    );
    rescale(c, 1000.0, "DCU-hours")
}

fn dataplex_datascan(ctx: &Ctx) -> Vec<Component> {
    let (kind, premium) = if ctx.block("data_quality_spec").is_some() {
        ("data quality", true)
    } else if ctx.block("data_profile_spec").is_some() {
        ("data profile", true)
    } else {
        ("data discovery", false)
    };
    vec![dataplex_dcu(ctx, format!("Scan processing ({kind}, {})", if premium { "premium" } else { "standard" }), premium)]
}

fn dataplex_task(ctx: &Ctx) -> Vec<Component> {
    let kind = if ctx.block("notebook").is_some() { "notebook" } else { "Spark" };
    vec![dataplex_dcu(ctx, format!("Task processing ({kind})"), false)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_versions() {
        assert_eq!(composer_major("composer-1.20.12-airflow-1.10.15"), Some(1));
        assert_eq!(composer_major("composer-2.9.7-airflow-2.9.3"), Some(2));
        assert_eq!(composer_major("composer-3-airflow-2.10.2-build.7"), Some(3));
        assert_eq!(composer_major("airflow-2"), None);
    }

    #[test]
    fn spark_memory() {
        assert_eq!(spark_mem_gb("16g"), Some(16.0));
        assert_eq!(spark_mem_gb("512m"), Some(0.5));
        assert_eq!(spark_mem_gb("2048"), Some(2.0));
        assert_eq!(spark_mem_gb("x"), None);
        // Default batch: driver + 2 executors of 4 vCPU / 12200m (+10%).
        assert!((spark_dcus(None) - 3.0 * (2.4 + 12200.0 / 1024.0 * 1.1 * 0.1)).abs() < 1e-9);
    }

    #[test]
    fn accelerators_and_locations() {
        assert_eq!(
            parse_worker_accelerator("worker_accelerator=type:nvidia-tesla-t4;count:2;install-nvidia-driver"),
            Some(("nvidia-tesla-t4".to_string(), 2.0))
        );
        assert_eq!(parse_worker_accelerator("enable_prime"), None);
        assert_eq!(bq_regions("eu"), vec!["eu".to_string(), "europe".to_string()]);
        assert_eq!(bq_regions("europe-west1"), vec!["europe-west1".to_string()]);
        assert_eq!(trailing_vcpus("composer-n1-webserver-4"), Some(4.0));
        assert_eq!(norm_key("flexrs_goal"), "flexrsgoal");
    }
}

pub(crate) mod rules {
    use super::*;
    use crate::cost::{CodeChange, CostedResource, Recommendation, Severity};
    use crate::resources::{Env, set_path};

    fn storage_cost(cr: &CostedResource) -> f64 {
        cr.components.iter().filter(|c| c.name.contains("storage")).filter_map(|c| c.monthly_cost).sum()
    }

    pub fn bq_dataset(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let dataset_attrs = if ctx.r.rtype == "google_bigquery_dataset" { Some(ctx.attrs) } else { table_dataset(ctx).map(|d| &d.attrs) };
        let physical = dataset_attrs.and_then(|a| a.get("storage_billing_model")).and_then(Value::as_str) == Some("PHYSICAL");
        let logical_gb = opt_sum(ctx.usage_num("storage_gb"), ctx.usage_num("long_term_storage_gb"));

        if !physical && logical_gb.is_some() {
            let current = storage_cost(cr);
            let knows_physical =
                ["physical_storage_gb", "long_term_physical_storage_gb", "compression_ratio"].iter().any(|k| ctx.usage_num(k).is_some());
            let alt = knows_physical.then(|| physical_storage_cost(ctx)).flatten();
            let target = if ctx.r.rtype == "google_bigquery_dataset" { "this dataset" } else { "the dataset" };
            match alt.and_then(|a| ctx.savings_vs(current, Some(a))) {
                Some(s) => {
                    let mut rec = ctx.rec(
                        "bq-physical-storage",
                        Severity::Low,
                        "Switch to physical (compressed) storage billing",
                        format!(
                            "With {} sizes, physical storage billing would cost ~{} instead of ~{} per month for {target}. Set storage_billing_model = \"PHYSICAL\" on google_bigquery_dataset. Physical billing also charges time travel and fail-safe bytes; lower max_time_travel_hours (e.g. 48) to limit them. The billing model can only be changed once every 14 days.",
                            sizes_basis(ctx),
                            ctx.money(current - s),
                            ctx.money(current)
                        ),
                        Some(s),
                    );
                    if ctx.r.rtype == "google_bigquery_dataset" {
                        rec.change = Some(CodeChange {
                            attribute: "storage_billing_model".into(),
                            from: "LOGICAL".into(),
                            to: "PHYSICAL".into(),
                            unset: ctx.get("storage_billing_model").is_none(),
                        });
                    }
                    out.push(rec);
                }
                None if !knows_physical => {
                    // Only logical bytes: the compression decides, give the break-even ratio.
                    let ratio = break_even_ratio(ctx);
                    let detail = match (ratio, logical_gb) {
                        (Some(r), Some(gb)) => format!(
                            "Only the logical size of {target} is known ({} GiB, ~{}/mo with logical billing). Physical billing costs {r:.1}x more per GiB here but is billed on compressed bytes: it is cheaper when the data compresses better than {r:.1}x, which is common (time travel and fail-safe bytes are added to physical storage). To compare both exactly, grant BigQuery Metadata Viewer on the dataset (or BigQuery Metadata Viewer and Job User on the project), or set usage 'compression_ratio' or 'physical_storage_gb'.",
                            fmt(gb.round()),
                            ctx.money(current)
                        ),
                        _ => "Physical storage costs about 2x more per GiB than logical storage but is billed on compressed bytes. It is cheaper when the compression ratio (INFORMATION_SCHEMA.TABLE_STORAGE: logical / physical bytes, including time travel) exceeds ~2x, which is common. Set usage 'physical_storage_gb' or 'compression_ratio' to quantify, then set storage_billing_model = \"PHYSICAL\".".to_string(),
                    };
                    out.push(ctx.rec(
                        "bq-physical-storage",
                        Severity::Info,
                        "Evaluate physical (compressed) storage billing",
                        detail,
                        None,
                    ));
                }
                None => {}
            }
        }

        // Physical billing of data that compresses poorly (or has a lot of time travel bytes).
        if physical
            && ctx.r.rtype == "google_bigquery_dataset"
            && logical_gb.is_some()
            && ctx.usage_num("physical_storage_gb").is_some()
            && let Some(s) = ctx.savings_vs(storage_cost(cr), logical_storage_cost(ctx))
        {
            let mut rec = ctx.rec(
                "bq-logical-storage",
                Severity::Low,
                "Switch to logical (uncompressed) storage billing",
                format!(
                    "With {} sizes, logical storage billing would cost ~{} instead of ~{} per month for this dataset: the data does not compress enough to make up for the higher physical rate and the time travel and fail-safe bytes. Set storage_billing_model = \"LOGICAL\". The billing model can only be changed once every 14 days.",
                    sizes_basis(ctx),
                    ctx.money(storage_cost(cr) - s),
                    ctx.money(storage_cost(cr))
                ),
                Some(s),
            );
            rec.change =
                Some(CodeChange { attribute: "storage_billing_model".into(), from: "PHYSICAL".into(), to: "LOGICAL".into(), unset: false });
            out.push(rec);
        }

        if ctx.r.rtype == "google_bigquery_dataset"
            && ctx.env == Env::NonProd
            && ctx.get("default_table_expiration_ms").is_none()
            && ctx.get("default_partition_expiration_ms").is_none()
        {
            out.push(ctx.rec(
                "bq-no-table-expiration-nonprod",
                Severity::Low,
                "Set a default table expiration on this non-production dataset",
                "Tables in dev/test datasets tend to accumulate and are billed for storage indefinitely. Set default_table_expiration_ms (or default_partition_expiration_ms for partitioned tables) so that temporary data is deleted automatically.",
                None,
            ));
        }

        if physical {
            let hours = dataset_attrs.and_then(|a| a.get("max_time_travel_hours")).and_then(Value::as_f64).unwrap_or(168.0);
            let tt = ctx.usage_num("time_travel_storage_gb");
            let unit = cr.components.iter().find(|c| c.name.starts_with("Time travel")).and_then(|c| c.unit_price);
            if let (true, Some(tt), Some(p)) = (hours > 48.0, tt, unit) {
                let s = tt * (1.0 - 48.0 / hours) * p;
                if s > 1.0 {
                    out.push(ctx.rec(
                        "bq-time-travel-window",
                        Severity::Low,
                        "Shorten the time travel window",
                        format!(
                            "With physical billing, time travel bytes are billed at the active physical rate. Reducing max_time_travel_hours from {} to 48 would cut time travel storage by ~{:.0}%. Only do so if 2 days of point-in-time recovery is enough.",
                            fmt(hours),
                            (1.0 - 48.0 / hours) * 100.0
                        ),
                        Some(s),
                    ));
                }
            }
        }
        out
    }

    /// "the observed" or "the provided" (usage file) storage sizes.
    fn sizes_basis(ctx: &Ctx) -> &'static str {
        if ctx.observed_keys.iter().any(|k| k == "physical_storage_gb" || k == "storage_gb") { "the observed" } else { "the provided" }
    }

    /// Compression ratio above which physical billing is cheaper for the
    /// logical sizes of this dataset/table (time travel and fail-safe aside).
    fn break_even_ratio(ctx: &Ctx) -> Option<f64> {
        let loc = if ctx.r.rtype == "google_bigquery_dataset" {
            bq_location(ctx)
        } else {
            table_dataset(ctx).map(bq_location_of).unwrap_or_else(|| "us".into())
        };
        let regions = bq_regions(&loc);
        let price = |pat: &'static str, group: &'static str| {
            ctx.price(bq_query(ctx, &regions, |r| Query::new(BQ, r, pat).group(group).ondemand())).map(|p| p.unit_price())
        };
        let (active, long_term) = (ctx.usage_num("storage_gb").unwrap_or(0.0), ctx.usage_num("long_term_storage_gb").unwrap_or(0.0));
        let logical = active * price("^Active Logical Storage", "ActiveStorage")?
            + long_term * price("^Long Term Logical Storage", "LongTermStorage")?;
        let physical = active * price("^Active Physical Storage", "PhysicalStorage")?
            + long_term * price("^Long-Term Physical Storage", "PhysicalStorage")?;
        (logical > 0.0).then(|| physical / logical)
    }

    /// Monthly storage cost of this dataset with logical billing.
    fn logical_storage_cost(ctx: &Ctx) -> Option<f64> {
        let comps = bq_storage(ctx, &bq_regions(&bq_location(ctx)), false, 168.0);
        if comps.iter().all(|c| c.monthly_cost.is_none()) {
            return None;
        }
        Some(comps.iter().filter_map(|c| c.monthly_cost).sum())
    }

    /// Monthly storage cost of this dataset/table with physical billing.
    fn physical_storage_cost(ctx: &Ctx) -> Option<f64> {
        let loc = if ctx.r.rtype == "google_bigquery_dataset" {
            bq_location(ctx)
        } else {
            table_dataset(ctx).map(bq_location_of).unwrap_or_else(|| "us".into())
        };
        let comps = bq_storage(ctx, &bq_regions(&loc), true, 168.0);
        if comps.iter().all(|c| c.monthly_cost.is_none()) {
            return None;
        }
        Some(comps.iter().filter_map(|c| c.monthly_cost).sum())
    }

    pub fn bq_reservation(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let r = reservation_of(ctx);
        let billed_baseline = r.baseline - r.covered;

        if ctx.env == Env::NonProd && billed_baseline > 0.0 {
            let new_max = r.max_slots.max(r.baseline);
            let new = ctx.reprice(|a| {
                a.insert("slot_capacity".into(), Value::Number(0.0));
                set_path(a, "autoscale.0.max_slots", Value::Number(new_max));
            });
            if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                out.push(ctx.rec(
                    "bq-reservation-baseline-idle",
                    Severity::Medium,
                    "Use autoscaling only (no baseline slots) for this non-production reservation",
                    format!(
                        "{} baseline slots are billed 24/7 even when no query runs. Non-production workloads are usually intermittent: set slot_capacity = 0 and rely on autoscale {{ max_slots = {} }}, which bills slots only while they are used (in 50-slot increments, 1-minute minimum).",
                        fmt(billed_baseline),
                        fmt(new_max)
                    ),
                    Some(s),
                ));
            }
        }

        if ctx.env == Env::NonProd && r.edition != "STANDARD" {
            let new = ctx.reprice(|a| {
                a.insert("edition".into(), Value::from("STANDARD"));
            });
            if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                out.push(ctx.rec(
                    "bq-edition-downgrade",
                    Severity::Low,
                    format!("Use the Standard edition instead of {} for non-production", edition_label(&r.edition)),
                    "Standard edition slots are ~33-60% cheaper. It lacks some features (CMEK, VPC Service Controls, BI Engine, BigQuery ML, fine-grained security, commitments) and is capped at 1,600 slots, which is usually acceptable for dev/test. Set edition = \"STANDARD\".",
                    Some(s),
                ));
            }
        }

        if ctx.env != Env::NonProd && r.edition != "STANDARD" && billed_baseline >= 50.0 {
            let hours = ctx.monthly_hours();
            let payg = slot_price(ctx, &r.loc, &r.edition, 0);
            let c1 = slot_price(ctx, &r.loc, &r.edition, 1);
            let c3 = slot_price(ctx, &r.loc, &r.edition, 3);
            if let (Some(p), Some(c1), Some(c3)) = (payg, c1, c3) {
                let s1 = (p - c1) * billed_baseline * hours;
                let s3 = (p - c3) * billed_baseline * hours;
                if s1 > 1.0 {
                    out.push(ctx.rec(
                        "bq-commitment",
                        Severity::Info,
                        "Cover the baseline slots with a capacity commitment",
                        format!(
                            "{} always-on baseline slots are billed at pay-as-you-go rates. A google_bigquery_capacity_commitment (same location and edition) would save ~{}/mo with a 1-year plan or ~{}/mo with a 3-year plan. Commit only to your steady-state baseline.",
                            fmt(billed_baseline),
                            ctx.money(s1),
                            ctx.money(s3)
                        ),
                        Some(s1),
                    ));
                }
            }
        }
        out
    }

    pub fn bq_commitment(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let edition = ctx.str("edition");
        let plan = ctx.str_or("plan", "ANNUAL").to_uppercase();
        if edition.is_none() && matches!(plan.as_str(), "FLEX" | "MONTHLY") {
            out.push(ctx.rec(
                "bq-legacy-flat-rate",
                Severity::Info,
                "Migrate this legacy flat-rate commitment to BigQuery editions",
                "Flat-rate FLEX/MONTHLY commitments are legacy and can no longer be purchased. Editions with autoscaling bill slots only while they are used and usually cost less for spiky workloads. Replace with a google_bigquery_reservation using an edition and autoscale.",
                None,
            ));
        }
        let loc = norm_location(&bq_location(ctx));
        let ed = edition_of(ctx.attrs);
        let has_reservation =
            ctx.siblings.iter().any(|r| r.rtype == "google_bigquery_reservation" && bq_location_of(r) == loc && edition_of(&r.attrs) == ed);
        if cr.monthly_cost > 0.0 && !has_reservation {
            out.push(ctx.rec(
                "bq-commitment-unassigned",
                Severity::Low,
                "No reservation in this project uses the committed slots",
                "Committed slots are billed for the whole term. Make sure a reservation of the same location and edition (possibly in another project of the administration project) consumes them, otherwise they only serve as idle capacity.",
                None,
            ));
        }
        out
    }

    pub fn dataflow(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let job = df_job(ctx);
        if !job.streaming && !job.flexrs && !job.prime && job.gpu.is_none() {
            let new = ctx.reprice(|a| set_path(a, "parameters.flexrs_goal", Value::from("COST_OPTIMIZED")));
            let s = ctx.savings_vs(cr.monthly_cost, new);
            out.push(ctx.rec(
                "dataflow-flexrs",
                Severity::Info,
                "Consider FlexRS for this batch job",
                "Flexible Resource Scheduling runs batch jobs on a mix of preemptible and regular VMs at ~40% lower vCPU/memory rates, in exchange for a delayed start (up to 6 hours). Suited to non-urgent batch pipelines: set the flexrs_goal=COST_OPTIMIZED pipeline option.",
                s,
            ));
        }
        if job.streaming && !job.streaming_engine && !job.prime {
            out.push(ctx.rec(
                "dataflow-streaming-engine",
                Severity::Low,
                "Enable Streaming Engine for this streaming job",
                "Streaming Engine moves shuffle and state out of the workers: jobs run on smaller workers (n1-standard-2 instead of n1-standard-4) with 30 GB instead of 400 GB disks and autoscale more responsively. It is billed per Streaming Engine Compute Unit, so validate the net effect. Set enable_streaming_engine = true.",
                None,
            ));
        }
        if job.max_workers.is_none() && ctx.usage_num("workers").is_none() {
            out.push(ctx.rec(
                "dataflow-max-workers",
                Severity::Low,
                "Cap the number of workers",
                "Without max_workers, autoscaling can scale a job up to 1,000 workers (batch) or 100 workers (streaming), which can cause unexpected spend on bad input or backlogs. Set max_workers to a sensible upper bound.",
                None,
            ));
        }
        out
    }

    pub fn dataproc(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        if ctx.block("cluster_config").is_none() {
            return out;
        }
        let roles = dp_roles(ctx);
        let (masters, workers, secondary) = (&roles[0], &roles[1], &roles[2]);
        let cc = "cluster_config.0";
        if ctx.env == Env::NonProd {
            if secondary.count <= 0.0 && workers.count > 2.0 && ctx.usage_num("workers").is_none() {
                let extra = workers.count - 2.0;
                let new = ctx.reprice(|a| {
                    set_path(a, &format!("{cc}.worker_config.0.num_instances"), Value::Number(2.0));
                    set_path(a, &format!("{cc}.preemptible_worker_config.0.num_instances"), Value::Number(extra));
                    set_path(a, &format!("{cc}.preemptible_worker_config.0.preemptibility"), Value::from("SPOT"));
                });
                if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                    out.push(ctx.rec(
                        "dataproc-preemptible-workers",
                        Severity::Medium,
                        "Use Spot secondary workers in this non-production cluster",
                        format!(
                            "Keep 2 primary workers and run the other {} as Spot secondary workers (preemptible_worker_config {{ num_instances = {}, preemptibility = \"SPOT\" }}): they are 60-91% cheaper. Secondary workers do not store HDFS data, so preemptions only slow jobs down.",
                            fmt(extra),
                            fmt(extra)
                        ),
                        Some(s),
                    ));
                }
            } else if secondary.count > 0.0 && secondary.pricing == Pricing::OnDemand {
                let new = ctx.reprice(|a| set_path(a, &format!("{cc}.preemptible_worker_config.0.preemptibility"), Value::from("SPOT")));
                if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                    out.push(ctx.rec(
                        "dataproc-preemptible-workers",
                        Severity::Medium,
                        "Use Spot VMs for the secondary workers",
                        "Secondary workers are non-preemptible. In non-production, set preemptible_worker_config { preemptibility = \"SPOT\" } to cut their cost by 60-91%.",
                        Some(s),
                    ));
                }
            }
            if masters.count > 1.0 {
                let new = ctx.reprice(|a| set_path(a, &format!("{cc}.master_config.0.num_instances"), Value::Number(1.0)));
                if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                    out.push(ctx.rec(
                        "dataproc-ha-nonprod",
                        Severity::Medium,
                        "Use a single master in this non-production cluster",
                        "High-availability clusters run 3 masters. Dev/test clusters rarely need master HA: set master_config { num_instances = 1 }.",
                        Some(s),
                    ));
                }
            }
            let lifecycle = ctx.get(&format!("{cc}.lifecycle_config.0"));
            let scheduled_delete =
                lifecycle.is_some_and(|l| ["idle_delete_ttl", "auto_delete_time", "auto_delete_ttl"].iter().any(|k| vstr(l, k).is_some()));
            if !scheduled_delete {
                out.push(ctx.rec(
                    "dataproc-idle-delete",
                    Severity::Medium,
                    "Delete this non-production cluster automatically when idle",
                    format!(
                        "The cluster costs ~{}/mo if left running. Set lifecycle_config {{ idle_delete_ttl = \"3600s\" }} (or auto_delete_time) so forgotten dev clusters are deleted, or use ephemeral clusters / Serverless for Apache Spark.",
                        ctx.money(cr.monthly_cost)
                    ),
                    None,
                ));
            }
        }
        if !dp_has_policy(ctx) && workers.count + secondary.count >= 4.0 {
            out.push(ctx.rec(
                "dataproc-autoscaling",
                Severity::Low,
                "Attach an autoscaling policy",
                "The cluster has a fixed size. A google_dataproc_autoscaling_policy scaling (secondary) workers with YARN demand avoids paying for idle workers between jobs.",
                None,
            ));
        }
        out
    }

    pub fn composer(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let version = composer_version(ctx);
        if version == 1 {
            out.push(ctx.rec(
                "composer1-to-2",
                Severity::Info,
                "Migrate from Cloud Composer 1",
                "Cloud Composer 1 has reached end of support. Composer 2/3 environments autoscale workers and bill for the resources actually used instead of a fixed GKE cluster, which usually lowers costs for intermittent workloads.",
                None,
            ));
            return out;
        }
        if ctx.env != Env::NonProd {
            return out;
        }
        let size = size_key(ctx.str_or("config.0.environment_size", "ENVIRONMENT_SIZE_SMALL"));
        if size != "small" {
            let new = ctx.reprice(|a| set_path(a, "config.0.environment_size", Value::from("ENVIRONMENT_SIZE_SMALL")));
            if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                out.push(ctx.rec(
                    "composer-env-size-nonprod",
                    Severity::Medium,
                    format!("Use a small environment size for this non-production environment (currently {size})"),
                    "The environment size drives the fixed infrastructure cost (Airflow database, queue, proxies) regardless of load. Small environments handle up to ~50 DAGs, which is usually enough for dev/test. Set environment_size = \"ENVIRONMENT_SIZE_SMALL\".",
                    Some(s),
                ));
            }
        }
        if ctx.str("config.0.resilience_mode") == Some("HIGH_RESILIENCE") {
            let new = ctx.reprice(|a| set_path(a, "config.0.resilience_mode", Value::from("STANDARD_RESILIENCE")));
            if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                out.push(ctx.rec(
                    "composer-high-resilience-nonprod",
                    Severity::Medium,
                    "Disable high resilience in this non-production environment",
                    "Highly resilient environments run zonal-redundant infrastructure at a higher fee. Dev/test environments rarely need it: remove resilience_mode = \"HIGH_RESILIENCE\".",
                    Some(s),
                ));
            }
        }
        out
    }

    pub fn data_fusion(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let t = ctx.str_or("type", "BASIC").to_uppercase();
        if ctx.env != Env::NonProd || t == "DEVELOPER" {
            return vec![];
        }
        let new = ctx.reprice(|a| {
            a.insert("type".into(), Value::from("DEVELOPER"));
        });
        match ctx.savings_vs(cr.monthly_cost, new) {
            Some(s) => vec![ctx.rec(
                "datafusion-developer-nonprod",
                Severity::Medium,
                format!("Use the Developer edition for this non-production instance (currently {t})"),
                "The Developer edition costs a fraction of Basic/Enterprise and is intended for development and testing (limited concurrent pipelines, no SLA). Set type = \"DEVELOPER\".",
                Some(s),
            )],
            None => vec![],
        }
    }
}
