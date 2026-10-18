//! `costerra breakdown`

use super::args::BreakdownArgs;
use super::common::{self, check_total_policy, emit_all, load_config, load_projects, load_usage, price, style};
use super::{insights, prices, ui};
use anyhow::{Result, bail};
use costerra::output::{self, Format};
use costerra::{diff, rightsize};

pub fn run(b: BreakdownArgs, color: bool) -> Result<i32> {
    let a = &b.run;
    a.outputs()?;
    ui::set_details(a.details);
    if b.rightsize && (a.eval.offline || a.no_observed) {
        bail!("--rightsize needs the observed usage of the resources: remove --offline / --no-observed");
    }
    let config = load_config(a)?;
    if b.rightsize && !config.observed_usage.unwrap_or(true) {
        bail!("--rightsize needs the observed usage of the resources: `observed_usage: false` is set in costerra.yml");
    }
    common::start_ui("Evaluating Terraform");
    let prefetch = prices::start_prefetch(a, &config);
    let usage = load_usage(a, &config)?;
    ui::phase("Evaluating Terraform");
    let projects = load_projects(&a.eval, &a.eval.path, &config)?;
    common::lint_usage(&usage, &projects);
    let observing = insights::start_observed(a, &config, &[&projects]);
    let pricer = prices::build_pricer(a, &config, &[&projects], &usage, prefetch)?;
    let observed = insights::finish_observed(observing);
    let show_recs = a.show_recommendations();
    let pending = insights::start(a, &config, &[&projects], pricer.billing_account().map(str::to_string));
    ui::phase("Pricing resources");
    let mut report = price(a, &projects, &config, &pricer, &usage, observed.as_ref(), show_recs || b.rightsize);
    // Before Google recommendations, which replace costerra's of the same kind.
    let plan = b.rightsize.then(|| rightsize::plan(&report, &projects, &a.eval.var_files));
    insights::finish(pending, &mut report, &projects);
    if !show_recs {
        for p in &mut report.projects {
            p.recommendations.clear();
        }
        report.potential_savings = 0.0;
    }
    ui::clear();
    common::report_auth_problems();
    let plan_diff = diff::from_plan(&report);
    emit_all(a, |format, to_file| {
        Ok(match format {
            Format::Json => {
                let mut j = serde_json::to_value(&report)?;
                if let Some(obj) = j.as_object_mut() {
                    // A plan JSON also carries the diff between prior and planned state.
                    if let Some(d) = &plan_diff {
                        obj.insert("plan_diff".into(), serde_json::to_value(d)?);
                    }
                    if let Some(p) = &plan {
                        obj.insert("rightsizing".into(), serde_json::to_value(p)?);
                    }
                }
                serde_json::to_string_pretty(&j)? + "\n"
            }
            Format::Markdown => output::markdown(&report, plan_diff.as_ref(), plan.as_ref()),
            Format::Table => {
                let st = style(a, color, to_file);
                let mut t = output::table(&report, &st);
                if let Some(d) = &plan_diff {
                    t.push('\n');
                    t.push_str(&output::diff_table(d, &st));
                }
                if let Some(p) = &plan {
                    let notes = observed.as_ref().map(|o| o.notes.as_slice()).unwrap_or_default();
                    t.push_str(&output::rightsizing(p, &report, &st, notes));
                }
                t
            }
        })
    })?;
    Ok(check_total_policy(a, &config, &report))
}
