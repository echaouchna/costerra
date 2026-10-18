//! Compute Engine: instances, disks, IPs, images, snapshots, MIGs,
//! reservations and commitments.

use super::machine::{self, COMPUTE, MachineType, Pricing};
use super::{Ctx, Spec, usage};
use crate::cost::{CodeChange, Component, HOURS_PER_MONTH};
use crate::hcl::value::Value;
use crate::pricing::query::Query;

pub fn specs() -> Vec<Spec> {
    const HRS: &[super::UsageParam] = usage!("monthly_hrs" => "Hours per month the instance runs (default 730)");
    const MIG: &[super::UsageParam] = usage!(
        "instances" => "Average number of instances (overrides target_size / autoscaler min)",
        "monthly_hrs" => "Hours per month each instance runs (default 730)",
    );
    vec![
        Spec::new("google_compute_instance", instance).usage(HRS).rules(rules::vm),
        Spec::new("google_compute_instance_from_template", instance_from_template).usage(HRS).rules(rules::vm),
        Spec::new("google_compute_instance_group_manager", mig).usage(MIG).rules(rules::vm),
        Spec::new("google_compute_region_instance_group_manager", mig).usage(MIG).rules(rules::vm),
        Spec::new("google_compute_disk", disk).rules(rules::disk),
        Spec::new("google_compute_region_disk", disk).rules(rules::disk),
        Spec::new("google_compute_address", address).rules(rules::address),
        Spec::new("google_compute_global_address", address).rules(rules::address),
        Spec::new("google_compute_snapshot", snapshot)
            .usage(usage!("storage_gb" => "Total snapshot storage in GiB (defaults to source disk size)")),
        Spec::new("google_compute_image", image).usage(usage!("storage_gb" => "Image storage in GiB")),
        Spec::new("google_compute_machine_image", machine_image).usage(usage!("storage_gb" => "Machine image storage in GiB")),
        Spec::new("google_compute_reservation", reservation).rules(rules::reservation),
        Spec::new("google_compute_region_commitment", commitment),
    ]
}

/// Sustained use discount multiplier for a machine family running `hours`
/// in a month (applies automatically to N1, N2, N2D, C2, M1, M2).
pub fn sud_multiplier(family: &str, hours: f64) -> f64 {
    let steps: [f64; 4] = match family {
        "n1" | "m1" | "m2" => [1.0, 0.8, 0.6, 0.4],
        "n2" | "n2d" | "c2" => [1.0, 0.8678, 0.733, 0.6],
        _ => return 1.0,
    };
    let f = (hours / HOURS_PER_MONTH).clamp(0.0, 1.0);
    if f == 0.0 {
        return 1.0;
    }
    let mut paid = 0.0;
    for (i, m) in steps.iter().enumerate() {
        let lo = i as f64 * 0.25;
        let seg = (f - lo).clamp(0.0, 0.25);
        paid += seg * m;
    }
    paid / f
}

#[derive(Clone)]
pub struct VmSpec {
    pub machine: String,
    pub pricing: Pricing,
    pub count: f64,
    /// (disk type, size GiB, provisioned iops, provisioned throughput)
    pub disks: Vec<(String, f64, Option<f64>, Option<f64>)>,
    pub local_ssd_gb: f64,
    pub gpus: Vec<(String, f64)>,
    pub ephemeral_ips: f64,
    pub image: Option<String>,
}

fn pricing_label(p: Pricing) -> &'static str {
    match p {
        Pricing::OnDemand => "on-demand",
        Pricing::Spot => "spot",
        Pricing::Commit1Yr => "1y commit",
        Pricing::Commit3Yr => "3y commit",
    }
}

/// Default boot disk type depending on machine family (Hyperdisk-only families).
pub fn default_boot_disk(family: &str) -> &'static str {
    match family {
        "c4" | "c4a" | "c4d" | "n4" | "n4a" | "n4d" | "h4d" | "x4" | "m4" | "a4" | "c4n" | "z4d" => "hyperdisk-balanced",
        _ => "pd-standard",
    }
}

/// Hourly price of a machine (vCPU + RAM + extended memory), before SUD.
pub fn machine_hourly(ctx: &Ctx, region: &str, mt: &MachineType, pricing: Pricing) -> Option<f64> {
    if mt.name == "f1-micro" || mt.name == "g1-small" {
        let d =
            if mt.name == "f1-micro" { "Micro Instance with burstable CPU running in" } else { "Small Instance with 1 VCPU running in" };
        let pat = if pricing == Pricing::Spot { format!("^Spot Preemptible {d}") } else { format!("^{d}") };
        let usage = if pricing == Pricing::Spot { "Preemptible" } else { "OnDemand" };
        return ctx.price(Query::new(COMPUTE, region, &pat).usage(usage)).map(|p| p.unit_price());
    }
    let up = machine::unit_prices(ctx.pricer, region, mt, pricing);
    let (cpu, ram) = (up.cpu?, up.ram?);
    let mut total = cpu * machine::billed_vcpus(mt) + ram * mt.memory_gb;
    if mt.extended_memory_gb > 0.0 {
        let stem = match mt.family.as_str() {
            "n1" => "Custom Extended Instance Ram",
            "n2" => "N2 Custom Extended Instance Ram",
            "n2d" => "N2D AMD Custom Extended Ram",
            "n4" => "N4 Custom Extended Instance Ram",
            _ => "Custom Extended Instance Ram",
        };
        let pat = format!("^{}{stem} running in", if pricing == Pricing::Spot { "Spot Preemptible " } else { "" });
        let p = ctx.price(Query::new(COMPUTE, region, &pat).family("Compute"))?;
        total += p.unit_price() * mt.extended_memory_gb;
    }
    Some(total)
}

