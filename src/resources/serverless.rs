//! Serverless and messaging: Cloud Run (services, jobs, worker pools), Cloud
//! Run functions (1st and 2nd gen), Pub/Sub, Cloud Scheduler, Cloud Tasks,
//! Workflows, Eventarc Advanced, API Gateway and App Engine.

use super::machine::COMPUTE;
use super::{Ctx, Env, Spec, UsageParam, usage};
use crate::cost::{Component, CostedResource, HOURS_PER_MONTH, Recommendation, Severity};
use crate::hcl::value::{Value, fmt_number};
use crate::pricing::query::Query;

const RUN: &str = "Cloud Run";
const FUNCTIONS: &str = "Cloud Run Functions";
const PUBSUB: &str = "Cloud Pub/Sub";
const SCHEDULER: &str = "Cloud Scheduler";
const TASKS: &str = "Cloud Tasks";
const WORKFLOWS: &str = "Workflows";
const EVENTARC: &str = "Eventarc";
const APP_ENGINE: &str = "App Engine";

const SECONDS_PER_MONTH: f64 = HOURS_PER_MONTH * 3600.0;
const DAYS_PER_MONTH: f64 = HOURS_PER_MONTH / 24.0;
const DEFAULT_REQUEST_MS: f64 = 200.0;

pub fn specs() -> Vec<Spec> {
    const RUN_SERVICE: &[UsageParam] = usage!(
        "monthly_requests" => "Requests per month",
        "average_request_duration_ms" => "Average request latency in ms (default 200)",
        "concurrency" => "Average concurrent requests per active instance (defaults to the configured max concurrency)",
        "monthly_vcpu_seconds" => "Billable vCPU-seconds per month (overrides the request-based estimate)",
        "monthly_memory_gib_seconds" => "Billable GiB-seconds per month (overrides the request-based estimate)",
        "monthly_egress_data_transfer_gb" => "Internet egress in GiB per month (priced at the intra-continent rate)",
    );
    const FUNCTION_V2: &[UsageParam] = usage!(
        "monthly_function_invocations" => "Invocations per month",
        "request_duration_ms" => "Average execution time in ms (default 200)",
        "concurrency" => "Average concurrent requests per active instance (defaults to max_instance_request_concurrency)",
        "monthly_vcpu_seconds" => "Billable vCPU-seconds per month (overrides the invocation-based estimate)",
        "monthly_memory_gib_seconds" => "Billable GiB-seconds per month (overrides the invocation-based estimate)",
        "monthly_outbound_data_gb" => "Internet egress in GiB per month (priced at the intra-continent rate)",
    );
    const FUNCTION_V1: &[UsageParam] = usage!(
        "monthly_function_invocations" => "Invocations per month",
        "request_duration_ms" => "Average execution time in ms (default 200, billed in 100 ms increments)",
        "monthly_outbound_data_gb" => "Outbound data transfer in GiB per month",
    );
    const RUN_JOB: &[UsageParam] = usage!(
        "monthly_executions" => "Job executions per month",
        "average_task_duration_seconds" => "Average task duration in seconds",
    );
    const WORKER_POOL: &[UsageParam] = usage!("instances" => "Average number of instances (overrides the configured instance count)");
    const TOPIC: &[UsageParam] = usage!(
        "monthly_message_data_tb" => "Published data in TiB per month (1 KB minimum per publish request)",
        "monthly_message_data_gb" => "Published data in GiB per month (alternative to monthly_message_data_tb)",
        "storage_gb" => "Average retained message storage in GiB (defaults to throughput x retention)",
    );
    const SUBSCRIPTION: &[UsageParam] = usage!(
        "monthly_message_data_tb" => "Delivered data in TiB per month",
        "monthly_message_data_gb" => "Delivered data in GiB per month (alternative to monthly_message_data_tb)",
        "storage_gb" => "Average unacknowledged backlog older than 1 day, in GiB",
        "retained_acked_storage_gb" => "Average retained acknowledged messages in GiB (defaults to throughput x retention)",
        "snapshot_storage_gb" => "Average snapshot storage in GiB",
    );
    vec![
        Spec::new("google_cloud_run_v2_service", run_service).usage(RUN_SERVICE).rules(run_rules),
        Spec::new("google_cloud_run_service", run_service).usage(RUN_SERVICE).rules(run_rules),
        Spec::new("google_cloud_run_v2_job", |c: &Ctx| finish(run_job(c))).usage(RUN_JOB).rules(run_job_rules),
        Spec::new("google_cloud_run_v2_worker_pool", |c: &Ctx| finish(worker_pool(c))).usage(WORKER_POOL).rules(worker_pool_rules),
        Spec::new("google_cloudfunctions2_function", run_service).usage(FUNCTION_V2).rules(run_rules),
        Spec::new("google_cloudfunctions_function", |c: &Ctx| finish(function_v1(c))).usage(FUNCTION_V1).rules(function_v1_rules),
        Spec::new("google_pubsub_topic", |c: &Ctx| finish(pubsub_topic(c))).usage(TOPIC).rules(pubsub_topic_rules),
        Spec::new("google_pubsub_subscription", |c: &Ctx| finish(pubsub_subscription(c))).usage(SUBSCRIPTION).rules(pubsub_subscription_rules),
        Spec::new("google_pubsub_lite_topic", pubsub_lite).rules(pubsub_lite_rules),
        Spec::new("google_pubsub_lite_subscription", pubsub_lite).rules(pubsub_lite_rules),
        Spec::new("google_pubsub_lite_reservation", pubsub_lite).rules(pubsub_lite_rules),
        Spec::new("google_cloud_scheduler_job", |c: &Ctx| finish(scheduler_job(c))).rules(scheduler_rules),
        Spec::new("google_cloud_tasks_queue", |c: &Ctx| finish(tasks_queue(c))).usage(usage!("monthly_operations" => "API calls, task deliveries and retries per month")),
        Spec::new("google_workflows_workflow", |c: &Ctx| finish(workflow(c))).usage(usage!(
            "monthly_internal_steps" => "Internal steps executed per month (Google APIs, control flow)",
            "monthly_external_steps" => "External HTTP steps executed per month",
        )),
        Spec::new("google_eventarc_message_bus", |c: &Ctx| finish(eventarc_bus(c))).usage(usage!("monthly_events" => "Events published to the bus per month (64 KiB chunks)")),
        Spec::new("google_eventarc_pipeline", |c: &Ctx| finish(eventarc_pipeline(c))).usage(usage!(
            "monthly_events" => "Events delivered through the pipeline per month",
            "monthly_transformations" => "Transformation operations per month (defaults to monthly_events when a transformation is configured)",
        )),
        Spec::new("google_api_gateway_gateway", |c: &Ctx| finish(api_gateway(c))).usage(usage!("monthly_calls" => "API calls per month")),
        Spec::new("google_app_engine_standard_app_version", |c: &Ctx| finish(app_engine_standard(c)))
            .usage(usage!("monthly_instance_hours" => "Instance hours per month beyond the always-on instances"))
            .rules(app_engine_standard_rules),
        Spec::new("google_app_engine_flexible_app_version", |c: &Ctx| finish(app_engine_flexible(c)))
            .usage(usage!("instances" => "Average number of instances (overrides min_total_instances / manual instances)"))
            .rules(app_engine_flexible_rules),
    ]
}

// ---- parsing helpers --------------------------------------------------------

/// Kubernetes-style CPU quantity (`"1"`, `"2000m"`, `"0.5"`) in vCPUs.
fn parse_cpu(s: &str) -> Option<f64> {
    let s = s.trim();
    match s.strip_suffix('m') {
        Some(m) => m.parse::<f64>().ok().map(|v| v / 1000.0),
        None => s.parse().ok(),
    }
}

