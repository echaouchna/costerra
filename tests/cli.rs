//! End-to-end tests of the `costerra` binary, priced with the committed golden
//! price list (no network, no credentials).

use costerra::pricing::catalog::{self, CatalogFile};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn testdata(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(p)
}

/// A cache directory containing the golden price list.
fn cache_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let src = std::fs::read_to_string(testdata("pricing/golden-catalog.json")).unwrap();
    let f: CatalogFile = serde_json::from_str(&src).unwrap();
    catalog::save(&catalog::catalog_path(dir.path(), "USD"), &f).unwrap();
    dir
}

fn costerra(cache: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_costerra"))
        .current_dir(cwd)
        .arg("--cache-dir")
        .arg(cache)
        .args(["--color", "never"])
        .args(args)
        .env("COSTERRA_NO_INSIGHTS", "1")
        .env("COSTERRA_NO_OBSERVED", "1")
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn copy_dir(src: &Path, dst: &Path) {
    for e in walkdir::WalkDir::new(src).into_iter().flatten() {
        let rel = e.path().strip_prefix(src).unwrap();
        let to = dst.join(rel);
        if e.file_type().is_dir() {
            std::fs::create_dir_all(&to).unwrap();
        } else {
            std::fs::copy(e.path(), &to).unwrap();
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", text(&o.stderr));
}

const OFFLINE: &[&str] = &["--offline", "--list-prices"];

#[test]
fn breakdown_json_and_markdown_agree_on_savings() {
    let cache = cache_dir();
    let mut args = vec!["breakdown", "terragrunt", "-f", "json"];
    args.extend(OFFLINE);
    let o = costerra(cache.path(), &testdata(""), &args);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let j: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!((j["monthly_cost"].as_f64().unwrap() - 1291.46).abs() < 0.01);
    assert!((j["potential_savings"].as_f64().unwrap() - 336.06).abs() < 0.01);

    let mut args = vec!["breakdown", "terragrunt", "-f", "markdown"];
    args.extend(OFFLINE);
    let o = costerra(cache.path(), &testdata(""), &args);
    let md = text(&o.stdout);
    assert!(md.contains("potential savings $336.06/month"), "{md}");
}

#[test]
fn table_groups_identical_instances() {
    let cache = cache_dir();
    let mut args = vec!["breakdown", "terragrunt"];
    args.extend(OFFLINE);
    let out = text(&costerra(cache.path(), &testdata(""), &args).stdout);
    assert!(out.contains("google_compute_instance.app[*] ×4"), "{out}");
    assert!(out.contains("2,920  hours"), "{out}");
    args.push("--no-group");
    let out = text(&costerra(cache.path(), &testdata(""), &args).stdout);
    assert!(out.contains("google_compute_instance.app[3]") && !out.contains("×4"), "{out}");
}

#[test]
fn diff_against_git_ref() {
    let cache = cache_dir();
    let repo = tempfile::tempdir().unwrap();
    copy_dir(&testdata("terragrunt"), repo.path());
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "base"]);
    let hcl = repo.path().join("live/prod/app/terragrunt.hcl");
    std::fs::write(&hcl, std::fs::read_to_string(&hcl).unwrap().replace("replicas     = 4", "replicas     = 5")).unwrap();
    let mut args = vec!["diff", ".", "--base", "main", "-f", "json", "--fail-increase-above", "100"];
    args.extend(OFFLINE);
    let o = costerra(cache.path(), repo.path(), &args);
    assert_eq!(o.status.code(), Some(2), "policy must fail: {}", text(&o.stderr));
    let j: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!((j["diff"]["diff"].as_f64().unwrap() - 253.12).abs() < 0.01, "{}", j["diff"]);
    assert!(text(&o.stderr).contains("monthly increase 253.12 exceeds limit 100.00"));
    // Read-only: no worktree left behind.
    assert!(!repo.path().join(".git/worktrees").exists());
}

#[test]
fn config_typos_are_errors() {
    let cache = cache_dir();
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&testdata("terragrunt"), dir.path());
    std::fs::write(dir.path().join("costerra.yml"), "polices:\n  max_monthly_cost: 1\n").unwrap();
    let mut args = vec!["breakdown", "."];
    args.extend(OFFLINE);
    let o = costerra(cache.path(), dir.path(), &args);
    assert_eq!(o.status.code(), Some(1));
    let err = text(&o.stderr);
    assert!(err.contains("polices") && err.contains("costerra.yml"), "{err}");

    std::fs::write(dir.path().join("costerra.yml"), "policies:\n  max_monthly_cost: 1\n").unwrap();
    let o = costerra(cache.path(), dir.path(), &args);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o.stderr));
}

