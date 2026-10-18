//! AI / ML: Vertex AI Workbench, legacy notebooks, Colab Enterprise, Vector
//! Search, Feature Store, TensorBoard, ML Metadata, prediction resource pools
//! and Cloud TPU.

use super::compute::{VmSpec, disk_components, machine_hourly, vm_components};
use super::machine::{self, COMPUTE, MachineType, Pricing};
use super::{Ctx, Spec, usage};
use crate::cost::Component;
use crate::hcl::value::{Value, fmt_number};
use crate::pricing::query::Query;

const VERTEX: &str = "Vertex AI";
/// Workbench / legacy notebooks management fees are published under this service.
const NOTEBOOKS: &str = "Notebooks";
/// TPU v2/v3/v4 are published under this service (v5e and later under Compute Engine).
const CLOUD_TPU: &str = "Cloud TPU";

pub fn specs() -> Vec<Spec> {
    const NB: &[super::UsageParam] = usage!("monthly_hrs" => "Hours per month the notebook VM runs (default 730)");
    const TPU: &[super::UsageParam] = usage!("monthly_hrs" => "Hours per month the TPU runs (default 730)");
    const PRED: &[super::UsageParam] = usage!(
        "replicas" => "Average number of serving replicas (default: min_replica_count)",
        "monthly_hrs" => "Hours per month the replicas run (default 730)",
    );
    vec![
        Spec::new("google_workbench_instance", workbench).usage(NB).rules(rules::notebook),
        Spec::new("google_notebooks_instance", notebooks_instance).usage(NB).rules(rules::notebook),
        Spec::new("google_notebooks_runtime", notebooks_runtime).usage(NB).rules(rules::notebook),
        Spec::new("google_colab_runtime", colab_runtime).usage(NB).rules(rules::notebook),
        Spec::new("google_tpu_v2_vm", tpu).usage(TPU).rules(rules::tpu),
        Spec::new("google_tpu_node", tpu).usage(TPU).rules(rules::tpu),
        Spec::new("google_vertex_ai_index_endpoint_deployed_index", deployed_index)
            .usage(usage!(
                "replicas" => "Average number of serving replicas (default: min_replica_count)",
                "shards" => "Number of index shards (index size / shard size; default 1)",
                "monthly_hrs" => "Hours per month the index is deployed (default 730)",
            ))
            .rules(rules::deployed_index),
        Spec::new("google_vertex_ai_index", index).usage(usage!(
            "monthly_index_build_gb" => "GiB processed per month by index builds/rebuilds (vectors x dimensions x 4 bytes)",
            "monthly_streaming_update_gb" => "GiB ingested per month through streaming updates (STREAM_UPDATE indexes)",
        )),
        Spec::new("google_vertex_ai_featurestore", featurestore)
            .usage(usage!(
                "online_storage_gb" => "Online store data in GiB",
                "offline_storage_gb" => "Offline store data in GiB",
            ))
            .rules(rules::featurestore),
        Spec::new("google_vertex_ai_feature_online_store", feature_online_store)
            .usage(usage!(
                "bigtable_storage_gb" => "Bigtable online serving storage in GiB",
                "serving_nodes" => "Average optimized online serving nodes (default 2, the minimum replica count)",
            ))
            .rules(rules::featurestore),
        Spec::new("google_vertex_ai_feature_online_store_featureview", featureview)
            .usage(usage!("monthly_data_processing_node_hours" => "Data processing node-hours per month used by feature syncs (~1 node-hour per 100 MiB)")),
        Spec::new("google_vertex_ai_tensorboard", tensorboard).usage(usage!("storage_gb" => "TensorBoard logs and metrics stored, in GiB")),
        Spec::new("google_vertex_ai_metadata_store", metadata_store).usage(usage!("storage_gb" => "ML Metadata stored, in GiB")),
        Spec::new("google_vertex_ai_deployment_resource_pool", resource_pool).usage(PRED).rules(rules::prediction),
        Spec::new("google_vertex_ai_endpoint_with_model_garden_deployment", model_garden).usage(PRED).rules(rules::prediction),
    ]
}

/// `NVIDIA_TESLA_T4` / `PD_BALANCED` style enums to Compute Engine names.
fn gce_name(t: &str) -> String {
    t.rsplit('/').next().unwrap_or(t).to_lowercase().replace('_', "-")
}

fn disk_type(t: Option<&str>, default: &str) -> String {
    match t.map(gce_name) {
        Some(t) if !t.is_empty() && t != "disk-type-unspecified" => t,
        _ => default.to_string(),
    }
}

/// `projects/p/locations/us-central1/...` -> `us-central1`.
fn location_from_name(v: &str) -> Option<String> {
    let i = v.find("locations/")?;
    v[i + "locations/".len()..].split('/').next().filter(|s| !s.is_empty()).map(str::to_string)
}

fn str_at<'a>(v: Option<&'a Value>, path: &str) -> Option<&'a str> {
    v?.path(path).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
}

fn num_at(v: Option<&Value>, path: &str) -> Option<f64> {
    v?.path(path).and_then(|x| x.as_f64())
}

// ---- notebooks ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Fees {
    /// Workbench instances: per-family CPU/RAM/GPU/disk fees.
    Workbench,
    /// Colab Enterprise runtimes: same fee grid, billed under Vertex AI.
    Colab,
    /// Legacy user-managed notebooks.
    UserManaged,
    /// Legacy managed notebooks.
    Managed,
}

struct NotebookVm {
    machine: String,
    gpus: Vec<(String, f64)>,
    /// (label, disk type, size GiB)
    disks: Vec<(&'static str, String, f64)>,
    external_ip: bool,
    running: bool,
    /// The VM runs in a Google-managed project (no sustained use discount).
    tenant: bool,
    fees: Fees,
}

impl NotebookVm {
    /// GPUs billed on the VM: bundled with the machine type, else attached.
    fn billed_gpus(&self) -> Vec<(String, f64)> {
        match machine::parse(&self.machine) {
            Some(mt) if !mt.accelerators.is_empty() => mt.accelerators,
            _ => self.gpus.clone(),
        }
    }
}

fn is_running(state: Option<&str>) -> bool {
    !matches!(state, Some("STOPPED" | "SUSPENDED"))
}

fn notebook_components(ctx: &Ctx, nb: &NotebookVm) -> Vec<Component> {
    let region = ctx.region();
    let mut out = vec![];
    let mt = machine::parse(&nb.machine);
    if nb.running {
        let bundled = mt.as_ref().is_some_and(|m| !m.accelerators.is_empty());
        let spec = VmSpec {
            machine: nb.machine.clone(),
            pricing: Pricing::OnDemand,
            count: 1.0,
            disks: vec![],
            local_ssd_gb: 0.0,
            gpus: if bundled { vec![] } else { nb.gpus.clone() },
            ephemeral_ips: if nb.external_ip { 1.0 } else { 0.0 },
            image: None,
        };
        let mut vm = vm_components(ctx, &region, &spec);
        if let (true, Some(mt), Some(first)) = (nb.tenant, &mt, vm.first_mut()) {
            // Runtimes in Google-managed projects are billed at list price.
            if let Some(h) = machine_hourly(ctx, &region, mt, Pricing::OnDemand) {
                first.unit_price = Some(h);
                first.monthly_cost = first.quantity.map(|q| q * h);
                first.note = None;
            }
        }
        out.extend(vm);
    }
    for (label, dtype, size) in &nb.disks {
        out.extend(disk_components(ctx, &region, label, dtype, *size, None, None, 1.0, false));
    }
    match nb.fees {
        Fees::Workbench | Fees::Colab => out.extend(grid_fees(ctx, &region, nb, mt.as_ref())),
        Fees::UserManaged | Fees::Managed => out.extend(legacy_fees(ctx, nb, mt.as_ref())),
    }
    if !nb.running
        && let Some(c) = out.first_mut()
    {
        c.note = Some("instance is stopped: only disks are billed".into());
    }
    out
}

/// Machine family label used in Workbench / Colab management fee SKUs.
fn fee_family(mt: &MachineType) -> Option<&'static str> {
    Some(match mt.family.as_str() {
        "e2" => "E2",
        "n1" => "N1",
        "n2" => "N2",
        "n2d" => "N2D AMD",
        "n4" => "N4",
        "a2" => "A2",
        "g2" => "G2",
        "g4" => "G4",
        "a3" if mt.name.contains("megagpu") => "A3 Mega",
        "a3" if mt.name.contains("ultragpu") => "A3 Ultra",
        "a3" => "A3",
        _ => return None,
    })
}