/// Kubernetes-style memory quantity (`"512Mi"`, `"2Gi"`, `"256M"`) in GiB.
fn parse_mem_gib(s: &str) -> Option<f64> {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let s = s.trim();
    let units: [(&str, f64); 10] = [
        ("Ki", 1024.0),
        ("Mi", 1024.0 * 1024.0),
        ("Gi", GIB),
        ("Ti", GIB * 1024.0),
        ("k", 1e3),
        ("K", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("B", 1.0),
    ];
    for (suffix, mult) in units {
        if let Some(n) = s.strip_suffix(suffix) {
            return n.trim().parse::<f64>().ok().map(|v| v * mult / GIB);
        }
    }
    s.parse::<f64>().ok().map(|v| v / GIB)
}

/// Duration string (`"604800s"`, `"10m"`, `"7d"`) in seconds.
fn parse_duration_secs(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1.0),
        'm' => (&s[..s.len() - 1], 60.0),
        'h' => (&s[..s.len() - 1], 3600.0),
        'd' => (&s[..s.len() - 1], 86400.0),
        _ => (s, 1.0),
    };
    num.parse::<f64>().ok().map(|v| v * mult)
}

/// CPU clock (GHz) allocated to a 1st gen function for its memory size.
fn gen1_ghz(memory_mb: f64) -> f64 {
    match memory_mb {
        m if m <= 128.0 => 0.2,
        m if m <= 256.0 => 0.4,
        m if m <= 512.0 => 0.8,
        m if m <= 1024.0 => 1.4,
        m if m <= 2048.0 => 2.4,
        _ => 4.8,
    }
}

/// Default vCPUs of a 2nd gen function for its memory size (GiB).
fn gen2_default_cpu(mem_gib: f64) -> f64 {
    match mem_gib {
        m if m <= 0.125 => 0.083,
        m if m <= 0.25 => 0.167,
        m if m <= 0.5 => 0.333,
        m if m <= 1.0 => 0.583,
        m if m <= 2.0 => 1.0,
        m if m <= 8.0 => 2.0,
        m if m <= 16.0 => 4.0,
        _ => 8.0,
    }
}

fn fmt(n: f64) -> String {
    fmt_number((n * 1000.0).round() / 1000.0)
}

fn days_label(d: f64) -> String {
    format!("{} day{}", fmt(d), if d == 1.0 { "" } else { "s" })
}

/// Post-process components: a usage-based component whose quantity is known
/// but whose price is missing is a pricing failure (not "depends on usage"),
/// and per-request prices are shown per million.
fn finish(mut comps: Vec<Component>) -> Vec<Component> {
    for c in comps.iter_mut() {
        if c.usage_based && c.quantity.is_some() && c.unit_price.is_none() {
            c.usage_based = false;
        }
        if matches!(c.unit.as_str(), "requests" | "invocations" | "operations" | "events" | "calls") {
            c.unit = format!("1M {}", c.unit);
            c.quantity = c.quantity.map(|q| q / 1e6);
            c.unit_price = c.unit_price.map(|p| p * 1e6);
        }
    }
    comps
}

fn total_of(comps: &[Component]) -> Option<f64> {
    if comps.iter().any(|c| c.monthly_cost.is_none() && !c.usage_based) {
        return None;
    }
    Some(CostedResource::total(comps))
}

fn str_map<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a str> {
    v.and_then(|m| m.get(key)).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
}

// ---- Cloud Run shapes -------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Billing {
    Request,
    Instance,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Product {
    Service,
    Function,
}

#[derive(Clone, Debug)]
struct Gpu {
    kind: String,
    count: f64,
    zonal_redundancy: bool,
}

#[derive(Clone, Debug)]
struct RunShape {
    vcpu: f64,
    mem_gib: f64,
    gpu: Option<Gpu>,
    /// Instances kept running all month (min instances or manual scaling).
    min_instances: f64,
    billing: Billing,
    /// True when `cpu_idle` is unset while `resources` is set (API default: always allocated).
    implicit_instance_billing: bool,
    concurrency: f64,
}

/// Sum CPU/memory limits across containers, with Cloud Run defaults.
fn container_totals(containers: &[&Value]) -> (f64, f64, f64) {
    let mut cpu = 0.0;
    let mut mem = 0.0;
    let mut gpus = 0.0;
    for c in containers {
        let limits = c.path("resources.0.limits");
        cpu += str_map(limits, "cpu").and_then(parse_cpu).unwrap_or(1.0);
        mem += str_map(limits, "memory").and_then(parse_mem_gib).unwrap_or(0.5);
        gpus += limits.and_then(|l| l.get("nvidia.com/gpu")).and_then(|v| v.as_f64()).unwrap_or(0.0);
    }
    if containers.is_empty() { (1.0, 0.5, 0.0) } else { (cpu, mem, gpus) }
}

fn default_concurrency(vcpu: f64) -> f64 {
    if vcpu < 1.0 { 1.0 } else { 80.0 }
}

fn gpu_of(accelerator: Option<&str>, count: f64, zonal_redundancy_disabled: bool) -> Option<Gpu> {
    let kind = accelerator?.to_string();
    Some(Gpu { kind, count: if count > 0.0 { count } else { 1.0 }, zonal_redundancy: !zonal_redundancy_disabled })
}

fn v2_service_shape(ctx: &Ctx) -> RunShape {
    let containers = ctx.blocks("template.0.containers");
    let (vcpu, mem_gib, gpu_count) = container_totals(&containers);
    let gpu = gpu_of(
        ctx.str("template.0.node_selector.0.accelerator"),
        gpu_count,
        ctx.bool_or("template.0.gpu_zonal_redundancy_disabled", false),
    );
    let has_resources = containers.iter().any(|c| c.path("resources.0").is_some_and(|r| !r.is_null()));
    let cpu_idle = containers.iter().find_map(|c| c.path("resources.0.cpu_idle").and_then(|v| v.as_bool()));
    let mut min = ctx.num_or("template.0.scaling.0.min_instance_count", 0.0).max(ctx.num_or("scaling.0.min_instance_count", 0.0));
    if ctx.str("scaling.0.scaling_mode") == Some("MANUAL") {
        min = ctx.num_or("scaling.0.manual_instance_count", min);
    }
    let implicit = cpu_idle.is_none() && has_resources && gpu.is_none();
    let billing = if gpu.is_some() || cpu_idle == Some(false) || implicit { Billing::Instance } else { Billing::Request };
    let concurrency = ctx.num("template.0.max_instance_request_concurrency").filter(|c| *c > 0.0).unwrap_or(default_concurrency(vcpu));
    RunShape { vcpu, mem_gib, gpu, min_instances: min, billing, implicit_instance_billing: implicit, concurrency }
}

fn v1_service_shape(ctx: &Ctx) -> RunShape {
    let containers = ctx.blocks("template.0.spec.0.containers");
    let (vcpu, mem_gib, gpu_count) = container_totals(&containers);
    let ann = ctx.get("template.0.metadata.0.annotations");
    let min = str_map(ann, "autoscaling.knative.dev/minScale").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let throttled = str_map(ann, "run.googleapis.com/cpu-throttling") != Some("false");
    let accel = str_map(ctx.get("template.0.spec.0.node_selector"), "run.googleapis.com/accelerator");
    let gpu = gpu_of(accel, gpu_count, str_map(ann, "run.googleapis.com/gpu-zonal-redundancy-disabled") == Some("true"));
    let billing = if gpu.is_some() || !throttled { Billing::Instance } else { Billing::Request };
    let concurrency = ctx.num("template.0.spec.0.container_concurrency").filter(|c| *c > 0.0).unwrap_or(default_concurrency(vcpu));
    RunShape { vcpu, mem_gib, gpu, min_instances: min, billing, implicit_instance_billing: false, concurrency }
}

