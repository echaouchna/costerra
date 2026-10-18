//! Cost result types.

use serde::{Deserialize, Serialize};

pub const HOURS_PER_MONTH: f64 = 730.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Component {
    pub name: String,
    pub unit: String,
    /// Monthly quantity (None when usage-based and no usage was provided).
    pub quantity: Option<f64>,
    pub unit_price: Option<f64>,
    pub monthly_cost: Option<f64>,
    /// Public list price per unit / monthly cost, when contract prices are used.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub list_unit_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub list_monthly_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sku: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sku_description: Option<String>,
    #[serde(default)]
    pub usage_based: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
}

impl Component {
    /// A component whose cost should be known but is not: either a fixed
    /// component without price, or a usage estimate whose SKU is missing.
    pub fn is_unpriced(&self) -> bool {
        self.monthly_cost.is_none() && (!self.usage_based || self.quantity.is_some())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Priced,
    Free,
    Unsupported,
}

/// Monthly cost with example usage profiles.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct UsageExamples {
    pub small: f64,
    pub medium: f64,
    pub large: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostedResource {
    pub address: String,
    #[serde(rename = "type")]
    pub rtype: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub region: Option<String>,
    pub status: Status,
    pub monthly_cost: f64,
    /// Cost at list prices, when contract prices are used.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub list_monthly_cost: Option<f64>,
    /// True if some usage-based components have no usage estimate.
    #[serde(default)]
    pub has_unpriced_usage: bool,
    /// Cost with example usage (small / medium / large profiles).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub usage_examples: Option<UsageExamples>,
    /// Same at public list prices (when contract prices are used).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub list_usage_examples: Option<UsageExamples>,
    /// Example profile applied to missing usage (`--usage-profile`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub usage_profile: Option<String>,
    pub components: Vec<Component>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

impl CostedResource {
    pub fn total(components: &[Component]) -> f64 {
        components.iter().filter_map(|c| c.monthly_cost).sum()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
}

fn default_source() -> String {
    "costerra".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recommendation {
    pub id: String,
    /// Where the recommendation comes from: costerra (static analysis of the
    /// code), gcp (Google Cloud Recommender, observed usage) or rabbit.
    #[serde(default = "default_source")]
    pub source: String,
    /// How to trace the recommendation back to its source: the costerra rule,
    /// the Recommender name and `gcloud … describe` command, or a Rabbit link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    pub severity: Severity,
    pub address: String,
    pub title: String,
    pub detail: String,
    /// Estimated monthly savings if applied (None when not quantifiable).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub monthly_savings: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReport {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub monthly_cost: f64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub list_monthly_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub usage_examples: Option<UsageExamples>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub list_usage_examples: Option<UsageExamples>,
    pub resources: Vec<CostedResource>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub recommendations: Vec<Recommendation>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
    /// Cost before the change (plan mode prior state), when known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub past_monthly_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub past_resources: Option<Vec<CostedResource>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub priced: usize,
    pub free: usize,
    pub unsupported: usize,
    pub unsupported_types: Vec<(String, usize)>,
    pub usage_based_without_usage: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub version: String,
    pub currency: String,
    pub pricing_date: Option<u64>,
    /// Billing account whose contract prices were used (None = list prices).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub billing_account: Option<String>,
    pub generated_at: u64,
    pub monthly_cost: f64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub list_monthly_cost: Option<f64>,
    /// Totals with example usage for resources without estimates.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub usage_examples: Option<UsageExamples>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub list_usage_examples: Option<UsageExamples>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub usage_profile: Option<String>,
    /// External recommendations not tied to a resource of this code
    /// (e.g. billing-account commitments), and notes about unavailable sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_recommendations: Vec<Recommendation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub insight_notes: Vec<String>,
    /// Free tiers of the services in this report (not deducted unless included).
    #[serde(default)]
    pub free_tiers: Vec<crate::free_tier::FreeTierInfo>,
    #[serde(default)]
    pub free_tier_included: bool,
    pub potential_savings: f64,
    pub projects: Vec<ProjectReport>,
    pub summary: Summary,
}

/// Total monthly savings of a set of (project, recommendation) pairs.
/// Alternatives for the same resource (Spot, a cheaper machine family, a
/// commitment…) are not cumulative: only the best one per resource counts.
/// Informational recommendations are left out.
pub fn potential_savings<'a>(recs: impl IntoIterator<Item = (&'a str, &'a Recommendation)>) -> f64 {
    let mut best: std::collections::HashMap<(&str, &str), f64> = std::collections::HashMap::new();
    for (project, r) in recs {
        if r.severity < Severity::Low {
            continue;
        }
        if let Some(s) = r.monthly_savings {
            let e = best.entry((project, r.address.as_str())).or_default();
            *e = e.max(s);
        }
    }
    best.values().sum()
}
