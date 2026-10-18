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

/// BigQuery storage billing model from observed dataset sizes: physical for
/// data that compresses well, logical for data that does not, with the line
/// to change or add.
#[test]
fn bigquery_billing_model_from_observed_sizes() {
    use costerra::observed::{Observed, ObservedResource};
    let Some(pricer) = pricer() else { return };
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(testdata("services/data/main.tf"), dir.path().join("main.tf")).unwrap();
    let opts = LoadOptions { offline: true, cache_dir: catalog::default_cache_dir(), ..Default::default() };
    let projects = project::load(dir.path(), &opts).expect("load");
    let name = projects[0].name.clone();
    let mut observed = Observed::default();
    let sizes = |v: &[(&str, f64)]| ObservedResource { usage: v.iter().map(|(k, x)| (k.to_string(), *x)).collect(), ..Default::default() };
    // 10x compression.
    observed.resources.insert(
        (name.clone(), "google_bigquery_dataset.analytics".into()),
        sizes(&[
            ("storage_gb", 2000.0),
            ("long_term_storage_gb", 0.0),
            ("physical_storage_gb", 200.0),
            ("long_term_physical_storage_gb", 0.0),
            ("time_travel_storage_gb", 20.0),
            ("fail_safe_storage_gb", 10.0),
        ]),
    );
    // Barely compressed, with many time travel bytes.
    observed.resources.insert(
        (name.clone(), "google_bigquery_dataset.raw".into()),
        sizes(&[
            ("storage_gb", 100.0),
            ("long_term_storage_gb", 0.0),
            ("physical_storage_gb", 95.0),
            ("long_term_physical_storage_gb", 0.0),
            ("time_travel_storage_gb", 60.0),
            ("fail_safe_storage_gb", 20.0),
        ]),
    );
    let (config, usage) = (Config::default(), UsageFile::default());
    let engine = Engine {
        pricer,
        list_pricer: None,
        usage_profile: None,
        examples: false,
        usage: &usage,
        config: &config,
        recommendations: true,
        observed: Some(&observed),
    };
    let report = engine.report(&projects);
    let plan = costerra::rightsize::plan(&report, &projects, &[]);
    let get = |addr: &str| plan.suggestions.iter().find(|s| s.address == addr).unwrap_or_else(|| panic!("{addr}: {plan:#?}"));

    let a = get("google_bigquery_dataset.analytics");
    assert_eq!((a.change.from.as_str(), a.change.to.as_str(), a.change.unset), ("LOGICAL", "PHYSICAL", true));
    let [l] = a.locations.as_slice() else { panic!("{a:?}") };
    assert!(l.insert && l.line == 13 && a.note.is_none(), "{a:?}");
    assert!(a.monthly_cost_after < a.monthly_cost);

    let r = get("google_bigquery_dataset.raw");
    assert_eq!((r.change.from.as_str(), r.change.to.as_str(), r.change.unset), ("PHYSICAL", "LOGICAL", false));
    let [l] = r.locations.as_slice() else { panic!("{r:?}") };
    assert!(!l.insert && l.line == 22 && r.note.is_none(), "{r:?}");

    change_by_hand(&plan);
    let src = std::fs::read_to_string(dir.path().join("main.tf")).unwrap();
    assert!(src.contains("resource \"google_bigquery_dataset\" \"analytics\" {\n  storage_billing_model = \"PHYSICAL\"\n"), "{src}");
    assert!(src.contains("storage_billing_model = \"LOGICAL\""), "{src}");
    // The edited code costs what the plan said.
    let projects = project::load(dir.path(), &opts).expect("reload");
    let after = Engine { recommendations: false, ..engine }.report(&projects);
    assert!((after.monthly_cost - plan.monthly_cost_after).abs() < 0.01, "{} vs {}", after.monthly_cost, plan.monthly_cost_after);
}

