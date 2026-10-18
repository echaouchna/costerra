//! `costerra breakdown`

use super::args::RunArgs;
use super::common::{self, check_total_policy, emit, load_config, load_projects, load_usage, price, style};
use super::{insights, prices, ui};
use anyhow::Result;
use costerra::diff;
use costerra::output::{self, Format};

pub fn run(a: RunArgs, color: bool) -> Result<i32> {
    let config = load_config(&a)?;
    if a.rabbit_review_dry_run {
        let root = insights::review_root(&a.eval.path);
        print!("{}", costerra::insights::review_dry_run(&root, &common::excludes(&a.eval, &config)));
        return Ok(0);
    }
    common::start_ui("Evaluating Terraform");
    let prefetch = prices::start_prefetch(&a, &config);
    let usage = load_usage(&a, &config)?;
    ui::phase("Evaluating Terraform");
    let projects = load_projects(&a.eval, &a.eval.path, &config)?;
    common::lint_usage(&usage, &projects);
    let pricer = prices::build_pricer(&a, &config, &[&projects], &usage, prefetch)?;
    let pending = insights::start(&a, &config, &[&projects], pricer.billing_account().map(str::to_string));
    ui::phase("Pricing resources");
    let mut report = price(&a, &projects, &config, &pricer, &usage);
    insights::finish(pending, &mut report, &projects);
    ui::clear();
    let plan_diff = diff::from_plan(&report);
    let text = match a.format {
        Format::Json => {
            let mut j = serde_json::to_value(&report)?;
            // A plan JSON also carries the diff between prior and planned state.
            if let (Some(d), Some(obj)) = (&plan_diff, j.as_object_mut()) {
                obj.insert("plan_diff".into(), serde_json::to_value(d)?);
            }
            serde_json::to_string_pretty(&j)? + "\n"
        }
        Format::Markdown => output::markdown(&report, plan_diff.as_ref()),
        Format::Table => {
            let st = style(&a, color);
            let mut t = output::table(&report, &st);
            if let Some(d) = &plan_diff {
                t.push('\n');
                t.push_str(&output::diff_table(d, &st));
            }
            t
        }
    };
    emit(&a, text)?;
    Ok(check_total_policy(&a, &config, &report))
}