/// GPU label (regex) used in Workbench / Colab management fee SKUs.
fn gpu_fee_stem(accel: &str) -> Option<&'static str> {
    Some(match gce_name(accel).trim_end_matches("-vws") {
        "nvidia-tesla-t4" => "T4",
        "nvidia-tesla-p4" => "P4",
        "nvidia-tesla-p100" => "P100",
        "nvidia-tesla-v100" => "V100",
        "nvidia-tesla-a100" => r"A100 ?\(40 ?GB\)",
        "nvidia-a100-80gb" => r"A100 ?\(80 ?GB\)",
        "nvidia-l4" => "L4",
        "nvidia-h100-80gb" => r"H100( ?\(80 ?GB\))?",
        "nvidia-h100-mega-80gb" => r"H100 Mega( ?\(80 ?GB\))?",
        "nvidia-h200-141gb" => r"H200 ?\(141 ?GB\)",
        "nvidia-b200" => r"B200 ?\(180 ?GB\)",
        "nvidia-rtx6000" | "nvidia-rtx-pro-6000" => "RTX 6000",
        _ => return None,
    })
}

fn disk_fee_stem(dtype: &str) -> Option<&'static str> {
    Some(match dtype {
        "pd-standard" => "Standard Persistent Disk",
        "pd-balanced" => "Balanced Persistent Disk",
        "pd-ssd" => "SSD Persistent Disk",
        "pd-extreme" => "Extreme Persistent Disk",
        "hyperdisk-balanced" => "Hyperdisk Balanced Persistent Disk",
        "hyperdisk-balanced-high-availability" => "Hyperdisk Balanced High Availability Persistent Disk",
        "hyperdisk-extreme" => "Hyperdisk Extreme Persistent Disk",
        _ => return None,
    })
}

/// Find a management fee SKU: Workbench SKUs (Notebooks service) and Colab
/// SKUs (Vertex AI) share the same price grid, so either can be used.
fn fee_sku(ctx: &Ctx, region: &str, colab_first: bool, kind: &str, item: &str) -> Option<(&'static str, String)> {
    let wb = (NOTEBOOKS, format!(r"^\s*Workbench Instances {kind} for GCE management fee - {item}( running)? in "));
    let colab = (VERTEX, format!(r"^\s*Vertex Colab {kind} for GCE management fee - {item}( running)? in "));
    let order = if colab_first { [colab, wb] } else { [wb, colab] };
    order.into_iter().find(|(svc, pat)| ctx.price(Query::new(svc, region, pat).ondemand()).is_some())
}

fn grid_fees(ctx: &Ctx, region: &str, nb: &NotebookVm, mt: Option<&MachineType>) -> Vec<Component> {
    let mut out = vec![];
    let colab = nb.fees == Fees::Colab;
    let label = if colab { "Colab" } else { "Workbench" };
    let missing = |what: &str| ctx.warn(format!("no {label} management fee SKU for {what} in {region}"));
    if nb.running {
        if let Some(mt) = mt {
            match fee_family(mt) {
                Some(fam) => {
                    let fam = regex::escape(fam);
                    match fee_sku(ctx, region, colab, "(CPU|GPU)", &format!("{fam} Instance Cores?")) {
                        Some((svc, pat)) => out.push(ctx.hourly_unit(
                            format!("{label} management fee (vCPU)"),
                            "vCPU-hours",
                            machine::billed_vcpus(mt),
                            Query::new(svc, region, &pat).ondemand(),
                        )),
                        None => missing(&format!("{} vCPUs", mt.family)),
                    }
                    match fee_sku(ctx, region, colab, "RAM", &format!("{fam} Instance Ram")) {
                        Some((svc, pat)) => out.push(ctx.hourly_unit(
                            format!("{label} management fee (memory)"),
                            "GiB-hours",
                            mt.memory_gb,
                            Query::new(svc, region, &pat).ondemand(),
                        )),
                        None => missing(&format!("{} memory", mt.family)),
                    }
                }
                None => missing(&format!("machine family {}", mt.family)),
            }
        }
        for (t, n) in nb.billed_gpus() {
            let found = gpu_fee_stem(&t).and_then(|stem| fee_sku(ctx, region, colab, "GPU", stem));
            match found {
                Some((svc, pat)) => out.push(ctx.hourly_unit(
                    format!("{label} management fee (GPU {} x{})", gce_name(&t), fmt_number(n)),
                    "GPU-hours",
                    n,
                    Query::new(svc, region, &pat).ondemand(),
                )),
                None => missing(&format!("GPU {t}")),
            }
        }
    }
    for (_, dtype, size) in &nb.disks {
        let found = disk_fee_stem(dtype).and_then(|stem| fee_sku(ctx, region, colab, "Storage", &regex::escape(stem)));
        match found {
            Some((svc, pat)) => out.push(ctx.monthly(
                format!("{label} management fee ({dtype}, {} GiB)", fmt_number(*size)),
                "GiB-months",
                *size,
                Query::new(svc, region, &pat).ondemand(),
            )),
            None => missing(&format!("{dtype} disks")),
        }
    }
    out
}