/// Report and rightsizing plan of a copy of a service fixture, with observed usage.
fn observed_plan(
    fixture: &str,
    observed: impl Fn(&str) -> costerra::observed::Observed,
) -> Option<(tempfile::TempDir, Report, costerra::rightsize::Plan, f64)> {
    let pricer = pricer()?;
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(testdata(&format!("services/{fixture}/main.tf")), dir.path().join("main.tf")).unwrap();
    let opts = LoadOptions { offline: true, cache_dir: catalog::default_cache_dir(), ..Default::default() };
    let projects = project::load(dir.path(), &opts).expect("load");
    let observed = observed(&projects[0].name);
    let (config, usage) = (Config::default(), UsageFile::default());
    let engine = Engine {
        pricer,
        list_pricer: None,
        usage_profile: None,
        examples: false,
        usage: &usage,
        config: &config,
        recommendations: true,
        observed: Some(&observed),
    };
    let report = engine.report(&projects);
    let plan = costerra::rightsize::plan(&report, &projects, &[]);
    change_by_hand(&plan);
    let projects = project::load(dir.path(), &opts).expect("reload");
    let after = Engine { recommendations: false, ..engine }.report(&projects).monthly_cost;
    Some((dir, report, plan, after))
}

#[test]
fn cloud_sql_tier_from_observed_cpu_and_memory() {
    use costerra::observed::{CpuStats, Observed, ObservedResource};
    let Some((dir, report, plan, after)) = observed_plan("database", |name| {
        let mut o = Observed::default();
        let low = |mean, p95| Some(CpuStats { mean, p95, max: p95, days: 30.0 });
        o.resources.insert(
            (name.into(), "google_sql_database_instance.prod_postgres".into()),
            ObservedResource { cpu: low(0.08, 0.2), memory: low(0.25, 0.35), ..Default::default() },
        );
        // Legacy tier, memory not observed: no smaller tier with the same memory.
        o.resources.insert(
            (name.into(), "google_sql_database_instance.dev_mysql_legacy".into()),
            ObservedResource { cpu: low(0.05, 0.1), ..Default::default() },
        );
        o
    }) else {
        return;
    };
    let ids: Vec<&str> = report.projects[0].recommendations.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids.iter().filter(|i| **i == "sql-rightsize").count(), 1, "{ids:?}");
    assert_eq!(plan.suggestions.len(), 1, "{plan:#?}");
    let s = &plan.suggestions[0];
    assert_eq!(
        (s.change.attribute.as_str(), s.change.from.as_str(), s.change.to.as_str()),
        ("settings.0.tier", "db-custom-4-16384", "db-custom-2-8192")
    );
    let [l] = s.locations.as_slice() else { panic!("{s:?}") };
    assert!(l.line == 23 && s.note.is_none(), "{s:?}");
    let src = std::fs::read_to_string(dir.path().join("main.tf")).unwrap();
    assert!(src.contains("tier              = \"db-custom-2-8192\""), "{src}");
    // The replica keeps its own tier.
    assert!(src.contains("tier        = \"db-custom-4-16384\""), "{src}");
    assert!((after - plan.monthly_cost_after).abs() < 0.01, "{after} vs {}", plan.monthly_cost_after);
}