/// Monthly cost components of one or more identical VMs.
pub fn vm_components(ctx: &Ctx, region: &str, spec: &VmSpec) -> Vec<Component> {
    let mut out = vec![];
    let hours = ctx.monthly_hours();
    let Some(mt) = machine::parse(&spec.machine) else {
        ctx.warn(format!("unknown machine type '{}'", spec.machine));
        return out;
    };
    let hourly = machine_hourly(ctx, region, &mt, spec.pricing);
    let sud = if spec.pricing == Pricing::OnDemand { sud_multiplier(&mt.family, hours) } else { 1.0 };
    let mut name = format!("Instance usage ({}, {})", mt.name, pricing_label(spec.pricing));
    if spec.count != 1.0 {
        name = format!("{name} x{}", crate::hcl::value::fmt_number(spec.count));
    }
    let mut c = ctx.fixed(name, "hours", Some(hours * spec.count), hourly.map(|h| h * sud));
    if sud < 1.0 {
        c.note = Some(format!("includes {:.0}% sustained use discount", (1.0 - sud) * 100.0));
    }
    out.push(c);

    // Operating system licenses.
    if let Some(img) = &spec.image
        && let Some(l) = license_component(ctx, img, &mt, hours * spec.count)
    {
        out.push(l);
    }

    // GPUs (attached + bundled with the machine type).
    let mut gpus = spec.gpus.clone();
    gpus.extend(mt.accelerators.iter().cloned());
    for (t, n) in gpus {
        if n <= 0.0 {
            continue;
        }
        let spot = spec.pricing == Pricing::Spot;
        match machine::gpu_pattern(&t, spot) {
            Some(pat) => {
                let usage = if spot { "Preemptible" } else { "OnDemand" };
                out.push(ctx.hourly_unit(
                    format!("GPU ({t}) x{}", crate::hcl::value::fmt_number(n * spec.count)),
                    "GPU-hours",
                    n * spec.count,
                    Query::new(COMPUTE, region, &pat).group("GPU").usage(usage),
                ));
            }
            None => ctx.warn(format!("unknown accelerator type '{t}'")),
        }
    }

    // Local SSD (attached scratch disks + bundled with -lssd machine types).
    let lssd = spec.local_ssd_gb + mt.local_ssd_gb;
    if lssd > 0.0 {
        let fam_stem = match mt.family.as_str() {
            "c4" | "c4a" | "c4d" | "z3" | "h4d" => Some(format!("^{} Instance Local SSD running in", mt.family.to_uppercase())),
            _ => None,
        };
        let fam_q = fam_stem.as_deref().map(|p| Query::new(COMPUTE, region, p).ondemand());
        let spot_q = Query::new(COMPUTE, region, "^SSD backed Local Storage attached to Spot Preemptible VMs").usage("Preemptible");
        let generic_q = Query::new(COMPUTE, region, "^SSD backed Local Storage").ondemand().exclude("Spot|Sole Tenancy|DWS");
        let q = fam_q
            .filter(|q| ctx.price(*q).is_some())
            .or_else(|| (spec.pricing == Pricing::Spot && ctx.price(spot_q).is_some()).then_some(spot_q))
            .unwrap_or(generic_q);
        out.push(ctx.monthly(
            format!("Local SSD ({} GiB)", crate::hcl::value::fmt_number(lssd)),
            "GiB-months",
            lssd * spec.count * hours / HOURS_PER_MONTH,
            q,
        ));
    }

    for (i, (dtype, size, iops, tp)) in spec.disks.iter().enumerate() {
        let label = if i == 0 { "Boot disk" } else { "Disk" };
        out.extend(disk_components(ctx, region, label, dtype, *size, *iops, *tp, spec.count, false));
    }

    if spec.ephemeral_ips > 0.0 {
        let (pat, usage) = if spec.pricing == Pricing::Spot {
            ("^External IP Charge on a Spot Preemptible VM", "OnDemand")
        } else {
            ("^External IP Charge on a Standard VM", "OnDemand")
        };
        out.push(ctx.hourly_unit(
            "External IP address",
            "hours",
            spec.ephemeral_ips * spec.count,
            Query::new(COMPUTE, "global", pat).usage(usage),
        ));
    }
    out
}

fn license_component(ctx: &Ctx, image: &str, mt: &MachineType, hours: f64) -> Option<Component> {
    let img = image.to_lowercase();
    let vcpus = mt.vcpus;
    if img.contains("windows") {
        let year = ["2025", "2022", "2019", "2016"].into_iter().find(|y| img.contains(y)).unwrap_or("2022");
        let pat = format!("^Licensing Fee for Windows Server {year} Datacenter Edition \\(CPU cost\\)");
        let p = ctx.price(Query::new(COMPUTE, "global", &pat)).or_else(|| {
            ctx.price(Query::new(COMPUTE, "global", "^Licensing Fee for Windows Server 2019 Datacenter Edition \\(CPU cost\\)"))
        });
        return Some(ctx.fixed(
            format!("Windows Server {year} license ({} vCPU)", vcpus),
            "hours",
            Some(hours),
            p.map(|p| p.unit_price() * vcpus.max(2.0)),
        ));
    }
    if img.contains("rhel") {
        let ver = ["10", "9", "8", "7"].into_iter().find(|v| img.contains(&format!("rhel-{v}"))).unwrap_or("9");
        let bucket = if vcpus <= 8.0 {
            "up to 8 VCPU"
        } else if vcpus < 128.0 {
            "9 to 127 VCPU"
        } else {
            "128 VCPU or more"
        };
        let pat = format!("^Licensing Fee for Red Hat Enterprise Linux {ver} on VM with {bucket}");
        let p = ctx.price(Query::new(COMPUTE, "global", &pat));
        return Some(ctx.fixed(format!("RHEL {ver} license"), "hours", Some(hours), p.map(|p| p.unit_price())));
    }
    if img.contains("sles") {
        let sap = img.contains("sap");
        let bucket = if vcpus <= 3.0 {
            "up to 3 VCPU"
        } else if vcpus < 6.0 {
            "4 to 5 VCPU"
        } else {
            "6 or more VCPU"
        };
        let pat = if sap {
            format!("^Licensing Fee for SUSE Linux Enterprise Server 15 for SAP on VM with {bucket}")
        } else {
            format!("^Licensing Fee for SUSE Linux Enterprise Server 15 on VM with {bucket}")
        };
        let p = ctx.price(Query::new(COMPUTE, "global", &pat));
        return Some(ctx.fixed("SLES license", "hours", Some(hours), p.map(|p| p.unit_price())));
    }
    None
}