/// Legacy notebooks bill a flat fee per vCPU and per GPU (standard / premium).
fn legacy_fees(ctx: &Ctx, nb: &NotebookVm, mt: Option<&MachineType>) -> Vec<Component> {
    if !nb.running {
        return vec![];
    }
    let (prefix, label) =
        if nb.fees == Fees::Managed { ("Managed Notebooks", "Managed notebooks") } else { ("Notebooks", "User-managed notebooks") };
    let cpu_pat = format!("^{prefix} CPU Management Fee");
    let cpu_q = Query::new(NOTEBOOKS, "global", &cpu_pat).ondemand();
    if ctx.price(cpu_q).is_none() {
        return vec![];
    }
    let mut out = vec![];
    if let Some(mt) = mt {
        out.push(ctx.hourly_unit(format!("{label} management fee (vCPU)"), "vCPU-hours", machine::billed_vcpus(mt), cpu_q));
    }
    for (t, n) in nb.billed_gpus() {
        let name = gce_name(&t);
        let tier = if name.contains("-t4") || name.contains("-p4") { "Standard" } else { "Premium" };
        let pat = format!("^{prefix} GPU {tier} Management Fee");
        out.push(ctx.hourly_unit(
            format!("{label} management fee (GPU {name} x{})", fmt_number(n)),
            "GPU-hours",
            n,
            Query::new(NOTEBOOKS, "global", &pat).ondemand(),
        ));
    }
    out
}

fn workbench_vm(ctx: &Ctx) -> NotebookVm {
    let machine = ctx.str("gce_setup.0.machine_type").map(str::to_string).unwrap_or_else(|| {
        ctx.warn("gce_setup.machine_type not set, assuming n1-standard-4");
        "n1-standard-4".into()
    });
    let default_disk = super::compute::default_boot_disk(&machine::family_of(&machine)).to_string();
    let gpus = ctx
        .blocks("gce_setup.0.accelerator_configs")
        .iter()
        .filter_map(|a| {
            let t = a.get("type")?.as_str()?;
            Some((gce_name(t), a.get("core_count").and_then(|c| c.as_f64()).unwrap_or(1.0)))
        })
        .collect();
    let boot_type = disk_type(ctx.str("gce_setup.0.boot_disk.0.disk_type"), &default_disk);
    let boot_size = ctx.num_or("gce_setup.0.boot_disk.0.disk_size_gb", 150.0);
    let data = ctx.block("gce_setup.0.data_disks");
    let data_type = disk_type(str_at(data, "disk_type"), &default_disk);
    let data_size = num_at(data, "disk_size_gb").unwrap_or(100.0);
    let static_ip = ctx
        .blocks("gce_setup.0.network_interfaces.0.access_configs")
        .iter()
        .any(|a| a.get("external_ip").is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()) || v.is_unknown()));
    NotebookVm {
        machine,
        gpus,
        disks: vec![("Boot disk", boot_type, boot_size), ("Data disk", data_type, data_size)],
        external_ip: !ctx.bool_or("gce_setup.0.disable_public_ip", false) && !static_ip,
        running: is_running(ctx.str("desired_state")),
        tenant: false,
        fees: Fees::Workbench,
    }
}

fn workbench(ctx: &Ctx) -> Vec<Component> {
    notebook_components(ctx, &workbench_vm(ctx))
}

fn legacy_accelerator(a: Option<&Value>) -> Vec<(String, f64)> {
    match str_at(a, "type") {
        Some(t) if !t.ends_with("UNSPECIFIED") => vec![(gce_name(t), num_at(a, "core_count").unwrap_or(1.0))],
        _ => vec![],
    }
}

fn legacy_fee_note(ctx: &Ctx, out: &mut [Component], managed: bool) {
    let prefix = if managed { "^Managed Notebooks CPU Management Fee" } else { "^Notebooks CPU Management Fee" };
    if ctx.price(Query::new(NOTEBOOKS, "global", prefix).ondemand()).is_none()
        && let Some(c) = out.iter_mut().find(|c| c.name.starts_with("Instance usage"))
    {
        c.note = Some(format!(
            "excludes the {} management fee (Notebooks SKUs are not in the pricing catalog)",
            if managed { "managed notebooks ($0.05/vCPU-hour list)" } else { "user-managed notebooks ($0.005/vCPU-hour list)" }
        ));
    }
}

fn notebooks_instance(ctx: &Ctx) -> Vec<Component> {
    let Some(machine) = ctx.str("machine_type") else {
        ctx.warn("machine_type is not known statically");
        return vec![];
    };
    let nb = NotebookVm {
        machine: machine.to_string(),
        gpus: legacy_accelerator(ctx.block("accelerator_config")),
        disks: vec![
            ("Boot disk", disk_type(ctx.str("boot_disk_type"), "pd-standard"), ctx.num_or("boot_disk_size_gb", 100.0)),
            ("Data disk", disk_type(ctx.str("data_disk_type"), "pd-standard"), ctx.num_or("data_disk_size_gb", 100.0)),
        ],
        external_ip: !ctx.bool_or("no_public_ip", false),
        running: is_running(ctx.str("desired_state")),
        tenant: false,
        fees: Fees::UserManaged,
    };
    let mut out = notebook_components(ctx, &nb);
    legacy_fee_note(ctx, &mut out, false);
    out
}

fn notebooks_runtime(ctx: &Ctx) -> Vec<Component> {
    let vm = ctx.block("virtual_machine.0.virtual_machine_config");
    let Some(machine) = str_at(vm, "machine_type") else {
        ctx.warn("virtual_machine_config.machine_type is not known statically");
        return vec![];
    };
    let disk = vm.and_then(|v| v.path("data_disk.0.initialize_params.0"));
    let nb = NotebookVm {
        machine: machine.to_string(),
        gpus: legacy_accelerator(vm.and_then(|v| v.path("accelerator_config.0"))),
        disks: vec![("Data disk", disk_type(str_at(disk, "disk_type"), "pd-standard"), num_at(disk, "disk_size_gb").unwrap_or(100.0))],
        external_ip: false,
        running: is_running(ctx.str("desired_state")),
        tenant: true,
        fees: Fees::Managed,
    };
    let mut out = notebook_components(ctx, &nb);
    legacy_fee_note(ctx, &mut out, true);
    out
}

fn colab_template<'a>(ctx: &Ctx<'a>) -> Option<&'a Value> {
    let t = ctx.str("notebook_runtime_template_ref.0.notebook_runtime_template")?;
    ctx.resolve_ref(t, "google_colab_runtime_template").map(|r| &r.attrs)
}

fn colab_runtime(ctx: &Ctx) -> Vec<Component> {
    let tpl = colab_template(ctx);
    if tpl.is_none() {
        ctx.warn("runtime template not resolved, assuming the default runtime (e2-standard-4, 100 GiB pd-standard)");
    }
    let machine = str_at(tpl, "machine_spec.0.machine_type").unwrap_or("e2-standard-4");
    let gpus = match str_at(tpl, "machine_spec.0.accelerator_type") {
        Some(t) if !t.ends_with("UNSPECIFIED") => vec![(gce_name(t), num_at(tpl, "machine_spec.0.accelerator_count").unwrap_or(1.0))],
        _ => vec![],
    };
    let nb = NotebookVm {
        machine: machine.to_string(),
        gpus,
        disks: vec![(
            "Data disk",
            disk_type(str_at(tpl, "data_persistent_disk_spec.0.disk_type"), "pd-standard"),
            num_at(tpl, "data_persistent_disk_spec.0.disk_size_gb").unwrap_or(100.0),
        )],
        external_ip: false,
        running: is_running(ctx.str("desired_state")),
        tenant: true,
        fees: Fees::Colab,
    };
    let mut out = notebook_components(ctx, &nb);
    if nb.running
        && let Some(c) = out.first_mut()
    {
        c.note = Some("billed under Vertex AI Colab SKUs (Compute Engine rates + management fee)".into());
    }
    out
}