fn function_v2_shape(ctx: &Ctx) -> RunShape {
    let mem_gib = ctx.str("service_config.0.available_memory").and_then(parse_mem_gib).unwrap_or(0.25);
    let vcpu = ctx.str("service_config.0.available_cpu").and_then(parse_cpu).unwrap_or_else(|| gen2_default_cpu(mem_gib));
    RunShape {
        vcpu,
        mem_gib,
        gpu: None,
        min_instances: ctx.num_or("service_config.0.min_instance_count", 0.0),
        billing: Billing::Request,
        implicit_instance_billing: false,
        concurrency: ctx.num("service_config.0.max_instance_request_concurrency").filter(|c| *c > 0.0).unwrap_or(1.0),
    }
}

fn shape_of(ctx: &Ctx) -> Option<(RunShape, Product)> {
    match ctx.r.rtype.as_str() {
        "google_cloud_run_v2_service" => Some((v2_service_shape(ctx), Product::Service)),
        "google_cloud_run_service" => Some((v1_service_shape(ctx), Product::Service)),
        "google_cloudfunctions2_function" => Some((function_v2_shape(ctx), Product::Function)),
        _ => None,
    }
}

struct Keys {
    requests: &'static str,
    duration: &'static str,
    egress: &'static str,
}

impl Product {
    fn keys(self) -> Keys {
        match self {
            Product::Service => {
                Keys { requests: "monthly_requests", duration: "average_request_duration_ms", egress: "monthly_egress_data_transfer_gb" }
            }
            Product::Function => {
                Keys { requests: "monthly_function_invocations", duration: "request_duration_ms", egress: "monthly_outbound_data_gb" }
            }
        }
    }
    fn rule_prefix(self) -> &'static str {
        match self {
            Product::Service => "run",
            Product::Function => "functions",
        }
    }
}

struct Traffic {
    requests: Option<f64>,
    /// Active (billable) instance-seconds per month.
    instance_secs: Option<f64>,
}

fn traffic(ctx: &Ctx, shape: &RunShape, keys: &Keys) -> Traffic {
    let requests = ctx.usage_num(keys.requests);
    if let Some(v) = ctx.usage_num("monthly_vcpu_seconds") {
        return Traffic { requests, instance_secs: Some(v / shape.vcpu.max(1e-9)) };
    }
    if let Some(m) = ctx.usage_num("monthly_memory_gib_seconds") {
        return Traffic { requests, instance_secs: Some(m / shape.mem_gib.max(1e-9)) };
    }
    let instance_secs = requests.map(|r| {
        let ms = ctx.usage_num(keys.duration).unwrap_or_else(|| {
            ctx.warn(format!("{} not set, assuming {DEFAULT_REQUEST_MS} ms per request", keys.duration));
            DEFAULT_REQUEST_MS
        });
        let conc = ctx.usage_num("concurrency").unwrap_or(shape.concurrency).max(1.0);
        r * ms / 1000.0 / conc
    });
    Traffic { requests, instance_secs }
}

fn tier_sku(stem: &str) -> String {
    format!("^{stem} (Tier 2 +)?\\(Request-based billing\\)$")
}

fn gpu_query_pattern(g: &Gpu) -> Option<String> {
    let name = match g.kind.as_str() {
        "nvidia-l4" => "NVIDIA L4",
        "nvidia-rtx-pro-6000" => "NVIDIA RTX Pro 6000",
        _ => return None,
    };
    let red = if g.zonal_redundancy { "zonal redundancy" } else { "no zonal redundancy" };
    Some(format!("^{name} GPU with {red} in "))
}

fn gpu_components(ctx: &Ctx, region: &str, g: &Gpu, fixed_secs: f64, usage_secs: Option<f64>) -> Vec<Component> {
    let Some(pat) = gpu_query_pattern(g) else {
        ctx.warn(format!("unknown Cloud Run accelerator '{}'", g.kind));
        return vec![];
    };
    let label = format!("GPU ({} x{}{})", g.kind, fmt(g.count), if g.zonal_redundancy { "" } else { ", no zonal redundancy" });
    let mut out = vec![];
    if fixed_secs > 0.0 {
        out.push(ctx.monthly(format!("{label}, always-on"), "GPU-seconds", fixed_secs * g.count, Query::new(RUN, region, &pat)));
    }
    out.push(ctx.usage_qty(label, "GPU-seconds", usage_secs.map(|s| s * g.count), Query::new(RUN, region, &pat)));
    out
}

/// Internet egress at the intra-continent rate (the only regional egress SKU).
fn egress_component(ctx: &Ctx, service: &'static str, region: &str, stem: &'static str, key: &str) -> Component {
    ctx.usage("Internet egress", "GiB", key, Query::new(service, region, stem).exclude("Intercontinental").group("PremiumInternetEgress"))
}

fn run_components(ctx: &Ctx, region: &str, s: &RunShape, product: Product, t: &Traffic) -> Vec<Component> {
    let mut out = vec![];
    let floor_secs = s.min_instances * SECONDS_PER_MONTH;
    let (cpu_l, mem_l) = (fmt(s.vcpu), fmt(s.mem_gib));
    let (svc, req_cpu, req_mem, idle_cpu, idle_mem, requests) = match product {
        Product::Service => (
            RUN,
            tier_sku("Services CPU"),
            tier_sku("Services Memory"),
            tier_sku("Services Min Instance CPU"),
            tier_sku("Services Min Instance Memory"),
            "^Requests$",
        ),
        Product::Function => (
            FUNCTIONS,
            "^Cloud Run functions CPU \\(Request-based billing\\) in ".to_string(),
            "^Cloud Run functions Memory \\(Request-based billing\\) in ".to_string(),
            "^Cloud Run functions Min-Instance CPU \\(Request-based billing\\) in ".to_string(),
            "^Cloud Run functions Min-Instance Memory \\(Request-based billing\\) in ".to_string(),
            "^Cloud Run Functions Invocations$",
        ),
    };
    let min_l = fmt(s.min_instances);
    let usage_secs = match s.billing {
        Billing::Request => {
            let active = t.instance_secs;
            out.push(ctx.usage_qty(
                format!("CPU (request-based, {cpu_l} vCPU)"),
                "vCPU-seconds",
                active.map(|a| a * s.vcpu),
                Query::new(svc, region, &req_cpu),
            ));
            out.push(ctx.usage_qty(
                format!("Memory (request-based, {mem_l} GiB)"),
                "GiB-seconds",
                active.map(|a| a * s.mem_gib),
                Query::new(svc, region, &req_mem),
            ));
            let (name, unit) = if product == Product::Function { ("Invocations", "invocations") } else { ("Requests", "requests") };
            out.push(ctx.usage_qty(name, unit, t.requests, Query::new(svc, "global", requests)));
            if floor_secs > 0.0 {
                let idle = (floor_secs - active.unwrap_or(0.0).min(floor_secs)).max(0.0);
                let mut c = ctx.monthly(
                    format!("Min instances idle CPU ({min_l} x {cpu_l} vCPU)"),
                    "vCPU-seconds",
                    idle * s.vcpu,
                    Query::new(svc, region, &idle_cpu),
                );
                c.note = Some("idle min instances are billed at the reduced idle rate".into());
                out.push(c);
                out.push(ctx.monthly(
                    format!("Min instances idle memory ({min_l} x {mem_l} GiB)"),
                    "GiB-seconds",
                    idle * s.mem_gib,
                    Query::new(svc, region, &idle_mem),
                ));
            }
            active
        }
        Billing::Instance => {
            let cpu_q = "^Services CPU \\(Instance-based billing\\) in ";
            let mem_q = "^Services Memory \\(Instance-based billing\\) in ";
            if floor_secs > 0.0 {
                let mut c = ctx.monthly(
                    format!("Always-on CPU ({min_l} x {cpu_l} vCPU, instance-based)"),
                    "vCPU-seconds",
                    floor_secs * s.vcpu,
                    Query::new(RUN, region, cpu_q),
                );
                c.note = Some("min instances with CPU always allocated are billed continuously".into());
                out.push(c);
                out.push(ctx.monthly(
                    format!("Always-on memory ({min_l} x {mem_l} GiB, instance-based)"),
                    "GiB-seconds",
                    floor_secs * s.mem_gib,
                    Query::new(RUN, region, mem_q),
                ));
            }
            let extra = t.instance_secs.map(|a| (a - floor_secs).max(0.0));
            let suffix = if floor_secs > 0.0 { ", above min instances" } else { "" };
            let mut c = ctx.usage_qty(
                format!("CPU (instance-based{suffix}, {cpu_l} vCPU)"),
                "vCPU-seconds",
                extra.map(|a| a * s.vcpu),
                Query::new(RUN, region, cpu_q),
            );
            c.note = Some("instance-based billing covers the whole instance lifetime, including idle time before shutdown".into());
            out.push(c);
            out.push(ctx.usage_qty(
                format!("Memory (instance-based{suffix}, {mem_l} GiB)"),
                "GiB-seconds",
                extra.map(|a| a * s.mem_gib),
                Query::new(RUN, region, mem_q),
            ));
            extra
        }
    };
    if let Some(g) = &s.gpu {
        out.extend(gpu_components(ctx, region, g, floor_secs, usage_secs));
    }
    let keys = product.keys();
    let egress_stem = match product {
        Product::Service => "^Cloud Run Network Internet Data Transfer Out",
        Product::Function => "^Cloud Run Functions Network Internet Data Transfer Out",
    };
    out.push(egress_component(ctx, svc, region, egress_stem, keys.egress));
    finish(out)
}

