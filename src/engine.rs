//! Cost estimation engine: prices every resource of every project and runs
//! optimization rules.

use crate::config::Config;
use crate::cost::{Basis, CostedResource, ProjectReport, Recommendation, Report, Status, Summary, UsageExamples};
use crate::hcl::value::{Map, Value};
use crate::observed::Observed;
use crate::pricing::query::Pricer;
use crate::profiles::{self, Profile};
use crate::project::Project;
use crate::resources::{Ctx, Env, is_free, registry};
use crate::tf::evaluate::ResourceInstance;
use crate::usage::UsageFile;
use rayon::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

pub struct Engine<'a> {
    pub pricer: &'a Pricer,
    /// List-price view of `pricer` when it holds contract prices.
    pub list_pricer: Option<&'a Pricer>,
    /// Apply this example profile to usage keys without estimates.
    pub usage_profile: Option<Profile>,
    /// Compute small/medium/large example costs for usage-based resources.
    pub examples: bool,
    pub usage: &'a UsageFile,
    pub config: &'a Config,
    pub recommendations: bool,
    /// Usage observed in Google Cloud (fills keys missing from usage files).
    pub observed: Option<&'a Observed>,
}

fn tokens(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()).map(|t| t.to_lowercase())
}

impl Engine<'_> {
    pub fn detect_env(&self, text: &str) -> Env {
        let mut env = Env::Unknown;
        for t in tokens(text) {
            if self.config.nonprod_patterns().contains(&t) {
                return Env::NonProd;
            }
            if self.config.prod_patterns().contains(&t) {
                env = Env::Prod;
            }
        }
        env
    }

    fn label_env(&self, r: &ResourceInstance) -> Env {
        let labels = r
            .attrs
            .get("labels")
            .or_else(|| r.attrs.get("resource_labels"))
            .or_else(|| r.attrs.get("user_labels"))
            .or_else(|| r.attrs.path("metadata.0.labels"))
            .or_else(|| r.attrs.path("settings.0.user_labels"));
        if let Some(m) = labels.and_then(|l| l.as_map()) {
            for k in ["env", "environment", "stage", "tier"] {
                if let Some(v) = m.get(k).and_then(|v| v.as_str()) {
                    let e = self.detect_env(v);
                    if e != Env::Unknown {
                        return e;
                    }
                }
            }
        }
        Env::Unknown
    }

    /// Environment of a resource: its labels, the project, then the labels of
    /// resources it references (e.g. node pool -> cluster), the GCP project id
    /// and finally its address.
    fn resource_env(&self, project_env: Env, r: &ResourceInstance, all: &[ResourceInstance]) -> Env {
        let e = self.label_env(r);
        if e != Env::Unknown {
            return e;
        }
        if project_env != Env::Unknown {
            return project_env;
        }
        for target in &r.refs {
            let prefix = if r.module_path.is_empty() { String::new() } else { format!("{}.", r.module_path) };
            let addr = format!("{prefix}{target}");
            if let Some(t) = all.iter().find(|x| x.address == addr || x.address.starts_with(&format!("{addr}["))) {
                let e = self.label_env(t);
                if e != Env::Unknown {
                    return e;
                }
            }
        }
        let e = r.provider.project.as_deref().map(|p| self.detect_env(p)).unwrap_or(Env::Unknown);
        if e != Env::Unknown { e } else { self.detect_env(&r.address) }
    }

    pub fn estimate_resources(
        &self,
        resources: &[ResourceInstance],
        project_env: Env,
        with_recs: bool,
    ) -> (Vec<CostedResource>, Vec<Recommendation>) {
        self.estimate_with_usage(resources, project_env, with_recs, self.usage, None)
    }

    fn estimate_with_usage(
        &self,
        resources: &[ResourceInstance],
        project_env: Env,
        with_recs: bool,
        usage_file: &UsageFile,
        project: Option<&str>,
    ) -> (Vec<CostedResource>, Vec<Recommendation>) {
        let default_region = self.config.default_region.as_deref().unwrap_or("us-central1");
        let results: Vec<(CostedResource, Vec<Recommendation>)> = resources
            .par_iter()
            .map(|r| {
                let Some(spec) = registry().get(r.rtype.as_str()) else {
                    let status = if is_free(&r.rtype) { Status::Free } else { Status::Unsupported };
                    return (
                        CostedResource {
                            address: r.address.clone(),
                            rtype: r.rtype.clone(),
                            region: None,
                            status,
                            monthly_cost: 0.0,
                            list_monthly_cost: None,
                            has_unpriced_usage: false,
                            usage_examples: None,
                            list_usage_examples: None,
                            usage_profile: None,
                            components: vec![],
                            warnings: vec![],
                        },
                        vec![],
                    );
                };
                let mut explicit = usage_file.for_resource(&r.rtype, &r.address);
                // Observed usage fills the keys the usage file does not set.
                let observed = project.zip(self.observed).and_then(|(p, o)| o.get(p, &r.address));
                let mut observed_keys: Vec<String> = vec![];
                for (k, v) in observed.iter().flat_map(|o| o.usage.iter()) {
                    if spec.usage.iter().any(|u| u.key == k) && !explicit.contains_key(k) {
                        explicit.insert(k.clone(), Value::Number(*v));
                        observed_keys.push(k.clone());
                    }
                }
                // Usage keys of this resource that have no estimate but an example value.
                let missing: Vec<&str> = spec
                    .usage
                    .iter()
                    .map(|u| u.key)
                    .filter(|k| !explicit.contains_key(*k) && profiles::value(k, Profile::Small).is_some())
                    .collect();
                let with_profile = |p: Profile| -> Map {
                    let mut u = explicit.clone();
                    for k in &missing {
                        if let Some(v) = profiles::value(k, p) {
                            u.insert(k.to_string(), Value::Number(v));
                        }
                    }
                    u
                };
                let env = self.resource_env(project_env, r, resources);
                let run = |pricer: &Pricer, usage: &Map| -> Vec<crate::cost::Component> {
                    let c = Ctx {
                        r,
                        attrs: &r.attrs,
                        pricer,
                        usage,
                        default_region,
                        env,
                        siblings: resources,
                        warnings: RefCell::new(vec![]),
                        location: RefCell::new(None),
                        observed,
                        observed_keys: &observed_keys,
                        basis: Cell::new(Basis::Code),
                    };
                    (spec.estimate)(&c)
                };
                let ctx = Ctx {
                    r,
                    attrs: &r.attrs,
                    pricer: self.pricer,
                    usage: &explicit,
                    default_region,
                    env,
                    siblings: resources,
                    warnings: RefCell::new(r.warnings.clone()),
                    location: RefCell::new(None),
                    observed,
                    observed_keys: &observed_keys,
                    basis: Cell::new(Basis::Code),
                };
                let base = (spec.estimate)(&ctx);
                let usage_based = !missing.is_empty() && base.iter().any(|c| c.usage_based && c.monthly_cost.is_none());
                // Example profiles only fill components the model could not price.
                let with_examples =
                    |pricer: &Pricer, base: &[crate::cost::Component], p: Profile| merge_components(base, run(pricer, &with_profile(p)));
                let mut components = match self.usage_profile {
                    Some(p) if usage_based => with_examples(self.pricer, &base, p),
                    _ => base.clone(),
                };
                let monthly_cost = CostedResource::total(&components);
                let list_monthly_cost = self.list_pricer.map(|lp| {
                    let lbase = run(lp, &explicit);
                    let list = match self.usage_profile {
                        Some(p) if usage_based => with_examples(lp, &lbase, p),
                        _ => lbase,
                    };
                    for c in components.iter_mut() {
                        if let Some(l) = list.iter().find(|l| l.name == c.name) {
                            c.list_unit_price = l.unit_price;
                            c.list_monthly_cost = l.monthly_cost;
                        }
                    }
                    CostedResource::total(&list)
                });
                let examples_for = |pricer: &Pricer, base: &[crate::cost::Component]| {
                    let total = |p: Profile| CostedResource::total(&with_examples(pricer, base, p));
                    UsageExamples { small: total(Profile::Small), medium: total(Profile::Medium), large: total(Profile::Large) }
                };
                let usage_examples = (self.examples && usage_based).then(|| examples_for(self.pricer, &base));
                let list_usage_examples =
                    self.list_pricer.filter(|_| self.examples && usage_based).map(|lp| examples_for(lp, &run(lp, &explicit)));
                let usage_profile = self.usage_profile.filter(|_| usage_based).map(|p| p.name().to_string());
                let region = ctx.location.borrow().clone().or_else(|| ctx.region_opt());
                let cr = CostedResource {
                    address: r.address.clone(),
                    rtype: r.rtype.clone(),
                    region,
                    status: Status::Priced,
                    monthly_cost,
                    list_monthly_cost,
                    has_unpriced_usage: components.iter().any(|c| c.usage_based && c.monthly_cost.is_none()),
                    usage_examples,
                    list_usage_examples,
                    usage_profile,
                    components,
                    warnings: vec![],
                };
                let mut cr = cr;
                cr.warnings = ctx.warnings.take();
                let mut recs = vec![];
                if with_recs {
                    if let Some(rules) = spec.rules {
                        ctx.reset_basis();
                        recs.extend(rules(&ctx, &cr));
                    }
                    ctx.reset_basis();
                    recs.extend(crate::recommend::generic(&ctx, &cr));
                    recs.retain(|x| !self.config.is_rule_disabled(&x.id));
                }
                (cr, recs)
            })
            .collect();
        let mut costed = Vec::with_capacity(results.len());
        let mut recs = vec![];
        for (c, r) in results {
            costed.push(c);
            recs.extend(r);
        }
        (costed, recs)
    }

    pub fn estimate_project(&self, p: &Project) -> ProjectReport {
        let env = self.config.project_env(&p.name).unwrap_or_else(|| self.detect_env(&p.name));
        // A costerra-usage.yml in the project directory overrides the global one.
        let local = ["costerra-usage.yml", "costerra-usage.yaml"]
            .iter()
            .map(|n| p.path.join(n))
            .find(|f| f.is_file())
            .and_then(|f| UsageFile::load(&f).map_err(|e| tracing::error!("{e:#}")).ok());
        let usage = match &local {
            Some(l) => self.usage.overlay(l),
            None => self.usage.clone(),
        };
        let (resources, recommendations) = self.estimate_with_usage(&p.resources, env, self.recommendations, &usage, Some(&p.name));
        let mut recommendations = group_recommendations(recommendations);
        recommendations.sort_by(|a, b| {
            b.severity.cmp(&a.severity).then(b.monthly_savings.unwrap_or(0.0).total_cmp(&a.monthly_savings.unwrap_or(0.0)))
        });
        let (past_monthly_cost, past_resources) = match &p.prior {
            Some(prior) => {
                let (past, _) = self.estimate_with_usage(prior, env, false, &usage, Some(&p.name));
                (Some(past.iter().map(|r| r.monthly_cost).sum()), Some(past))
            }
            None => (None, None),
        };
        ProjectReport {
            name: p.name.clone(),
            path: p.path.to_string_lossy().to_string(),
            kind: format!("{:?}", p.kind).to_lowercase(),
            monthly_cost: crate::cost::sum(resources.iter().map(|r| r.monthly_cost)),
            list_monthly_cost: self.list_pricer.map(|_| resources.iter().map(|r| r.list_monthly_cost.unwrap_or(r.monthly_cost)).sum()),
            usage_examples: example_totals(&resources, |r| r.usage_examples.as_ref(), |r| r.monthly_cost),
            list_usage_examples: self.list_pricer.and_then(|_| {
                example_totals(&resources, |r| r.list_usage_examples.as_ref(), |r| r.list_monthly_cost.unwrap_or(r.monthly_cost))
            }),
            resources,
            recommendations,
            warnings: p.warnings.clone(),
            past_monthly_cost,
            past_resources,
        }
    }

    pub fn report(&self, projects: &[Project]) -> Report {
        let reports: Vec<ProjectReport> = projects.par_iter().map(|p| self.estimate_project(p)).collect();
        build_report(reports, self.pricer)
    }
}

