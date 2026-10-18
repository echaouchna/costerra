//! Usage file: estimates for usage-based costs (storage GB, egress, requests…).
//!
//! The format is compatible with Infracost usage files:
//!
//! ```yaml
//! version: 0.1
//! resource_type_default_usage:
//!   google_storage_bucket:
//!     storage_gb: 100
//! resource_usage:
//!   google_storage_bucket.assets:
//!     storage_gb: 1500
//!   module.app.google_compute_instance.web[*]:
//!     monthly_hrs: 200
//! ```

use crate::hcl::value::{Map, Value};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct UsageFile {
    defaults: HashMap<String, Map>,
    exact: HashMap<String, Map>,
    patterns: Vec<(String, Map)>,
    /// Problems found while reading the file (unknown top-level keys…).
    pub warnings: Vec<String>,
}

/// Resource type of a usage file address: `module.a.google_x.y[0]` -> `google_x`.
fn address_type(address: &str) -> Option<String> {
    // Drop instance keys first: they may contain dots (`["a.b"]`).
    let mut plain = String::with_capacity(address.len());
    let mut depth = 0;
    for c in address.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    let parts: Vec<&str> = plain.split('.').collect();
    let n = parts.len();
    (n >= 2).then(|| parts[n - 2].to_string()).filter(|t| t.starts_with("google") && !t.contains('*'))
}

fn to_map(v: &Value) -> Map {
    v.as_map().cloned().unwrap_or_default()
}

/// Wildcard match where `*` matches any sequence of characters.
pub fn wildcard_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut pos = 0;
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() {
            continue;
        }
        if i == 0 {
            if !s.starts_with(p) {
                return false;
            }
            pos = p.len();
        } else if i == parts.len() - 1 {
            return s.len() >= pos + p.len() && s[pos..].ends_with(p);
        } else {
            match s[pos..].find(p) {
                Some(idx) => pos += idx + p.len(),
                None => return false,
            }
        }
    }
    true
}