// ---- Cloud TPU ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum TpuGen {
    V2,
    V3,
    V4,
    V5e,
    V5p,
    V6e,
    V7x,
}

impl TpuGen {
    fn label(self) -> &'static str {
        match self {
            TpuGen::V2 => "v2",
            TpuGen::V3 => "v3",
            TpuGen::V4 => "v4",
            TpuGen::V5e => "v5e",
            TpuGen::V5p => "v5p",
            TpuGen::V6e => "v6e",
            TpuGen::V7x => "7x",
        }
    }

    /// SKU stem for per-chip pricing under Compute Engine (v5e and later).
    fn chip_stem(self) -> Option<&'static str> {
        Some(match self {
            TpuGen::V5e => "TpuV5e",
            TpuGen::V5p => "TpuV5p",
            TpuGen::V6e => "TpuV6e",
            TpuGen::V7x => "TPU7x",
            _ => return None,
        })
    }
}

/// `v3-8`, `v5litepod-16`, `v5p-8`, `v6e-4`, `tpu7x-8` -> (generation, chips).
/// The suffix counts TensorCores for v2/v3/v4/v5p/7x (2 per chip) and chips
/// for v5e/v6e.
fn parse_tpu_type(s: &str) -> Option<(TpuGen, f64)> {
    let s = s.to_lowercase();
    let (g, n) = s.rsplit_once('-')?;
    let n: f64 = n.parse().ok()?;
    let (generation, cores_per_chip) = match g {
        "v2" => (TpuGen::V2, 2.0),
        "v3" => (TpuGen::V3, 2.0),
        "v4" => (TpuGen::V4, 2.0),
        "v5litepod" | "v5e" => (TpuGen::V5e, 1.0),
        "v5p" => (TpuGen::V5p, 2.0),
        "v6e" => (TpuGen::V6e, 1.0),
        "tpu7x" | "v7x" => (TpuGen::V7x, 2.0),
        _ => return None,
    };
    (n > 0.0).then(|| (generation, (n / cores_per_chip).max(1.0)))
}

/// accelerator_config { type = "V5LITE_POD", topology = "2x4" } -> (generation, chips).
fn parse_tpu_config(t: &str, topology: &str) -> Option<(TpuGen, f64)> {
    let generation = match t.to_uppercase().as_str() {
        "V2" => TpuGen::V2,
        "V3" => TpuGen::V3,
        "V4" => TpuGen::V4,
        "V5LITE_POD" | "V5E" => TpuGen::V5e,
        "V5P" => TpuGen::V5p,
        "V6E" => TpuGen::V6e,
        "TPU7X" | "V7X" => TpuGen::V7x,
        _ => return None,
    };
    let chips: Option<f64> = topology.split('x').map(|d| d.trim().parse::<f64>().ok()).product();
    Some((generation, chips.filter(|c| *c > 0.0)?))
}

struct TpuSpec {
    generation: TpuGen,
    chips: f64,
    shape: String,
    spot: bool,
    reserved: bool,
}

fn tpu_spec(ctx: &Ctx) -> Option<TpuSpec> {
    let (parsed, shape) = if let (Some(t), Some(topo)) = (ctx.str("accelerator_config.0.type"), ctx.str("accelerator_config.0.topology")) {
        (parse_tpu_config(t, topo), format!("{t} {topo}"))
    } else {
        let at = match ctx.str("accelerator_type") {
            Some(a) => a,
            None if ctx.r.rtype == "google_tpu_v2_vm" && !ctx.is_unknown("accelerator_type") => "v2-8",
            None => {
                ctx.warn("accelerator_type is not known statically");
                return None;
            }
        };
        (parse_tpu_type(at), at.to_string())
    };
    let Some((generation, chips)) = parsed else {
        ctx.warn(format!("unknown TPU accelerator type '{shape}'"));
        return None;
    };
    Some(TpuSpec {
        generation,
        chips,
        shape,
        spot: ctx.bool_or("scheduling_config.0.spot", false) || ctx.bool_or("scheduling_config.0.preemptible", false),
        reserved: ctx.bool_or("scheduling_config.0.reserved", false),
    })
}

fn tpu_chip_query(stem: &str, spot: bool) -> (String, &'static str) {
    if spot {
        (format!("^{stem} attached to Spot Preemptible VMs running in"), "Preemptible")
    } else {
        (format!("^{stem} running in"), "OnDemand")
    }
}

fn tpu(ctx: &Ctx) -> Vec<Component> {
    let Some(t) = tpu_spec(ctx) else { return vec![] };
    let region = ctx.region();
    let mode = if t.spot { "spot" } else { "on-demand" };
    let name = format!("TPU {} ({}, {} chips, {mode})", t.generation.label(), t.shape, fmt_number(t.chips));
    let mut c = match t.generation.chip_stem() {
        Some(stem) => {
            let (pat, usage) = tpu_chip_query(stem, t.spot);
            ctx.hourly_unit(name, "chip-hours", t.chips, Query::new(COMPUTE, &region, &pat).group("TPU").usage(usage))
        }
        None => legacy_tpu(ctx, &region, &t, name),
    };
    if t.reserved {
        c.note = Some("consumes reserved capacity: billed through the reservation (shown at on-demand rates)".into());
    }
    vec![c]
}

/// TPU v2/v3/v4 are priced per device (8 TensorCores = 4 chips).
fn legacy_tpu(ctx: &Ctx, region: &str, t: &TpuSpec, name: String) -> Component {
    let g = t.generation.label();
    let devices = t.chips / 4.0;
    let pod = if t.generation == TpuGen::V4 || t.chips > 4.0 { "Pod " } else { "" };
    let pat = format!("^Tpu-{g} {}{pod}Accelerator", if t.spot { "Preemptible " } else { "" });
    let q = Query::new(CLOUD_TPU, region, &pat);
    if ctx.price(q).is_some() {
        return ctx.hourly_unit(name, "device-hours", devices, q);
    }
    let hours = ctx.monthly_hours() * devices;
    if matches!(t.generation, TpuGen::V2 | TpuGen::V3) {
        // Vertex AI training on TPU = Cloud TPU price + Vertex management fee (per core).
        let train_pat = format!("^Vertex AI: Training/Pipelines on Tpu-{g} {pod}Accelerator");
        let fee_pat = format!("^Vertex AI: Training/Pipelines Management fee on Tpu-{g} {pod}Accelerator");
        let train = ctx.price(Query::new(VERTEX, region, &train_pat).ondemand());
        let fee = ctx.price(Query::new(VERTEX, region, &fee_pat).ondemand());
        if let (Some(train), Some(fee)) = (train, fee) {
            let mut per_device = (train.unit_price() - fee.unit_price()) * 8.0;
            let mut note = "Cloud TPU price derived from Vertex AI training SKUs (training minus management fee)".to_string();
            if t.spot {
                per_device *= 0.3;
                note.push_str("; preemptible estimated at 30% of on-demand");
                ctx.warn(format!("preemptible TPU {g} price estimated at 30% of on-demand"));
            }
            let mut c = ctx.fixed(name, "device-hours", Some(hours), Some(per_device));
            c.note = Some(note);
            return c;
        }
    }
    ctx.warn(format!("TPU {g} is priced under the 'Cloud TPU' billing service, which is not in the pricing catalog for {region}"));
    ctx.fixed(name, "device-hours", Some(hours), None)
}