#[test]
fn invalid_var_is_rejected() {
    let cache = cache_dir();
    let o = costerra(cache.path(), &testdata(""), &["breakdown", "terraform/basic", "--offline", "--var", "foo"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("expected NAME=VALUE"), "{}", text(&o.stderr));
}

#[test]
fn usage_file_typos_are_reported() {
    let cache = cache_dir();
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&testdata("terragrunt"), dir.path());
    std::fs::write(
        dir.path().join("costerra-usage.yml"),
        "version: 0.1\nresource_usage:\n  google_compute_instance.app[0]:\n    monthly_hrz: 10\n  google_compute_instanc.typo:\n    monthly_hrs: 1\n",
    )
    .unwrap();
    let mut args = vec!["breakdown", "."];
    args.extend(OFFLINE);
    let o = costerra(cache.path(), dir.path(), &args);
    assert!(o.status.success());
    let err = text(&o.stderr);
    assert!(err.contains("'monthly_hrz' is not a usage key of google_compute_instance"), "{err}");
    assert!(err.contains("google_compute_instanc.typo matches no resource"), "{err}");
}

#[test]
fn usage_init_does_not_overwrite() {
    let cache = cache_dir();
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&testdata("terraform/basic"), dir.path());
    let o = costerra(cache.path(), dir.path(), &["usage", "init", ".", "--offline"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let first = std::fs::read_to_string(dir.path().join("costerra-usage.yml")).unwrap();
    assert!(first.contains("google_compute_instance"), "{first}");
    // The template must be valid YAML (addresses with quotes are quoted).
    costerra::usage::UsageFile::load(&dir.path().join("costerra-usage.yml")).unwrap();
    let o = costerra(cache.path(), dir.path(), &["usage", "init", ".", "--offline"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("--force"));
    let o = costerra(cache.path(), dir.path(), &["usage", "init", ".", "--offline", "--force"]);
    assert!(o.status.success());
}

#[test]
fn observed_usage_settings_and_removed_rabbit_options() {
    let cache = cache_dir();
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&testdata("terragrunt"), dir.path());
    let mut args = vec!["breakdown", ".", "--no-observed"];
    args.extend(OFFLINE);

    std::fs::write(dir.path().join("costerra.yml"), "observed_usage: false\nquota_project: my-quota\n").unwrap();
    let o = costerra(cache.path(), dir.path(), &args);
    assert!(o.status.success(), "{}", text(&o.stderr));

    // Rabbit was removed in 0.2.0: its keys and flags are errors.
    std::fs::write(dir.path().join("costerra.yml"), "rabbit_review: true\n").unwrap();
    let o = costerra(cache.path(), dir.path(), &args);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("rabbit_review"), "{}", text(&o.stderr));
    std::fs::remove_file(dir.path().join("costerra.yml")).unwrap();
    let o = costerra(cache.path(), dir.path(), &["breakdown", ".", "--rabbit-review"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("--rabbit-review"), "{}", text(&o.stderr));
}

#[test]
fn recommendations_say_what_they_are_based_on() {
    let cache = cache_dir();
    let mut args = vec!["breakdown", "terragrunt", "-f", "json"];
    args.extend(OFFLINE);
    let o = costerra(cache.path(), &testdata(""), &args);
    let j: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let recs: Vec<&serde_json::Value> =
        j["projects"].as_array().unwrap().iter().flat_map(|p| p["recommendations"].as_array().unwrap()).collect();
    assert!(!recs.is_empty());
    assert!(recs.iter().all(|r| r["basis"] == "code"), "no usage file nor observed usage");
    assert!(j.get("observed").is_none());

    let mut args = vec!["breakdown", "terragrunt"];
    args.extend(OFFLINE);
    let out = text(&costerra(cache.path(), &testdata(""), &args).stdout);
    assert!(out.contains("Source: costerra rule compute-cud, based on the code"), "{out}");
}

#[test]
fn unpriced_resources_cost_zero_not_minus_zero() {
    let cache = cache_dir();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.tf"), "resource \"google_storage_bucket\" \"b\" {\n  name     = \"b\"\n  location = \"EU\"\n}\n")
        .unwrap();
    let mut args = vec!["breakdown", ".", "-f", "json"];
    args.extend(OFFLINE);
    let o = costerra(cache.path(), dir.path(), &args);
    let out = text(&o.stdout);
    assert!(!out.contains("-0.0"), "{out}");
}
