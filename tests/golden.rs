//! Golden-value tests: exact monthly costs of `testdata/` fixtures, priced
//! with a small committed price list (`testdata/pricing/golden-catalog.json`,
//! only the SKUs these fixtures use). They need no network nor credentials,
//! and catch pricing regressions in the cost models.
//!
//! Regenerate the price list from a synced catalog when a model starts using
//! other SKUs: `just golden-catalog` (then review the diff of expected values).

use costerra::config::Config;
use costerra::cost::{Basis, Report};
use costerra::engine::Engine;
use costerra::observed::{CpuStats, Observed, ObservedResource};
use costerra::pricing::catalog::{self, Catalog, CatalogFile, ServiceSkus};
use costerra::pricing::query::Pricer;
use costerra::project::{self, LoadOptions};
use costerra::usage::UsageFile;
use std::path::{Path, PathBuf};

const FIXTURES: &[&str] = &["terragrunt", "terraform/basic"];

fn testdata(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(p)
}

fn golden_path() -> PathBuf {
    testdata("pricing/golden-catalog.json")
}

fn report(pricer: &Pricer, path: &Path) -> Report {
    report_observed(pricer, path, |_| None)
}

/// Report with observed usage built from the loaded projects.
fn report_observed(pricer: &Pricer, path: &Path, observe: impl Fn(&[project::Project]) -> Option<Observed>) -> Report {
    let opts = LoadOptions { offline: true, cache_dir: std::env::temp_dir().join("costerra-golden"), ..Default::default() };
    let projects = project::load(path, &opts).expect("load fixture");
    let observed = observe(&projects);
    let (config, usage) = (Config::default(), UsageFile::default());
    let engine = Engine {
        pricer,
        list_pricer: None,
        usage_profile: None,
        examples: false,
        usage: &usage,
        config: &config,
        recommendations: true,
        observed: observed.as_ref(),
    };
    engine.report(&projects)
}

fn golden_pricer() -> Pricer {
    let src = std::fs::read_to_string(golden_path()).expect("golden catalog (run `just golden-catalog`)");
    let f: CatalogFile = serde_json::from_str(&src).expect("golden catalog JSON");
    Pricer::new(Some(Catalog::from_file(f)), false)
}

fn assert_money(what: &str, got: f64, want: f64) {
    assert!((got - want).abs() < 0.005, "{what}: got {got:.2}, want {want:.2}");
}

#[test]
fn terragrunt_costs() {
    let r = report(&golden_pricer(), &testdata("terragrunt"));
    let totals: Vec<(&str, f64)> = r.projects.iter().map(|p| (p.name.as_str(), p.monthly_cost)).collect();
    let want = [("live/dev/app", 255.63), ("live/dev/network", 7.30), ("live/prod/app", 1012.47), ("live/prod/network", 16.06)];
    assert_eq!(totals.len(), want.len(), "{totals:?}");
    for ((name, got), (wname, w)) in totals.iter().zip(want) {
        assert_eq!(*name, wname);
        assert_money(name, *got, w);
    }
    assert_money("total", r.monthly_cost, 1291.46);
    // Spot (dev) and the machine-family change are alternatives for the same
    // instances: only the best one per resource is counted.
    assert_money("potential savings", r.potential_savings, 336.06);
    let vm = &r.projects[2].resources[0];
    assert_eq!(vm.components.len(), 2);
    assert_money("n2-standard-8 instance", vm.components[0].monthly_cost.unwrap_or_default(), 249.82);
    assert_money("boot disk", vm.components[1].monthly_cost.unwrap_or_default(), 3.30);
}

#[test]
fn terraform_basic_costs() {
    let r = report(&golden_pricer(), &testdata("terraform/basic"));
    let p = &r.projects[0];
    let cost = |addr: &str| p.resources.iter().find(|r| r.address == addr).map(|r| r.monthly_cost);
    for (addr, want) in GOLDEN_BASIC {
        assert_money(addr, cost(addr).unwrap_or_else(|| panic!("{addr} missing")), *want);
    }
    assert_money("total", r.monthly_cost, GOLDEN_BASIC_TOTAL);
}

