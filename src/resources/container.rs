//! Google Kubernetes Engine: clusters (Standard and Autopilot) and node pools.
//!
//! Standard node pools are priced as Compute Engine VMs through
//! `compute::vm_components`, so machine prices, sustained use discounts, GPUs,
//! local SSDs and OS licenses stay consistent with instances and MIGs.

use super::compute::{VmSpec, default_boot_disk, vm_components};
use super::machine::{self, Pricing};
use super::{Ctx, Spec, is_zone, usage};
use crate::cost::Component;
use crate::hcl::value::{Value, fmt_number};
use crate::pricing::query::{Query, normalize_region};
use crate::tf::evaluate::ResourceInstance;

const GKE: &str = "Kubernetes Engine";

/// Zones used by a regional cluster when `node_locations` is not set.
const DEFAULT_REGIONAL_ZONES: usize = 3;
const DEFAULT_MACHINE: &str = "e2-medium";
const DEFAULT_DISK_GB: f64 = 100.0;
const LOCAL_SSD_GB: f64 = 375.0;

pub fn specs() -> Vec<Spec> {
    vec![
        Spec::new("google_container_cluster", cluster)
            .usage(usage!(
                "nodes" => "Average node count per zone of the default / inline node pools (overrides initial_node_count, node_count and autoscaling min)",
                "autopilot_vcpu_count" => "Autopilot: average vCPUs requested by running Pods",
                "autopilot_memory_gb" => "Autopilot: average memory (GiB) requested by running Pods",
                "autopilot_ephemeral_storage_gb" => "Autopilot: average ephemeral storage (GiB) requested by running Pods",
                "autopilot_spot_vcpu_count" => "Autopilot: average vCPUs requested by Spot Pods",
                "autopilot_spot_memory_gb" => "Autopilot: average memory (GiB) requested by Spot Pods",
                "autopilot_compute_class" => "Autopilot compute class of the Pods: autopilot (default, container-optimized), balanced or scale-out",
                "monthly_extended_support_hrs" => "Extended channel: hours per month the cluster runs a minor version in its extended support period",
                "monthly_hrs" => "Hours per month the cluster and its nodes run (default 730)",
            ))
            .rules(rules::cluster),
        Spec::new("google_container_node_pool", node_pool)
            .usage(usage!(
                "nodes" => "Average node count per zone (overrides node_count, initial_node_count and autoscaling min)",
                "monthly_hrs" => "Hours per month each node runs (default 730)",
            ))
            .rules(rules::node_pool),
    ]
}

// ---- attribute helpers on arbitrary values (parent cluster, pool blocks) ----

fn get<'v>(v: &'v Value, path: &str) -> Option<&'v Value> {
    v.path(path).filter(|x| !x.is_null() && !x.is_unknown())
}

fn vstr<'v>(v: &'v Value, path: &str) -> Option<&'v str> {
    get(v, path).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
}

fn vnum(v: &Value, path: &str) -> Option<f64> {
    get(v, path).and_then(|x| x.as_f64())
}

fn vbool(v: &Value, path: &str) -> Option<bool> {
    get(v, path).and_then(|x| x.as_bool())
}

/// First element of a nested block (list of maps, or a bare map).
fn block<'v>(v: &'v Value, key: &str) -> Option<&'v Value> {
    match get(v, key)? {
        Value::List(l) => l.first().filter(|b| b.as_map().is_some()),
        m @ Value::Map(_) => Some(m),
        _ => None,
    }
}

fn blocks<'v>(v: &'v Value, key: &str) -> Vec<&'v Value> {
    match get(v, key) {
        Some(Value::List(l)) => l.iter().filter(|b| b.as_map().is_some()).collect(),
        Some(m @ Value::Map(_)) => vec![m],
        _ => vec![],
    }
}

fn str_list(v: &Value, key: &str) -> Vec<String> {
    get(v, key)
        .and_then(|l| l.as_list())
        .map(|l| l.iter().filter_map(|s| s.as_str()).filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

// ---- cluster / pool model ------------------------------------------------

/// The parts of a cluster that affect node pool pricing.
struct Cluster<'a> {
    attrs: &'a Value,
    location: Option<String>,
    node_locations: Vec<String>,
    private_nodes: bool,
    autopilot: bool,
}

impl<'a> Cluster<'a> {
    fn of(r: &'a ResourceInstance, attrs: &'a Value) -> Cluster<'a> {
        let location =
            vstr(attrs, "location").map(str::to_string).or_else(|| r.provider.zone.clone()).or_else(|| r.provider.region.clone());
        Cluster {
            attrs,
            location,
            node_locations: str_list(attrs, "node_locations"),
            private_nodes: vbool(attrs, "private_cluster_config.0.enable_private_nodes").unwrap_or(false),
            autopilot: vbool(attrs, "enable_autopilot").unwrap_or(false),
        }
    }

    fn regional(&self) -> bool {
        self.location.as_deref().is_some_and(|l| !is_zone(l))
    }
}