#[test]
fn cloud_run_min_instances_from_observed_requests() {
    use costerra::observed::{Observed, ObservedResource};
    let Some((dir, _, plan, after)) = observed_plan("serverless", |name| {
        let mut o = Observed::default();
        let requests =
            |n: f64| ObservedResource { usage: [("monthly_requests".to_string(), n)].into_iter().collect(), ..Default::default() };
        o.resources.insert((name.into(), "google_cloud_run_v2_service.api".into()), requests(500.0));
        // Busy enough to keep its warm instance.
        o.resources.insert((name.into(), "google_cloud_run_v2_service.backoffice_dev".into()), requests(2_000_000.0));
        o
    }) else {
        return;
    };
    let s = plan.suggestions.iter().find(|s| s.address == "google_cloud_run_v2_service.api").unwrap_or_else(|| panic!("{plan:#?}"));
    assert_eq!(
        (s.change.attribute.as_str(), s.change.from.as_str(), s.change.to.as_str()),
        ("template.0.scaling.0.min_instance_count", "2", "0")
    );
    assert!(matches!(s.locations.as_slice(), [l] if l.line == 21) && s.note.is_none(), "{s:?}");
    assert!(!plan.suggestions.iter().any(|s| s.address.contains("backoffice_dev")), "{plan:#?}");
    let src = std::fs::read_to_string(dir.path().join("main.tf")).unwrap();
    assert!(src.contains("min_instance_count = 0"), "{src}");
    assert!((after - plan.monthly_cost_after).abs() < 0.01, "{after} vs {}", plan.monthly_cost_after);
}

#[test]
fn cloud_run_limits_follow_observed_cpu_and_memory() {
    use costerra::observed::{CpuStats, Observed, ObservedResource};
    let use_of = |p95: f64, max: f64| Some(CpuStats { mean: p95, p95, max, days: 30.0 });
    let Some((dir, report, plan, after)) = observed_plan("serverless", |name| {
        let mut o = Observed::default();
        // Oversized: 10% CPU, 20% memory at peak.
        o.resources.insert(
            (name.into(), "google_cloud_run_v2_service.api".into()),
            ObservedResource { cpu: use_of(0.1, 0.2), memory: use_of(0.15, 0.2), ..Default::default() },
        );
        // Two containers: the metrics are not per container, nothing is changed.
        o.resources.insert(
            (name.into(), "google_cloud_run_v2_service.reports".into()),
            ObservedResource { cpu: use_of(0.05, 0.1), memory: use_of(0.05, 0.1), ..Default::default() },
        );
        // Saturated: CPU at 90%, memory close to the limit.
        o.resources.insert(
            (name.into(), "google_cloud_run_v2_service.backoffice_dev".into()),
            ObservedResource { cpu: use_of(0.9, 1.0), memory: use_of(0.85, 0.95), ..Default::default() },
        );
        o
    }) else {
        return;
    };
    let get = |addr: &str, id: &str| {
        plan.suggestions
            .iter()
            .find(|s| s.address == addr && s.change.attribute.ends_with(id))
            .unwrap_or_else(|| panic!("{addr} {id}: {plan:#?}"))
    };
    let s = get("google_cloud_run_v2_service.api", "limits.cpu");
    assert_eq!((s.change.from.as_str(), s.change.to.as_str()), ("2", "1"));
    assert!(s.savings() > 0.0 && s.note.is_none(), "{s:?}");
    let s = get("google_cloud_run_v2_service.api", "limits.memory");
    assert_eq!((s.change.from.as_str(), s.change.to.as_str()), ("1Gi", "512Mi"));
    let s = get("google_cloud_run_v2_service.backoffice_dev", "limits.cpu");
    assert_eq!((s.change.from.as_str(), s.change.to.as_str()), ("1000m", "2000m"));
    assert!(s.savings() < 0.0, "upsizing costs more: {s:?}");
    let s = get("google_cloud_run_v2_service.backoffice_dev", "limits.memory");
    assert_eq!((s.change.from.as_str(), s.change.to.as_str()), ("512Mi", "1Gi"));
    assert!(!plan.suggestions.iter().any(|s| s.address.ends_with(".reports")), "{plan:#?}");
    let recs = &report.projects[0].recommendations;
    let up = recs.iter().find(|r| r.id == "run-rightsize-memory" && r.address.contains("backoffice_dev")).unwrap();
    assert!(up.title.starts_with("Increase the memory") && up.monthly_savings.is_some_and(|s| s < 0.0), "{up:?}");

    let src = std::fs::read_to_string(dir.path().join("main.tf")).unwrap();
    for want in [r#"cpu    = "1""#, r#"memory = "512Mi""#, r#"cpu    = "2000m""#, r#"memory = "1Gi""#] {
        assert!(src.contains(want), "missing {want}: {src}");
    }
    assert!((after - plan.monthly_cost_after).abs() < 0.01, "{after} vs {}", plan.monthly_cost_after);
}