const API_VM: &str = "module.vm[\"api\"].google_compute_instance.this";

/// The api VM of testdata/terraform/basic (n1-standard-4, dev), always on,
/// with this CPU use over 30 days.
fn api_vm_observed(mean: f64, p95: f64) -> impl Fn(&[project::Project]) -> Option<Observed> {
    move |projects| {
        let mut o = Observed::default();
        let usage = [("monthly_hrs".to_string(), 730.0)].into_iter().collect();
        let cpu = Some(CpuStats { mean, p95, max: p95, days: 30.0 });
        o.resources.insert(
            (projects[0].name.clone(), API_VM.to_string()),
            ObservedResource { usage, cpu, committed_fraction: None, memory: None, console: None },
        );
        Some(o)
    }
}

fn api_recs(r: &Report) -> Vec<(&str, Basis)> {
    r.projects[0].recommendations.iter().filter(|x| x.address == API_VM).map(|x| (x.id.as_str(), x.basis)).collect()
}

#[test]
fn recommendations_without_observed_usage_are_based_on_the_code() {
    let r = report(&golden_pricer(), &testdata("terraform/basic"));
    let recs = api_recs(&r);
    assert!(recs.contains(&("compute-spot-nonprod", Basis::Code)), "{recs:?}");
    assert!(!recs.iter().any(|(id, _)| ["compute-idle", "compute-rightsize", "compute-schedule-nonprod"].contains(id)), "{recs:?}");
}

#[test]
fn idle_vm_from_observed_cpu() {
    let r = report_observed(&golden_pricer(), &testdata("terraform/basic"), api_vm_observed(0.01, 0.02));
    let recs = api_recs(&r);
    assert_eq!(recs, vec![("compute-idle", Basis::Observed)]);
    let idle = r.projects[0].recommendations.iter().find(|x| x.id == "compute-idle").unwrap();
    assert_money("idle savings", idle.monthly_savings.unwrap(), 148.95);
}

#[test]
fn rightsize_and_schedule_from_observed_usage() {
    let r = report_observed(&golden_pricer(), &testdata("terraform/basic"), api_vm_observed(0.10, 0.20));
    let recs = api_recs(&r);
    assert!(recs.contains(&("compute-rightsize", Basis::Observed)), "{recs:?}");
    assert!(recs.contains(&("compute-schedule-nonprod", Basis::Observed)), "{recs:?}");
    let rs = r.projects[0].recommendations.iter().find(|x| x.id == "compute-rightsize").unwrap();
    assert_eq!(rs.title, "Downsize n1-standard-4 to n1-standard-2");
    // A saturated VM gets no rightsizing nor cheaper family.
    let r = report_observed(&golden_pricer(), &testdata("terraform/basic"), api_vm_observed(0.70, 0.95));
    let recs = api_recs(&r);
    assert!(!recs.iter().any(|(id, _)| ["compute-rightsize", "compute-machine-family"].contains(id)), "{recs:?}");
    // … but twice the vCPUs, at a higher cost.
    let up = r.projects[0].recommendations.iter().find(|x| x.id == "compute-upsize").unwrap_or_else(|| panic!("{recs:?}"));
    assert_eq!(up.title, "Upsize n1-standard-4 to n1-standard-8: its CPU is saturated");
    assert!(up.monthly_savings.is_some_and(|s| s < 0.0), "{up:?}");
    assert_eq!(up.change.as_ref().map(|c| c.to.as_str()), Some("n1-standard-8"));
}