fn run_service(ctx: &Ctx) -> Vec<Component> {
    let Some((shape, product)) = shape_of(ctx) else { return vec![] };
    if shape.implicit_instance_billing {
        ctx.warn("resources is set without cpu_idle: the Cloud Run API then allocates CPU always (instance-based billing); set cpu_idle = true for request-based billing");
    }
    let t = traffic(ctx, &shape, &product.keys());
    run_components(ctx, &ctx.region(), &shape, product, &t)
}

fn run_rules(ctx: &Ctx, _cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let Some((shape, product)) = shape_of(ctx) else { return out };
    let region = ctx.region();
    let keys = product.keys();
    let cost = |s: &RunShape| total_of(&run_components(ctx, &region, s, product, &traffic(ctx, s, &keys)));
    let Some(current) = cost(&shape) else { return out };
    let prefix = product.rule_prefix();
    let nonprod = ctx.env == Env::NonProd;

    if shape.min_instances > 0.0 && nonprod {
        let alt = RunShape { min_instances: 0.0, ..shape.clone() };
        if let Some(s) = ctx.savings_vs(current, cost(&alt)) {
            let attr = match ctx.r.rtype.as_str() {
                "google_cloud_run_v2_service" => "template.scaling.min_instance_count (and scaling.min_instance_count)",
                "google_cloud_run_service" => "the autoscaling.knative.dev/minScale annotation",
                _ => "service_config.min_instance_count",
            };
            out.push(ctx.rec(
                &format!("{prefix}-min-instances-nonprod"),
                Severity::Medium,
                format!("Scale this non-production {} to zero", if product == Product::Service { "service" } else { "function" }),
                format!(
                    "{} min instance(s) are kept warm all month (~{}/mo). Non-production traffic rarely justifies avoiding cold starts: set {attr} to 0. Caveat: the first request after idle pays a cold start.",
                    fmt(shape.min_instances),
                    ctx.money(s)
                ),
                Some(s),
            ));
        }
    }

    if product == Product::Service && shape.billing == Billing::Instance && shape.gpu.is_none() {
        let alt = RunShape { billing: Billing::Request, ..shape.clone() };
        let t = traffic(ctx, &shape, &keys);
        let floor = shape.min_instances * SECONDS_PER_MONTH;
        let low_traffic = t.instance_secs.is_some_and(|a| floor > 0.0 && a < floor * 0.3);
        let attr = if ctx.r.rtype == "google_cloud_run_service" {
            "the run.googleapis.com/cpu-throttling annotation to \"true\""
        } else {
            "cpu_idle = true on the containers' resources"
        };
        let why = if shape.implicit_instance_billing {
            "cpu_idle is not set while a resources block is, so the Cloud Run API allocates CPU all the time (instance-based billing). "
        } else {
            "CPU is always allocated (instance-based billing). "
        };
        let savings = ctx.savings_vs(current, cost(&alt));
        if (nonprod || low_traffic || shape.implicit_instance_billing) && (savings.is_some() || shape.implicit_instance_billing) {
            let sev = if savings.is_some() && (nonprod || low_traffic) { Severity::Medium } else { Severity::Low };
            out.push(ctx.rec(
                "run-cpu-always-allocated",
                sev,
                "Use request-based billing (CPU only allocated during requests)",
                format!(
                    "{why}Idle instances are then billed at full vCPU/memory rates, whereas request-based billing charges idle min instances at a much lower rate and only bills CPU while requests are served. Set {attr}. Keep instance-based billing for background work after responses, high sustained utilization or GPUs."
                ),
                savings,
            ));
        }
    }

    if let Some(g) = &shape.gpu
        && g.zonal_redundancy
        && nonprod
    {
        let alt = RunShape { gpu: Some(Gpu { zonal_redundancy: false, ..g.clone() }), ..shape.clone() };
        if let Some(s) = ctx.savings_vs(current, cost(&alt)) {
            out.push(ctx.rec(
                "run-gpu-zonal-redundancy-nonprod",
                Severity::Low,
                "Disable GPU zonal redundancy for this non-production service",
                "GPU instances with zonal redundancy reserve failover capacity and cost ~55% more per GPU-second. Set template.gpu_zonal_redundancy_disabled = true (v1: the run.googleapis.com/gpu-zonal-redundancy-disabled annotation). Caveat: capacity is not guaranteed during a zonal outage.",
                Some(s),
            ));
        }
    }

    if product == Product::Service && shape.vcpu >= 4.0 && shape.mem_gib >= 8.0 {
        out.push(ctx.rec(
            "run-oversized",
            Severity::Info,
            format!("Large instance size ({} vCPU / {} GiB)", fmt(shape.vcpu), fmt(shape.mem_gib)),
            "Cloud Run bills CPU and memory per instance-second, so oversized instances multiply the cost of every request and of min instances. Check CPU/memory utilization metrics and right-size resources.limits; raise concurrency instead of instance size for I/O-bound services.",
            None,
        ));
    }
    out
}

// ---- Cloud Run jobs & worker pools -----------------------------------------

fn run_job(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let containers = ctx.blocks("template.0.template.0.containers");
    let (vcpu, mem, gpu_count) = container_totals(&containers);
    let tasks = ctx.num_or("template.0.task_count", 1.0);
    let task_secs = match (ctx.usage_num("monthly_executions"), ctx.usage_num("average_task_duration_seconds")) {
        (Some(e), Some(d)) => Some(e * tasks * d),
        (Some(e), None) => {
            let timeout = ctx.str("template.0.template.0.timeout").and_then(parse_duration_secs).unwrap_or(600.0);
            ctx.warn(format!("average_task_duration_seconds not set, assuming the task timeout ({}s)", fmt(timeout)));
            Some(e * tasks * timeout)
        }
        _ => None,
    };
    let label = format!("{} task{}", fmt(tasks), if tasks == 1.0 { "" } else { "s" });
    let mut out = vec![
        ctx.usage_qty(
            format!("CPU ({} vCPU, {label}/execution)", fmt(vcpu)),
            "vCPU-seconds",
            task_secs.map(|s| s * vcpu),
            Query::new(RUN, &region, "^Jobs CPU in "),
        ),
        ctx.usage_qty(
            format!("Memory ({} GiB, {label}/execution)", fmt(mem)),
            "GiB-seconds",
            task_secs.map(|s| s * mem),
            Query::new(RUN, &region, "^Jobs Memory in "),
        ),
    ];
    let gpu = gpu_of(
        ctx.str("template.0.template.0.node_selector.0.accelerator"),
        gpu_count,
        ctx.bool_or("template.0.template.0.gpu_zonal_redundancy_disabled", false),
    );
    if let Some(g) = gpu {
        out.extend(gpu_components(ctx, &region, &g, 0.0, task_secs));
    }
    out
}