#[test]
fn cloud_sql_short_of_cpu_or_memory_is_upsized() {
    use costerra::observed::{CpuStats, Observed, ObservedResource};
    let use_of = |p95: f64| Some(CpuStats { mean: p95, p95, max: p95, days: 30.0 });
    let Some((dir, report, plan, after)) = observed_plan("database", |name| {
        let mut o = Observed::default();
        o.resources.insert(
            (name.into(), "google_sql_database_instance.dev_postgres".into()),
            ObservedResource { cpu: use_of(0.4), memory: use_of(0.95), ..Default::default() },
        );
        o
    }) else {
        return;
    };
    let rec = report.projects[0].recommendations.iter().find(|r| r.id == "sql-upsize").unwrap_or_else(|| panic!("{report:#?}"));
    assert!(rec.title.contains("short of memory") && rec.monthly_savings.is_some_and(|s| s < 0.0), "{rec:?}");
    let s = &plan.suggestions[0];
    assert_eq!((s.change.from.as_str(), s.change.to.as_str()), ("db-custom-2-7680", "db-custom-4-15360"));
    assert!(s.monthly_cost_after > s.monthly_cost);
    assert!(std::fs::read_to_string(dir.path().join("main.tf")).unwrap().contains("\"db-custom-4-15360\""));
    assert!((after - plan.monthly_cost_after).abs() < 0.01, "{after} vs {}", plan.monthly_cost_after);
}

#[test]
fn bigquery_with_logical_bytes_only_gives_the_break_even_ratio() {
    use costerra::observed::{Observed, ObservedResource};
    let Some((_, report, plan, _)) = observed_plan("data", |name| {
        let mut o = Observed::default();
        o.resources.insert(
            (name.into(), "google_bigquery_dataset.analytics".into()),
            ObservedResource { usage: [("storage_gb".to_string(), 2000.0)].into_iter().collect(), ..Default::default() },
        );
        o
    }) else {
        return;
    };
    let rec = report.projects[0]
        .recommendations
        .iter()
        .find(|r| r.id == "bq-physical-storage" && r.address == "google_bigquery_dataset.analytics")
        .unwrap();
    assert!(
        rec.detail.contains("Only the logical size") && rec.detail.contains("2000 GiB") && rec.detail.contains("compresses better than"),
        "{rec:?}"
    );
    assert!(rec.change.is_none() && rec.monthly_savings.is_none());
    // Advice only: no change of the code.
    assert!(!plan.suggestions.iter().any(|s| s.address == "google_bigquery_dataset.analytics"), "{plan:#?}");
}

#[test]
fn cloud_run_change_without_priced_usage_is_not_estimated() {
    use costerra::observed::{CpuStats, Observed, ObservedResource};
    let Some(pricer) = pricer() else { return };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.tf"),
        r#"provider "google" {
  project = "p"
  region  = "europe-west1"
}