/// Observed usage of the 4 always-on production VMs of testdata/terragrunt,
/// with these shares of their vCPUs already committed.
fn prod_app_observed(committed: &'static [f64]) -> impl Fn(&[project::Project]) -> Option<Observed> {
    move |_| {
        let mut o = Observed::default();
        for (i, f) in committed.iter().enumerate() {
            let usage = [("monthly_hrs".to_string(), 730.0)].into_iter().collect();
            o.resources.insert(
                ("live/prod/app".to_string(), format!("google_compute_instance.app[{i}]")),
                ObservedResource { usage, cpu: None, committed_fraction: Some(*f), memory: None, console: None },
            );
        }
        Some(o)
    }
}

fn prod_app_cud(r: &Report) -> Option<costerra::cost::Recommendation> {
    r.projects.iter().find(|p| p.name == "live/prod/app")?.recommendations.iter().find(|x| x.id == "compute-cud").cloned()
}

#[test]
fn commitment_advice_leaves_out_committed_vcpus() {
    let pricer = golden_pricer();
    let path = testdata("terragrunt");
    let full = prod_app_cud(&report(&pricer, &path)).expect("compute-cud for always-on production VMs");
    assert_eq!(full.basis, Basis::Code);
    let none = prod_app_cud(&report_observed(&pricer, &path, prod_app_observed(&[0.0, 0.0, 0.0, 0.0]))).unwrap();
    assert_eq!(none.basis, Basis::Observed);
    assert_money("nothing committed", none.monthly_savings.unwrap(), full.monthly_savings.unwrap());
    // One VM fully and one half covered: 1.5 of 4 VMs' vCPUs are committed.
    let part = prod_app_cud(&report_observed(&pricer, &path, prod_app_observed(&[1.0, 0.5, 0.0, 0.0]))).unwrap();
    assert_money("partly committed", part.monthly_savings.unwrap(), full.monthly_savings.unwrap() * 2.5 / 4.0);
    assert!(prod_app_cud(&report_observed(&pricer, &path, prod_app_observed(&[1.0, 1.0, 1.0, 1.0]))).is_none());
}

#[test]
fn commitment_advice_needs_an_always_on_vm() {
    let observe = |_: &[project::Project]| {
        let mut o = Observed::default();
        for i in 0..4 {
            let usage = [("monthly_hrs".to_string(), 300.0)].into_iter().collect();
            o.resources.insert(
                ("live/prod/app".to_string(), format!("google_compute_instance.app[{i}]")),
                ObservedResource { usage, ..Default::default() },
            );
        }
        Some(o)
    };
    assert!(prod_app_cud(&report_observed(&golden_pricer(), &testdata("terragrunt"), observe)).is_none());
}

/// Expected costs of testdata/terraform/basic (list prices of the golden catalog).
const GOLDEN_BASIC: &[(&str, f64)] = &[
    ("google_compute_address.static", 3.65),
    ("google_compute_address.unused", 7.30),
    ("google_compute_disk.orphan", 85.00),
    ("google_compute_instance.gpu", 741.75),
    ("google_compute_region_instance_group_manager.web", 193.22),
    ("module.vm[\"api\"].google_compute_instance.this", 148.95),
    ("module.vm[\"worker\"].google_compute_instance.this", 62.46),
];
const GOLDEN_BASIC_TOTAL: f64 = 1242.33;