/// Persistent disk / Hyperdisk components.
#[allow(clippy::too_many_arguments)]
pub fn disk_components(
    ctx: &Ctx,
    region: &str,
    label: &str,
    dtype: &str,
    size: f64,
    iops: Option<f64>,
    throughput: Option<f64>,
    count: f64,
    regional: bool,
) -> Vec<Component> {
    let dtype = dtype.rsplit('/').next().unwrap_or(dtype);
    let fmt = |s: f64| crate::hcl::value::fmt_number(s);
    let cap_name = format!("{label} ({}{dtype}, {} GiB)", if regional { "regional " } else { "" }, fmt(size));
    let mut out = vec![];
    let q = |pat: &'static str| Query::new(COMPUTE, region, pat).ondemand();
    let (std_pat, bal_pat, ssd_pat) = if regional {
        ("^Regional Storage PD Capacity", "^Regional Balanced PD Capacity", "^Regional SSD backed PD Capacity")
    } else {
        ("^Storage PD Capacity", "^Balanced PD Capacity", "^SSD backed PD Capacity")
    };
    match dtype {
        "pd-standard" => out.push(ctx.monthly(cap_name, "GiB-months", size * count, q(std_pat))),
        "pd-balanced" => out.push(ctx.monthly(cap_name, "GiB-months", size * count, q(bal_pat))),
        "pd-ssd" => out.push(ctx.monthly(cap_name, "GiB-months", size * count, q(ssd_pat))),
        "pd-extreme" => {
            out.push(ctx.monthly(cap_name, "GiB-months", size * count, q("^Extreme PD Capacity")));
            let i = iops.unwrap_or(2500.0);
            out.push(ctx.monthly(format!("{label} provisioned IOPS ({})", fmt(i)), "IOPS-months", i * count, q("^Extreme PD IOPS")));
        }
        "hyperdisk-balanced" | "hyperdisk-balanced-high-availability" => {
            let ha = dtype.ends_with("availability");
            let (cp, ip, tp_pat) = if ha {
                (
                    "^Hyperdisk Balanced High Availability Capacity in",
                    "^Hyperdisk Balanced High Availability Iops in",
                    "^Hyperdisk Balanced High Availability Throughput in",
                )
            } else {
                ("^Hyperdisk Balanced Capacity in", "^Hyperdisk Balanced IOPS in", "^Hyperdisk Balanced Throughput in")
            };
            out.push(ctx.monthly(cap_name, "GiB-months", size * count, q(cp)));
            // 3,000 IOPS and 140 MiB/s are included per disk.
            let i = iops.unwrap_or(3000.0);
            let t = throughput.unwrap_or(140.0);
            if i > 3000.0 {
                out.push(ctx.monthly(
                    format!("{label} provisioned IOPS (above 3,000 baseline)"),
                    "IOPS-months",
                    (i - 3000.0) * count,
                    q(ip),
                ));
            }
            if t > 140.0 {
                out.push(ctx.monthly(
                    format!("{label} provisioned throughput (above 140 MiB/s baseline)"),
                    "MiBps-months",
                    (t - 140.0) * count,
                    q(tp_pat),
                ));
            }
        }
        "hyperdisk-extreme" => {
            out.push(ctx.monthly(cap_name, "GiB-months", size * count, q("^Hyperdisk Extreme Capacity in")));
            let i = iops.unwrap_or_else(|| (size * 2.0).clamp(2500.0, 350_000.0));
            out.push(ctx.monthly(
                format!("{label} provisioned IOPS ({})", fmt(i)),
                "IOPS-months",
                i * count,
                q("^Hyperdisk Extreme IOPS in"),
            ));
        }
        "hyperdisk-throughput" => {
            out.push(ctx.monthly(cap_name, "GiB-months", size * count, q("^Hyperdisk Throughput Capacity in")));
            let t = throughput.unwrap_or_else(|| (size / 1024.0 * 90.0).max(20.0));
            out.push(ctx.monthly(
                format!("{label} provisioned throughput ({} MiB/s)", fmt(t)),
                "MiBps-months",
                t * count,
                q("^Hyperdisk Throughput Throughput in"),
            ));
        }
        "hyperdisk-ml" => {
            out.push(ctx.monthly(cap_name, "GiB-months", size * count, q("^Hyperdisk ML Capacity in")));
            let t = throughput.unwrap_or_else(|| (size * 0.12).max(400.0));
            out.push(ctx.monthly(
                format!("{label} provisioned throughput ({} MiB/s)", fmt(t)),
                "MiBps-months",
                t * count,
                q("^Hyperdisk ML Throughput in"),
            ));
        }
        "local-ssd" => out.push(ctx.monthly(cap_name, "GiB-months", size * count, q("^SSD backed Local Storage"))),
        other => ctx.warn(format!("unknown disk type '{other}'")),
    }
    out
}

fn pricing_of(sched: Option<&Value>) -> Pricing {
    let Some(s) = sched else { return Pricing::OnDemand };
    let spot = s.get("provisioning_model").and_then(|v| v.as_str()) == Some("SPOT")
        || s.get("preemptible").and_then(|v| v.as_bool()) == Some(true);
    if spot { Pricing::Spot } else { Pricing::OnDemand }
}

fn gpus_of(v: &[&Value]) -> Vec<(String, f64)> {
    v.iter()
        .filter_map(|g| {
            let t = g.get("type")?.as_str()?.to_string();
            let n = g.get("count").and_then(|c| c.as_f64()).unwrap_or(1.0);
            Some((t, n))
        })
        .collect()
}

fn count_ephemeral_ips(nics: &[&Value]) -> f64 {
    nics.iter()
        .flat_map(|n| n.get("access_config").and_then(|a| a.as_list()).cloned().unwrap_or_default())
        .filter(|ac| {
            // A nat_ip reference means a static address (priced on the address).
            !matches!(ac.get("nat_ip"), Some(Value::String(s)) if !s.is_empty()) && !matches!(ac.get("nat_ip"), Some(Value::Unknown))
        })
        .count() as f64
}