// ---- Vector Search -------------------------------------------------------------

fn deployed_index_region(ctx: &Ctx) -> String {
    if let Some(ep) = ctx.str("index_endpoint") {
        if let Some(r) = ctx.resolve_ref(ep, "google_vertex_ai_index_endpoint")
            && let Some(reg) = r.attrs.get("region").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
        {
            return reg.to_string();
        }
        if let Some(l) = location_from_name(ep) {
            return l;
        }
    }
    ctx.region()
}

struct Replicas {
    machine: String,
    min: f64,
    max: f64,
    /// Path of the min replica attribute (for repricing).
    min_path: &'static str,
}

fn deployed_index_shape(ctx: &Ctx) -> Replicas {
    let shard_machine = || {
        let shard = ctx
            .str("index")
            .and_then(|i| ctx.resolve_ref(i, "google_vertex_ai_index"))
            .and_then(|r| r.attrs.path("metadata.0.config.0.shard_size").and_then(|v| v.as_str()).map(str::to_string));
        match shard.as_deref() {
            Some("SHARD_SIZE_SMALL") => "e2-standard-2",
            Some("SHARD_SIZE_LARGE") => "e2-highmem-16",
            _ => "e2-standard-16",
        }
    };
    if ctx.block("dedicated_resources").is_some() {
        let machine =
            ctx.str("dedicated_resources.0.machine_spec.0.machine_type").map(str::to_string).unwrap_or_else(|| shard_machine().into());
        let min = ctx.num_or("dedicated_resources.0.min_replica_count", 1.0);
        Replicas {
            machine,
            min,
            max: ctx.num_or("dedicated_resources.0.max_replica_count", min),
            min_path: "dedicated_resources.0.min_replica_count",
        }
    } else {
        let min = ctx.num_or("automatic_resources.0.min_replica_count", 2.0);
        Replicas {
            machine: shard_machine().into(),
            min,
            max: ctx.num_or("automatic_resources.0.max_replica_count", min),
            min_path: "automatic_resources.0.min_replica_count",
        }
    }
}

fn replicas_used(ctx: &Ctx, r: &Replicas) -> f64 {
    ctx.usage_num("replicas").unwrap_or_else(|| {
        if r.max > r.min {
            ctx.warn(format!(
                "autoscaled between {} and {} replicas; estimating with min (set usage 'replicas' to override)",
                fmt_number(r.min),
                fmt_number(r.max)
            ));
        }
        r.min
    })
}

fn deployed_index(ctx: &Ctx) -> Vec<Component> {
    let region = deployed_index_region(ctx);
    let shape = deployed_index_shape(ctx);
    let replicas = replicas_used(ctx, &shape);
    let shards = ctx.usage_num("shards").unwrap_or(1.0);
    let nodes = replicas * shards;
    let pat = format!("^Vector Search Index Serving {} in", regex::escape(&shape.machine));
    let mut c = ctx.hourly(
        format!("Index serving nodes ({}) x{}", shape.machine, fmt_number(nodes)),
        nodes,
        Query::new(VERTEX, &region, &pat).ondemand(),
    );
    if ctx.usage_num("shards").is_none() {
        c.note = Some("1 shard assumed: nodes = replicas x shards (set usage 'shards' for large indexes)".into());
    }
    vec![c]
}

fn index(ctx: &Ctx) -> Vec<Component> {
    let mut out = vec![ctx.usage(
        "Index building",
        "GiB",
        "monthly_index_build_gb",
        Query::new(VERTEX, "global", "^Vector Search Index Building").ondemand(),
    )];
    if ctx.str("index_update_method") == Some("STREAM_UPDATE") {
        out.push(ctx.usage(
            "Streaming updates",
            "GiB",
            "monthly_streaming_update_gb",
            Query::new(VERTEX, "global", "^Vector Search Index Streaming Update").ondemand(),
        ));
    }
    out
}

// ---- Feature Store ---------------------------------------------------------------

/// Legacy featurestore online serving nodes: (nodes, max, fixed).
fn featurestore_nodes(ctx: &Ctx) -> (f64, f64, bool) {
    if let Some(n) = ctx.num("online_serving_config.0.fixed_node_count") {
        return (n, n, true);
    }
    match ctx.num("online_serving_config.0.scaling.0.min_node_count") {
        Some(min) => (min, ctx.num_or("online_serving_config.0.scaling.0.max_node_count", min), false),
        None => (0.0, 0.0, false),
    }
}

fn featurestore(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let (nodes, max, fixed) = featurestore_nodes(ctx);
    let mut out = vec![];
    if nodes > 0.0 {
        if max > nodes {
            ctx.warn(format!("online serving autoscales between {} and {} nodes; estimating with min", fmt_number(nodes), fmt_number(max)));
        }
        out.push(ctx.hourly(
            format!("Online serving nodes ({}) x{}", if fixed { "fixed" } else { "autoscaling min" }, fmt_number(nodes)),
            nodes,
            Query::new(VERTEX, &region, r"^Feature Store online serving node\.").ondemand(),
        ));
    }
    out.push(ctx.usage(
        "Online storage",
        "GiB-months",
        "online_storage_gb",
        Query::new(VERTEX, &region, r"^Feature Store online storage\.").ondemand(),
    ));
    out.push(ctx.usage(
        "Offline storage",
        "GiB-months",
        "offline_storage_gb",
        Query::new(VERTEX, &region, r"^Feature Store offline storage\.").ondemand(),
    ));
    out
}

fn feature_online_store(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    if ctx.block("bigtable").is_some() {
        let min = ctx.num_or("bigtable.0.auto_scaling.0.min_node_count", 1.0);
        let max = ctx.num_or("bigtable.0.auto_scaling.0.max_node_count", min);
        if max > min {
            ctx.warn(format!("Bigtable serving autoscales between {} and {} nodes; estimating with min", fmt_number(min), fmt_number(max)));
        }
        return vec![
            ctx.hourly(
                format!("Bigtable online serving nodes x{}", fmt_number(min)),
                min,
                Query::new(VERTEX, &region, r"^\s*Feature Store Bigtable online serving node for").ondemand(),
            ),
            ctx.usage(
                "Bigtable online serving storage",
                "GiB-months",
                "bigtable_storage_gb",
                Query::new(VERTEX, &region, r"^\s*Feature Store storage for Bigtable online serving in").ondemand(),
            ),
        ];
    }
    if ctx.block("optimized").is_some() {
        let nodes = ctx.usage_num("serving_nodes").unwrap_or(2.0);
        let mut c = ctx.hourly(
            format!("Optimized online serving nodes x{}", fmt_number(nodes)),
            nodes,
            Query::new(VERTEX, &region, r"^\s*Feature Store optimized online serving node in").ondemand(),
        );
        c.note = Some("autoscaled: 2 replicas minimum, each node includes 200 GB of storage".into());
        return vec![c];
    }
    ctx.warn("neither bigtable nor optimized serving is configured");
    vec![]
}