/// Number of zones a node pool spans, and whether its location is a region.
///
/// `pool_locs` is the node pool's own `node_locations` (the complete list);
/// the cluster-level list of a zonal cluster excludes the cluster's zone.
fn zone_count(location: Option<&str>, pool_locs: &[String], cluster_locs: &[String]) -> Option<(usize, bool)> {
    let regional = location.is_some_and(|l| !is_zone(l));
    if !pool_locs.is_empty() {
        return Some((pool_locs.len(), regional));
    }
    match location {
        Some(z) if is_zone(z) => {
            let extra = cluster_locs.iter().filter(|l| l.as_str() != z).count();
            Some((1 + extra, false))
        }
        Some(_) if !cluster_locs.is_empty() => Some((cluster_locs.len(), true)),
        Some(_) => Some((DEFAULT_REGIONAL_ZONES, true)),
        None => None,
    }
}

/// A group of identical GKE nodes.
#[derive(Clone)]
struct Pool {
    label: String,
    spec: VmSpec,
    region: String,
    zones: usize,
    regional: bool,
    autoscaled: bool,
}

impl Pool {
    fn nodes(&self) -> f64 {
        self.spec.count
    }
}

fn gpus_of(nc: &Value) -> Vec<(String, f64)> {
    blocks(nc, "guest_accelerator")
        .into_iter()
        .filter_map(|g| {
            let t = vstr(g, "type")?.to_string();
            Some((t, vnum(g, "count").unwrap_or(1.0)))
        })
        .collect()
}

/// VM shape of a node from a `node_config` block (provider defaults applied).
fn node_spec(ctx: &Ctx, nc: Option<&Value>, private: bool) -> Option<VmSpec> {
    let empty = Value::Null;
    let nc = nc.unwrap_or(&empty);
    if nc.path("machine_type").is_some_and(Value::is_unknown) {
        ctx.warn("node machine_type is not known statically");
        return None;
    }
    let machine_type = vstr(nc, "machine_type").unwrap_or(DEFAULT_MACHINE);
    let Some(mt) = machine::parse(machine_type) else {
        ctx.warn(format!("unknown machine type '{machine_type}'"));
        return None;
    };
    let boot = block(nc, "boot_disk");
    let default_disk = match default_boot_disk(&mt.family) {
        "hyperdisk-balanced" => "hyperdisk-balanced",
        _ => "pd-balanced",
    };
    let disk_type = boot.and_then(|b| vstr(b, "disk_type")).or_else(|| vstr(nc, "disk_type")).unwrap_or(default_disk).to_string();
    let disk_size = boot.and_then(|b| vnum(b, "size_gb")).or_else(|| vnum(nc, "disk_size_gb")).unwrap_or(DEFAULT_DISK_GB);
    let iops = boot.and_then(|b| vnum(b, "provisioned_iops"));
    let throughput = boot.and_then(|b| vnum(b, "provisioned_throughput"));

    let spot = vbool(nc, "spot").unwrap_or(false) || vbool(nc, "preemptible").unwrap_or(false);
    let ssd_count = vnum(nc, "local_ssd_count").unwrap_or(0.0)
        + vnum(nc, "ephemeral_storage_local_ssd_config.0.local_ssd_count").unwrap_or(0.0)
        + vnum(nc, "local_nvme_ssd_block_config.0.local_ssd_count").unwrap_or(0.0);
    // `-lssd` machine types bundle their local SSDs; the count only restates them.
    let local_ssd_gb = if mt.local_ssd_gb > 0.0 { 0.0 } else { ssd_count * LOCAL_SSD_GB };

    let image_type = vstr(nc, "image_type").unwrap_or("COS_CONTAINERD").to_uppercase();
    let image = image_type.contains("WINDOWS").then(|| "windows-server-2022".to_string());

    Some(VmSpec {
        machine: mt.name.clone(),
        pricing: if spot { Pricing::Spot } else { Pricing::OnDemand },
        count: 1.0,
        disks: vec![(disk_type, disk_size, iops, throughput)],
        local_ssd_gb,
        gpus: gpus_of(nc),
        ephemeral_ips: if private { 0.0 } else { 1.0 },
        image,
    })
}

