//! `costerra diff`

use super::args::DiffArgs;
use super::common::{self, check_total_policy, emit_all, load_config, load_projects, load_usage, price, style};
use super::ui::{self, notice};
use super::{billing, insights, prices};
use anyhow::{Context, Result, bail};
use costerra::cost::Report;
use costerra::diff::{self, DiffReport};
use costerra::output::{self, Format};
use costerra::project::Project;
use std::path::{Path, PathBuf};

/// Extract the tree of `git_ref` into a temporary directory with `git archive`.
/// Unlike `git worktree add`, this only reads the repository: it works on
/// read-only checkouts and as a different user (containers, CI), and leaves
/// nothing behind. Returns the directory and the analysed path inside it.
fn checkout_base(path: &Path, git_ref: &str) -> Result<(tempfile::TempDir, PathBuf)> {
    let abs = std::path::absolute(path)?;
    let start = if abs.is_file() { abs.parent().map(Path::to_path_buf).unwrap_or_else(|| abs.clone()) } else { abs.clone() };
    let o = std::process::Command::new("git").arg("-C").arg(&start).args(["rev-parse", "--show-toplevel"]).output()?;
    if !o.status.success() {
        bail!("{} is not inside a git repository: {}", start.display(), String::from_utf8_lossy(&o.stderr).trim());
    }
    let repo = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
    // `--show-toplevel` resolves symlinks: compare canonical paths.
    let canon = abs.canonicalize().unwrap_or_else(|_| abs.clone());
    let rel = canon.strip_prefix(&repo).or_else(|_| abs.strip_prefix(&repo)).unwrap_or(Path::new("")).to_path_buf();
    let dir = tempfile::Builder::new().prefix("costerra-base-").tempdir()?;
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["archive", "--format=tar", git_ref])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running git archive")?;
    let stdout = child.stdout.take().context("git archive output")?;
    let unpacked = tar::Archive::new(stdout).unpack(dir.path());
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("cannot read {git_ref}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    unpacked.with_context(|| format!("extracting {git_ref}"))?;
    // Mark the repository root, as in the head checkout: project names,
    // `get_repo_root()` and config discovery stop at `.git`.
    std::fs::create_dir_all(dir.path().join(".git"))?;
    let base_path = dir.path().join(rel);
    Ok((dir, base_path))
}

pub fn run(d: DiffArgs, color: bool) -> Result<i32> {
    let a = &d.run;
    a.outputs()?;
    billing::check(a)?;
    ui::set_details(a.details);
    common::start_ui("Evaluating Terraform");
    let config = load_config(a)?;
    let prefetch = prices::start_prefetch(a, &config);
    let usage = load_usage(a, &config)?;
    ui::phase("Evaluating Terraform");
    let head_projects = load_projects(&a.eval, &a.eval.path, &config)?;
    common::lint_usage(&usage, &head_projects);

    enum Base {
        Report(Box<Report>),
        Projects(Vec<Project>),
        Empty,
        Plan,
    }
    let base = if let Some(b) = &d.baseline {
        let src = std::fs::read_to_string(b).with_context(|| format!("reading {}", b.display()))?;
        Base::Report(Box::new(serde_json::from_str(&src).context("parsing baseline report")?))
    } else if let Some(r) = &d.base {
        let (_base_dir, base_path) = checkout_base(&a.eval.path, r)?;
        if base_path.exists() { Base::Projects(load_projects(&a.eval, &base_path, &config)?) } else { Base::Empty }
    } else {
        Base::Plan
    };

    let sets: Vec<&[Project]> = match &base {
        Base::Projects(p) => vec![&head_projects, p],
        _ => vec![&head_projects],
    };
    let observing = insights::start_observed(a, &config, &sets);
    let pricer = prices::build_pricer(a, &config, &sets, &usage, prefetch)?;
    let observed = insights::finish_observed(observing);
    let pending = insights::start(a, &config, &[&head_projects], pricer.billing_account().map(str::to_string));
    // The billing of the resources of the head, as in `breakdown`: the chart
    // is compared with the estimate of the head.
    let billing_pending = billing::start(a, &config, &[&head_projects], pricer.billing_account().map(str::to_string));
    ui::phase("Pricing resources");
    let mut head = price(a, &head_projects, &config, &pricer, &usage, observed.as_ref(), a.show_recommendations());
    insights::finish(pending, &mut head, &head_projects);
    let dr: DiffReport = match base {
        Base::Report(b) => diff::compare(&b, &head),
        Base::Projects(p) => diff::compare(&price(a, &p, &config, &pricer, &usage, observed.as_ref(), a.show_recommendations()), &head),
        Base::Empty => diff::compare(&Report { projects: vec![], monthly_cost: 0.0, ..head.clone() }, &head),
        Base::Plan => match diff::from_plan(&head) {
            Some(pd) => pd,
            None => {
                ui::clear();
                bail!("specify --base <git-ref> or --baseline <report.json> (or pass a plan JSON)")
            }
        },
    };
    let history = billing::finish(billing_pending);
    ui::clear();
    common::report_auth_problems();

    emit_all(a, |format, to_file| {
        Ok(match format {
            Format::Json => {
                let mut j = serde_json::json!({ "diff": dr, "report": head });
                if let (Some(b), Some(obj)) = (&history, j.as_object_mut()) {
                    billing::json(b, obj)?;
                }
                serde_json::to_string_pretty(&j)? + "\n"
            }
            Format::Markdown => output::markdown(&head, Some(&dr), None, history.as_ref()),
            Format::Table => {
                let st = style(a, color, to_file);
                let mut t = output::diff_table(&dr, &st);
                if a.show_recommendations() && head.projects.iter().any(|p| !p.recommendations.is_empty()) {
                    t.push('\n');
                    t.push_str(&output::summary(&head, &st));
                }
                if let Some(h) = &history {
                    t.push_str(&billing::table(h, &head, &st));
                }
                t
            }
        })
    })?;

    let mut code = 0;
    if let Some(l) = d.fail_increase_above.or(config.policies.max_increase)
        && dr.diff > l
    {
        notice("policy", format!("monthly increase {:.2} exceeds limit {l:.2}", dr.diff));
        code = 2;
    }
    if let Some(l) = d.fail_increase_percent.or(config.policies.max_increase_percent)
        && let Some(p) = dr.diff_percent().filter(|p| *p > l)
    {
        notice("policy", format!("monthly increase {p:.1}% exceeds limit {l:.1}%"));
        code = 2;
    }
    Ok(code.max(check_total_policy(a, &config, &head)))
}