fn run_job_rules(ctx: &Ctx, _cr: &CostedResource) -> Vec<Recommendation> {
    let containers = ctx.blocks("template.0.template.0.containers");
    let (vcpu, mem, _) = container_totals(&containers);
    if vcpu >= 4.0 && mem >= 8.0 {
        return vec![ctx.rec(
            "run-oversized",
            Severity::Info,
            format!("Large task size ({} vCPU / {} GiB)", fmt(vcpu), fmt(mem)),
            "Job tasks are billed per vCPU-second and GiB-second for their whole duration. Check task utilization and right-size resources.limits, or split work across more, smaller tasks.",
            None,
        )];
    }
    vec![]
}

fn worker_pool_instances(ctx: &Ctx) -> f64 {
    if let Some(n) = ctx.usage_num("instances") {
        return n;
    }
    match ctx.str_or("scaling.0.scaling_mode", "MANUAL") {
        "AUTOMATIC" => ctx.num_or("scaling.0.min_instance_count", 0.0),
        _ => ctx.num("scaling.0.manual_instance_count").unwrap_or_else(|| {
            ctx.warn("scaling.manual_instance_count not set, assuming 1 instance");
            1.0
        }),
    }
}

fn worker_pool(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let containers = ctx.blocks("template.0.containers");
    let (vcpu, mem, gpu_count) = container_totals(&containers);
    let n = worker_pool_instances(ctx);
    if n <= 0.0 {
        return vec![];
    }
    let secs = n * SECONDS_PER_MONTH;
    let mut out = vec![
        ctx.monthly(
            format!("CPU ({} x {} vCPU)", fmt(n), fmt(vcpu)),
            "vCPU-seconds",
            secs * vcpu,
            Query::new(RUN, &region, "^Worker Pools CPU in "),
        ),
        ctx.monthly(
            format!("Memory ({} x {} GiB)", fmt(n), fmt(mem)),
            "GiB-seconds",
            secs * mem,
            Query::new(RUN, &region, "^Worker Pools Memory in "),
        ),
    ];
    let gpu = gpu_of(
        ctx.str("template.0.node_selector.0.accelerator"),
        gpu_count,
        ctx.bool_or("template.0.gpu_zonal_redundancy_disabled", false),
    );
    if let Some(g) = gpu {
        out.extend(gpu_components(ctx, &region, &g, secs, Some(0.0)).into_iter().filter(|c| !c.usage_based));
    }
    out
}

fn worker_pool_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    if ctx.env != Env::NonProd || cr.monthly_cost < 1.0 {
        return vec![];
    }
    vec![ctx.rec(
        "run-worker-pool-nonprod",
        Severity::Low,
        "Scale this non-production worker pool down when idle",
        format!(
            "Worker pool instances are billed continuously (~{}/mo). Reduce scaling.manual_instance_count outside working hours (e.g. from a scheduled job) or to 0 when the environment is unused.",
            ctx.money(cr.monthly_cost)
        ),
        None,
    )]
}

// ---- Cloud Run functions (1st gen) -----------------------------------------

fn function_v1(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let mem_mb = ctx.num_or("available_memory_mb", 256.0);
    let ghz = gen1_ghz(mem_mb);
    let gib = mem_mb / 1024.0;
    let invocations = ctx.usage_num("monthly_function_invocations");
    let active_secs = invocations.map(|n| {
        let ms = ctx.usage_num("request_duration_ms").unwrap_or_else(|| {
            ctx.warn(format!("request_duration_ms not set, assuming {DEFAULT_REQUEST_MS} ms per invocation"));
            DEFAULT_REQUEST_MS
        });
        n * (ms / 100.0).ceil() * 0.1
    });
    let q = |stem: &str| format!("^Cloud Run functions \\(1st Gen\\) {stem} (Tier 2 )?\\(Request-based billing\\)$");
    let mut out = vec![
        ctx.usage_qty(
            "Invocations",
            "invocations",
            invocations,
            Query::new(FUNCTIONS, "global", "^Cloud Run Functions \\(1st Gen\\) Invocations$"),
        ),
        ctx.usage_qty(
            format!("CPU ({} GHz)", fmt(ghz)),
            "GHz-seconds",
            active_secs.map(|s| s * ghz),
            Query::new(FUNCTIONS, &region, &q("CPU")),
        ),
        ctx.usage_qty(
            format!("Memory ({} MB)", fmt(mem_mb)),
            "GiB-seconds",
            active_secs.map(|s| s * gib),
            Query::new(FUNCTIONS, &region, &q("Memory")),
        ),
    ];
    let min = ctx.num_or("min_instances", 0.0);
    if min > 0.0 {
        let floor = min * SECONDS_PER_MONTH;
        let idle = (floor - active_secs.unwrap_or(0.0).min(floor)).max(0.0);
        out.push(ctx.monthly(
            format!("Min instances idle CPU ({} x {} GHz)", fmt(min), fmt(ghz)),
            "GHz-seconds",
            idle * ghz,
            Query::new(FUNCTIONS, &region, &q("Min Instance CPU")),
        ));
        out.push(ctx.monthly(
            format!("Min instances idle memory ({} x {} MB)", fmt(min), fmt(mem_mb)),
            "GiB-seconds",
            idle * gib,
            Query::new(FUNCTIONS, &region, &q("Min Instance Memory")),
        ));
    }
    out.push(ctx.usage(
        "Outbound data transfer",
        "GiB",
        "monthly_outbound_data_gb",
        Query::new(FUNCTIONS, &region, "^Cloud Run Functions \\(1st Gen\\) Network Data Transfer Out from "),
    ));
    out
}

/// The 2nd gen equivalent of a 1st gen function (same memory, default CPU).
fn gen2_equivalent_cost(ctx: &Ctx) -> Option<f64> {
    let mem_gib = ctx.num_or("available_memory_mb", 256.0) / 1024.0;
    let shape = RunShape {
        vcpu: gen2_default_cpu(mem_gib),
        mem_gib,
        gpu: None,
        min_instances: ctx.num_or("min_instances", 0.0),
        billing: Billing::Request,
        implicit_instance_billing: false,
        concurrency: 1.0,
    };
    let keys = Product::Function.keys();
    let comps = run_components(ctx, &ctx.region(), &shape, Product::Function, &traffic(ctx, &shape, &keys));
    total_of(&comps.into_iter().filter(|c| c.name != "Internet egress").collect::<Vec<_>>())
}