/// Build a node pool from a pool definition: a `google_container_node_pool`,
/// an inline `node_pool` block, or the cluster itself (default pool).
///
/// `usage_nodes` is the per-zone node count from the usage file.
#[allow(clippy::too_many_arguments)]
fn build_pool(
    ctx: &Ctx,
    label: String,
    p: &Value,
    cluster: Option<&Cluster>,
    location: Option<&str>,
    pool_locs: &[String],
    usage_nodes: Option<f64>,
    fallback_region: &str,
) -> Option<Pool> {
    let cluster_locs = cluster.map(|c| c.node_locations.as_slice()).unwrap_or(&[]);
    let (zones, regional) = zone_count(location, pool_locs, cluster_locs).unwrap_or_else(|| {
        ctx.warn("cluster location unknown, assuming a single zone");
        (1, false)
    });
    let region = location.map(normalize_region).unwrap_or_else(|| fallback_region.to_string());
    let z = zones as f64;

    let auto = block(p, "autoscaling");
    let initial = vnum(p, "initial_node_count");
    let mut autoscaled = false;
    let count = if let Some(n) = usage_nodes {
        autoscaled = auto.is_some();
        n * z
    } else if let Some(a) = auto {
        let (tmin, tmax) = (vnum(a, "total_min_node_count"), vnum(a, "total_max_node_count"));
        let (min, max, scope) = if tmin.is_some() || tmax.is_some() {
            let min = tmin.unwrap_or(0.0);
            (min, tmax.unwrap_or(min), "in total")
        } else {
            let min = vnum(a, "min_node_count").unwrap_or(0.0);
            (min * z, vnum(a, "max_node_count").map(|m| m * z).unwrap_or(min * z), "across all zones")
        };
        autoscaled = max > min;
        let est = if min > 0.0 { min } else { initial.map(|i| i * z).unwrap_or(0.0) };
        if autoscaled {
            ctx.warn(format!(
                "{label}: autoscaled between {} and {} nodes {scope}; estimating with {} (set usage 'nodes' per zone to override)",
                fmt_number(min),
                fmt_number(max),
                fmt_number(est)
            ));
        }
        est
    } else if let Some(n) = vnum(p, "node_count").or(initial) {
        n * z
    } else {
        ctx.warn(format!("{label}: node count unknown, assuming 1 node per zone"));
        z
    };

    let private = vbool(p, "network_config.0.enable_private_nodes").unwrap_or_else(|| cluster.is_some_and(|c| c.private_nodes));
    let mut spec = node_spec(ctx, block(p, "node_config"), private)?;
    spec.count = count;
    Some(Pool { label, spec, region, zones, regional, autoscaled })
}

/// Node pools priced on the cluster resource: inline `node_pool` blocks, or
/// the default pool unless it is removed (or the cluster is Autopilot).
fn cluster_pools(ctx: &Ctx, cl: &Cluster, usage_nodes: Option<f64>) -> Vec<Pool> {
    if cl.autopilot {
        return vec![];
    }
    let fallback = ctx.region();
    let location = cl.location.as_deref();
    let inline = blocks(cl.attrs, "node_pool");
    if !inline.is_empty() {
        return inline
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let name = vstr(b, "name").map(str::to_string).unwrap_or_else(|| format!("#{}", i + 1));
                let locs = str_list(b, "node_locations");
                build_pool(ctx, format!("Node pool {name}"), b, Some(cl), location, &locs, usage_nodes, &fallback)
            })
            .collect();
    }
    if vbool(cl.attrs, "remove_default_node_pool").unwrap_or(false) {
        return vec![];
    }
    build_pool(ctx, "Default pool".into(), cl.attrs, Some(cl), location, &[], usage_nodes, &fallback).into_iter().collect()
}

/// The parent cluster of a `google_container_node_pool`.
fn parent_cluster<'a>(ctx: &Ctx<'a>) -> Option<&'a ResourceInstance> {
    let c = ctx.str("cluster")?;
    ctx.resolve_ref(c, "google_container_cluster").filter(|r| r.rtype == "google_container_cluster")
}

fn node_pool_model(ctx: &Ctx) -> Option<Pool> {
    let parent = parent_cluster(ctx);
    let cl = parent.map(|r| Cluster::of(r, &r.attrs));
    if parent.is_none() {
        ctx.warn("parent cluster not found in this configuration; zones inferred from the node pool location");
    }
    let location = ctx.str("location").map(str::to_string).or_else(|| cl.as_ref().and_then(|c| c.location.clone()));
    let name = ctx.str("name").unwrap_or(&ctx.r.name);
    build_pool(
        ctx,
        format!("Node pool {name}"),
        ctx.attrs,
        cl.as_ref(),
        location.as_deref(),
        &str_list(ctx.attrs, "node_locations"),
        ctx.usage_num("nodes"),
        &ctx.region(),
    )
}

fn pool_components(ctx: &Ctx, pool: &Pool) -> Vec<Component> {
    vm_components(ctx, &pool.region, &pool.spec)
}

fn node_pool(ctx: &Ctx) -> Vec<Component> {
    let Some(pool) = node_pool_model(ctx) else { return vec![] };
    let mut comps = pool_components(ctx, &pool);
    if let Some(c) = comps.first_mut() {
        let zones = format!("{} zone{}", pool.zones, if pool.zones > 1 { "s" } else { "" });
        c.note = Some(match c.note.take() {
            Some(n) => format!("{zones}; {n}"),
            None => zones,
        });
    }
    comps
}

// ---- Autopilot -------------------------------------------------------------

struct AutopilotSkus {
    label: &'static str,
    cpu: &'static str,
    mem: &'static str,
    spot_cpu: &'static str,
    spot_mem: &'static str,
    ephemeral: &'static str,
}