fn featureview(ctx: &Ctx) -> Vec<Component> {
    let region = match ctx.str("region") {
        Some(r) => r.to_string(),
        None => ctx
            .str("feature_online_store")
            .and_then(|s| ctx.resolve_ref(s, "google_vertex_ai_feature_online_store"))
            .and_then(|r| r.attrs.get("region").and_then(|v| v.as_str()).map(str::to_string))
            .unwrap_or_else(|| ctx.region()),
    };
    vec![ctx.usage(
        "Data processing (feature syncs)",
        "node-hours",
        "monthly_data_processing_node_hours",
        Query::new(VERTEX, &region, r"^\s*Feature Store data processing node in").ondemand(),
    )]
}

// ---- TensorBoard / Metadata ---------------------------------------------------------

fn tensorboard(ctx: &Ctx) -> Vec<Component> {
    vec![ctx.usage("TensorBoard storage", "GiB-months", "storage_gb", Query::new(VERTEX, "global", "^Tensorboard storage").ondemand())]
}

fn metadata_store(ctx: &Ctx) -> Vec<Component> {
    vec![ctx.usage("Metadata storage", "GiB-months", "storage_gb", Query::new(VERTEX, "global", "^Metadata storage").ondemand())]
}

// ---- Online prediction -----------------------------------------------------------------

/// Machine family label of Vertex AI prediction node SKUs.
fn prediction_family(mt: &MachineType) -> Option<&'static str> {
    Some(match mt.family.as_str() {
        "n1" => "N1 Predefined",
        "n2" => "N2 Predefined",
        "n2d" => "N2D Predefined",
        "n4" => "N4 Predefined",
        "e2" => "E2",
        "a2" => "A2",
        "c2" => "C2 Predefined",
        "c2d" => "C2D Predefined",
        "c3" => "C3 Predefined",
        "c3d" => "C3D Predefined",
        "c4d" => "C4D Predefined",
        "g2" => "G2 Predefined",
        "g4" => "G4 Predefined",
        "a3" if mt.name.contains("megagpu") => "A3 Mega Predefined",
        "a3" if mt.name.contains("ultragpu") => "A3 Ultra Predefined",
        "a3" => "A3 Predefined",
        _ => return None,
    })
}

fn prediction_gpu(accel: &str) -> Option<&'static str> {
    Some(match gce_name(accel).as_str() {
        "nvidia-tesla-t4" => "Nvidia Tesla T4 GPU",
        "nvidia-tesla-p4" => "Nvidia Tesla P4 GPU",
        "nvidia-tesla-p100" => "Nvidia Tesla P100 GPU",
        "nvidia-tesla-v100" => "Nvidia Tesla V100 GPU",
        "nvidia-tesla-k80" => "Nvidia Tesla K80 GPU",
        "nvidia-tesla-a100" => "Nvidia Tesla A100 GPU",
        "nvidia-a100-80gb" => "Nvidia A100 80gb GPU",
        "nvidia-l4" => "Nvidia L4 GPU",
        "nvidia-h100-80gb" => "Nvidia H100 80gb GPU",
        "nvidia-h100-mega-80gb" => "Nvidia H100 Mega 80gb GPU",
        "nvidia-h200-141gb" => "NVIDIA H200 141GB",
        "nvidia-rtx-pro-6000" | "nvidia-rtx6000" => "Nvidia RTX 6000 GPU",
        _ => return None,
    })
}

/// Dedicated prediction resources (`dedicated_resources` block at `base`).
fn prediction_shape(ctx: &Ctx, base: &str) -> Option<(Replicas, Vec<(String, f64)>)> {
    let d = ctx.block(base)?;
    let machine = str_at(Some(d), "machine_spec.0.machine_type").unwrap_or("n1-standard-2").to_string();
    let accel = match str_at(Some(d), "machine_spec.0.accelerator_type") {
        Some(t) if !t.ends_with("UNSPECIFIED") => vec![(gce_name(t), num_at(Some(d), "machine_spec.0.accelerator_count").unwrap_or(1.0))],
        _ => vec![],
    };
    let min = num_at(Some(d), "min_replica_count").unwrap_or(1.0);
    let max = num_at(Some(d), "max_replica_count").unwrap_or(min);
    let min_path = if base == "dedicated_resources" {
        "dedicated_resources.0.min_replica_count"
    } else {
        "deploy_config.0.dedicated_resources.0.min_replica_count"
    };
    Some((Replicas { machine, min, max, min_path }, accel))
}

fn prediction_components(ctx: &Ctx, region: &str, shape: &Replicas, accel: &[(String, f64)]) -> Vec<Component> {
    let mut out = vec![];
    let Some(mt) = machine::parse(&shape.machine) else {
        ctx.warn(format!("unknown machine type '{}'", shape.machine));
        return out;
    };
    let n = replicas_used(ctx, shape);
    let hours = ctx.monthly_hours();
    let hourly = prediction_family(&mt).and_then(|fam| {
        let fam = regex::escape(fam);
        let core = ctx
            .price(Query::new(VERTEX, region, &format!("^Vertex AI: Online/Batch Prediction {fam} Instance Core running in")).ondemand())?;
        let ram = ctx
            .price(Query::new(VERTEX, region, &format!("^Vertex AI: Online/Batch Prediction {fam} Instance Ram running in")).ondemand())?;
        Some(core.unit_price() * mt.vcpus + ram.unit_price() * mt.memory_gb)
    });
    out.push(ctx.fixed(format!("Prediction nodes ({}) x{}", mt.name, fmt_number(n)), "hours", Some(n * hours), hourly));
    let gpus = if accel.is_empty() { mt.accelerators.clone() } else { accel.to_vec() };
    for (t, count) in gpus {
        match prediction_gpu(&t) {
            Some(stem) => {
                let pat = format!("^Vertex AI: Online/Batch Prediction {} (running )?in", regex::escape(stem));
                out.push(ctx.hourly_unit(
                    format!("GPU ({t}) x{}", fmt_number(count * n)),
                    "GPU-hours",
                    count * n,
                    Query::new(VERTEX, region, &pat).ondemand(),
                ));
            }
            None => ctx.warn(format!("no Vertex AI prediction SKU for accelerator '{t}'")),
        }
    }
    out
}

