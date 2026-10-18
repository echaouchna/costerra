//! Cross-cutting optimization rules applied to every priced resource.
//! Service-specific rules live next to their cost models (`Spec::rules`).

use crate::cost::{CostedResource, Recommendation, Severity};
use crate::hcl::value::Value;
use crate::resources::{Ctx, is_zone};

pub const REGIONS: &[(&str, &str)] = &[
    ("africa-south1", "Johannesburg"),
    ("asia-east1", "Taiwan"),
    ("asia-east2", "Hong Kong"),
    ("asia-northeast1", "Tokyo"),
    ("asia-northeast2", "Osaka"),
    ("asia-northeast3", "Seoul"),
    ("asia-south1", "Mumbai"),
    ("asia-south2", "Delhi"),
    ("asia-southeast1", "Singapore"),
    ("asia-southeast2", "Jakarta"),
    ("australia-southeast1", "Sydney"),
    ("australia-southeast2", "Melbourne"),
    ("europe-central2", "Warsaw"),
    ("europe-north1", "Finland"),
    ("europe-north2", "Stockholm"),
    ("europe-southwest1", "Madrid"),
    ("europe-west1", "Belgium"),
    ("europe-west2", "London"),
    ("europe-west3", "Frankfurt"),
    ("europe-west4", "Netherlands"),
    ("europe-west6", "Zurich"),
    ("europe-west8", "Milan"),
    ("europe-west9", "Paris"),
    ("europe-west10", "Berlin"),
    ("europe-west12", "Turin"),
    ("me-central1", "Doha"),
    ("me-central2", "Dammam"),
    ("me-west1", "Tel Aviv"),
    ("northamerica-northeast1", "Montréal"),
    ("northamerica-northeast2", "Toronto"),
    ("northamerica-south1", "Querétaro"),
    ("southamerica-east1", "São Paulo"),
    ("southamerica-west1", "Santiago"),
    ("us-central1", "Iowa"),
    ("us-east1", "South Carolina"),
    ("us-east4", "N. Virginia"),
    ("us-east5", "Columbus"),
    ("us-south1", "Dallas"),
    ("us-west1", "Oregon"),
    ("us-west2", "Los Angeles"),
    ("us-west3", "Salt Lake City"),
    ("us-west4", "Las Vegas"),
];

fn geo(region: &str) -> &str {
    match region.split('-').next().unwrap_or("") {
        "us" | "northamerica" => "na",
        "australia" | "asia" => "apac",
        other => other,
    }
}

pub fn region_name(r: &str) -> &'static str {
    REGIONS.iter().find(|(c, _)| *c == r).map(|(_, n)| *n).unwrap_or("")
}

pub fn generic(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    if let Some(r) = cheaper_region(ctx, cr) {
        out.push(r);
    }
    out
}

/// Suggest a cheaper region in the same geography for significant resources.
fn cheaper_region(ctx: &Ctx, cr: &CostedResource) -> Option<Recommendation> {
    if cr.monthly_cost < 25.0 || cr.components.iter().any(|c| c.is_unpriced()) {
        return None;
    }
    let current = cr.region.clone()?;
    if !REGIONS.iter().any(|(c, _)| *c == current) {
        return None;
    }
    let g = geo(&current);
    let mut best: Option<(&str, f64)> = None;
    for (cand, _) in REGIONS.iter().filter(|(c, _)| *c != current && geo(c) == g) {
        let new = ctx.reprice(|a| {
            a.insert("region".into(), Value::from(*cand));
            for k in ["zone", "location"] {
                if let Some(Value::String(s)) = a.get(k) {
                    let nv = if is_zone(s) {
                        format!("{cand}-b")
                    } else if s.contains('-') {
                        cand.to_string()
                    } else {
                        continue;
                    };
                    a.insert(k.to_string(), Value::String(nv));
                }
            }
        });
        if let Some(n) = new
            && n > 0.0
            && best.is_none_or(|b| n < b.1)
        {
            best = Some((cand, n));
        }
    }
    let (cand, n) = best?;
    let s = cr.monthly_cost - n;
    if s < 10.0 || s / cr.monthly_cost < 0.10 {
        return None;
    }
    Some(ctx.rec(
        "region-cheaper",
        Severity::Info,
        format!("{cand} ({}) is {:.0}% cheaper than {current} ({})", region_name(cand), s / cr.monthly_cost * 100.0, region_name(&current)),
        "The same configuration would cost less in another region of the same geography. Consider it for new workloads if latency, data residency and inter-region egress allow.",
        Some(s),
    ))
}