fn autopilot_skus(class: &str) -> Option<AutopilotSkus> {
    Some(match class {
        "autopilot" | "general-purpose" | "" => AutopilotSkus {
            label: "general-purpose",
            cpu: "^Autopilot Pod mCPU Requests",
            mem: "^Autopilot Pod Memory Requests",
            spot_cpu: "^Autopilot Spot Pod mCPU Requests",
            spot_mem: "^Autopilot Spot Pod Memory Requests",
            ephemeral: "^Autopilot SSD Pod Ephemeral Storage Requests",
        },
        "balanced" => AutopilotSkus {
            label: "Balanced",
            cpu: "^Autopilot Balanced Pod mCPU Requests",
            mem: "^Autopilot Balanced Pod Memory Requests",
            spot_cpu: "^Autopilot Balanced Spot Pod mCPU Requests",
            spot_mem: "^Autopilot Balanced Spot Pod Memory Requests",
            ephemeral: "^Autopilot Pod Ephemeral Storage Requests",
        },
        "scale-out" => AutopilotSkus {
            label: "Scale-Out",
            cpu: "^Autopilot Scale-Out x86 Pod mCPU Requests",
            mem: "^Autopilot Scale-Out x86 Pod Memory Requests",
            spot_cpu: "^Autopilot Scale-Out x86 Spot Pod mCPU Requests",
            spot_mem: "^Autopilot Scale-Out x86 Spot Pod Memory Requests",
            ephemeral: "^Autopilot Pod Ephemeral Storage Requests",
        },
        _ => return None,
    })
}

fn autopilot_class(ctx: &Ctx) -> AutopilotSkus {
    let class = ctx.usage_str("autopilot_compute_class").unwrap_or("autopilot").to_lowercase();
    autopilot_skus(&class).unwrap_or_else(|| {
        ctx.warn(format!("unknown autopilot_compute_class '{class}', using general-purpose Autopilot prices"));
        autopilot_skus("autopilot").expect("default class")
    })
}

/// A usage estimate that cannot be priced (SKU missing in the region) must
/// not count as "depends on usage", otherwise repricing treats it as free.
fn priced_usage(mut c: Component) -> Component {
    if c.quantity.is_some() && c.monthly_cost.is_none() {
        c.usage_based = false;
    }
    c
}

/// Autopilot CPU SKUs are priced per mCPU-hour: show them per vCPU-hour.
fn per_vcpu(mut c: Component) -> Component {
    c.quantity = c.quantity.map(|q| q / 1000.0);
    c.unit_price = c.unit_price.map(|p| p * 1000.0);
    c.unit = "vCPU-hours".into();
    c
}

fn autopilot_components(ctx: &Ctx, region: &str) -> Vec<Component> {
    let skus = autopilot_class(ctx);
    let hours = ctx.monthly_hours();
    let q = |pat, usage| Query::new(GKE, region, pat).usage(usage);
    let vcpu = |key: &str| ctx.usage_num(key).map(|n| n * 1000.0 * hours);
    let gib_hours = |key: &str| ctx.usage_num(key).map(|n| n * hours);
    let mut out = vec![
        per_vcpu(ctx.usage_qty(
            format!("Autopilot Pod vCPU requests ({})", skus.label),
            "mCPU-hours",
            vcpu("autopilot_vcpu_count"),
            q(skus.cpu, "OnDemand"),
        )),
        ctx.usage_qty(
            format!("Autopilot Pod memory requests ({})", skus.label),
            "GiB-hours",
            gib_hours("autopilot_memory_gb"),
            q(skus.mem, "OnDemand"),
        ),
        ctx.usage_qty(
            "Autopilot Pod ephemeral storage requests",
            "GiB-hours",
            gib_hours("autopilot_ephemeral_storage_gb"),
            q(skus.ephemeral, "OnDemand"),
        ),
    ];
    if ctx.usage_num("autopilot_spot_vcpu_count").is_some() || ctx.usage_num("autopilot_spot_memory_gb").is_some() {
        out.push(per_vcpu(ctx.usage_qty(
            format!("Autopilot Spot Pod vCPU requests ({})", skus.label),
            "mCPU-hours",
            vcpu("autopilot_spot_vcpu_count"),
            q(skus.spot_cpu, "Preemptible"),
        )));
        out.push(ctx.usage_qty(
            format!("Autopilot Spot Pod memory requests ({})", skus.label),
            "GiB-hours",
            gib_hours("autopilot_spot_memory_gb"),
            q(skus.spot_mem, "Preemptible"),
        ));
    }
    out.into_iter().map(priced_usage).collect()
}

// ---- cluster ---------------------------------------------------------------