fn resource_pool(ctx: &Ctx) -> Vec<Component> {
    let Some((shape, accel)) = prediction_shape(ctx, "dedicated_resources") else {
        ctx.warn("dedicated_resources not set");
        return vec![];
    };
    prediction_components(ctx, &ctx.region(), &shape, &accel)
}

fn model_garden(ctx: &Ctx) -> Vec<Component> {
    let Some((shape, accel)) = prediction_shape(ctx, "deploy_config.0.dedicated_resources") else {
        ctx.warn("deploy_config.dedicated_resources not set: Model Garden picks the default machine for the model, which cannot be priced statically");
        return vec![];
    };
    if ctx.bool_or("deploy_config.0.dedicated_resources.0.spot", false) {
        ctx.warn("Spot serving is priced at on-demand rates");
    }
    prediction_components(ctx, &ctx.region(), &shape, &accel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tpu_types() {
        assert_eq!(parse_tpu_type("v2-8"), Some((TpuGen::V2, 4.0)));
        assert_eq!(parse_tpu_type("v3-32"), Some((TpuGen::V3, 16.0)));
        assert_eq!(parse_tpu_type("v4-8"), Some((TpuGen::V4, 4.0)));
        assert_eq!(parse_tpu_type("v5litepod-16"), Some((TpuGen::V5e, 16.0)));
        assert_eq!(parse_tpu_type("v5p-8"), Some((TpuGen::V5p, 4.0)));
        assert_eq!(parse_tpu_type("v6e-4"), Some((TpuGen::V6e, 4.0)));
        assert_eq!(parse_tpu_type("tpu7x-8"), Some((TpuGen::V7x, 4.0)));
        assert_eq!(parse_tpu_type("v9-8"), None);
        assert_eq!(parse_tpu_type("v5p"), None);
    }

    #[test]
    fn tpu_configs() {
        assert_eq!(parse_tpu_config("V5LITE_POD", "2x4"), Some((TpuGen::V5e, 8.0)));
        assert_eq!(parse_tpu_config("V4", "2x2x1"), Some((TpuGen::V4, 4.0)));
        assert_eq!(parse_tpu_config("V6E", "axb"), None);
    }

    #[test]
    fn names() {
        assert_eq!(gce_name("NVIDIA_TESLA_T4"), "nvidia-tesla-t4");
        assert_eq!(disk_type(Some("PD_BALANCED"), "pd-standard"), "pd-balanced");
        assert_eq!(disk_type(Some("DISK_TYPE_UNSPECIFIED"), "pd-standard"), "pd-standard");
        assert_eq!(location_from_name("projects/p/locations/europe-west4/indexEndpoints/1").as_deref(), Some("europe-west4"));
        assert_eq!(gpu_fee_stem("NVIDIA_TESLA_T4_VWS"), Some("T4"));
    }
}

mod rules {
    use super::*;
    use crate::cost::{CostedResource, HOURS_PER_MONTH, Recommendation, Severity};
    use crate::resources::{Env, set_path};

    /// ~10 hours per working day.
    const BUSINESS_HOURS: f64 = 217.0;

    fn idle_shutdown_missing(ctx: &Ctx) -> Option<&'static str> {
        match ctx.r.rtype.as_str() {
            "google_workbench_instance" | "google_notebooks_instance" => {
                let key = if ctx.r.rtype == "google_workbench_instance" { "gce_setup.0.metadata" } else { "metadata" };
                let v = ctx.get(key).and_then(|m| m.get("idle-timeout-seconds")).and_then(|v| v.as_f64());
                (v.is_none_or(|s| s <= 0.0)).then_some("set the metadata key idle-timeout-seconds (e.g. \"10800\" for 3 hours)")
            }
            "google_notebooks_runtime" => (!ctx.bool_or("software_config.0.idle_shutdown", true))
                .then_some("set software_config { idle_shutdown = true, idle_shutdown_timeout = 180 }"),
            "google_colab_runtime" => {
                let t = str_at(colab_template(ctx), "idle_shutdown_config.0.idle_timeout");
                matches!(t, Some("0s" | "0")).then_some("set idle_shutdown_config.idle_timeout on the runtime template (e.g. \"10800s\")")
            }
            _ => None,
        }
    }

    pub fn notebook(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let running = is_running(ctx.str("desired_state"));
        let legacy = matches!(ctx.r.rtype.as_str(), "google_notebooks_instance" | "google_notebooks_runtime");
        let prefix = match ctx.r.rtype.as_str() {
            "google_colab_runtime" => "colab",
            "google_workbench_instance" => "workbench",
            _ => "notebooks",
        };

        if legacy {
            out.push(ctx.rec(
                "notebooks-legacy-migrate",
                Severity::Medium,
                "Migrate this legacy notebook to a Vertex AI Workbench instance",
                "User-managed notebooks (google_notebooks_instance) and managed notebooks (google_notebooks_runtime) are deprecated and no longer supported. \
                 Recreate the environment as a google_workbench_instance: it has built-in idle shutdown and a lower management fee than managed notebooks ($0.05/vCPU-hour). \
                 Back up the data disk before migrating.",
                None,
            ));
        }

        if running
            && ctx.usage_num("monthly_hrs").is_none()
            && let Some(how) = idle_shutdown_missing(ctx)
        {
            let stopped = ctx.reprice(|a| {
                a.insert("desired_state".into(), Value::from("STOPPED"));
            });
            let hours = ctx.monthly_hours();
            if let Some(stopped) = stopped.filter(|_| hours > BUSINESS_HOURS) {
                let compute = cr.monthly_cost - stopped;
                let s = compute * (1.0 - BUSINESS_HOURS / hours);
                if s > 1.0 {
                    out.push(ctx.rec(
                            &format!("{prefix}-idle-shutdown"),
                            Severity::Medium,
                            "Configure idle shutdown for this notebook",
                            format!(
                                "Idle shutdown is not enabled, so the VM (and its GPUs) keep running after users stop working. To enable it, {how}. \
                                 The estimate assumes ~{} hours of use per month instead of {}; disks are still billed while stopped.",
                                fmt_number(BUSINESS_HOURS),
                                fmt_number(hours)
                            ),
                            Some(s),
                        ));
                }
            }
        }

        if ctx.env == Env::NonProd && running {
            let gpu_cost: f64 = cr.components.iter().filter(|c| c.name.contains("GPU")).filter_map(|c| c.monthly_cost).sum();
            if gpu_cost > 1.0 {
                out.push(ctx.rec(
                    &format!("{prefix}-gpu-nonprod"),
                    Severity::Info,
                    "GPU attached to a non-production notebook",
                    format!(
                        "GPUs account for ~{}/mo of this notebook. Make sure they are needed for interactive work; otherwise remove the accelerator \
                         and run GPU training as Vertex AI custom jobs (billed per use), or start the notebook only when needed.",
                        ctx.money(gpu_cost)
                    ),
                    None,
                ));
            }
        }
        out
    }

    pub fn tpu(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let Some(t) = tpu_spec(ctx) else { return out };
        let node = ctx.r.rtype == "google_tpu_node";
        if node {
            out.push(ctx.rec(
                "tpu-node-legacy",
                Severity::Info,
                "Migrate from the TPU Node architecture to TPU VMs",
                "google_tpu_node uses the legacy TPU Node architecture, where a separate user VM drives the TPU over the network. \
                 TPU VMs (google_tpu_v2_vm) give direct access to the TPU host, support Spot and the newer generations (v5e, v6e) that cost less per unit of performance.",
                None,
            ));
        }
        if ctx.env == Env::NonProd && !t.spot && !t.reserved {
            let key = if node { "scheduling_config.0.preemptible" } else { "scheduling_config.0.spot" };
            let new = ctx.reprice(|a| set_path(a, key, Value::from(true)));
            if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                let attr = if node { "scheduling_config { preemptible = true }" } else { "scheduling_config { spot = true }" };
                out.push(ctx.rec(
                    "tpu-spot-nonprod",
                    Severity::Medium,
                    "Use Spot TPUs for this non-production TPU",
                    format!(
                        "Spot TPUs cost ~{}% less than on-demand. They can be preempted at any time, which is usually fine for experiments and \
                         checkpointed training. Set {attr}.",
                        ((s / cr.monthly_cost) * 100.0).round()
                    ),
                    Some(s),
                ));
            }
        }
        if ctx.env != Env::NonProd
            && !t.spot
            && !t.reserved
            && ctx.monthly_hours() >= HOURS_PER_MONTH * 0.9
            && let Some(stem) = t.generation.chip_stem()
        {
            let region = ctx.region();
            let commit = |years: &str, usage: &str| {
                let pat = format!("^Commitment v1: {stem} running in .* for {years}");
                ctx.price(Query::new(COMPUTE, &region, &pat).group("TPU").usage(usage))
                    .map(|p| p.unit_price() * t.chips * ctx.monthly_hours())
            };
            if let (Some(c1), Some(c3)) = (commit("1 Year", "Commit1Yr"), commit("3 Years", "Commit3Yr")) {
                let (s1, s3) = (cr.monthly_cost - c1, cr.monthly_cost - c3);
                if s1 > 1.0 {
                    out.push(ctx.rec(
                            "tpu-cud",
                            Severity::Info,
                            "Cover this always-on TPU with a committed use discount",
                            format!(
                                "A TPU {} commitment for {} chips would save ~{}/mo (1 year) or ~{}/mo (3 years). Commit only to your steady-state baseline.",
                                t.generation.label(),
                                fmt_number(t.chips),
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

    fn min_replicas_rec(ctx: &Ctx, cr: &CostedResource, id: &str, what: &str, r: &Replicas) -> Option<Recommendation> {
        if ctx.env != Env::NonProd || r.min <= 1.0 || ctx.usage_num("replicas").is_some() {
            return None;
        }
        let path = r.min_path;
        let new = ctx.reprice(|a| set_path(a, path, Value::from(1.0)));
        let s = ctx.savings_vs(cr.monthly_cost, new)?;
        let attr = path.replace(".0.", ".");
        Some(ctx.rec(
            id,
            Severity::Medium,
            format!("Reduce the minimum replicas of this non-production {what}"),
            format!(
                "{} replicas are always running. One replica is usually enough outside production (no SLA is offered with a single replica). \
                 Set {attr} = 1 and keep max_replica_count for bursts.",
                fmt_number(r.min)
            ),
            Some(s),
        ))
    }

    pub fn deployed_index(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let shape = deployed_index_shape(ctx);
        out.extend(min_replicas_rec(ctx, cr, "vector-search-min-replicas-nonprod", "Vector Search deployment", &shape));
        let alt = match shape.machine.as_str() {
            "n1-standard-16" => Some("e2-standard-16"),
            "n1-standard-32" => Some("e2-highmem-16"),
            _ => None,
        };
        if let (Some(alt), true) = (alt, ctx.block("dedicated_resources").is_some()) {
            let new = ctx.reprice(|a| set_path(a, "dedicated_resources.0.machine_spec.0.machine_type", Value::from(alt)));
            if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                out.push(ctx.rec(
                    "vector-search-machine-type",
                    Severity::Low,
                    format!("Serve the index on {alt} instead of {}", shape.machine),
                    format!("{} is kept for compatibility; Google recommends {alt} for cost efficiency with the same shard sizes. Validate latency/QPS after switching.", shape.machine),
                    Some(s),
                ));
            }
        }
        out
    }

    pub fn prediction(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let base = if ctx.r.rtype == "google_vertex_ai_deployment_resource_pool" {
            "dedicated_resources"
        } else {
            "deploy_config.0.dedicated_resources"
        };
        let Some((shape, _)) = prediction_shape(ctx, base) else { return vec![] };
        min_replicas_rec(ctx, cr, "vertex-prediction-min-replicas-nonprod", "prediction deployment", &shape).into_iter().collect()
    }

    pub fn featurestore(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let nonprod = ctx.env == Env::NonProd;
        if ctx.r.rtype == "google_vertex_ai_featurestore" {
            out.push(ctx.rec(
                "featurestore-legacy",
                Severity::Info,
                "Plan the migration to the new Vertex AI Feature Store",
                "google_vertex_ai_featurestore is Vertex AI Feature Store (Legacy), which is deprecated. The new Feature Store keeps offline data in BigQuery \
                 and serves online through google_vertex_ai_feature_online_store (optimized nodes are much cheaper than legacy serving nodes).",
                None,
            ));
            let (nodes, _, fixed) = featurestore_nodes(ctx);
            if nonprod && nodes > 1.0 {
                let path =
                    if fixed { "online_serving_config.0.fixed_node_count" } else { "online_serving_config.0.scaling.0.min_node_count" };
                let new = ctx.reprice(|a| set_path(a, path, Value::from(1.0)));
                if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                    let how = if fixed {
                        "Replace fixed_node_count with online_serving_config { scaling { min_node_count = 1, max_node_count = N } } so nodes follow the load."
                    } else {
                        "Set online_serving_config.scaling.min_node_count = 1."
                    };
                    out.push(ctx.rec(
                        "featurestore-nodes-nonprod",
                        Severity::Medium,
                        "Reduce online serving nodes of this non-production featurestore",
                        format!("{} online serving nodes are billed around the clock. {how}", fmt_number(nodes)),
                        Some(s),
                    ));
                }
            }
        } else if nonprod && ctx.block("bigtable").is_some() {
            let min = ctx.num_or("bigtable.0.auto_scaling.0.min_node_count", 1.0);
            if min > 1.0 {
                let new = ctx.reprice(|a| set_path(a, "bigtable.0.auto_scaling.0.min_node_count", Value::from(1.0)));
                if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
                    out.push(ctx.rec(
                        "featurestore-nodes-nonprod",
                        Severity::Medium,
                        "Reduce the minimum Bigtable serving nodes of this non-production online store",
                        format!("{} Bigtable serving nodes are always running. Set bigtable.auto_scaling.min_node_count = 1 and let autoscaling add nodes under load.", fmt_number(min)),
                        Some(s),
                    ));
                }
            }
        }
        out
    }
}
