//! `costerra usage …` and `costerra resources`

use super::args::EvalArgs;
use super::common::{load_eval_config, load_opts};
use super::ui::notice;
use anyhow::{Result, bail};
use costerra::profiles::{self, Profile};
use costerra::project;
use costerra::resources::registry;
use std::path::Path;

pub fn profiles(rtype: Option<String>) -> Result<i32> {
    let keys: Vec<&str> = match &rtype {
        Some(t) => match registry().get(t.as_str()) {
            Some(s) => s.usage.iter().map(|u| u.key).collect(),
            None => bail!("{t} is not a priced resource type (see `costerra resources`)"),
        },
        None => profiles::VALUES.iter().map(|(k, _, _)| *k).collect(),
    };
    println!("Example usage profiles (monthly values applied to usage keys without estimates):");
    for p in Profile::ALL {
        println!("  {:<7} {}", p.name(), p.description());
    }
    println!();
    println!("{:<38} {:>10} {:>10} {:>10}  unit", "usage key", "small", "medium", "large");
    for k in keys {
        let Some(unit) = profiles::unit(k) else { continue };
        let v = |p| profiles::value(k, p).map(profiles::fmt_value).unwrap_or_default();
        println!("{k:<38} {:>10} {:>10} {:>10}  {unit}", v(Profile::Small), v(Profile::Medium), v(Profile::Large));
    }
    Ok(0)
}

pub fn resources(filter: Option<String>) -> Result<i32> {
    let mut specs: Vec<_> = registry().values().filter(|s| filter.as_ref().is_none_or(|f| s.rtype.contains(f.as_str()))).collect();
    specs.sort_by_key(|s| s.rtype);
    for s in &specs {
        println!("{}", s.rtype);
        for u in s.usage {
            println!("    {:<36} {}", u.key, u.description);
        }
    }
    println!("\n{} resource types priced", specs.len());
    Ok(0)
}

/// Generate a usage file template for the usage-based resources of the code,
/// evaluated with the same options as `breakdown` (config, var files, excludes).
pub fn init(eval: &EvalArgs, out: &Path, force: bool) -> Result<i32> {
    if out.exists() && !force {
        bail!("{} already exists (use --force to overwrite it)", out.display());
    }
    let config = load_eval_config(eval)?;
    let projects = project::load(&eval.path, &load_opts(eval, &config))?;
    let mut types: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
    for p in &projects {
        for r in &p.resources {
            if let Some(s) = registry().get(r.rtype.as_str())
                && !s.usage.is_empty()
            {
                types.entry(s.rtype).or_default().push(r.address.clone());
            }
        }
    }
    let fmt = |k: &str, p: Profile| profiles::value(k, p).map(profiles::fmt_value);
    let mut y = String::from(
        "# costerra usage file — estimates for usage-based costs.\n# Values are monthly. Remove the comment marker to set a value.\nversion: 0.1\n\nresource_type_default_usage:\n",
    );
    for t in types.keys() {
        let s = &registry()[t];
        y.push_str(&format!("  {t}:\n"));
        for u in s.usage {
            let hint = match fmt(u.key, Profile::Medium) {
                Some(m) => format!(
                    " (examples: small {}, medium {m}, large {})",
                    fmt(u.key, Profile::Small).unwrap_or_default(),
                    fmt(u.key, Profile::Large).unwrap_or_default()
                ),
                None => String::new(),
            };
            y.push_str(&format!("    # {}: 0  # {}{hint}\n", u.key, u.description));
        }
    }
    y.push_str("\nresource_usage:\n");
    for (t, addrs) in &types {
        let s = &registry()[t];
        let mut addrs = addrs.clone();
        addrs.sort();
        addrs.dedup();
        for a in addrs {
            // Addresses with instance keys contain quotes / brackets: quote them for YAML.
            y.push_str(&format!("  {}:\n", serde_json::Value::String(a)));
            for u in s.usage {
                y.push_str(&format!("    # {}: 0\n", u.key));
            }
        }
    }
    std::fs::write(out, y)?;
    notice("info", format!("wrote {} ({} resource types with usage-based costs)", out.display(), types.len()));
    Ok(0)
}
