//! Cost differences between two reports (or plan prior vs planned state).

use crate::cost::{CostedResource, ProjectReport, Report};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceDiff {
    pub address: String,
    #[serde(rename = "type")]
    pub rtype: String,
    pub change: Change,
    pub past: f64,
    pub new: f64,
    pub diff: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectDiff {
    pub name: String,
    pub past: f64,
    pub new: f64,
    pub diff: f64,
    pub resources: Vec<ResourceDiff>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub currency: String,
    pub past: f64,
    pub new: f64,
    pub diff: f64,
    pub projects: Vec<ProjectDiff>,
}

impl DiffReport {
    pub fn diff_percent(&self) -> Option<f64> {
        (self.past > 0.0).then(|| self.diff / self.past * 100.0)
    }
}

fn index(rs: &[CostedResource]) -> BTreeMap<&str, &CostedResource> {
    rs.iter().map(|r| (r.address.as_str(), r)).collect()
}

fn diff_resources(past: &[CostedResource], new: &[CostedResource]) -> Vec<ResourceDiff> {
    let p = index(past);
    let n = index(new);
    let keys: BTreeSet<&str> = p.keys().chain(n.keys()).cloned().collect();
    let mut out = vec![];
    for k in keys {
        let (pc, nc) = (p.get(k), n.get(k));
        let past = pc.map(|r| r.monthly_cost).unwrap_or(0.0);
        let newv = nc.map(|r| r.monthly_cost).unwrap_or(0.0);
        let change = match (pc, nc) {
            (None, Some(_)) => Change::Added,
            (Some(_), None) => Change::Removed,
            _ => {
                if (past - newv).abs() < 0.005 && pc.map(|r| &r.components) == nc.map(|r| &r.components) {
                    continue;
                }
                Change::Changed
            }
        };
        if (past - newv).abs() < 0.005 && change != Change::Changed {
            // Added/removed resources without cost (free) are not interesting.
            if past == 0.0 && newv == 0.0 {
                continue;
            }
        }
        let rtype = nc.or(pc).map(|r| r.rtype.clone()).unwrap_or_default();
        out.push(ResourceDiff { address: k.to_string(), rtype, change, past, new: newv, diff: newv - past });
    }
    out.sort_by(|a, b| b.diff.abs().total_cmp(&a.diff.abs()));
    out
}

fn project_diff(name: &str, past: &[CostedResource], new: &[CostedResource]) -> ProjectDiff {
    let pt = crate::cost::sum(past.iter().map(|r| r.monthly_cost));
    let nt = crate::cost::sum(new.iter().map(|r| r.monthly_cost));
    ProjectDiff { name: name.to_string(), past: pt, new: nt, diff: nt - pt, resources: diff_resources(past, new) }
}

/// Compare a base report with a head report, matching projects by name.
pub fn compare(base: &Report, head: &Report) -> DiffReport {
    let mut b: BTreeMap<&str, &ProjectReport> = base.projects.iter().map(|p| (p.name.as_str(), p)).collect();
    let h: BTreeMap<&str, &ProjectReport> = head.projects.iter().map(|p| (p.name.as_str(), p)).collect();
    // A single project on both sides is the same project, even if renamed.
    if b.len() == 1 && h.len() == 1 {
        let hn = *h.keys().next().unwrap();
        let bp = *b.values().next().unwrap();
        b = BTreeMap::from([(hn, bp)]);
    }
    let names: BTreeSet<&str> = b.keys().chain(h.keys()).cloned().collect();
    let empty: Vec<CostedResource> = vec![];
    let projects: Vec<ProjectDiff> = names
        .into_iter()
        .map(|n| project_diff(n, b.get(n).map(|p| &p.resources).unwrap_or(&empty), h.get(n).map(|p| &p.resources).unwrap_or(&empty)))
        .collect();
    finish(head.currency.clone(), projects)
}

/// Diff from plan JSON projects (prior state vs planned values).
pub fn from_plan(head: &Report) -> Option<DiffReport> {
    if head.projects.is_empty() {
        return None;
    }
    let mut projects = vec![];
    for p in &head.projects {
        let past = p.past_resources.as_ref()?;
        projects.push(project_diff(&p.name, past, &p.resources));
    }
    Some(finish(head.currency.clone(), projects))
}

fn finish(currency: String, projects: Vec<ProjectDiff>) -> DiffReport {
    let past = crate::cost::sum(projects.iter().map(|p| p.past));
    let new = crate::cost::sum(projects.iter().map(|p| p.new));
    DiffReport { currency, past, new, diff: new - past, projects }
}