/// Build a VmSpec from google_compute_instance attributes.
fn instance_spec(ctx: &Ctx, attrs: &Value) -> Option<VmSpec> {
    let machine = match attrs.get("machine_type") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Unknown) => {
            ctx.warn("machine_type is not known statically");
            return None;
        }
        _ => {
            ctx.warn("machine_type not set");
            return None;
        }
    };
    let family = machine::family_of(&machine);
    let mut disks = vec![];
    let boot = attrs.get("boot_disk").and_then(|b| b.as_list()).and_then(|l| l.first());
    let mut image = None;
    if let Some(b) = boot {
        let has_source = matches!(b.get("source"), Some(Value::String(_)));
        if !has_source {
            let ip = b.get("initialize_params").and_then(|l| l.as_list()).and_then(|l| l.first());
            let size = ip.and_then(|p| p.get("size")).and_then(|v| v.as_f64()).unwrap_or(10.0);
            let dtype = ip.and_then(|p| p.get("type")).and_then(|v| v.as_str()).unwrap_or(default_boot_disk(&family)).to_string();
            let iops = ip.and_then(|p| p.get("provisioned_iops")).and_then(|v| v.as_f64());
            let tp = ip.and_then(|p| p.get("provisioned_throughput")).and_then(|v| v.as_f64());
            image = ip.and_then(|p| p.get("image")).and_then(|v| v.as_str()).map(str::to_string);
            disks.push((dtype, size, iops, tp));
        }
    }
    let scratch: f64 = attrs
        .get("scratch_disk")
        .and_then(|l| l.as_list())
        .map(|l| l.iter().map(|d| d.get("size").and_then(|s| s.as_f64()).unwrap_or(375.0)).sum())
        .unwrap_or(0.0);
    let gpus = gpus_of(&attrs.get("guest_accelerator").and_then(|g| g.as_list()).map(|l| l.iter().collect::<Vec<_>>()).unwrap_or_default());
    let nics: Vec<&Value> = attrs.get("network_interface").and_then(|n| n.as_list()).map(|l| l.iter().collect()).unwrap_or_default();
    let sched = attrs.get("scheduling").and_then(|s| s.as_list()).and_then(|l| l.first());
    Some(VmSpec {
        machine,
        pricing: pricing_of(sched),
        count: 1.0,
        disks,
        local_ssd_gb: scratch,
        gpus,
        ephemeral_ips: count_ephemeral_ips(&nics),
        image,
    })
}

/// Build a VmSpec from instance template attributes.
pub fn template_spec(ctx: &Ctx, attrs: &Value) -> Option<VmSpec> {
    let machine = match attrs.get("machine_type") {
        Some(Value::String(s)) => s.clone(),
        _ => {
            ctx.warn("instance template machine_type is not known statically");
            return None;
        }
    };
    let family = machine::family_of(&machine);
    let mut disks = vec![];
    let mut local = 0.0;
    let mut image = None;
    for d in attrs.get("disk").and_then(|l| l.as_list()).into_iter().flatten() {
        let t = d.get("type").and_then(|v| v.as_str()).unwrap_or("PERSISTENT");
        let dtype = d.get("disk_type").and_then(|v| v.as_str()).unwrap_or(default_boot_disk(&family)).to_string();
        let size = d.get("disk_size_gb").and_then(|v| v.as_f64()).unwrap_or(if t == "SCRATCH" { 375.0 } else { 10.0 });
        if t == "SCRATCH" || dtype == "local-ssd" {
            local += size;
            continue;
        }
        if matches!(d.get("source"), Some(Value::String(_))) {
            continue;
        }
        let boot = d.get("boot").and_then(|v| v.as_bool()).unwrap_or(disks.is_empty());
        if boot && image.is_none() {
            image = d.get("source_image").and_then(|v| v.as_str()).map(str::to_string);
        }
        let iops = d.get("provisioned_iops").and_then(|v| v.as_f64());
        let tp = d.get("provisioned_throughput").and_then(|v| v.as_f64());
        if boot {
            disks.insert(0, (dtype, size, iops, tp));
        } else {
            disks.push((dtype, size, iops, tp));
        }
    }
    let gpus = gpus_of(&attrs.get("guest_accelerator").and_then(|g| g.as_list()).map(|l| l.iter().collect::<Vec<_>>()).unwrap_or_default());
    let nics: Vec<&Value> = attrs.get("network_interface").and_then(|n| n.as_list()).map(|l| l.iter().collect()).unwrap_or_default();
    let sched = attrs.get("scheduling").and_then(|s| s.as_list()).and_then(|l| l.first());
    Some(VmSpec {
        machine,
        pricing: pricing_of(sched),
        count: 1.0,
        disks,
        local_ssd_gb: local,
        gpus,
        ephemeral_ips: count_ephemeral_ips(&nics),
        image,
    })
}

fn instance(ctx: &Ctx) -> Vec<Component> {
    let Some(spec) = instance_spec(ctx, ctx.attrs) else { return vec![] };
    let region = ctx.region();
    vm_components(ctx, &region, &spec)
}

fn instance_from_template(ctx: &Ctx) -> Vec<Component> {
    let mut attrs = ctx.attrs.as_map().cloned().unwrap_or_default();
    if let Some(t) = ctx.str("source_instance_template").and_then(|t| ctx.resolve_ref(t, "google_compute"))
        && let Some(tm) = t.attrs.as_map()
    {
        for (k, v) in tm {
            if !matches!(attrs.get(k), Some(v2) if !v2.is_null() && !v2.is_unknown()) {
                attrs.insert(k.clone(), v.clone());
            }
        }
    }
    let v = Value::Map(attrs);
    let spec = if v.get("boot_disk").is_some() { instance_spec(ctx, &v) } else { template_spec(ctx, &v) };
    let Some(spec) = spec else { return vec![] };
    vm_components(ctx, &ctx.region(), &spec)
}

/// Find the autoscaler targeting a MIG, returning (min, max) replicas.
pub fn autoscaler_for(ctx: &Ctx) -> Option<(f64, f64)> {
    let short = format!("{}.{}", ctx.r.rtype, ctx.r.name);
    let a = ctx.siblings.iter().find(|s| {
        (s.rtype == "google_compute_autoscaler" || s.rtype == "google_compute_region_autoscaler")
            && (s.refs.contains(&short)
                || s.attrs
                    .get("target")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.ends_with(&ctx.r.address) || ctx.attrs.get("self_link").and_then(|x| x.as_str()) == Some(t)))
    })?;
    let p = a.attrs.path("autoscaling_policy.0")?;
    let min = p.get("min_replicas").and_then(|v| v.as_f64()).unwrap_or(1.0);
    let max = p.get("max_replicas").and_then(|v| v.as_f64()).unwrap_or(min);
    Some((min, max))
}