/// Combine a model's own estimate with an estimate using example usage:
/// components the model priced are kept, unpriced usage-based components are
/// taken from the example run.
fn merge_components(base: &[crate::cost::Component], example: Vec<crate::cost::Component>) -> Vec<crate::cost::Component> {
    let priced = |c: &crate::cost::Component| c.monthly_cost.is_some();
    let mut out: Vec<crate::cost::Component> = example
        .into_iter()
        .map(|e| match base.iter().find(|b| b.name == e.name && priced(b)) {
            Some(b) => b.clone(),
            None => e,
        })
        .collect();
    for b in base.iter().filter(|b| priced(b)) {
        if !out.iter().any(|o| o.name == b.name) {
            out.push(b.clone());
        }
    }
    out
}

/// Totals with example usage: resources without examples count at their cost.
fn example_totals(
    resources: &[CostedResource],
    examples: impl Fn(&CostedResource) -> Option<&UsageExamples>,
    cost: impl Fn(&CostedResource) -> f64,
) -> Option<UsageExamples> {
    if !resources.iter().any(|r| examples(r).is_some()) {
        return None;
    }
    let sum = |f: fn(&UsageExamples) -> f64| crate::cost::sum(resources.iter().map(|r| examples(r).map(f).unwrap_or_else(|| cost(r))));
    Some(UsageExamples { small: sum(|e| e.small), medium: sum(|e| e.medium), large: sum(|e| e.large) })
}