/// Rebuild `testdata/pricing/golden-catalog.json` from the synced USD
/// catalog: only the SKUs used by the fixtures are kept.
#[test]
#[ignore = "regenerates the golden catalog from a synced catalog (just golden-catalog)"]
fn regenerate_golden_catalog() {
    let full = catalog::load(&catalog::catalog_path(&catalog::default_cache_dir(), "USD")).expect("synced catalog (costerra pricing sync)");
    full.load_all();
    let pricer = Pricer::new(Some(full), false);
    for f in FIXTURES {
        report(&pricer, &testdata(f));
    }
    let used: std::collections::HashSet<String> = pricer.used_skus().into_iter().collect();
    let full = pricer.into_catalog().expect("catalog");
    let mut services = vec![];
    let names: Vec<String> = full.service_names().map(str::to_string).collect();
    for name in names {
        let Some(s) = full.service(&name) else { continue };
        let skus: Vec<_> = s.skus.iter().filter(|k| used.contains(&k.id)).cloned().collect();
        if !skus.is_empty() {
            services.push(ServiceSkus { id: s.id.clone(), name: name.clone(), skus });
        }
    }
    services.sort_by(|a, b| a.name.cmp(&b.name));
    let f = CatalogFile {
        version: catalog::CATALOG_VERSION,
        currency: "USD".into(),
        fetched_at: full.fetched_at,
        billing_account: None,
        services,
    };
    std::fs::create_dir_all(golden_path().parent().unwrap()).unwrap();
    std::fs::write(golden_path(), serde_json::to_string_pretty(&f).unwrap() + "\n").unwrap();
    eprintln!("wrote {} ({} SKUs)", golden_path().display(), used.len());
}

/// Observed CPU (mean, p95) of always-on VMs, by (project, address); an
/// empty project is the first one.
fn vms_observed(vms: &'static [(&'static str, &'static str, f64, f64)]) -> impl Fn(&[project::Project]) -> Option<Observed> {
    move |projects| {
        let mut o = Observed::default();
        for (project, address, mean, p95) in vms {
            let project = if project.is_empty() { projects[0].name.as_str() } else { project };
            let usage = [("monthly_hrs".to_string(), 730.0)].into_iter().collect();
            let cpu = Some(CpuStats { mean: *mean, p95: *p95, max: *p95, days: 30.0 });
            o.resources.insert(
                (project.to_string(), address.to_string()),
                ObservedResource { usage, cpu, committed_fraction: None, memory: None, console: None },
            );
        }
        Some(o)
    }
}

/// A copy of a fixture directory, to edit.
fn fixture_copy(p: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let src = testdata(p);
    for e in walkdir::WalkDir::new(&src).into_iter().flatten() {
        let to = dir.path().join(e.path().strip_prefix(&src).unwrap());
        if e.file_type().is_dir() {
            std::fs::create_dir_all(&to).unwrap();
        } else {
            std::fs::copy(e.path(), &to).unwrap();
        }
    }
    dir
}

fn rightsize_plan(path: &Path, observe: impl Fn(&[project::Project]) -> Option<Observed>) -> costerra::rightsize::Plan {
    let opts = LoadOptions { offline: true, cache_dir: std::env::temp_dir().join("costerra-golden"), ..Default::default() };
    let projects = project::load(path, &opts).expect("load fixture");
    let r = report_observed(&golden_pricer(), path, observe);
    costerra::rightsize::plan(&r, &projects, &[])
}

const PROD_APP: [&str; 4] = [
    "google_compute_instance.app[0]",
    "google_compute_instance.app[1]",
    "google_compute_instance.app[2]",
    "google_compute_instance.app[3]",
];

#[test]
fn rightsize_points_at_the_terragrunt_input_and_shows_the_cost_after() {
    let dir = fixture_copy("terragrunt");
    static VMS: [(&str, &str, f64, f64); 4] = [
        ("live/prod/app", PROD_APP[0], 0.08, 0.12),
        ("live/prod/app", PROD_APP[1], 0.08, 0.12),
        ("live/prod/app", PROD_APP[2], 0.08, 0.12),
        ("live/prod/app", PROD_APP[3], 0.08, 0.12),
    ];
    let plan = rightsize_plan(dir.path(), vms_observed(&VMS));
    assert_eq!(plan.suggestions.len(), 1, "{plan:?}");
    let s = &plan.suggestions[0];
    assert_eq!((s.change.from.as_str(), s.change.to.as_str(), s.instances), ("n2-standard-8", "n2-standard-4", 4));
    let [l] = s.locations.as_slice() else { panic!("{s:?}") };
    assert!(l.file.ends_with("live/prod/app/terragrunt.hcl") && l.line == 15, "{l:?}");
    assert!(s.note.is_none(), "{s:?}");
    assert_money("before", s.monthly_cost, 1012.47);
    assert_money("total after", plan.monthly_cost_after, plan.monthly_cost - s.savings());

    assert_eq!(change_by_hand(&plan), 1);
    let src = std::fs::read_to_string(&l.file).unwrap();
    assert!(src.contains("machine_type = \"n2-standard-4\"") && !src.contains("n2-standard-8"), "{src}");
    // The edited code costs what the plan said.
    let after = report(&golden_pricer(), dir.path());
    assert_money("re-estimated total", after.monthly_cost, plan.monthly_cost_after);
}