fn mig(ctx: &Ctx) -> Vec<Component> {
    let tpl_ref = ctx
        .blocks("version")
        .iter()
        .find_map(|v| v.get("instance_template").and_then(|t| t.as_str()))
        .or_else(|| ctx.str("instance_template"));
    let Some(tpl) = tpl_ref.and_then(|t| ctx.resolve_ref(t, "google_compute")) else {
        ctx.warn("instance template could not be resolved");
        return vec![];
    };
    let count = if let Some(n) = ctx.usage_num("instances") {
        n
    } else if let Some((min, max)) = autoscaler_for(ctx) {
        if max > min {
            ctx.warn(format!("autoscaled between {min} and {max} instances; estimating with min (set usage 'instances' to override)"));
        }
        min
    } else if let Some(n) = ctx.num("target_size") {
        n
    } else {
        ctx.warn("target_size unknown, assuming 1 instance");
        1.0
    };
    let Some(mut spec) = template_spec(ctx, &tpl.attrs) else { return vec![] };
    spec.count = count;
    if count == 0.0 {
        return vec![];
    }
    let region = ctx.region();
    vm_components(ctx, &region, &spec)
}

fn disk(ctx: &Ctx) -> Vec<Component> {
    let regional = ctx.r.rtype == "google_compute_region_disk";
    let dtype = ctx.str_or("type", "pd-standard");
    let size = match ctx.num("size") {
        Some(s) => s,
        None => {
            if ctx.get("image").is_some() || ctx.get("snapshot").is_some() {
                ctx.warn("disk size not set, assuming 10 GiB (image/snapshot size)");
            }
            if dtype.starts_with("hyperdisk") { 100.0 } else { 10.0 }
        }
    };
    let region = ctx.region();
    disk_components(ctx, &region, "Storage", dtype, size, ctx.num("provisioned_iops"), ctx.num("provisioned_throughput"), 1.0, regional)
}

/// Other resources of the project that reference `self`.
pub fn referencing<'a>(ctx: &Ctx<'a>) -> Vec<&'a crate::tf::evaluate::ResourceInstance> {
    let short = format!("{}.{}", ctx.r.rtype, ctx.r.name);
    let ip = ctx.str("address");
    ctx.siblings
        .iter()
        .filter(|s| s.address != ctx.r.address && (s.refs.contains(&short) || ip.is_some_and(|ip| s.attrs.contains_string(ip))))
        .collect()
}

/// True if any other resource in the project references `self`.
pub fn is_referenced(ctx: &Ctx) -> bool {
    !referencing(ctx).is_empty()
}

fn address(ctx: &Ctx) -> Vec<Component> {
    let internal = ctx.str("address_type").is_some_and(|t| t == "INTERNAL")
        || ctx.str("purpose").is_some_and(|p| {
            matches!(p, "VPC_PEERING" | "PRIVATE_SERVICE_CONNECT" | "GCE_ENDPOINT" | "SHARED_LOADBALANCER_VIP" | "IPSEC_INTERCONNECT")
        });
    if internal {
        return vec![];
    }
    let region = if ctx.r.rtype == "google_compute_global_address" { "global".to_string() } else { ctx.region() };
    let users = referencing(ctx);
    // Addresses attached to forwarding rules or Classic VPN gateways are not charged.
    let lb_only = !users.is_empty()
        && users.iter().all(|u| {
            matches!(
                u.rtype.as_str(),
                "google_compute_forwarding_rule" | "google_compute_global_forwarding_rule" | "google_compute_vpn_gateway"
            )
        });
    if lb_only {
        let mut c = ctx.fixed("IP address (forwarding rule)", "hours", Some(ctx.monthly_hours()), Some(0.0));
        c.note = Some("no charge while attached to a forwarding rule".into());
        vec![c]
    } else if !users.is_empty() {
        vec![ctx.hourly("IP address (in use)", 1.0, Query::new(COMPUTE, "global", "^External IP Charge on a Standard VM"))]
    } else {
        let q = if region == "global" {
            Query::new(COMPUTE, "us-central1", "^Static Ip Charge")
        } else {
            Query::new(COMPUTE, &region, "^Static Ip Charge")
        };
        let mut c = ctx.hourly("IP address (unused)", 1.0, q);
        c.note = Some("not referenced by any resource in this project".into());
        vec![c]
    }
}

fn snapshot(ctx: &Ctx) -> Vec<Component> {
    let size = ctx.usage_num("storage_gb").or_else(|| {
        let src = ctx.str("source_disk")?;
        let d = ctx.resolve_ref(src, "google_compute")?;
        d.attrs.get("size").and_then(|s| s.as_f64())
    });
    let loc = ctx
        .get("storage_locations")
        .and_then(|l| l.as_list())
        .and_then(|l| l.first())
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| ctx.region());
    vec![ctx.usage_qty("Snapshot storage", "GiB-months", size, Query::new(COMPUTE, &loc, "^Storage PD Snapshot").ondemand())]
}

fn image(ctx: &Ctx) -> Vec<Component> {
    let size = ctx.usage_num("storage_gb").or_else(|| ctx.num("disk_size_gb"));
    let loc = ctx
        .get("storage_locations")
        .and_then(|l| l.as_list())
        .and_then(|l| l.first())
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| ctx.region());
    vec![ctx.usage_qty("Image storage", "GiB-months", size, Query::new(COMPUTE, &loc, "^Storage Image").ondemand())]
}

fn machine_image(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    vec![ctx.usage("Machine image storage", "GiB-months", "storage_gb", Query::new(COMPUTE, &region, "^Storage Machine Image").ondemand())]
}

