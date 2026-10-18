//! End-to-end checks over `testdata/`. Requires a synced pricing catalog
//! (`costerra pricing sync`, or COSTERRA_CATALOG); skipped otherwise.

use costerra::config::Config;
use costerra::cost::{Report, Status};
use costerra::engine::Engine;
use costerra::pricing::catalog;
use costerra::pricing::query::Pricer;
use costerra::project::{self, LoadOptions};
use costerra::usage::UsageFile;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The synced catalog. Locally the tests are skipped (with a message) when
/// it is missing; in CI (`CI` set) a missing catalog fails, so that the tests
/// cannot pass without checking anything. `COSTERRA_SKIP_CATALOG_TESTS=1`
/// skips them explicitly (e.g. CI without an API key: the golden tests
/// still check exact costs with the committed price list).
fn pricer() -> Option<&'static Pricer> {
    static P: OnceLock<Option<Pricer>> = OnceLock::new();
    P.get_or_init(|| {
        let path = catalog::catalog_path(&catalog::default_cache_dir(), "USD");
        match catalog::load(&path) {
            Ok(c) => Some(Pricer::new(Some(c), false)),
            Err(e) if std::env::var_os("COSTERRA_SKIP_CATALOG_TESTS").is_some() => {
                eprintln!("skipping catalog fixture tests: {e:#}");
                None
            }
            Err(e) if std::env::var_os("CI").is_some() => {
                panic!(
                    "no synced pricing catalog at {} ({e:#}): run `costerra pricing sync` or set COSTERRA_SKIP_CATALOG_TESTS=1",
                    path.display()
                )
            }
            Err(e) => {
                eprintln!("skipping catalog fixture tests (run `costerra pricing sync`): {e:#}");
                None
            }
        }
    })
    .as_ref()
}

fn run(path: &Path) -> Option<Report> {
    let pricer = pricer()?;
    let opts = LoadOptions { offline: true, cache_dir: catalog::default_cache_dir(), ..Default::default() };
    let projects = project::load(path, &opts).expect("load");
    let config = Config::default();
    let usage = match std::fs::read_dir(path).ok()?.flatten().find(|e| e.file_name().to_string_lossy().ends_with("usage.yml")) {
        Some(e) => UsageFile::load(&e.path()).expect("usage file"),
        None => UsageFile::default(),
    };
    let engine = Engine {
        pricer,
        list_pricer: None,
        usage_profile: None,
        examples: true,
        usage: &usage,
        config: &config,
        recommendations: true,
        observed: None,
    };
    Some(engine.report(&projects))
}

fn testdata(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(p)
}

fn assert_fully_priced(report: &Report) {
    for p in &report.projects {
        for r in &p.resources {
            for w in &r.warnings {
                assert!(!w.contains("no price found"), "{} / {}: {w}", p.name, r.address);
                assert!(!w.contains("unknown machine type"), "{} / {}: {w}", p.name, r.address);
            }
            if r.status == Status::Priced {
                for c in &r.components {
                    assert!(!c.is_unpriced(), "{} / {}: component '{}' has no price", p.name, r.address, c.name);
                }
            }
        }
    }
}

#[test]
fn terraform_basic() {
    let Some(r) = run(&testdata("terraform/basic")) else { return };
    assert_fully_priced(&r);
    let p = &r.projects[0];
    let addrs: Vec<&str> = p.resources.iter().filter(|r| r.monthly_cost > 0.0).map(|r| r.address.as_str()).collect();
    for a in [
        "google_compute_instance.gpu",
        "module.vm[\"api\"].google_compute_instance.this",
        "module.vm[\"worker\"].google_compute_instance.this",
        "google_compute_region_instance_group_manager.web",
        "google_compute_disk.orphan",
    ] {
        assert!(addrs.contains(&a), "{a} not priced: {addrs:?}");
    }
    let ids: Vec<&str> = p.recommendations.iter().map(|r| r.id.as_str()).collect();
    for id in ["compute-spot-nonprod", "disk-unattached", "ip-unused", "disk-ssd-to-balanced"] {
        assert!(ids.contains(&id), "missing recommendation {id}: {ids:?}");
    }
}

#[test]
fn terragrunt_live() {
    let Some(r) = run(&testdata("terragrunt")) else { return };
    assert_fully_priced(&r);
    let names: Vec<&str> = r.projects.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["live/dev/app", "live/dev/network", "live/prod/app", "live/prod/network"]);
    let prod = r.projects.iter().find(|p| p.name == "live/prod/app").unwrap();
    assert_eq!(prod.resources.iter().filter(|r| r.rtype == "google_compute_instance").count(), 4);
    assert_eq!(prod.resources[0].region.as_deref(), Some("europe-west4"));
}

#[test]
fn service_fixtures_fully_priced() {
    let dir = testdata("services");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten().filter(|e| e.path().is_dir()) {
        let Some(r) = run(&e.path()) else { return };
        assert_fully_priced(&r);
        assert!(r.monthly_cost > 0.0, "{}: zero total", e.path().display());
    }
}