#[test]
fn rightsize_warns_about_a_value_shared_with_busy_instances() {
    static VMS: [(&str, &str, f64, f64); 4] = [
        ("live/prod/app", PROD_APP[0], 0.08, 0.12),
        ("live/prod/app", PROD_APP[1], 0.08, 0.12),
        ("live/prod/app", PROD_APP[2], 0.08, 0.12),
        ("live/prod/app", PROD_APP[3], 0.70, 0.95),
    ];
    let plan = rightsize_plan(&testdata("terragrunt"), vms_observed(&VMS));
    let s = &plan.suggestions[0];
    assert_eq!(s.instances, 3);
    assert!(s.note.as_deref().unwrap().contains("only 3 of the 4 instances"), "{s:?}");
}

#[test]
fn rightsize_finds_literals_and_defaults_in_terraform() {
    static VMS: [(&str, &str, f64, f64); 2] =
        [("", "google_compute_instance.gpu", 0.05, 0.10), ("", "module.vm[\"api\"].google_compute_instance.this", 0.10, 0.20)];
    let plan = rightsize_plan(&testdata("terraform/basic"), vms_observed(&VMS));
    let at = |addr: &str| {
        let s = plan.suggestions.iter().find(|s| s.address == addr).unwrap_or_else(|| panic!("{addr}: {plan:?}"));
        let [l] = s.locations.as_slice() else { panic!("{s:?}") };
        (l.file.file_name().unwrap().to_string_lossy().to_string(), l.line, s.note.is_none())
    };
    // In the resource block.
    assert_eq!(at("google_compute_instance.gpu"), ("main.tf".into(), 59, true));
    // The optional() default of the root module variable (the worker sets its own).
    assert_eq!(at("module.vm[\"api\"].google_compute_instance.this"), ("main.tf".into(), 16, true));
}

/// Make the suggested changes by hand, as a user would (costerra never edits
/// the code): replace the value on its line, or add the missing attribute.
fn change_by_hand(plan: &costerra::rightsize::Plan) -> usize {
    let mut done = 0;
    // Bottom-up, so that an added line does not shift the next ones.
    let mut todo: Vec<_> = plan.suggestions.iter().filter(|s| s.note.is_none() && s.locations.len() == 1).collect();
    todo.sort_by_key(|s| std::cmp::Reverse(s.locations[0].line));
    for s in todo {
        let [l] = s.locations.as_slice() else { continue };
        let src = std::fs::read_to_string(&l.file).unwrap();
        let mut lines: Vec<String> = src.split('\n').map(str::to_string).collect();
        if l.insert {
            let indent: String = lines[l.line].chars().take_while(|c| c.is_whitespace()).collect();
            lines.insert(l.line, format!("{indent}{}", costerra::rightsize::inserted(&s.change)));
        } else {
            lines[l.line - 1] = costerra::rightsize::replace_value(&lines[l.line - 1], &s.change).unwrap();
        }
        std::fs::write(&l.file, lines.join("\n")).unwrap();
        costerra::tf::parse::invalidate(l.file.parent().unwrap());
        done += 1;
    }
    done
}