fn reservation(ctx: &Ctx) -> Vec<Component> {
    let Some(sr) = ctx.block("specific_reservation") else { return vec![] };
    let count = sr.get("count").and_then(|c| c.as_f64()).unwrap_or(1.0);
    let Some(ip) = sr.path("instance_properties.0") else { return vec![] };
    let Some(machine) = ip.get("machine_type").and_then(|m| m.as_str()) else {
        ctx.warn("reservation machine_type unknown");
        return vec![];
    };
    let gpus = gpus_of(&ip.get("guest_accelerators").and_then(|g| g.as_list()).map(|l| l.iter().collect::<Vec<_>>()).unwrap_or_default());
    let lssd: f64 = ip
        .get("local_ssds")
        .and_then(|l| l.as_list())
        .map(|l| l.iter().map(|d| d.get("disk_size_gb").and_then(|s| s.as_f64()).unwrap_or(375.0)).sum())
        .unwrap_or(0.0);
    let spec = VmSpec {
        machine: machine.to_string(),
        pricing: Pricing::OnDemand,
        count,
        disks: vec![],
        local_ssd_gb: lssd,
        gpus,
        ephemeral_ips: 0.0,
        image: None,
    };
    let mut comps = vm_components(ctx, &ctx.region(), &spec);
    if let Some(c) = comps.first_mut() {
        c.note = Some("reserved capacity is billed whether or not VMs consume it".into());
    }
    comps
}