/// Strip the instance key of an address: `a.b[0]` / `a.b["k"]` -> `a.b[*]`.
pub fn address_group(addr: &str) -> String {
    match addr.rfind('[') {
        Some(i) if addr.ends_with(']') && !addr[i..].contains('.') => format!("{}[*]", &addr[..i]),
        _ => addr.to_string(),
    }
}

/// Merge identical recommendations for instances of the same counted
/// resource (count / for_each) into a single entry with summed savings.
fn group_recommendations(recs: Vec<Recommendation>) -> Vec<Recommendation> {
    let mut groups: indexmap::IndexMap<(String, String, String), (Recommendation, usize)> = indexmap::IndexMap::new();
    for r in recs {
        let key = (r.id.clone(), address_group(&r.address), r.title.clone());
        match groups.get_mut(&key) {
            Some((g, n)) => {
                *n += 1;
                g.monthly_savings = match (g.monthly_savings, r.monthly_savings) {
                    (Some(a), Some(b)) => Some(a + b),
                    (a, b) => a.or(b),
                };
            }
            None => {
                groups.insert(key, (r, 1));
            }
        }
    }
    groups
        .into_iter()
        .map(|((_, group, _), (mut r, n))| {
            if n > 1 {
                r.address = format!("{group} ({n} instances)");
                r.monthly_savings = r.monthly_savings.map(|s| (s * 100.0).round() / 100.0);
            }
            r
        })
        .collect()
}

