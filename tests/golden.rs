//! Golden-value tests: exact monthly costs of `testdata/` fixtures, priced
//! with a small committed price list (`testdata/pricing/golden-catalog.json`,
//! only the SKUs these fixtures use). They need no network nor credentials,
//! and catch pricing regressions in the cost models.
//!
//! Regenerate the price list from a synced catalog when a model starts using
//! other SKUs: `just golden-catalog` (then review the diff of expected values).

use costerra::config::Config;
use costerra::cost::Report;
use costerra::engine::Engine;
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
    let opts = LoadOptions { offline: true, cache_dir: std::env::temp_dir().join("costerra-golden"), ..Default::default() };
    let projects = project::load(path, &opts).expect("load fixture");
    let (config, usage) = (Config::default(), UsageFile::default());
    let engine =
        Engine { pricer, list_pricer: None, usage_profile: None, examples: false, usage: &usage, config: &config, recommendations: true };
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