fn commitment(ctx: &Ctx) -> Vec<Component> {
    let years = match ctx.str_or("plan", "TWELVE_MONTH") {
        "THIRTY_SIX_MONTH" => "3",
        _ => "1",
    };
    let ctype = ctx.str_or("type", "GENERAL_PURPOSE");
    let family = match ctype {
        "GENERAL_PURPOSE" => "n1",
        "GENERAL_PURPOSE_N2" => "n2",
        "GENERAL_PURPOSE_N2D" => "n2d",
        "GENERAL_PURPOSE_E2" => "e2",
        "GENERAL_PURPOSE_T2D" => "t2d",
        "GENERAL_PURPOSE_N4" => "n4",
        "GENERAL_PURPOSE_C3" | "COMPUTE_OPTIMIZED_C3" => "c3",
        "COMPUTE_OPTIMIZED_C3D" => "c3d",
        "COMPUTE_OPTIMIZED" => "c2",
        "COMPUTE_OPTIMIZED_C2D" => "c2d",
        "COMPUTE_OPTIMIZED_H3" => "h3",
        "GENERAL_PURPOSE_C4" | "COMPUTE_OPTIMIZED_C4" => "c4",
        "MEMORY_OPTIMIZED" => "m1",
        "MEMORY_OPTIMIZED_M3" => "m3",
        "ACCELERATOR_OPTIMIZED" => "a2",
        "ACCELERATOR_OPTIMIZED_A3" => "a3",
        "GRAPHICS_OPTIMIZED" => "g2",
        other => {
            ctx.warn(format!("unsupported commitment type {other}"));
            return vec![];
        }
    };
    let pricing = if years == "3" { Pricing::Commit3Yr } else { Pricing::Commit1Yr };
    let mt = MachineType {
        name: format!("{family}-standard-1"),
        family: family.to_string(),
        vcpus: 1.0,
        memory_gb: 1.0,
        shared_core: false,
        custom: false,
        extended_memory_gb: 0.0,
        accelerators: vec![],
        local_ssd_gb: 0.0,
    };
    let region = ctx.region();
    let Some((cpu_re, ram_re)) = machine::sku_patterns(&mt, pricing) else { return vec![] };
    let usage = if years == "3" { "Commit3Yr" } else { "Commit1Yr" };
    let mut out = vec![];
    for r in ctx.blocks("resources") {
        let amount = r.get("amount").and_then(|a| a.as_f64()).unwrap_or(0.0);
        match r.get("type").and_then(|t| t.as_str()) {
            Some("VCPU") => out.push(ctx.hourly_unit(
                format!("Committed vCPUs ({family}, {years}y)"),
                "vCPU-hours",
                amount,
                Query::new(COMPUTE, &region, &cpu_re).usage(usage),
            )),
            Some("MEMORY") => out.push(ctx.hourly_unit(
                format!("Committed memory ({family}, {years}y)"),
                "GiB-hours",
                amount / 1024.0,
                Query::new(COMPUTE, &region, &ram_re).usage(usage),
            )),
            Some("LOCAL_SSD") => out.push(ctx.monthly(
                format!("Committed local SSD ({years}y)"),
                "GiB-months",
                amount,
                Query::new(COMPUTE, &region, "^Commitment v1: Local SSD").usage(usage),
            )),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sud() {
        assert!((sud_multiplier("n1", 730.0) - 0.7).abs() < 1e-6);
        assert!((sud_multiplier("n2", 730.0) - 0.8002).abs() < 1e-3);
        assert_eq!(sud_multiplier("e2", 730.0), 1.0);
        assert_eq!(sud_multiplier("n1", 100.0), 1.0);
    }
}

pub(crate) mod rules {
    use super::*;
    use crate::cost::{CostedResource, Recommendation, Severity};
    use crate::resources::Env;

    /// The VM shape and count behind an instance / MIG resource.
    pub fn vm_spec_for(ctx: &Ctx) -> Option<VmSpec> {
        match ctx.r.rtype.as_str() {
            "google_compute_instance" => instance_spec(ctx, ctx.attrs),
            "google_compute_instance_group_manager" | "google_compute_region_instance_group_manager" => {
                let tpl_ref = ctx
                    .blocks("version")
                    .iter()
                    .find_map(|v| v.get("instance_template").and_then(|t| t.as_str()))
                    .or_else(|| ctx.str("instance_template"))?;
                let tpl = ctx.resolve_ref(tpl_ref, "google_compute")?;
                let mut s = template_spec(ctx, &tpl.attrs)?;
                s.count = ctx
                    .usage_num("instances")
                    .or_else(|| autoscaler_for(ctx).map(|a| a.0))
                    .or_else(|| ctx.num("target_size"))
                    .unwrap_or(1.0);
                Some(s)
            }
            _ => None,
        }
    }

    pub fn total(ctx: &Ctx, spec: &VmSpec) -> Option<f64> {
        let comps = vm_components(ctx, &ctx.region(), spec);
        if comps.is_empty() || comps.iter().any(|c| c.is_unpriced()) {
            return None;
        }
        Some(crate::cost::CostedResource::total(&comps))
    }

    pub fn vm(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let Some(spec) = vm_spec_for(ctx) else { return out };
        let Some(current) = total(ctx, &spec) else { return out };
        let Some(mt) = machine::parse(&spec.machine) else { return out };
        let hours = ctx.monthly_hours();

        // Observed CPU use (Cloud Monitoring), for single VMs.
        let cpu = if ctx.r.rtype == "google_compute_instance" { ctx.observed_cpu().filter(|c| c.days >= 7.0) } else { None };
        let idle = cpu.is_some_and(|c| c.p95 < 0.03);
        let busy = cpu.is_some_and(|c| c.p95 >= 0.85);

        // Idle VM: no meaningful CPU use over the observation window.
        if let Some(c) = cpu.filter(|_| idle) {
            out.push(ctx.rec(
                "compute-idle",
                Severity::Medium,
                "This VM looks idle",
                format!(
                    "CPU use stayed below 3% for {:.0} days (mean {:.1}%, 95th percentile {:.1}%). Stop or delete it if it is not needed; a stopped VM only pays for its disks and IPs.",
                    c.days,
                    c.mean * 100.0,
                    c.p95 * 100.0
                ),
                Some(cr.monthly_cost),
            ));
            // Other advice does not matter for a VM that is not used.
            return out;
        }

        // 1. Spot VMs for non-production.
        if ctx.env == Env::NonProd && spec.pricing == Pricing::OnDemand && mt.accelerators.is_empty() {
            let spot = VmSpec {
                pricing: Pricing::Spot,
                disks: spec.disks.clone(),
                gpus: spec.gpus.clone(),
                image: spec.image.clone(),
                machine: spec.machine.clone(),
                ..spec.clone()
            };
            if let Some(s) = ctx.savings_vs(current, total(ctx, &spot)) {
                out.push(ctx.rec(
                    "compute-spot-nonprod",
                    Severity::Medium,
                    format!("Use Spot VMs for this non-production {}", if spec.count > 1.0 { "instance group" } else { "instance" }),
                    "Spot VMs are 60-91% cheaper than on-demand. They can be preempted, which is usually acceptable for dev/test, CI and fault-tolerant workloads. Set scheduling { provisioning_model = \"SPOT\" }.",
                    Some(s),
                ));
            }
        }

        // Rightsize: halve the vCPUs of a VM that uses little of its CPU.
        if let Some(c) = cpu.filter(|c| !idle && c.p95 < 0.35 && c.mean < 0.2)
            && !mt.custom
            && mt.vcpus >= 4.0
            && let Some((prefix, _)) = mt.name.rsplit_once('-')
        {
            let cand = format!("{prefix}-{}", (mt.vcpus / 2.0) as u32);
            if machine::parse(&cand).is_some_and(|m| m.family == mt.family) {
                let alt = VmSpec {
                    machine: cand.clone(),
                    disks: spec.disks.clone(),
                    gpus: spec.gpus.clone(),
                    image: spec.image.clone(),
                    ..spec.clone()
                };
                if let Some(s) = ctx.savings_vs(current, total(ctx, &alt)) {
                    let mut rec = ctx.rec(
                        "compute-rightsize",
                        if s > 100.0 { Severity::Medium } else { Severity::Low },
                        format!("Downsize {} to {cand}", mt.name),
                        format!(
                            "CPU use over {:.0} days: mean {:.1}%, 95th percentile {:.1}% (suggested below a mean of 20% and a 95th percentile of 35%). Half the vCPUs would still leave headroom. Check memory use first (it needs the Ops Agent to be measured).",
                            c.days,
                            c.mean * 100.0,
                            c.p95 * 100.0
                        ),
                        Some(s),
                    );
                    rec.change =
                        Some(CodeChange { attribute: "machine_type".into(), from: mt.name.clone(), to: cand.clone(), unset: false });
                    out.push(rec);
                }
            }
        }

        // Upsize: double the vCPUs of a VM whose CPU is saturated.
        if let Some(c) = cpu.filter(|c| busy && c.days >= 7.0)
            && !mt.custom
            && let Some((prefix, _)) = mt.name.rsplit_once('-')
        {
            let cand = format!("{prefix}-{}", (mt.vcpus * 2.0) as u32);
            if machine::parse(&cand).is_some_and(|m| m.family == mt.family) {
                let alt = VmSpec {
                    machine: cand.clone(),
                    disks: spec.disks.clone(),
                    gpus: spec.gpus.clone(),
                    image: spec.image.clone(),
                    ..spec.clone()
                };
                let extra = total(ctx, &alt).map(|n| n - current);
                let mut rec = ctx.rec(
                    "compute-upsize",
                    Severity::Medium,
                    format!("Upsize {} to {cand}: its CPU is saturated", mt.name),
                    format!(
                        "CPU use over {:.0} days: mean {:.1}%, 95th percentile {:.1}% (suggested from a 95th percentile of 85%). The VM is short of CPU: twice the vCPUs cost more but remove the bottleneck (or spread the load over more instances).",
                        c.days,
                        c.mean * 100.0,
                        c.p95 * 100.0
                    ),
                    extra.map(|e| -e),
                );
                rec.change = Some(CodeChange { attribute: "machine_type".into(), from: mt.name.clone(), to: cand, unset: false });
                out.push(rec);
            }
        }

        // Always-on non-production VM: stop it outside working hours.
        if ctx.env == Env::NonProd && !idle && ctx.observed_keys.iter().any(|k| k == "monthly_hrs") && hours >= HOURS_PER_MONTH * 0.9 {
            // 10 hours a day, 5 days a week.
            let working = 50.0 * 52.0 / 12.0;
            let machine_cost: f64 =
                cr.components.iter().filter(|c| c.name.starts_with("Instance usage")).filter_map(|c| c.monthly_cost).sum();
            let s = machine_cost * (1.0 - working / hours);
            if s > 5.0 {
                out.push(ctx.rec(
                    "compute-schedule-nonprod",
                    Severity::Low,
                    "Stop this non-production VM outside working hours",
                    format!(
                        "It ran ~{hours:.0} hours a month over the last 30 days. An instance schedule (resource policy) that runs it 10 hours a day on weekdays pays for ~{working:.0} hours; disks are still billed."
                    ),
                    Some(s),
                ));
            }
        }

        // 2. Cheaper machine family with the same shape (not for VMs whose CPU is saturated).
        if !busy && !mt.custom && !mt.shared_core && mt.accelerators.is_empty() && spec.gpus.is_empty() && spec.pricing != Pricing::Spot {
            let parts: Vec<&str> = mt.name.splitn(2, '-').collect();
            if parts.len() == 2 {
                let rest = parts[1];
                let mut best: Option<(String, f64)> = None;
                for fam in ["e2", "n2d", "t2d", "n4", "n4d", "n2", "c3d"] {
                    if fam == mt.family {
                        continue;
                    }
                    let cand = format!("{fam}-{rest}");
                    let Some(cm) = machine::parse(&cand) else { continue };
                    if cm.vcpus != mt.vcpus || cm.memory_gb + 0.01 < mt.memory_gb {
                        continue;
                    }
                    let mut disks = spec.disks.clone();
                    // Hyperdisk-only families need a hyperdisk boot disk.
                    if default_boot_disk(fam) == "hyperdisk-balanced" {
                        for d in disks.iter_mut() {
                            if d.0.starts_with("pd-") {
                                d.0 = "hyperdisk-balanced".into();
                            }
                        }
                    }
                    let alt = VmSpec { machine: cand.clone(), disks, gpus: spec.gpus.clone(), image: spec.image.clone(), ..spec.clone() };
                    if let Some(t) = total(ctx, &alt)
                        && best.as_ref().is_none_or(|b| t < b.1)
                    {
                        best = Some((cand, t));
                    }
                }
                if let Some((cand, t)) = best {
                    let s = current - t;
                    if s > 5.0 && s / current > 0.08 {
                        let sev = if s > 100.0 { Severity::Medium } else { Severity::Low };
                        out.push(ctx.rec(
                            "compute-machine-family",
                            sev,
                            format!("Switch {} to {cand}", mt.name),
                            format!(
                                "{cand} has the same vCPU count and at least as much memory for {}% less. Validate CPU platform and performance requirements before switching.",
                                ((s / current) * 100.0).round()
                            ),
                            Some(s),
                        ));
                    }
                }
            }
        }

        // 3. Committed use discounts for always-on production VMs.
        if ctx.env != Env::NonProd && spec.pricing == Pricing::OnDemand && hours >= HOURS_PER_MONTH * 0.9 {
            let region = ctx.region();
            let od = machine_hourly(ctx, &region, &mt, Pricing::OnDemand).map(|h| h * sud_multiplier(&mt.family, hours));
            let c1 = machine_hourly(ctx, &region, &mt, Pricing::Commit1Yr);
            let c3 = machine_hourly(ctx, &region, &mt, Pricing::Commit3Yr);
            // Share of the vCPUs already covered by active commitments.
            let covered = if ctx.r.rtype == "google_compute_instance" { ctx.committed_fraction().unwrap_or(0.0) } else { 0.0 };
            if let (Some(od), Some(c1), Some(c3)) = (od, c1, c3)
                && covered < 0.99
            {
                let s1 = (od - c1) * hours * spec.count * (1.0 - covered);
                let s3 = (od - c3) * hours * spec.count * (1.0 - covered);
                if s3 > 5.0 {
                    let already = if covered > 0.0 {
                        format!(" Active commitments already cover {:.0}% of its vCPUs; the savings are for the rest.", covered * 100.0)
                    } else {
                        String::new()
                    };
                    out.push(ctx.rec(
                        "compute-cud",
                        Severity::Info,
                        "Cover this always-on VM with a committed use discount",
                        format!(
                            "A resource-based CUD for {} vCPU / {} GiB would save ~{}/mo (1 year) or ~{}/mo (3 years). CUDs apply across the project/billing account, so commit to your steady-state baseline.{already}",
                            machine::billed_vcpus(&mt) * spec.count * (1.0 - covered),
                            mt.memory_gb * spec.count * (1.0 - covered),
                            ctx.money(s1),
                            ctx.money(s3)
                        ),
                        Some(s1),
                    ));
                }
            }
        }

        // 4. SSD persistent disks -> balanced.
        if spec.disks.iter().any(|d| d.0 == "pd-ssd") {
            let mut disks = spec.disks.clone();
            for d in disks.iter_mut() {
                if d.0 == "pd-ssd" {
                    d.0 = "pd-balanced".into();
                }
            }
            let alt = VmSpec { disks, gpus: spec.gpus.clone(), image: spec.image.clone(), machine: spec.machine.clone(), ..spec.clone() };
            if let Some(s) = ctx.savings_vs(current, total(ctx, &alt)) {
                out.push(ctx.rec(
                    "disk-ssd-to-balanced",
                    Severity::Low,
                    "Use pd-balanced instead of pd-ssd",
                    "pd-balanced provides SSD-backed storage with lower IOPS/GiB at ~40% lower cost. It suits most boot disks and general workloads.",
                    Some(s),
                ));
            }
        }
        out
    }

    pub fn disk(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        if cr.monthly_cost <= 0.0 {
            return out;
        }
        if !is_referenced(ctx) {
            out.push(ctx.rec(
                "disk-unattached",
                Severity::Medium,
                "Disk is not attached to any instance in this project",
                "Unattached persistent disks are billed for their full provisioned size. Delete the disk or snapshot it (snapshots are cheaper) if it is no longer needed. It may be attached from outside this configuration.",
                Some(cr.monthly_cost),
            ));
        }
        if ctx.str("type") == Some("pd-ssd") {
            let new = ctx.reprice(|a| {
                a.insert("type".into(), Value::from("pd-balanced"));
            });
            if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                out.push(ctx.rec(
                    "disk-ssd-to-balanced",
                    Severity::Low,
                    "Use pd-balanced instead of pd-ssd",
                    "pd-balanced costs ~40% less than pd-ssd and is sufficient unless you need more than 6 IOPS/GiB.",
                    Some(s),
                ));
            }
        }
        out
    }

    pub fn address(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        if cr.monthly_cost > 0.0 && cr.components.iter().any(|c| c.name.contains("unused")) {
            return vec![ctx.rec(
                "ip-unused",
                Severity::Medium,
                "Static IP address is not used by any resource in this project",
                "Reserved external IPs that are not attached are billed at the higher idle rate. Release it if it is not needed.",
                Some(cr.monthly_cost),
            )];
        }
        vec![]
    }

    pub fn reservation(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        if cr.monthly_cost <= 0.0 {
            return vec![];
        }
        vec![ctx.rec(
            "compute-reservation-idle",
            Severity::Info,
            "Make sure this reservation is consumed",
            "Reservations are billed at on-demand rates whether or not VMs use them. Pair them with committed use discounts and monitor their utilization.",
            None,
        )]
    }
}