resource "google_cloud_run_v2_service" "jobs" {
  name     = "jobs"
  location = "europe-west1"
  template {
    containers {
      image = "img"
      resources {
        limits = {
          "cpu"    = "2000m"
          "memory" = "2048Mi" # for the workers
        }
        cpu_idle = true
      }
    }
  }
}
"#,
    )
    .unwrap();
    let opts = LoadOptions { offline: true, cache_dir: catalog::default_cache_dir(), ..Default::default() };
    let projects = project::load(dir.path(), &opts).expect("load");
    let mut observed = Observed::default();
    let use_of = |p95: f64, max: f64| Some(CpuStats { mean: p95, p95, max, days: 30.0 });
    observed.resources.insert(
        (projects[0].name.clone(), "google_cloud_run_v2_service.jobs".into()),
        ObservedResource { cpu: use_of(0.5, 0.7), memory: use_of(0.84, 1.0), ..Default::default() },
    );
    let (config, usage) = (Config::default(), UsageFile::default());
    let engine = Engine {
        pricer,
        list_pricer: None,
        usage_profile: None,
        examples: false,
        usage: &usage,
        config: &config,
        recommendations: true,
        observed: Some(&observed),
    };
    let report = engine.report(&projects);
    let plan = costerra::rightsize::plan(&report, &projects, &[]);
    assert_eq!(plan.suggestions.len(), 1, "{plan:#?}");
    let s = &plan.suggestions[0];
    assert_eq!((s.change.from.as_str(), s.change.to.as_str()), ("2048Mi", "4Gi"));
    assert!(!s.cost_known && s.note.is_none(), "{s:?}");
    assert_eq!(s.locations[0].text, r#""memory" = "2048Mi" # for the workers"#);
    let st = costerra::output::Style { color: false, show_all: false, show_warnings: false, group: true, details: false };
    let out = costerra::output::rightsizing(&plan, &report, &st, &[]);
    assert!(out.contains("change not estimated") && !out.contains("saves $0.00"), "{out}");
    assert!(out.contains(r#"+ "memory" = "4Gi" # for the workers"#), "{out}");
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

#[test]
fn gcs_read_from_another_region() {
    use costerra::observed::{Observed, ObservedResource};
    let Some(pricer) = pricer() else { return };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.tf"),
        r#"provider "google" {
  project = "p"
  region  = "europe-west1"
}

resource "google_storage_bucket" "data" {
  name     = "acme-data"
  location = "europe-west1"
}

resource "google_storage_bucket" "shared" {
  name     = "acme-shared"
  location = "EU"
}

# Reads acme-data from another region, acme-shared from inside the EU multi-region.
resource "google_cloud_run_v2_service" "api" {
  name     = "api"
  location = "us-central1"
  template {
    containers {
      image = "img"
      env {
        name  = "DATA"
        value = "gs://${google_storage_bucket.data.name}/in"
      }
    }
  }
}

resource "google_compute_instance" "worker" {
  name         = "worker"
  machine_type = "e2-small"
  zone         = "europe-west1-b"
  metadata = {
    data   = google_storage_bucket.data.name
    shared = google_storage_bucket.shared.name
  }
  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
    }
  }
  network_interface {
    network = "default"
  }
}
"#,
    )
    .unwrap();
    let opts = LoadOptions { offline: true, cache_dir: catalog::default_cache_dir(), ..Default::default() };
    let projects = project::load(dir.path(), &opts).expect("load");
    let mut observed = Observed::default();
    observed.resources.insert(
        (projects[0].name.clone(), "google_storage_bucket.data".into()),
        ObservedResource { usage: [("sent_gb".to_string(), 2000.0)].into_iter().collect(), ..Default::default() },
    );
    let (config, usage) = (Config::default(), UsageFile::default());
    let engine = Engine {
        pricer,
        list_pricer: None,
        usage_profile: None,
        examples: false,
        usage: &usage,
        config: &config,
        recommendations: true,
        observed: Some(&observed),
    };
    let report = engine.report(&projects);
    let recs: Vec<_> = report.projects[0].recommendations.iter().filter(|r| r.id == "gcs-cross-region").collect();
    assert_eq!(recs.len(), 1, "{recs:#?}");
    let r = recs[0];
    assert_eq!(r.address, "google_storage_bucket.data");
    assert!(r.detail.contains("google_cloud_run_v2_service.api (us-central1)") && !r.detail.contains("worker"), "{r:?}");
    assert!(r.detail.contains("sent 2000 GiB a month") && r.monthly_savings.is_some_and(|s| s > 1.0), "{r:?}");
    assert_eq!(r.basis, costerra::cost::Basis::Observed);
}