fn function_v1_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let min = ctx.num_or("min_instances", 0.0);
    if min > 0.0 && ctx.env == Env::NonProd {
        let new = ctx.reprice(|a| {
            a.insert("min_instances".into(), Value::Number(0.0));
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "functions-min-instances-nonprod",
                Severity::Medium,
                "Scale this non-production function to zero",
                format!(
                    "{} min instance(s) are kept warm all month. Set min_instances = 0 for non-production; the trade-off is cold starts.",
                    fmt(min)
                ),
                Some(s),
            ));
        }
    }
    let egress = cr.components.iter().find(|c| c.name == "Outbound data transfer").and_then(|c| c.monthly_cost).unwrap_or(0.0);
    let gen2 = gen2_equivalent_cost(ctx);
    let savings = gen2.and_then(|g| ctx.savings_vs(cr.monthly_cost - egress, Some(g)));
    out.push(ctx.rec(
        "functions-gen1-to-gen2",
        Severity::Info,
        "Migrate this 1st gen function to Cloud Run functions (2nd gen)",
        format!(
            "1st gen functions handle one request per instance and bill in 100 ms increments. 2nd gen (google_cloudfunctions2_function) runs on Cloud Run: up to 1000 concurrent requests per instance, longer timeouts, larger instances and traffic splitting. With concurrency > 1, billable instance time drops roughly in proportion.{}",
            gen2.map(|g| format!(" At the same memory and concurrency 1, the 2nd gen compute cost would be ~{}/mo.", ctx.money(g))).unwrap_or_default()
        ),
        savings,
    ));
    out
}

// ---- Pub/Sub ------------------------------------------------------------------

fn throughput_tib(ctx: &Ctx) -> Option<f64> {
    ctx.usage_num("monthly_message_data_tb").or_else(|| ctx.usage_num("monthly_message_data_gb").map(|g| g / 1024.0))
}

fn retention_days(ctx: &Ctx, default: Option<f64>) -> Option<f64> {
    ctx.str("message_retention_duration").and_then(parse_duration_secs).map(|s| s / 86400.0).or(default)
}

fn has_block(ctx: &Ctx, path: &str) -> bool {
    !ctx.blocks(path).is_empty()
}

fn pubsub_topic(ctx: &Ctx) -> Vec<Component> {
    let tib = throughput_tib(ctx);
    let ingestion = ctx.block("ingestion_data_source_settings");
    let source = ingestion.and_then(|i| {
        [
            ("aws_kinesis", "Kinesis Data Streams"),
            ("cloud_storage", "Google Cloud Storage"),
            ("azure_event_hubs", "Azure Event Hubs"),
            ("aws_msk", "AWS MSK"),
            ("confluent_cloud", "Confluent Cloud"),
        ]
        .into_iter()
        .find(|(k, _)| i.get(k).and_then(|b| b.as_list()).is_some_and(|l| !l.is_empty()))
    });
    let mut out = vec![];
    match source {
        Some((_, name)) => {
            let pat = format!("^Message Delivery From {name}$");
            out.push(ctx.usage_qty(format!("Import throughput (from {name})"), "TiB", tib, Query::new(PUBSUB, "global", &pat)));
        }
        None => {
            out.push(ctx.usage_qty("Message publishing (throughput)", "TiB", tib, Query::new(PUBSUB, "global", "^Message Delivery Basic$")))
        }
    }
    if let Some(days) = retention_days(ctx, None) {
        let gb = ctx.usage_num("storage_gb").or_else(|| tib.map(|t| t * 1024.0 * days / DAYS_PER_MONTH));
        out.push(ctx.usage_qty(
            format!("Message retention storage ({})", days_label(days)),
            "GiB-months",
            gb,
            Query::new(PUBSUB, "global", "^Topics message backlog$"),
        ));
    }
    if has_block(ctx, "message_transforms") {
        out.push(ctx.usage_qty(
            "Single message transforms",
            "TiB",
            tib,
            Query::new(PUBSUB, "global", "^Message Transform Data Processing$"),
        ));
    }
    out
}

fn pubsub_topic_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let Some(days) = retention_days(ctx, None) else { return vec![] };
    if days < 7.0 || ctx.env != Env::NonProd {
        return vec![];
    }
    let storage = cr.components.iter().find(|c| c.name.starts_with("Message retention")).and_then(|c| c.monthly_cost);
    let savings = storage.map(|c| c * (1.0 - 1.0 / days)).filter(|s| *s > 1.0);
    vec![ctx.rec(
        "pubsub-long-retention",
        Severity::Low,
        format!("Topic retains messages for {} days in a non-production environment", fmt(days)),
        "Topic retention stores every published message for the whole retention window at $0.27/GiB-month. For dev/test, a short message_retention_duration (e.g. \"86400s\") or no topic retention is usually enough.",
        savings,
    )]
}

fn pubsub_subscription(ctx: &Ctx) -> Vec<Component> {
    let tib = throughput_tib(ctx);
    let (label, pat) = if has_block(ctx, "bigquery_config") {
        ("Message delivery to BigQuery", "^Message Delivery to BigQuery$")
    } else if has_block(ctx, "cloud_storage_config") {
        ("Message delivery to Cloud Storage", "^Message Delivery to Google Cloud Storage$")
    } else if has_block(ctx, "bigtable_config") {
        ("Message delivery to Bigtable", "^Message Delivery to Bigtable$")
    } else {
        ("Message delivery (throughput)", "^Message Delivery Basic$")
    };
    let mut out = vec![ctx.usage_qty(label, "TiB", tib, Query::new(PUBSUB, "global", pat))];
    if ctx.bool_or("retain_acked_messages", false) {
        let days = retention_days(ctx, Some(7.0)).unwrap_or(7.0);
        let gb = ctx.usage_num("retained_acked_storage_gb").or_else(|| tib.map(|t| t * 1024.0 * days / DAYS_PER_MONTH));
        out.push(ctx.usage_qty(
            format!("Retained acknowledged messages ({})", days_label(days)),
            "GiB-months",
            gb,
            Query::new(PUBSUB, "global", "^Subscriptions retained acknowledged messages$"),
        ));
    }
    let mut backlog = ctx.usage(
        "Unacknowledged backlog (older than 1 day)",
        "GiB-months",
        "storage_gb",
        Query::new(PUBSUB, "global", "^Subscriptions message backlog$"),
    );
    backlog.note = Some("only messages not already retained by the topic are billed".into());
    out.push(backlog);
    if ctx.usage_num("snapshot_storage_gb").is_some() {
        out.push(ctx.usage(
            "Snapshot storage",
            "GiB-months",
            "snapshot_storage_gb",
            Query::new(PUBSUB, "global", "^Snapshots message backlog$"),
        ));
    }
    if has_block(ctx, "message_transforms") {
        out.push(ctx.usage_qty(
            "Single message transforms",
            "TiB",
            tib,
            Query::new(PUBSUB, "global", "^Message Transform Data Processing$"),
        ));
    }
    out
}

fn pubsub_subscription_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    if !ctx.bool_or("retain_acked_messages", false) {
        return out;
    }
    let days = retention_days(ctx, Some(7.0)).unwrap_or(7.0);
    let retained = cr.components.iter().find(|c| c.name.starts_with("Retained acknowledged")).and_then(|c| c.monthly_cost);
    let topic_days = ctx
        .str("topic")
        .and_then(|t| ctx.resolve_ref(t, "google_pubsub_topic"))
        .and_then(|t| t.attrs.get("message_retention_duration").and_then(|v| v.as_str()).and_then(parse_duration_secs))
        .map(|s| s / 86400.0);
    if topic_days.is_some_and(|t| t >= days) {
        out.push(ctx.rec(
            "pubsub-duplicate-retention",
            Severity::Low,
            "Subscription retains acknowledged messages already retained by its topic",
            format!(
                "The topic retains messages for {}, which already lets every subscription seek back {} at no extra per-subscription cost. Set retain_acked_messages = false and seek to a timestamp instead.",
                days_label(topic_days.unwrap_or(days)),
                days_label(days)
            ),
            retained.filter(|s| *s > 1.0),
        ));
    } else if days >= 7.0 {
        out.push(ctx.rec(
            "pubsub-long-retention",
            Severity::Low,
            format!("Subscription retains acknowledged messages for {} days", fmt(days)),
            "Retaining acknowledged messages is the most expensive replay mechanism ($0.27/GiB-month per subscription). Prefer topic message retention (shared by all subscriptions) or snapshots, or shorten message_retention_duration / set retain_acked_messages = false if replay is not needed.",
            retained.filter(|s| *s > 1.0),
        ));
    }
    out
}