fn cluster(ctx: &Ctx) -> Vec<Component> {
    let cl = Cluster::of(ctx.r, ctx.attrs);
    let region = cl.location.as_deref().map(normalize_region).unwrap_or_else(|| ctx.region());
    let (mode, pat) = if cl.autopilot {
        ("Autopilot", "^Autopilot Kubernetes Clusters$")
    } else if cl.regional() {
        ("regional", "^Regional Kubernetes Clusters$")
    } else {
        ("zonal", "^Zonal Kubernetes Clusters$")
    };
    let mut out = vec![];
    let mut fee = ctx.hourly(format!("Cluster management fee ({mode})"), 1.0, Query::new(GKE, "global", pat).ondemand());
    if cl.autopilot || !cl.regional() {
        fee.note =
            Some("the GKE free tier ($74.40/month per billing account) covers one zonal or Autopilot cluster; not deducted here".into());
    }
    out.push(fee);

    if ctx.str("release_channel.0.channel") == Some("EXTENDED") {
        let mut c = ctx.usage(
            "Extended support period fee",
            "hours",
            "monthly_extended_support_hrs",
            Query::new(GKE, "global", "^Extended Period Kubernetes Clusters$").ondemand(),
        );
        c.note = Some("charged only once the cluster's minor version leaves standard support".into());
        out.push(c);
    }

    if cl.autopilot {
        out.extend(autopilot_components(ctx, &region));
        return out;
    }

    if ctx.bool("cluster_autoscaling.0.enabled") == Some(true) {
        ctx.warn("node auto-provisioning is enabled: auto-created node pools are not estimated");
    }
    for pool in cluster_pools(ctx, &cl, ctx.usage_num("nodes")) {
        for mut c in pool_components(ctx, &pool) {
            c.name = format!("{}: {}", pool.label, c.name);
            out.push(c);
        }
    }
    out
}

pub(crate) mod rules {
    use super::*;
    use crate::cost::{CostedResource, HOURS_PER_MONTH, Recommendation, Severity};
    use crate::resources::Env;
    use crate::resources::compute::{machine_hourly, sud_multiplier};

    fn cost_of(ctx: &Ctx, region: &str, spec: &VmSpec) -> Option<f64> {
        let comps = vm_components(ctx, region, spec);
        if comps.is_empty() || comps.iter().any(|c| c.is_unpriced()) {
            return None;
        }
        Some(CostedResource::total(&comps))
    }

    fn title(pool: &Pool, multi: bool, t: &str) -> String {
        if multi { format!("{t} ({})", pool.label) } else { t.to_string() }
    }

    /// Cheapest predefined machine of another family with the same shape.
    fn cheaper_family(ctx: &Ctx, region: &str, spec: &VmSpec, mt: &machine::MachineType) -> Option<(String, f64)> {
        let (_, rest) = mt.name.split_once('-')?;
        let mut best: Option<(String, f64)> = None;
        for fam in ["e2", "n2d", "t2d", "n4", "n4d", "n2", "c3d"] {
            if fam == mt.family {
                continue;
            }
            let cand = format!("{fam}-{rest}");
            let Some(cm) = machine::parse(&cand) else { continue };
            if cm.vcpus != mt.vcpus || cm.memory_gb + 0.01 < mt.memory_gb || cm.local_ssd_gb < mt.local_ssd_gb {
                continue;
            }
            let mut disks = spec.disks.clone();
            if default_boot_disk(fam) == "hyperdisk-balanced" {
                for d in disks.iter_mut().filter(|d| d.0.starts_with("pd-")) {
                    d.0 = "hyperdisk-balanced".into();
                }
            }
            let alt = VmSpec { machine: cand.clone(), disks, ..spec.clone() };
            if let Some(t) = cost_of(ctx, region, &alt)
                && best.as_ref().is_none_or(|b| t < b.1)
            {
                best = Some((cand, t));
            }
        }
        best
    }