fn project_example_totals(
    projects: &[ProjectReport],
    examples: impl Fn(&ProjectReport) -> Option<&UsageExamples>,
    cost: impl Fn(&ProjectReport) -> f64,
) -> Option<UsageExamples> {
    if !projects.iter().any(|p| examples(p).is_some()) {
        return None;
    }
    let sum = |f: fn(&UsageExamples) -> f64| crate::cost::sum(projects.iter().map(|p| examples(p).map(f).unwrap_or_else(|| cost(p))));
    Some(UsageExamples { small: sum(|e| e.small), medium: sum(|e| e.medium), large: sum(|e| e.large) })
}

pub fn build_report(projects: Vec<ProjectReport>, pricer: &Pricer) -> Report {
    let mut priced = 0;
    let mut free = 0;
    let mut unsupported = 0;
    let mut missing_usage = 0;
    let mut unsupported_types: BTreeMap<String, usize> = BTreeMap::new();
    for p in &projects {
        for r in &p.resources {
            match r.status {
                Status::Priced => priced += 1,
                Status::Free => free += 1,
                Status::Unsupported => {
                    if r.rtype.starts_with("google") {
                        unsupported += 1;
                        *unsupported_types.entry(r.rtype.clone()).or_default() += 1;
                    }
                }
            }
            if r.has_unpriced_usage {
                missing_usage += 1;
            }
        }
    }
    let potential_savings =
        crate::cost::potential_savings(projects.iter().flat_map(|p| p.recommendations.iter().map(move |r| (p.name.as_str(), r))));
    let mut ut: Vec<(String, usize)> = unsupported_types.into_iter().collect();
    ut.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Report {
        version: env!("CARGO_PKG_VERSION").to_string(),
        currency: pricer.currency().to_string(),
        pricing_date: pricer.catalog.as_ref().map(|c| c.fetched_at),
        billing_account: pricer.billing_account().map(str::to_string),
        generated_at: crate::pricing::sync::now(),
        monthly_cost: crate::cost::sum(projects.iter().map(|p| p.monthly_cost)),
        usage_examples: project_example_totals(&projects, |p| p.usage_examples.as_ref(), |p| p.monthly_cost),
        list_usage_examples: pricer.billing_account().and_then(|_| {
            project_example_totals(&projects, |p| p.list_usage_examples.as_ref(), |p| p.list_monthly_cost.unwrap_or(p.monthly_cost))
        }),
        free_tiers: crate::free_tier::for_resources(
            projects
                .iter()
                .flat_map(|p| p.resources.iter())
                .filter(|r| r.status == Status::Priced)
                .map(|r| (r.rtype.as_str(), r.region.as_deref())),
        ),
        free_tier_included: pricer.include_free_tier,
        external_recommendations: vec![],
        insight_notes: vec![],
        observed: None,
        usage_profile: None,
        list_monthly_cost: pricer.billing_account().map(|_| projects.iter().map(|p| p.list_monthly_cost.unwrap_or(p.monthly_cost)).sum()),
        potential_savings,
        projects,
        summary: Summary { priced, free, unsupported, unsupported_types: ut, usage_based_without_usage: missing_usage },
    }
}