fn pubsub_lite(ctx: &Ctx) -> Vec<Component> {
    ctx.warn("Pub/Sub Lite was turned down on 2026-03-18; this resource can no longer be provisioned");
    vec![]
}

fn pubsub_lite_rules(ctx: &Ctx, _cr: &CostedResource) -> Vec<Recommendation> {
    vec![ctx.rec(
        "pubsub-lite-turned-down",
        Severity::Info,
        "Pub/Sub Lite is no longer available",
        "Google turned Pub/Sub Lite down on March 18, 2026. Migrate to Pub/Sub (google_pubsub_topic / google_pubsub_subscription) or Managed Service for Apache Kafka.",
        None,
    )]
}

// ---- Cloud Scheduler / Cloud Tasks / Workflows / Eventarc / API Gateway ------

fn scheduler_job(ctx: &Ctx) -> Vec<Component> {
    let mut c = ctx.monthly("Scheduler job", "job-days", DAYS_PER_MONTH, Query::new(SCHEDULER, "global", "^Jobs$"));
    c.note = Some(if ctx.bool_or("paused", false) {
        "paused jobs are billed too; the first 3 jobs per billing account are free".into()
    } else {
        "the first 3 jobs per billing account are free".into()
    });
    vec![c]
}

fn scheduler_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut jobs: Vec<&str> = ctx.siblings.iter().filter(|s| s.rtype == ctx.r.rtype).map(|s| s.address.as_str()).collect();
    jobs.sort_unstable();
    if jobs.len() <= 3 || jobs.first() != Some(&ctx.r.address.as_str()) {
        return vec![];
    }
    let paused =
        ctx.siblings.iter().filter(|s| s.rtype == ctx.r.rtype && s.attrs.get("paused").and_then(|p| p.as_bool()) == Some(true)).count();
    vec![ctx.rec(
        "scheduler-job-count",
        Severity::Info,
        format!("{} Cloud Scheduler jobs in this project (~{}/mo)", jobs.len(), ctx.money(cr.monthly_cost * jobs.len() as f64)),
        format!(
            "Each job costs $0.10 per 31 days, paused or not ({paused} paused here); only 3 jobs per billing account are free. Delete unused or paused jobs and consolidate jobs that call the same target on the same schedule."
        ),
        None,
    )]
}

fn tasks_queue(ctx: &Ctx) -> Vec<Component> {
    vec![ctx.usage("Operations", "operations", "monthly_operations", Query::new(TASKS, "global", "^Operations$"))]
}

fn workflow(ctx: &Ctx) -> Vec<Component> {
    vec![
        ctx.usage("Internal steps", "steps", "monthly_internal_steps", Query::new(WORKFLOWS, "global", "^Internal Steps$")),
        ctx.usage("External HTTP steps", "steps", "monthly_external_steps", Query::new(WORKFLOWS, "global", "^External Steps$")),
    ]
}

fn eventarc_bus(ctx: &Ctx) -> Vec<Component> {
    vec![ctx.usage("Events published", "events", "monthly_events", Query::new(EVENTARC, "global", "^Events Published to an Eventarc Bus"))]
}

fn eventarc_pipeline(ctx: &Ctx) -> Vec<Component> {
    let mut out = vec![ctx.usage(
        "Events processed",
        "events",
        "monthly_events",
        Query::new(EVENTARC, "global", "^Events sent to an Eventarc Pipeline"),
    )];
    let transforms =
        ctx.blocks("mediations").iter().any(|m| m.get("transformation").and_then(|t| t.as_list()).is_some_and(|l| !l.is_empty()));
    if transforms {
        let qty = ctx.usage_num("monthly_transformations").or_else(|| ctx.usage_num("monthly_events"));
        out.push(ctx.usage_qty(
            "Transformations",
            "operations",
            qty,
            Query::new(EVENTARC, "global", "^Number of transformation operations executed in an Eventarc Pipeline"),
        ));
    }
    out
}

/// API Gateway calls are billed under "Google Service Control", which is not in
/// the synced catalog, so the published USD list price is used.
fn api_gateway(ctx: &Ctx) -> Vec<Component> {
    let calls = ctx.usage_num("monthly_calls");
    let usd = ctx.pricer.currency() == "USD";
    let free = if ctx.pricer.include_free_tier { 2e6 } else { 0.0 };
    let tiered = |n: f64| {
        let billable = (n - free).max(0.0);
        let first = billable.min(1e9 - free);
        (first * 3.0 + (billable - first).max(0.0) * 1.5) / 1e6
    };
    if !usd {
        ctx.warn("API Gateway call pricing is only known in USD (no Cloud Billing Catalog SKU)");
    }
    vec![Component {
        name: "API calls".into(),
        unit: "calls".into(),
        quantity: calls,
        unit_price: usd.then_some(3e-6),
        monthly_cost: calls.filter(|_| usd).map(tiered),
        list_unit_price: None,
        list_monthly_cost: None,
        sku: None,
        sku_description: None,
        usage_based: true,
        note: Some(
            "published list price: $3.00/million up to 1B calls, $1.50/million beyond; first 2M calls per billing account free".into(),
        ),
    }]
}

// ---- App Engine ---------------------------------------------------------------

/// Region of the App Engine application (from google_app_engine_application).
fn app_engine_region(ctx: &Ctx) -> String {
    let loc = ctx
        .siblings
        .iter()
        .find(|s| s.rtype == "google_app_engine_application")
        .and_then(|a| a.attrs.get("location_id").and_then(|l| l.as_str()).map(str::to_string));
    match loc.as_deref() {
        Some("us-central") => "us-central1".into(),
        Some("europe-west") => "europe-west1".into(),
        Some(l) if !l.is_empty() => l.to_string(),
        _ => ctx.r.provider.region.clone().unwrap_or_else(|| ctx.region()),
    }
}

/// App Engine SKUs are global and named after the location's city.
fn app_engine_city(region: &str, flex: bool) -> &'static str {
    match region {
        "europe-west1" if flex => " EMEA",
        "us-central1" | "us-east1" | "europe-west1" => "",
        "us-east4" => " Northern Virginia",
        "us-west1" => " Oregon",
        "us-west2" => " Los Angeles",
        "us-west3" => " Salt Lake City",
        "us-west4" => " Las Vegas",
        "northamerica-northeast1" => " Montreal",
        "southamerica-east1" => " Sao Paulo",
        "europe-west2" => " London",
        "europe-west3" => " Frankfurt",
        "europe-west6" => " Zurich",
        "europe-central2" => " Warsaw",
        "asia-east1" => " Taiwan",
        "asia-east2" => " Hong Kong",
        "asia-northeast1" => " Japan",
        "asia-northeast2" => " Osaka",
        "asia-northeast3" => " Seoul",
        "asia-south1" => " Mumbai",
        "asia-southeast1" => " Singapore",
        "asia-southeast2" => " Jakarta",
        "australia-southeast1" => " Sydney",
        _ => "",
    }
}

fn app_engine_pattern(ctx: &Ctx, stem: &str, region: &str, flex: bool) -> String {
    let city = app_engine_city(region, flex);
    let pat = format!("^{stem}{city}$");
    if city.is_empty() || ctx.price(Query::new(APP_ENGINE, "global", &pat)).is_some() {
        return pat;
    }
    ctx.warn(format!("no App Engine price for {region}, using the us-central1 rate"));
    format!("^{stem}$")
}