impl UsageFile {
    pub fn load(path: &Path) -> Result<UsageFile> {
        let src = std::fs::read_to_string(path).with_context(|| format!("reading usage file {}", path.display()))?;
        let v = Value::parse_yaml(&src)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("parsing usage file {}", path.display()))?;
        let mut u = UsageFile::default();
        if let Some(m) = v.as_map() {
            for k in m.keys().filter(|k| !matches!(k.as_str(), "version" | "resource_type_default_usage" | "resource_usage")) {
                u.warnings
                    .push(format!("{}: unknown key '{k}' (expected version, resource_type_default_usage, resource_usage)", path.display()));
            }
        } else if !v.is_null() {
            anyhow::bail!("parsing usage file {}: expected a mapping", path.display());
        }
        if let Some(Value::Map(d)) = v.get("resource_type_default_usage") {
            for (k, m) in d {
                u.defaults.insert(k.clone(), to_map(m));
            }
        }
        if let Some(Value::Map(r)) = v.get("resource_usage") {
            for (k, m) in r {
                if k.contains('*') {
                    u.patterns.push((k.clone(), to_map(m)));
                } else {
                    u.exact.insert(k.clone(), to_map(m));
                }
            }
        }
        Ok(u)
    }

    /// Overlay `other` on top of `self` (other wins).
    pub fn overlay(&self, other: &UsageFile) -> UsageFile {
        let mut u = self.clone();
        for (k, v) in &other.defaults {
            u.defaults.entry(k.clone()).or_default().extend(v.clone());
        }
        for (k, v) in &other.exact {
            u.exact.entry(k.clone()).or_default().extend(v.clone());
        }
        u.patterns.extend(other.patterns.iter().cloned());
        u.warnings.extend(other.warnings.iter().cloned());
        u
    }

    /// Entries that have no effect: addresses matching no resource, and keys
    /// that the resource type does not use (typos such as `monthly_hrz`).
    /// `resources` are (type, address) pairs; `keys_of` returns the usage
    /// keys of a priced type, `None` for types costerra does not price.
    pub fn lint<'a>(&self, resources: &[(&str, &str)], keys_of: impl Fn(&str) -> Option<Vec<&'a str>>) -> Vec<String> {
        let mut out = self.warnings.clone();
        let check_keys = |what: &str, rtype: &str, m: &Map, out: &mut Vec<String>| match keys_of(rtype) {
            Some(keys) => {
                for k in m.keys().filter(|k| !keys.contains(&k.as_str())) {
                    let hint =
                        if keys.is_empty() { "it has no usage keys".to_string() } else { format!("valid keys: {}", keys.join(", ")) };
                    out.push(format!("usage file: {what}: '{k}' is not a usage key of {rtype} ({hint})"));
                }
            }
            None => out.push(format!("usage file: {what}: {rtype} is not a priced resource type, its usage is ignored")),
        };
        let mut defaults: Vec<_> = self.defaults.iter().collect();
        defaults.sort_by(|a, b| a.0.cmp(b.0));
        for (t, m) in defaults {
            check_keys(&format!("resource_type_default_usage.{t}"), t, m, &mut out);
        }
        let mut exact: Vec<_> = self.exact.iter().collect();
        exact.sort_by(|a, b| a.0.cmp(b.0));
        for (addr, m) in exact {
            let matched = resources.iter().any(|(_, a)| a == addr || a.split('[').next() == Some(addr.as_str()));
            if !matched {
                out.push(format!("usage file: resource_usage.{addr} matches no resource"));
            } else if let Some(t) = address_type(addr) {
                check_keys(&format!("resource_usage.{addr}"), &t, m, &mut out);
            }
        }
        for (p, m) in &self.patterns {
            if !resources.iter().any(|(_, a)| wildcard_match(p, a)) {
                out.push(format!("usage file: resource_usage.{p} matches no resource"));
            } else if let Some(t) = address_type(p) {
                check_keys(&format!("resource_usage.{p}"), &t, m, &mut out);
            }
        }
        out
    }

    /// Usage values for a resource: type defaults < wildcard patterns < exact address.
    pub fn for_resource(&self, rtype: &str, address: &str) -> Map {
        let mut out = self.defaults.get(rtype).cloned().unwrap_or_default();
        for (p, m) in &self.patterns {
            if wildcard_match(p, address) {
                out.extend(m.clone());
            }
        }
        // Also allow addressing all instances of a counted resource without index.
        let base = address.split('[').next().unwrap_or(address);
        if base != address
            && let Some(m) = self.exact.get(base)
        {
            out.extend(m.clone());
        }
        if let Some(m) = self.exact.get(address) {
            out.extend(m.clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard() {
        assert!(wildcard_match("module.a.google_x.y[*]", "module.a.google_x.y[3]"));
        assert!(wildcard_match("*.google_x.y", "module.a.google_x.y"));
        assert!(!wildcard_match("module.b.*", "module.a.google_x.y"));
        assert!(wildcard_match("*", "anything"));
    }

    #[test]
    fn lint_finds_typos_and_unmatched_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("u.yml");
        std::fs::write(
            &p,
            "version: 0.1\nresorce_usage: {}\nresource_type_default_usage:\n  google_storage_bucket:\n    storage_gbs: 1\nresource_usage:\n  google_compute_instance.vm[0]:\n    monthly_hrz: 10\n  google_compute_instance.typo:\n    monthly_hrs: 10\n  module.x[*].google_compute_instance.vm:\n    monthly_hrs: 1\n",
        )
        .unwrap();
        let u = UsageFile::load(&p).unwrap();
        let w = u.lint(&[("google_compute_instance", "google_compute_instance.vm[0]")], |t| match t {
            "google_compute_instance" => Some(vec!["monthly_hrs"]),
            "google_storage_bucket" => Some(vec!["storage_gb"]),
            _ => None,
        });
        let all = w.join("\n");
        assert!(all.contains("unknown key 'resorce_usage'"), "{all}");
        assert!(all.contains("'storage_gbs' is not a usage key"), "{all}");
        assert!(all.contains("'monthly_hrz' is not a usage key"), "{all}");
        assert!(all.contains("google_compute_instance.typo matches no resource"), "{all}");
        assert!(all.contains("module.x[*].google_compute_instance.vm matches no resource"), "{all}");
        assert_eq!(w.len(), 5, "{all}");
        assert_eq!(address_type(r#"module.a["x.y"].google_x.n["a.b"]"#).as_deref(), Some("google_x"));
    }
}