    fn pool_rules(ctx: &Ctx, env: Env, pool: &Pool, multi: bool, out: &mut Vec<Recommendation>) {
        let spec = &pool.spec;
        let region = pool.region.as_str();
        if spec.count <= 0.0 {
            return;
        }
        let Some(current) = cost_of(ctx, region, spec) else { return };
        let Some(mt) = machine::parse(&spec.machine) else { return };
        let has_gpu = !spec.gpus.is_empty() || !mt.accelerators.is_empty();
        let hours = ctx.monthly_hours();

        if env == Env::NonProd && spec.pricing == Pricing::OnDemand {
            let alt = VmSpec { pricing: Pricing::Spot, ..spec.clone() };
            if let Some(s) = ctx.savings_vs(current, cost_of(ctx, region, &alt)) {
                out.push(ctx.rec(
                    "gke-spot-nodepool-nonprod",
                    Severity::Medium,
                    title(pool, multi, "Use Spot nodes for this non-production node pool"),
                    "Spot VMs are 60-91% cheaper than on-demand nodes. GKE drains preempted nodes gracefully (30 s notice), which suits dev/test, CI and stateless workloads. Set node_config { spot = true }; keep a small on-demand pool if some workloads cannot tolerate preemption, and use PodDisruptionBudgets.",
                    Some(s),
                ));
            }
        }

        if env == Env::NonProd && pool.regional && pool.zones > 1 {
            let alt = VmSpec { count: spec.count / pool.zones as f64, ..spec.clone() };
            if let Some(s) = ctx.savings_vs(current, cost_of(ctx, region, &alt)) {
                out.push(ctx.rec(
                    "gke-regional-nonprod",
                    Severity::Medium,
                    title(pool, multi, &format!("Run this non-production node pool in a single zone instead of {}", pool.zones)),
                    format!(
                        "In a regional cluster, node counts apply per zone, so this pool runs {} nodes across {} zones. For non-production, set node_locations = [\"{region}-b\"] on the node pool (or the cluster), or use a zonal cluster (location = \"{region}-b\"), which also qualifies for the GKE free-tier credit. Caveat: a zone outage takes the workloads down.",
                        fmt_number(spec.count),
                        pool.zones
                    ),
                    Some(s),
                ));
            }
        }

        if !mt.custom
            && !mt.shared_core
            && !has_gpu
            && spec.pricing == Pricing::OnDemand
            && let Some((cand, t)) = cheaper_family(ctx, region, spec, &mt)
        {
            let s = current - t;
            if s > 5.0 && s / current > 0.08 {
                out.push(ctx.rec(
                    "gke-machine-family",
                    if s > 100.0 { Severity::Medium } else { Severity::Low },
                    title(pool, multi, &format!("Switch nodes from {} to {cand}", mt.name)),
                    format!(
                        "{cand} has the same vCPU count and at least as much memory for {}% less. Changing node_config.machine_type recreates the node pool: create a new pool, cordon/drain the old one, then delete it. Validate CPU platform, regional availability and performance first.",
                        ((s / current) * 100.0).round()
                    ),
                    Some(s),
                ));
            }
        }

        if env != Env::NonProd && spec.pricing == Pricing::OnDemand && hours >= HOURS_PER_MONTH * 0.9 {
            let od = machine_hourly(ctx, region, &mt, Pricing::OnDemand).map(|h| h * sud_multiplier(&mt.family, hours));
            let c1 = machine_hourly(ctx, region, &mt, Pricing::Commit1Yr);
            let c3 = machine_hourly(ctx, region, &mt, Pricing::Commit3Yr);
            if let (Some(od), Some(c1), Some(c3)) = (od, c1, c3) {
                let base = if pool.autoscaled { "minimum " } else { "" };
                let s1 = (od - c1) * hours * spec.count;
                let s3 = (od - c3) * hours * spec.count;
                if s3 > 5.0 {
                    out.push(ctx.rec(
                        "gke-cud",
                        Severity::Info,
                        title(pool, multi, "Cover always-on GKE nodes with a committed use discount"),
                        format!(
                            "GKE Standard nodes are Compute Engine VMs. A resource-based CUD for the {base}{} vCPU / {} GiB of this pool would save ~{}/mo (1 year) or ~{}/mo (3 years). Commit only to the steady-state baseline across the project; Compute Flexible CUDs are an alternative when machine families change.",
                            fmt_number(machine::billed_vcpus(&mt) * spec.count),
                            fmt_number(mt.memory_gb * spec.count),
                            ctx.money(s1),
                            ctx.money(s3)
                        ),
                        Some(s1),
                    ));
                }
            }
        }

        if let Some((dtype, size, _, _)) = spec.disks.first() {
            let new_type = if dtype == "pd-ssd" { "pd-balanced" } else { dtype.as_str() };
            let new_size = size.min(super::DEFAULT_DISK_GB);
            if new_type != dtype || new_size < *size {
                let mut disks = spec.disks.clone();
                disks[0] = (new_type.to_string(), new_size, disks[0].2, disks[0].3);
                let alt = VmSpec { disks, ..spec.clone() };
                if let Some(s) = ctx.savings_vs(current, cost_of(ctx, region, &alt)) {
                    let mut changes = vec![];
                    if new_type != dtype {
                        changes.push("disk_type = \"pd-balanced\" (SSD-backed, ~40% cheaper than pd-ssd)".to_string());
                    }
                    if new_size < *size {
                        changes.push(format!("disk_size_gb = {} (currently {})", fmt_number(new_size), fmt_number(*size)));
                    }
                    out.push(ctx.rec(
                        "gke-node-disk",
                        Severity::Low,
                        title(pool, multi, "Right-size node boot disks"),
                        format!(
                            "Node boot disks mostly hold the OS, container images and emptyDir volumes, and are billed per node. Consider {}. Keep larger/faster disks only for image-heavy or I/O-bound nodes (check kubelet ephemeral-storage usage). Changing the boot disk recreates the nodes.",
                            changes.join(" and ")
                        ),
                        Some(s),
                    ));
                }
            }
        }

        if spec.ephemeral_ips > 0.0 {
            let alt = VmSpec { ephemeral_ips: 0.0, ..spec.clone() };
            if let Some(s) = ctx.savings_vs(current, cost_of(ctx, region, &alt)) {
                out.push(ctx.rec(
                    "gke-private-nodes",
                    Severity::Low,
                    title(pool, multi, "Use private nodes to avoid external IP charges"),
                    format!(
                        "Each of the {} nodes has an external IPv4 address billed hourly. Private nodes (private_cluster_config {{ enable_private_nodes = true }} or node pool network_config {{ enable_private_nodes = true }}) remove that cost and reduce the attack surface. Outbound internet access then needs Cloud NAT (billed per VM-hour and per GiB processed).",
                        fmt_number(spec.count)
                    ),
                    Some(s),
                ));
            }
        }
    }