/// (instance class, F1/B1-equivalent multiplier, frontend?)
fn standard_class(ctx: &Ctx) -> (String, f64, bool) {
    let basic_or_manual = has_block(ctx, "basic_scaling") || has_block(ctx, "manual_scaling");
    let class = ctx.str("instance_class").unwrap_or(if basic_or_manual { "B1" } else { "F1" }).to_uppercase();
    let mult = match class.as_str() {
        "F2" | "B2" => 2.0,
        "F4" | "B4" => 4.0,
        "F4_1G" | "B4_1G" => 6.0,
        "B8" => 8.0,
        _ => 1.0,
    };
    let frontend = class.starts_with('F');
    (class, mult, frontend)
}

fn standard_always_on(ctx: &Ctx) -> f64 {
    if let Some(n) = ctx.num("manual_scaling.0.instances") {
        return n;
    }
    ctx.num_or("automatic_scaling.0.standard_scheduler_settings.0.min_instances", 0.0)
        .max(ctx.num_or("automatic_scaling.0.min_idle_instances", 0.0))
}

fn app_engine_standard(ctx: &Ctx) -> Vec<Component> {
    let region = app_engine_region(ctx);
    let (class, mult, frontend) = standard_class(ctx);
    let stem = if frontend { "Frontend Instances" } else { "Backend Instances" };
    let pat = app_engine_pattern(ctx, stem, &region, false);
    let note = (mult > 1.0).then(|| format!("{class} is billed as {} {}1 instance-hours per hour", fmt(mult), &class[..1]));
    let mut out = vec![];
    let always_on = standard_always_on(ctx);
    if always_on > 0.0 {
        let mut c = ctx.monthly(
            format!("Instances ({class}, {} always on)", fmt(always_on)),
            "instance-hours",
            always_on * HOURS_PER_MONTH * mult,
            Query::new(APP_ENGINE, "global", &pat),
        );
        c.note = note.clone();
        out.push(c);
    }
    let mut c = ctx.usage_qty(
        format!("Instances ({class}, on demand)"),
        "instance-hours",
        ctx.usage_num("monthly_instance_hours").map(|h| h * mult),
        Query::new(APP_ENGINE, "global", &pat),
    );
    c.note = note;
    out.push(c);
    out
}

fn app_engine_standard_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let always_on = standard_always_on(ctx);
    if ctx.env != Env::NonProd || always_on <= 0.0 || has_block(ctx, "manual_scaling") {
        return vec![];
    }
    let fixed = cr.components.iter().filter(|c| !c.usage_based).filter_map(|c| c.monthly_cost).sum::<f64>();
    let Some(s) = ctx.savings_vs(cr.monthly_cost, Some(cr.monthly_cost - fixed)) else { return vec![] };
    vec![ctx.rec(
        "appengine-min-instances-nonprod",
        Severity::Medium,
        "Let this non-production App Engine version scale to zero",
        format!(
            "{} instance(s) are kept running all month. Set automatic_scaling.standard_scheduler_settings.min_instances and min_idle_instances to 0 for non-production; the trade-off is cold starts.",
            fmt(always_on)
        ),
        Some(s),
    )]
}

fn flex_instances(ctx: &Ctx) -> f64 {
    ctx.usage_num("instances")
        .or_else(|| ctx.num("manual_scaling.0.instances"))
        .unwrap_or_else(|| ctx.num_or("automatic_scaling.0.min_total_instances", 2.0))
}

fn app_engine_flexible(ctx: &Ctx) -> Vec<Component> {
    let region = app_engine_region(ctx);
    let n = flex_instances(ctx);
    if n <= 0.0 {
        return vec![];
    }
    let cpu = ctx.num_or("resources.0.cpu", 1.0);
    let mem = ctx.num_or("resources.0.memory_gb", 0.6);
    let disk = ctx.num_or("resources.0.disk_gb", 10.0);
    let cpu_pat = app_engine_pattern(ctx, "Flex Instance Core Hours", &region, true);
    let ram_pat = app_engine_pattern(ctx, "Flex Instance RAM", &region, true);
    let mut mem_c = ctx.hourly_unit(
        format!("Memory ({} x {} GiB)", fmt(n), fmt(mem)),
        "GiB-hours",
        n * mem,
        Query::new(APP_ENGINE, "global", &ram_pat),
    );
    mem_c.note = Some("the runtime's own memory overhead is billed on top of memory_gb".into());
    vec![
        ctx.hourly_unit(
            format!("vCPU ({} x {} vCPU)", fmt(n), fmt(cpu)),
            "vCPU-hours",
            n * cpu,
            Query::new(APP_ENGINE, "global", &cpu_pat),
        ),
        mem_c,
        ctx.monthly(
            format!("Disk ({} x {} GiB)", fmt(n), fmt(disk)),
            "GiB-months",
            n * disk,
            Query::new(COMPUTE, &region, "^Storage PD Capacity").ondemand(),
        ),
    ]
}

fn app_engine_flexible_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let n = flex_instances(ctx);
    if ctx.env == Env::NonProd && n > 1.0 && !has_block(ctx, "manual_scaling") {
        let new = ctx.reprice(|a| super::set_path(a, "automatic_scaling.0.min_total_instances", Value::Number(1.0)));
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "appengine-flex-min-instances-nonprod",
                Severity::Medium,
                format!("Run 1 instance instead of {} for this non-production flexible version", fmt(n)),
                "The flexible environment defaults to min_total_instances = 2 and never scales to zero. Set automatic_scaling.min_total_instances = 1 for non-production.",
                Some(s),
            ));
        }
    }
    if cr.monthly_cost > 20.0 {
        out.push(ctx.rec(
            "appengine-flex-to-cloud-run",
            Severity::Info,
            "Consider Cloud Run instead of the App Engine flexible environment",
            "Flexible environment instances are VMs billed continuously (no scale to zero, no free tier). Cloud Run runs the same containers with per-request billing and scale to zero, which is usually much cheaper for variable traffic.",
            None,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_quantities() {
        assert_eq!(parse_cpu("1"), Some(1.0));
        assert_eq!(parse_cpu("2000m"), Some(2.0));
        assert_eq!(parse_cpu("0.5"), Some(0.5));
        assert_eq!(parse_cpu("abc"), None);
    }

    #[test]
    fn memory_quantities() {
        assert_eq!(parse_mem_gib("512Mi"), Some(0.5));
        assert_eq!(parse_mem_gib("2Gi"), Some(2.0));
        assert!((parse_mem_gib("256M").unwrap() - 0.2384).abs() < 1e-3);
        assert!((parse_mem_gib("1G").unwrap() - 0.9313).abs() < 1e-3);
        assert_eq!(parse_mem_gib("1073741824"), Some(1.0));
        assert_eq!(parse_mem_gib("x"), None);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("604800s"), Some(604800.0));
        assert_eq!(parse_duration_secs("10m"), Some(600.0));
        assert_eq!(parse_duration_secs("7d"), Some(604800.0));
        assert_eq!(parse_duration_secs("86400.5s"), Some(86400.5));
    }

    #[test]
    fn function_sizes() {
        assert_eq!(gen1_ghz(128.0), 0.2);
        assert_eq!(gen1_ghz(256.0), 0.4);
        assert_eq!(gen1_ghz(2048.0), 2.4);
        assert_eq!(gen1_ghz(8192.0), 4.8);
        assert_eq!(gen2_default_cpu(0.25), 0.167);
        assert_eq!(gen2_default_cpu(2.0), 1.0);
        assert_eq!(gen2_default_cpu(4.0), 2.0);
        assert_eq!(gen2_default_cpu(32.0), 8.0);
    }

    #[test]
    fn app_engine_cities() {
        assert_eq!(app_engine_city("europe-west1", false), "");
        assert_eq!(app_engine_city("europe-west1", true), " EMEA");
        assert_eq!(app_engine_city("asia-northeast1", false), " Japan");
    }
}
