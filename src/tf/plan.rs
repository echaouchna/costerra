//! Reading `terraform show -json` output (plan or state).

use super::evaluate::{ProviderCfg, ResourceInstance};
use super::parse::Mode;
use crate::hcl::value::Value;
use anyhow::{Context, Result};
use serde_json::Value as J;
use std::collections::HashMap;
use std::path::Path;

pub struct PlanResources {
    /// Resources after the plan is applied (or current state).
    pub planned: Vec<ResourceInstance>,
    /// Resources before the plan (prior state), if available.
    pub prior: Option<Vec<ResourceInstance>>,
}

fn provider_cfg(plan: &J) -> ProviderCfg {
    let pc = &plan["configuration"]["provider_config"];
    let mut cfg = ProviderCfg::default();
    for key in ["google", "google-beta"] {
        let ex = &pc[key]["expressions"];
        let get = |k: &str| ex[k]["constant_value"].as_str().map(str::to_string);
        cfg.project = cfg.project.or(get("project"));
        cfg.region = cfg.region.or(get("region"));
        cfg.zone = cfg.zone.or(get("zone"));
    }
    cfg
}

/// Configuration expressions per resource, keyed by module path (without
/// instance keys) + `type.name`, e.g. `module.vm.google_compute_instance.this`.
fn collect_config(module: &J, prefix: &str, out: &mut HashMap<String, J>) {
    for r in module["resources"].as_array().into_iter().flatten() {
        if let Some(a) = r["address"].as_str() {
            out.insert(format!("{prefix}{a}"), r["expressions"].clone());
        }
    }
    if let Some(calls) = module["module_calls"].as_object() {
        for (name, call) in calls {
            collect_config(&call["module"], &format!("{prefix}module.{name}."), out);
        }
    }
}

fn strip_keys(addr: &str) -> String {
    let mut out = String::with_capacity(addr.len());
    let mut depth = 0;
    let mut in_str = false;
    for c in addr.chars() {
        match c {
            '"' if depth > 0 => in_str = !in_str,
            '[' if !in_str => depth += 1,
            ']' if !in_str => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Resource address referenced by an expression reference like
/// `google_compute_address.nat[0].address`, relative to the module prefix.
fn ref_target(reference: &str, module_prefix: &str) -> Option<(String, String)> {
    let first = reference.split('.').next()?;
    if matches!(first, "var" | "local" | "module" | "data" | "each" | "count" | "path" | "terraform" | "self") || !first.contains('_') {
        return None;
    }
    let mut parts = reference.splitn(3, '.');
    let ty = parts.next()?;
    let name = parts.next()?;
    let base = name.split('[').next().unwrap_or(name);
    Some((format!("{module_prefix}{ty}.{name}"), format!("{ty}.{base}")))
}

/// Walk expressions alongside values: collect referenced resources and fill
/// null attributes that reference other resources with synthetic ids.
fn apply_exprs(expr: &J, val: &mut Value, prefix: &str, refs: &mut Vec<String>) {
    let (Some(eo), Value::Map(vm)) = (expr.as_object(), val) else { return };
    for (k, e) in eo {
        if let Some(list) = e.as_array() {
            if let Some(Value::List(vl)) = vm.get_mut(k) {
                for (ei, vi) in list.iter().zip(vl.iter_mut()) {
                    apply_exprs(ei, vi, prefix, refs);
                }
            }
            continue;
        }
        let Some(rs) = e["references"].as_array() else { continue };
        let mut target = None;
        for r in rs.iter().filter_map(|r| r.as_str()) {
            if let Some((addr, short)) = ref_target(r, prefix) {
                if !refs.contains(&short) {
                    refs.push(short);
                }
                target.get_or_insert(addr);
            }
        }
        if let Some(t) = target {
            let cur = vm.get(k);
            if cur.is_none() || matches!(cur, Some(Value::Null | Value::Unknown)) {
                vm.insert(k.clone(), Value::String(format!("{}{t}", crate::REF_PREFIX)));
            }
        }
    }
}

fn walk(module: &J, provider: &ProviderCfg, config: &HashMap<String, J>, out: &mut Vec<ResourceInstance>) {
    for r in module["resources"].as_array().into_iter().flatten() {
        if r["mode"].as_str() != Some("managed") {
            continue;
        }
        let address = r["address"].as_str().unwrap_or_default().to_string();
        let module_path = module["address"].as_str().unwrap_or_default().to_string();
        let mut attrs = Value::from_json(&r["values"]);
        let mut refs = vec![];
        let prefix = if module_path.is_empty() { String::new() } else { format!("{module_path}.") };
        if let Some(expr) = config.get(&strip_keys(&address)) {
            apply_exprs(expr, &mut attrs, &prefix, &mut refs);
        }
        if let Value::Map(m) = &mut attrs {
            for k in ["id", "self_link", "self_link_unique"] {
                if !matches!(m.get(k), Some(Value::String(_))) {
                    m.insert(k.to_string(), Value::String(format!("{}{address}", crate::REF_PREFIX)));
                }
            }
        }
        refs.sort();
        out.push(ResourceInstance {
            address,
            mode: Mode::Managed,
            rtype: r["type"].as_str().unwrap_or_default().to_string(),
            name: r["name"].as_str().unwrap_or_default().to_string(),
            module_path,
            attrs,
            provider: provider.clone(),
            file: None,
            warnings: vec![],
            refs,
        });
    }
    for c in module["child_modules"].as_array().into_iter().flatten() {
        walk(c, provider, config, out);
    }
}

pub fn load_plan(path: &Path) -> Result<PlanResources> {
    let src = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let plan: J = serde_json::from_slice(&src).with_context(|| format!("parsing {}", path.display()))?;
    let provider = provider_cfg(&plan);
    let mut config = HashMap::new();
    collect_config(&plan["configuration"]["root_module"], "", &mut config);

    let mut planned = vec![];
    let is_plan = plan.get("planned_values").is_some();
    if is_plan {
        walk(&plan["planned_values"]["root_module"], &provider, &config, &mut planned);
    } else {
        walk(&plan["values"]["root_module"], &provider, &config, &mut planned);
    }
    let prior = if is_plan {
        let mut p = vec![];
        walk(&plan["prior_state"]["values"]["root_module"], &provider, &config, &mut p);
        Some(p)
    } else {
        None
    };
    Ok(PlanResources { planned, prior })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_stripped() {
        assert_eq!(strip_keys(r#"module.vm["a.b"].google_x.y[0]"#), "module.vm.google_x.y");
    }

    #[test]
    fn ref_targets() {
        assert_eq!(
            ref_target("google_compute_address.nat[0].address", "module.a."),
            Some(("module.a.google_compute_address.nat[0]".into(), "google_compute_address.nat".into()))
        );
        assert_eq!(ref_target("var.x", ""), None);
    }
}