    fn no_autoscaling(ctx: &Ctx, p: &Value, pool: &Pool, multi: bool, out: &mut Vec<Recommendation>) {
        if block(p, "autoscaling").is_some() || pool.nodes() < 2.0 {
            return;
        }
        out.push(ctx.rec(
            "gke-no-autoscaling",
            Severity::Info,
            title(pool, multi, "Enable autoscaling on this fixed-size node pool"),
            format!(
                "The pool always runs {} nodes. With autoscaling {{ min_node_count / max_node_count }} (or total_* counts) the cluster autoscaler removes idle nodes off-peak and adds capacity under load. Pair it with accurate Pod resource requests; savings depend on load variability.",
                fmt_number(pool.nodes())
            ),
            None,
        ));
    }

    pub fn node_pool(ctx: &Ctx, _cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let Some(pool) = node_pool_model(ctx) else { return out };
        let env = ctx.env;
        pool_rules(ctx, env, &pool, false, &mut out);
        no_autoscaling(ctx, ctx.attrs, &pool, false, &mut out);
        out
    }

    /// `google_container_node_pool` resources attached to the cluster.
    fn attached_pools<'a>(ctx: &Ctx<'a>) -> Vec<&'a ResourceInstance> {
        ctx.siblings
            .iter()
            .filter(|s| s.rtype == "google_container_node_pool")
            .filter(|s| {
                vstr(&s.attrs, "cluster")
                    .and_then(|c| ctx.resolve_ref(c, "google_container_cluster"))
                    .is_some_and(|r| r.address == ctx.r.address)
            })
            .collect()
    }

    pub fn cluster(ctx: &Ctx, _cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let cl = Cluster::of(ctx.r, ctx.attrs);
        let env = ctx.env;

        if ctx.str("release_channel.0.channel") == Some("EXTENDED") {
            let fee = ctx
                .price(Query::new(GKE, "global", "^Extended Period Kubernetes Clusters$").ondemand())
                .map(|p| p.unit_price() * HOURS_PER_MONTH);
            out.push(ctx.rec(
                "gke-extended-channel",
                Severity::Info,
                "Plan upgrades before the extended support period",
                format!(
                    "The Extended release channel adds a fee of ~{}/mo per cluster once the minor version leaves standard support (up to 24 months). Upgrade to a minor version in standard support in time, or use the REGULAR/STABLE channel if long-term version pinning is not required.",
                    fee.map(|f| ctx.money(f)).unwrap_or_else(|| ctx.money(365.0))
                ),
                None,
            ));
        }

        if cl.autopilot {
            autopilot_rules(ctx, env, &cl, &mut out);
            return out;
        }

        let pools = cluster_pools(ctx, &cl, ctx.usage_num("nodes"));
        let multi = pools.len() > 1;
        let inline = blocks(ctx.attrs, "node_pool");
        for (i, pool) in pools.iter().enumerate() {
            pool_rules(ctx, env, pool, multi, &mut out);
            if let Some(b) = inline.get(i) {
                no_autoscaling(ctx, b, pool, multi, &mut out);
            }
        }

        let attached = attached_pools(ctx);
        let default_pool = pools.iter().find(|p| p.label == "Default pool");
        if let Some(dp) = default_pool
            && !attached.is_empty()
            && let Some(s) = cost_of(ctx, &dp.region, &dp.spec).filter(|s| *s > 1.0)
        {
            out.push(ctx.rec(
                "gke-default-node-pool",
                Severity::Low,
                "Remove the default node pool",
                format!(
                    "This cluster also has {} separately managed node pool(s), but its default pool ({} x {}) keeps running. Set remove_default_node_pool = true (with initial_node_count = 1) unless workloads rely on it.",
                    attached.len(),
                    fmt_number(dp.nodes()),
                    dp.spec.machine
                ),
                Some(s),
            ));
        }

        // Small Standard clusters: Autopilot bills Pod requests instead of nodes.
        let fallback = ctx.region();
        let mut all: Vec<Pool> = pools.clone();
        for r in &attached {
            let loc = vstr(&r.attrs, "location").or(cl.location.as_deref());
            if let Some(p) =
                build_pool(ctx, r.name.clone(), &r.attrs, Some(&cl), loc, &str_list(&r.attrs, "node_locations"), None, &fallback)
            {
                all.push(p);
            }
        }
        let has_gpu =
            all.iter().any(|p| !p.spec.gpus.is_empty() || machine::parse(&p.spec.machine).is_some_and(|m| !m.accelerators.is_empty()));
        let vcpus: f64 = all.iter().filter_map(|p| machine::parse(&p.spec.machine).map(|m| machine::billed_vcpus(&m) * p.nodes())).sum();
        if !all.is_empty() && !has_gpu && vcpus > 0.0 && vcpus <= 8.0 {
            out.push(ctx.rec(
                "gke-autopilot-consider",
                Severity::Info,
                "Consider Autopilot for this small Standard cluster",
                format!(
                    "The cluster's node pools total only ~{} vCPUs. On small clusters, system Pods, DaemonSets and unallocated node capacity are a large share of the node bill; Autopilot charges only for running Pod requests (and one Autopilot cluster is covered by the free tier). Compare with your actual Pod requests before migrating.",
                    fmt_number(vcpus)
                ),
                None,
            ));
        }
        out
    }

    fn autopilot_rules(ctx: &Ctx, env: Env, cl: &Cluster, out: &mut Vec<Recommendation>) {
        let region = cl.location.as_deref().map(normalize_region).unwrap_or_else(|| ctx.region());
        let skus = autopilot_class(ctx);
        let hours = ctx.monthly_hours();
        let rate = |pat: &str, usage: &str| ctx.price(Query::new(GKE, &region, pat).usage(usage)).map(|p| p.unit_price());
        let vcpu = ctx.usage_num("autopilot_vcpu_count").unwrap_or(0.0);
        let mem = ctx.usage_num("autopilot_memory_gb").unwrap_or(0.0);
        if vcpu <= 0.0 && mem <= 0.0 {
            return;
        }
        let (Some(cpu), Some(ram)) = (rate(skus.cpu, "OnDemand"), rate(skus.mem, "OnDemand")) else { return };
        let ondemand = (cpu * 1000.0 * vcpu + ram * mem) * hours;

        if env == Env::NonProd
            && let (Some(scpu), Some(sram)) = (rate(skus.spot_cpu, "Preemptible"), rate(skus.spot_mem, "Preemptible"))
            && let Some(s) = ctx.savings_vs(ondemand, Some((scpu * 1000.0 * vcpu + sram * mem) * hours))
        {
            out.push(ctx.rec(
                "gke-autopilot-spot-nonprod",
                Severity::Medium,
                "Run non-production Autopilot workloads on Spot Pods",
                "Spot Pods cost 60-91% less than regular Autopilot Pods. Select them with the nodeSelector cloud.google.com/gke-spot: \"true\" (or the autopilot-spot compute class); Pods can be evicted with a 25 s notice, which is usually fine for dev/test and batch workloads.",
                Some(s),
            ));
        }

        if env != Env::NonProd && hours >= crate::cost::HOURS_PER_MONTH * 0.9 {
            let commit = |years: &str| {
                let pat = format!("^Commitment - dollar based v1: Kubernetes Engine Autopilot .* for {years}");
                let usage = if years.starts_with('1') { "Commit1Yr" } else { "Commit3Yr" };
                // Dollar-based commitments are priced per $0.01 of on-demand spend.
                ctx.price(Query::new(GKE, &region, &pat).usage(usage)).map(|p| p.unit_price() * 100.0).filter(|f| *f > 0.0 && *f < 1.0)
            };
            if let (Some(f1), Some(f3)) = (commit("1 year"), commit("3 years")) {
                let (s1, s3) = (ondemand * (1.0 - f1), ondemand * (1.0 - f3));
                if s3 > 5.0 {
                    out.push(ctx.rec(
                        "gke-autopilot-cud",
                        Severity::Info,
                        "Cover steady Autopilot usage with a Kubernetes Engine CUD",
                        format!(
                            "Kubernetes Engine committed use discounts ({:.0}% for 1 year, {:.0}% for 3 years) apply to Autopilot Pod vCPU, memory and ephemeral storage. For the estimated baseline this saves ~{}/mo (1 year) or ~{}/mo (3 years). Commit only to the steady-state spend.",
                            (1.0 - f1) * 100.0,
                            (1.0 - f3) * 100.0,
                            ctx.money(s1),
                            ctx.money(s3)
                        ),
                        Some(s1),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones() {
        let none: Vec<String> = vec![];
        let locs = vec!["europe-west1-b".to_string(), "europe-west1-c".to_string()];
        assert_eq!(zone_count(Some("europe-west1"), &none, &none), Some((3, true)));
        assert_eq!(zone_count(Some("europe-west1"), &none, &locs), Some((2, true)));
        assert_eq!(zone_count(Some("europe-west1-b"), &none, &none), Some((1, false)));
        // Zonal cluster: node_locations lists additional zones.
        assert_eq!(zone_count(Some("europe-west1-d"), &none, &locs), Some((3, false)));
        assert_eq!(zone_count(Some("europe-west1-b"), &none, &locs), Some((2, false)));
        // Node pool node_locations is the complete list.
        assert_eq!(zone_count(Some("europe-west1"), &locs[..1], &locs), Some((1, true)));
        assert_eq!(zone_count(None, &none, &none), None);
    }

    #[test]
    fn mcpu_to_vcpu() {
        let c = Component {
            name: "x".into(),
            unit: "mCPU-hours".into(),
            quantity: Some(730_000.0),
            unit_price: Some(0.0000445),
            monthly_cost: Some(32.485),
            list_unit_price: None,
            list_monthly_cost: None,
            sku: None,
            sku_description: None,
            usage_based: true,
            note: None,
        };
        let v = per_vcpu(c);
        assert_eq!(v.quantity, Some(730.0));
        assert!((v.unit_price.unwrap() - 0.0445).abs() < 1e-12);
        assert_eq!(v.monthly_cost, Some(32.485));
    }

    #[test]
    fn autopilot_classes() {
        assert!(autopilot_skus("autopilot").is_some());
        assert!(autopilot_skus("balanced").is_some());
        assert!(autopilot_skus("scale-out").is_some());
        assert!(autopilot_skus("performance").is_none());
    }
}
