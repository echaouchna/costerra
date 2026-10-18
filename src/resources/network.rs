//! Networking: Cloud Load Balancing (forwarding rules, proxies, Cloud CDN,
//! Cloud Armor), Private Service Connect, Cloud NAT, Cloud VPN, Cloud
//! Interconnect, Serverless VPC Access, Cloud DNS, Network Connectivity
//! Center, Packet Mirroring, Secure Web Proxy and data transfer helpers.

use super::compute::{VmSpec, vm_components};
use super::machine::{COMPUTE, Pricing};
use super::{Ctx, Env, Spec, UsageParam, usage};
use crate::cost::{Component, CostedResource, Recommendation, Severity};
use crate::hcl::value::{Value, fmt_number};
use crate::pricing::catalog::Sku;
use crate::pricing::query::{Query, normalize_region};
use crate::tf::evaluate::ResourceInstance;

const NET: &str = "Networking";
const NET_SEC: &str = "Network Security";
const DNS: &str = "Cloud DNS";

pub fn specs() -> Vec<Spec> {
    const FR: &[UsageParam] = usage!(
        "monthly_data_processed_gb" => "Inbound data (requests) processed by the load balancer / PSC endpoint, in GiB",
        "monthly_outbound_data_processed_gb" => "Outbound data (responses) processed by the load balancer, in GiB",
        "monthly_internet_egress_gb" => "Internet egress served through this external load balancer, in GiB",
        "proxy_instances" => "Average Envoy proxy instances of an internal Application Load Balancer (default 3, the minimum)",
    );
    const CDN: &[UsageParam] = usage!(
        "monthly_cache_egress_gb" => "Cloud CDN cache egress (responses served from cache), in GiB",
        "monthly_cache_fill_gb" => "Cloud CDN cache fill from the origin, in GiB",
        "monthly_cache_lookup_requests" => "Cloud CDN cache lookup requests (GET/HEAD)",
    );
    const ARMOR: &[UsageParam] = usage!("monthly_requests" => "Requests evaluated by the security policy");
    const ARMOR_TIER: &[UsageParam] = usage!(
        "protected_resources" => "Cloud Armor Enterprise protected resources (defaults to external backend services/buckets in the project)",
        "monthly_data_processed_gb" => "Data transferred to the internet from protected resources, in GiB",
    );
    const NAT: &[UsageParam] = usage!(
        "instances" => "VM instances using the NAT gateway (defaults to VMs without external IP in the region)",
        "nat_ips" => "NAT IP addresses in use (defaults to nat_ips or the auto-allocated estimate)",
        "monthly_data_processed_gb" => "Data processed by the gateway (inbound + outbound), in GiB",
    );
    const EGRESS: &[UsageParam] = usage!("monthly_egress_gb" => "Data sent through this resource, in GiB");
    const DP: &[UsageParam] = usage!("monthly_data_processed_gb" => "Data processed, in GiB");
    const VPC_CONN: &[UsageParam] = usage!(
        "instances" => "Average connector instances (defaults to min_instances)",
        "monthly_hrs" => "Hours per month the connector runs (default 730)",
    );
    const ATTACH: &[UsageParam] = usage!(
        "bandwidth" => "Partner VLAN attachment bandwidth (e.g. BPS_1G), set by the partner",
        "monthly_egress_gb" => "Data transfer out through the attachment, in GiB",
    );
    vec![
        Spec::new("google_compute_forwarding_rule", forwarding_rule).usage(FR).rules(rules::forwarding_rule),
        Spec::new("google_compute_global_forwarding_rule", forwarding_rule).usage(FR).rules(rules::forwarding_rule),
        Spec::new("google_compute_backend_service", cdn).usage(CDN).rules(rules::cdn),
        Spec::new("google_compute_backend_bucket", cdn).usage(CDN).rules(rules::cdn),
        Spec::new("google_compute_security_policy", security_policy).usage(ARMOR).rules(rules::security_policy),
        Spec::new("google_compute_region_security_policy", security_policy).usage(ARMOR).rules(rules::security_policy),
        Spec::new("google_compute_project_cloud_armor_tier", armor_tier).usage(ARMOR_TIER).rules(rules::armor_tier),
        Spec::new("google_compute_router_nat", nat).usage(NAT).rules(rules::nat),
        Spec::new("google_compute_vpn_tunnel", vpn_tunnel).usage(EGRESS),
        Spec::new("google_compute_vpn_gateway", |_| vec![]).rules(rules::classic_vpn),
        Spec::new("google_compute_interconnect_attachment", interconnect_attachment).usage(ATTACH),
        Spec::new("google_compute_interconnect", interconnect),
        Spec::new("google_vpc_access_connector", vpc_connector).usage(VPC_CONN).rules(rules::vpc_connector),
        Spec::new("google_dns_managed_zone", dns_zone)
            .usage(usage!("monthly_queries" => "DNS queries answered by the zone"))
            .rules(rules::dns_zone),
        Spec::new("google_compute_packet_mirroring", packet_mirroring).usage(DP),
        Spec::new("google_network_connectivity_spoke", ncc_spoke).usage(DP),
        Spec::new("google_network_services_gateway", swp_gateway).usage(DP),
        Spec::new("google_network_security_firewall_endpoint", ngfw_endpoint)
            .usage(usage!("monthly_data_processed_gb" => "Traffic inspected by the endpoint in GiB per month"))
            .rules(rules::ngfw_endpoint),
        Spec::new("google_network_management_connectivity_test", connectivity_test).usage(
            usage!("monthly_runs" => "Test runs per month (runs on creation and on each rerun; first 20 per billing account are free)"),
        ),
    ]
}

// ---- shared helpers ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Geo {
    NorthAmerica,
    Europe,
    Asia,
    Oceania,
    SouthAmerica,
    MiddleEast,
    Africa,
}

fn geo(region: &str) -> Geo {
    let r = region.to_lowercase();
    let p = r.split('-').next().unwrap_or("");
    match p {
        "us" | "northamerica" | "nam" => Geo::NorthAmerica,
        "europe" | "eu" | "eur" => Geo::Europe,
        "southamerica" => Geo::SouthAmerica,
        "australia" => Geo::Oceania,
        "me" => Geo::MiddleEast,
        "africa" => Geo::Africa,
        _ => Geo::Asia,
    }
}

/// Region of any project resource (attributes, then provider defaults).
fn res_region(ctx: &Ctx, r: &ResourceInstance) -> String {
    for k in ["region", "location", "zone"] {
        if let Some(s) = r.attrs.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty() && *s != "global") {
            return normalize_region(&s.to_lowercase());
        }
    }
    r.provider.region.clone().or_else(|| r.provider.zone.as_deref().map(normalize_region)).unwrap_or_else(|| ctx.default_region.to_string())
}

fn short_addr(r: &ResourceInstance) -> String {
    format!("{}.{}", r.rtype, r.name)
}

/// True if `s` references `target` (by Terraform reference or by name).
fn references(s: &ResourceInstance, attr: &str, target: &ResourceInstance) -> bool {
    if s.refs.contains(&short_addr(target)) {
        return true;
    }
    let name = target.attrs.get("name").and_then(|v| v.as_str());
    match (s.attrs.get(attr).and_then(|v| v.as_str()), name) {
        (Some(v), Some(n)) => v == n || v.rsplit('/').next() == Some(n),
        _ => false,
    }
}

// ---- data transfer -----------------------------------------------------------

/// Internet data transfer out from `region`. Premium tier is priced for
/// worldwide destinations excluding China and Australia; Standard tier is
/// priced by source region.
pub fn internet_egress_component(ctx: &Ctx, region: &str, gb: Option<f64>, premium: bool) -> Component {
    if premium {
        let q = Query::new(COMPUTE, region, "^Network Internet Data Transfer Out from .* to Americas$")
            .group("PremiumInternetEgress")
            .ondemand();
        let mut c = ctx.usage_qty("Internet egress (premium tier)", "GiB", gb, q);
        c.note = Some("worldwide destinations excl. China and Australia".into());
        c
    } else {
        let q =
            Query::new(COMPUTE, region, "^Network Standard Data Transfer Out to Internet from").group("StandardInternetEgress").ondemand();
        ctx.usage_qty("Internet egress (standard tier)", "GiB", gb, q)
    }
}

/// Inter-region data transfer (VM-to-VM / service-to-service) from `src` to `dst`.
pub fn inter_region_egress_component(ctx: &Ctx, src: &str, dst: &str, gb: Option<f64>) -> Component {
    let (src, dst) = (normalize_region(src), normalize_region(dst));
    let name = format!("Inter-region egress ({src} to {dst})");
    let skus =
        ctx.pricer.search(Query::new(COMPUTE, &src, "^Network Inter Region Data Transfer Out from").group("InterregionEgress").ondemand());
    let has = |s: &Sku, r: &str| s.regions.iter().any(|x| &**x == r);
    let sku =
        skus.iter().filter(|s| s.regions.first().is_some_and(|f| **f == *src) && has(s, &dst)).min_by_key(|s| s.regions.len()).or_else(
            || skus.iter().filter(|s| has(s, &dst) && !(s.regions.len() == 2 && *s.regions[0] == *dst)).min_by_key(|s| s.regions.len()),
        );
    let Some(sku) = sku else {
        if ctx.pricer.catalog.is_some() {
            ctx.warn(format!("no price found for '{name}'"));
        }
        return ctx.usage_qty(name, "GiB", gb, Query::new(COMPUTE, &src, "^$"));
    };
    let price = ctx.pricer.price_of(sku);
    Component {
        name,
        unit: "GiB".into(),
        quantity: gb,
        unit_price: Some(price.unit_price()),
        monthly_cost: gb.map(|g| price.cost(g)),
        list_unit_price: None,
        list_monthly_cost: None,
        sku: Some(price.sku_id.clone()),
        sku_description: Some(price.description.clone()),
        usage_based: true,
        note: None,
    }
}

// ---- Cloud NGFW Enterprise / Network Intelligence Center ----------------------

fn ngfw_endpoint(ctx: &Ctx) -> Vec<Component> {
    let name = match ctx.str("location").filter(|z| !z.contains('<')) {
        Some(z) => format!("Firewall endpoint uptime ({z})"),
        None => "Firewall endpoint uptime".to_string(),
    };
    let mut up = ctx.hourly(name, 1.0, Query::new(NET_SEC, "global", "^Cloud NGFW Enterprise Endpoint Uptime").ondemand());
    up.note = Some("billed per zonal endpoint; one endpoint can serve several VPC networks through associations".into());
    vec![
        up,
        ctx.usage(
            "Inspected traffic",
            "GiB",
            "monthly_data_processed_gb",
            Query::new(NET_SEC, "global", "^Cloud NGFW Enterprise Data Processing").ondemand(),
        ),
    ]
}

fn connectivity_test(ctx: &Ctx) -> Vec<Component> {
    vec![ctx.usage(
        "Test runs",
        "runs",
        "monthly_runs",
        Query::new(NET, "global", "^Network Intelligence Center Connectivity Tests Runs").ondemand(),
    )]
}

// ---- load balancing ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Http,
    Tcp,
    ServiceAttachment,
    GoogleApis,
    Vpn,
    Other,
}

fn target_kind(r: &ResourceInstance) -> Target {
    let t = r.attrs.get("target").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
    if t == "all-apis" || t == "vpc-sc" {
        return Target::GoogleApis;
    }
    let mut hint = t;
    for x in &r.refs {
        hint.push(' ');
        hint.push_str(&x.to_lowercase());
    }
    let any = |pats: &[&str]| pats.iter().any(|p| hint.contains(p));
    if any(&["httpproxies", "httpsproxies", "_http_proxy", "_https_proxy"]) {
        Target::Http
    } else if any(&["tcpproxies", "sslproxies", "_tcp_proxy", "_ssl_proxy"]) {
        Target::Tcp
    } else if any(&["serviceattachments", "_service_attachment"]) {
        Target::ServiceAttachment
    } else if any(&["targetvpngateways", "google_compute_vpn_gateway"]) {
        Target::Vpn
    } else {
        Target::Other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lb {
    /// Billed per forwarding rule (first 5 per project/region share a minimum).
    Rule {
        fr: &'static str,
        dp: &'static str,
        global: bool,
        external: bool,
    },
    /// Envoy-based internal Application Load Balancer billed per proxy instance.
    Proxy {
        proxy: &'static str,
        dp: &'static str,
    },
    /// Private Service Connect endpoint.
    Psc {
        google_apis: bool,
    },
    Free(&'static str),
}

const CLASSIC_FR: &str = "Cloud Load Balancer Forwarding Rule";

fn classify(r: &ResourceInstance) -> Lb {
    let global = r.rtype == "google_compute_global_forwarding_rule";
    let a = &r.attrs;
    let t = target_kind(r);
    match t {
        Target::GoogleApis => return Lb::Psc { google_apis: true },
        Target::ServiceAttachment => return Lb::Psc { google_apis: false },
        Target::Vpn => return Lb::Free("forwarding rules of Classic VPN gateways are not charged"),
        _ => {}
    }
    if a.get("is_mirroring_collector").and_then(|v| v.as_bool()) == Some(true) {
        return Lb::Free("Packet Mirroring collector forwarding rules are not charged");
    }
    let scheme = a.get("load_balancing_scheme").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or("EXTERNAL");
    let rule = |fr, dp, global, external| Lb::Rule { fr, dp, global, external };
    match (global, scheme, t) {
        (_, "INTERNAL_SELF_MANAGED", _) => Lb::Free("Cloud Service Mesh forwarding rules are not charged"),
        (true, "INTERNAL_MANAGED", Target::Tcp) => rule(
            "Cross Regional Internal Proxy Network Load Balancer Forwarding Rule",
            "Cross Regional Internal Proxy Network Load Balancer",
            false,
            false,
        ),
        (true, "INTERNAL_MANAGED", _) => Lb::Proxy {
            proxy: "Cross Regional Internal Application Load Balancer Proxy Instance Charge",
            dp: "Cross Regional Internal Application Load Balancer",
        },
        (true, _, Target::Tcp) => rule(CLASSIC_FR, "Global External Proxy Network Load Balancer", true, true),
        (true, _, _) => rule(CLASSIC_FR, "Global External Application Load Balancer", true, true),
        (false, "INTERNAL_MANAGED", Target::Tcp) => rule(
            "Regional Internal Proxy Network Load Balancer Forwarding Rule",
            "Regional Internal Proxy Network Load Balancer",
            false,
            false,
        ),
        (false, "INTERNAL_MANAGED", _) => Lb::Proxy {
            proxy: "Regional Internal Application Load Balancer Proxy Instance Charge",
            dp: "Regional Internal Application Load Balancer",
        },
        (false, "EXTERNAL_MANAGED", Target::Tcp) => rule(
            "Regional External Proxy Network Load Balancer Forwarding Rule",
            "Regional External Proxy Network Load Balancer",
            false,
            true,
        ),
        (false, "EXTERNAL_MANAGED", _) => {
            rule("Regional External Application Load Balancer Forwarding Rule", "Regional External Application Load Balancer", false, true)
        }
        (false, "INTERNAL", _) => rule(CLASSIC_FR, "Regional Internal Passthrough Network Load Balancer", false, false),
        (false, _, _) => rule(CLASSIC_FR, "Regional External Passthrough Network Load Balancer", false, true),
    }
}

fn fr_location(ctx: &Ctx, r: &ResourceInstance, global_sku: bool) -> String {
    if global_sku { "global".to_string() } else { res_region(ctx, r) }
}

/// Forwarding rules of the project billed on the same SKU pair and location,
/// sorted by address.
fn fr_bucket<'a>(ctx: &Ctx<'a>, fr: &str, global: bool, loc: &str) -> Vec<&'a ResourceInstance> {
    let mut v: Vec<&ResourceInstance> = ctx
        .siblings
        .iter()
        .filter(|s| s.rtype == "google_compute_forwarding_rule" || s.rtype == "google_compute_global_forwarding_rule")
        .filter(|s| matches!(classify(s), Lb::Rule { fr: f, global: g, .. } if f == fr && g == global))
        .filter(|s| fr_location(ctx, s, global) == loc)
        .collect();
    v.sort_by(|a, b| a.address.cmp(&b.address));
    v
}

/// SKU description patterns of the (first-5 minimum, additional rule) charges.
fn rule_patterns(fr: &str, global: bool) -> (String, String) {
    if global {
        (format!("^{fr} Minimum Global$"), format!("^{fr} Additional Global$"))
    } else {
        (format!("^{fr} Minimum for "), format!("^{fr} Additional for "))
    }
}

fn forwarding_rule(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let mut out = vec![];
    let dp_components = |out: &mut Vec<Component>, dp: &str| {
        let inb = format!("^{dp} Inbound Data Processing for ");
        let outb = format!("^{dp} Outbound Data Processing for ");
        out.push(ctx.usage(
            "Inbound data processed",
            "GiB",
            "monthly_data_processed_gb",
            Query::new(NET, &region, &inb).exclude("Opt-in").ondemand(),
        ));
        out.push(ctx.usage(
            "Outbound data processed",
            "GiB",
            "monthly_outbound_data_processed_gb",
            Query::new(NET, &region, &outb).exclude("Opt-in").ondemand(),
        ));
    };
    match classify(ctx.r) {
        Lb::Free(why) => {
            let mut c = ctx.fixed("Forwarding rule", "hours", Some(ctx.monthly_hours()), Some(0.0));
            c.note = Some(why.into());
            out.push(c);
        }
        Lb::Psc { google_apis } => {
            let mut c = ctx.hourly(
                "PSC endpoint",
                1.0,
                Query::new(NET, "global", "^Networking Private Service Connect Consumer End Point$").ondemand(),
            );
            if google_apis {
                c.note = Some("endpoint for Google APIs: no data processing charge".into());
                out.push(c);
            } else {
                out.push(c);
                out.push(ctx.usage(
                    "PSC data processed",
                    "GiB",
                    "monthly_data_processed_gb",
                    Query::new(NET, "global", "^Networking Private Service Connect Consumer Data Processing$").ondemand(),
                ));
            }
        }
        Lb::Proxy { proxy, dp } => {
            let n = ctx.usage_num("proxy_instances").unwrap_or(3.0);
            let pat = format!("^{proxy} for ");
            let mut c = ctx.hourly_unit(
                format!("Proxy instances ({})", fmt_number(n)),
                "proxy-hours",
                n,
                Query::new(NET, &region, &pat).ondemand(),
            );
            if ctx.usage_num("proxy_instances").is_none() {
                c.note = Some("minimum of 3 proxies per forwarding rule; more are allocated with traffic (usage 'proxy_instances')".into());
            }
            out.push(c);
            dp_components(&mut out, dp);
        }
        Lb::Rule { fr, dp, global, external } => {
            let loc = if global { "global".to_string() } else { region.clone() };
            let bucket = fr_bucket(ctx, fr, global, &loc);
            let n = bucket.len().max(1);
            let rank = bucket.iter().position(|s| s.address == ctx.r.address).unwrap_or(0);
            let (pmin_pat, padd_pat) = rule_patterns(fr, global);
            let (qmin, qadd) = (Query::new(NET, &loc, &pmin_pat).ondemand(), Query::new(NET, &loc, &padd_pat).ondemand());
            let (pmin, padd) = (ctx.price(qmin).map(|p| p.unit_price()), ctx.price(qadd).map(|p| p.unit_price()));
            let mut c = if rank < 5 {
                let k = n.min(5);
                let name = if k == 1 { "Forwarding rule".to_string() } else { format!("Forwarding rule (1/{k} of the first-5 minimum)") };
                ctx.hourly(name, 1.0 / k as f64, qmin)
            } else {
                ctx.hourly(format!("Forwarding rule (additional, #{})", rank + 1), 1.0, qadd)
            };
            if let (Some(pm), Some(pa)) = (pmin, padd) {
                c.note =
                    Some(format!("{} forwarding rule(s) of this kind in {loc}: the first 5 share {pm}/h, each additional costs {pa}/h", n));
            }
            out.push(c);
            dp_components(&mut out, dp);
            if external {
                let premium = global || ctx.str_or("network_tier", "PREMIUM") != "STANDARD";
                out.push(internet_egress_component(ctx, &region, ctx.usage_num("monthly_internet_egress_gb"), premium));
            }
        }
    }
    out
}

#[derive(Clone, Copy)]
struct CdnSkus {
    dest: &'static str,
    fill: &'static str,
    fill_region: &'static str,
}

fn cdn_skus(region: &str) -> CdnSkus {
    let (dest, fill, fill_region) = match geo(region) {
        Geo::Europe => ("Europe", "Europe", "europe-west1"),
        Geo::NorthAmerica => ("North America", "North America", "us-central1"),
        Geo::Asia => ("Asia", "Asia Pacific", "asia-east1"),
        Geo::Oceania => ("Oceania", "Oceania", "australia-southeast1"),
        Geo::SouthAmerica => ("Latin America", "South America", "southamerica-east1"),
        Geo::MiddleEast | Geo::Africa => ("Other", "Middle East", "me-central1"),
    };
    CdnSkus { dest, fill, fill_region }
}

/// Backend services and buckets are free; Cloud CDN adds usage-based charges.
fn cdn(ctx: &Ctx) -> Vec<Component> {
    if !ctx.bool_or("enable_cdn", false) {
        return vec![];
    }
    let s = cdn_skus(&ctx.region());
    let egress = format!("^Networking Cloud CDN Traffic Cache Data Transfer to {}$", s.dest);
    let fill = format!("^Network CDN {} Intra-Region Cache Fill$", s.fill);
    vec![
        ctx.usage(
            format!("CDN cache egress (to {})", s.dest),
            "GiB",
            "monthly_cache_egress_gb",
            Query::new(NET, "global", &egress).ondemand(),
        ),
        ctx.usage("CDN cache fill (intra-region)", "GiB", "monthly_cache_fill_gb", Query::new(COMPUTE, s.fill_region, &fill).ondemand()),
        ctx.usage(
            "CDN cache lookups",
            "requests",
            "monthly_cache_lookup_requests",
            Query::new(NET, "global", "^Networking Cloud Cdn Cache Lookups$").ondemand(),
        ),
    ]
}

// ---- Cloud Armor -------------------------------------------------------------

fn armor_tier_of<'a>(ctx: &Ctx<'a>) -> Option<&'a str> {
    ctx.siblings
        .iter()
        .find(|s| s.rtype == "google_compute_project_cloud_armor_tier")
        .and_then(|s| s.attrs.get("cloud_armor_tier").and_then(|v| v.as_str()))
}

fn policy_rule_count(ctx: &Ctx) -> f64 {
    let regional = ctx.r.rtype == "google_compute_region_security_policy";
    let inline = ctx.blocks(if regional { "rules" } else { "rule" }).len();
    let rule_type = if regional { "google_compute_region_security_policy_rule" } else { "google_compute_security_policy_rule" };
    let separate = ctx.siblings.iter().filter(|s| s.rtype == rule_type && references(s, "security_policy", ctx.r)).count();
    // Without inline rules the API creates the default rule.
    (if inline == 0 { 1 } else { inline } + separate) as f64
}

fn security_policy(ctx: &Ctx) -> Vec<Component> {
    let sfx = if ctx.r.rtype == "google_compute_region_security_policy" { " Regional" } else { "" };
    let rules = policy_rule_count(ctx);
    if armor_tier_of(ctx).is_some_and(|t| t.starts_with("CA_ENTERPRISE")) {
        let mut c = ctx.fixed(format!("Security policy ({} rules)", fmt_number(rules)), "months", Some(1.0), Some(0.0));
        c.note = Some("policies, rules and requests are included in Cloud Armor Enterprise".into());
        return vec![c];
    }
    let (p, r, q) = (
        format!("^Networking Cloud Armor Policy{sfx}$"),
        format!("^Networking Cloud Armor Rule{sfx}$"),
        format!("^Networking Cloud Armor Requests{sfx}$"),
    );
    vec![
        ctx.monthly("Security policy", "months", 1.0, Query::new(NET, "global", &p).ondemand()),
        ctx.monthly(format!("Rules ({})", fmt_number(rules)), "rule-months", rules, Query::new(NET, "global", &r).ondemand()),
        ctx.usage("Requests", "requests", "monthly_requests", Query::new(NET, "global", &q).ondemand()),
    ]
}

/// Backends that count as Cloud Armor Enterprise protected resources.
fn protected_resources(ctx: &Ctx) -> f64 {
    ctx.siblings
        .iter()
        .filter(|s| {
            let scheme = |d: &str| s.attrs.get("load_balancing_scheme").and_then(|v| v.as_str()).unwrap_or(d).starts_with("EXTERNAL");
            match s.rtype.as_str() {
                "google_compute_backend_service" => scheme("EXTERNAL"),
                "google_compute_region_backend_service" => scheme("INTERNAL"),
                "google_compute_backend_bucket" | "google_compute_target_pool" | "google_compute_target_instance" => true,
                _ => false,
            }
        })
        .count() as f64
}

fn armor_tier(ctx: &Ctx) -> Vec<Component> {
    let tier = ctx.str_or("cloud_armor_tier", "CA_STANDARD");
    let (plan, included, sub_q, res_pat, dp_pat) = match tier {
        "CA_ENTERPRISE_PAYGO" => (
            "Paygo",
            2.0,
            Query::new(NET, "global", "^Networking Cloud Armor Enterprise Paygo: Enrollment$").ondemand(),
            "^Networking Cloud Armor Enterprise Paygo: Protected Resource$",
            "^Networking Cloud Armor Enterprise Paygo: Data Processing Fee for Load Balancer$",
        ),
        "CA_ENTERPRISE_ANNUAL" => (
            "Annual",
            100.0,
            Query::new(COMPUTE, "global", "^Monthly Fee for Cloud Armor Enterprise Annual Subscription$").usage("Commit1Mo"),
            "^Networking Cloud Armor Enterprise Annual: Protected Resource$",
            "^Networking Cloud Armor Enterprise Annual: Data Processing Fee for Load Balancer$",
        ),
        _ => return vec![],
    };
    let n = ctx.usage_num("protected_resources").unwrap_or_else(|| protected_resources(ctx));
    let mut sub = ctx.monthly(format!("Cloud Armor Enterprise {plan} subscription"), "months", 1.0, sub_q);
    sub.note = Some(if plan == "Annual" {
        "billed once per billing account (1-year commitment), covers 100 protected resources across projects".into()
    } else {
        "includes 2 protected resources per project".into()
    });
    let mut out = vec![sub];
    if n > included {
        let p = ctx.price(Query::new(NET, "global", res_pat).ondemand()).map(|p| p.unit_price());
        out.push(ctx.fixed(format!("Protected resources beyond {}", fmt_number(included)), "resource-months", Some(n - included), p));
    }
    out.push(ctx.usage(
        "Data processing (internet egress of protected resources)",
        "GiB",
        "monthly_data_processed_gb",
        Query::new(NET, "global", dp_pat).ondemand(),
    ));
    out
}

// ---- Cloud NAT ---------------------------------------------------------------

/// Ports available per NAT IP address.
const NAT_PORTS_PER_IP: f64 = 64512.0;

fn nat_ips_needed(vms: f64, min_ports_per_vm: f64) -> f64 {
    (vms.max(1.0) * min_ports_per_vm / NAT_PORTS_PER_IP).ceil().max(1.0)
}

fn has_external_ip(r: &ResourceInstance) -> bool {
    r.attrs
        .get("network_interface")
        .and_then(|n| n.as_list())
        .is_some_and(|nics| nics.iter().any(|n| n.get("access_config").and_then(|a| a.as_list()).is_some_and(|l| !l.is_empty())))
}

/// VMs without an external IP in `region` (instances, MIGs, GKE node pools).
fn private_vms_in_region(ctx: &Ctx, region: &str) -> f64 {
    let num = |r: &ResourceInstance, p: &str| r.attrs.path(p).and_then(|v| v.as_f64());
    ctx.siblings
        .iter()
        .filter(|s| res_region(ctx, s) == region)
        .map(|s| match s.rtype.as_str() {
            "google_compute_instance" if !has_external_ip(s) => 1.0,
            "google_compute_instance_group_manager" | "google_compute_region_instance_group_manager" => {
                num(s, "target_size").unwrap_or(0.0)
            }
            "google_container_node_pool" => {
                if let Some(t) = num(s, "autoscaling.0.total_min_node_count") {
                    return t;
                }
                let per_zone = num(s, "node_count")
                    .or_else(|| num(s, "autoscaling.0.min_node_count"))
                    .or_else(|| num(s, "initial_node_count"))
                    .unwrap_or(0.0);
                let zones = match s.attrs.get("node_locations").and_then(|v| v.as_list()) {
                    Some(l) if !l.is_empty() => l.len() as f64,
                    _ => {
                        let loc = s.attrs.get("location").and_then(|v| v.as_str()).unwrap_or("");
                        if super::is_zone(loc) { 1.0 } else { 3.0 }
                    }
                };
                per_zone * zones
            }
            _ => 0.0,
        })
        .sum()
}

/// (VM count, estimated from the project?)
fn nat_vms(ctx: &Ctx, region: &str) -> Option<(f64, bool)> {
    if let Some(n) = ctx.usage_num("instances") {
        return Some((n, false));
    }
    let n = private_vms_in_region(ctx, region);
    (n > 0.0).then_some((n, true))
}

fn nat_min_ports(ctx: &Ctx) -> f64 {
    let dpa = ctx.bool_or("enable_dynamic_port_allocation", false);
    ctx.num("min_ports_per_vm").unwrap_or(if dpa { 32.0 } else { 64.0 })
}

/// (manual NAT IPs listed, of which reserved as google_compute_address in this project)
fn nat_manual_ips(ctx: &Ctx) -> (f64, f64) {
    let Some(list) = ctx.attrs.get("nat_ips").and_then(|v| v.as_list()) else { return (0.0, 0.0) };
    let on_addr = list
        .iter()
        .filter(|v| {
            v.as_str().and_then(|s| ctx.resolve_ref(s, "google_compute_address")).is_some_and(|r| r.rtype == "google_compute_address")
        })
        .count();
    (list.len() as f64, on_addr as f64)
}

fn nat(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let dp = |pat: &str| ctx.usage("Data processing", "GiB", "monthly_data_processed_gb", Query::new(NET, "global", pat).ondemand());
    if ctx.str("type") == Some("PRIVATE") {
        return vec![
            ctx.hourly("Private NAT gateway", 1.0, Query::new(NET, "global", "^Networking Private Nat Gateway Uptime$").ondemand()),
            dp("^Networking Private Nat Data Processing$"),
        ];
    }
    // VMs are counted in the router's region (stable when the NAT is repriced elsewhere).
    let vm_region = ctx
        .str("router")
        .and_then(|r| ctx.resolve_ref(r, "google_compute_router"))
        .map(|r| res_region(ctx, r))
        .unwrap_or_else(|| region.clone());
    let vms = nat_vms(ctx, &vm_region);
    let gw_q = Query::new(NET, "global", "^Networking Cloud Nat Gateway Uptime$").ondemand();
    let mut out = vec![];
    let mut gw = match vms {
        Some((n, estimated)) => {
            let mut c = ctx.hourly_unit(
                format!("NAT gateway ({} VMs{})", fmt_number(n), if n > 32.0 { ", billed as 32" } else { "" }),
                "VM-hours",
                n.min(32.0),
                gw_q,
            );
            if estimated {
                c.note = Some(format!("{} VMs without external IP found in {vm_region} (override with usage 'instances')", fmt_number(n)));
            }
            c
        }
        None => {
            let mut c = ctx.usage_qty("NAT gateway (per VM)", "VM-hours", None, gw_q);
            c.note = Some("no VM found in this region: set usage 'instances'".into());
            c
        }
    };
    let (ips, on_addr) = if let Some(n) = ctx.usage_num("nat_ips") {
        (n, 0.0)
    } else if ctx.str("nat_ip_allocate_option") == Some("MANUAL_ONLY") {
        let (total, on_addr) = nat_manual_ips(ctx);
        (total - on_addr, on_addr)
    } else {
        (nat_ips_needed(vms.map(|v| v.0).unwrap_or(1.0), nat_min_ports(ctx)), 0.0)
    };
    if on_addr > 0.0 {
        let n = format!("{} NAT IP(s) are billed on their google_compute_address resources", fmt_number(on_addr));
        gw.note = Some(match gw.note.take() {
            Some(x) => format!("{x}; {n}"),
            None => n,
        });
    }
    out.push(gw);
    if ips > 0.0 {
        out.push(ctx.hourly_unit(
            format!("NAT IP addresses ({})", fmt_number(ips)),
            "IP-hours",
            ips,
            Query::new(NET, "global", "^Networking Cloud NAT IP Usage$").ondemand(),
        ));
    }
    out.push(dp("^Networking Cloud Nat Data Processing$"));
    out
}

// ---- Cloud VPN / Interconnect -----------------------------------------------------

fn vpn_tunnel(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let mut out = vec![ctx.hourly("VPN tunnel", 1.0, Query::new(NET, &region, "^Networking Cloud VPN Tunnel [A-Za-z]+[0-9]+$").ondemand())];
    let gb = ctx.usage_num("monthly_egress_gb");
    let local = ["vpn_gateway", "target_vpn_gateway"]
        .iter()
        .find_map(|k| ctx.str(k))
        .and_then(|g| ctx.resolve_ref(g, "google_compute_"))
        .map(|g| res_region(ctx, g))
        .unwrap_or_else(|| region.clone());
    let peer = ctx.str("peer_gcp_gateway").and_then(|p| ctx.resolve_ref(p, "google_compute_ha_vpn_gateway"));
    if ctx.get("peer_gcp_gateway").is_some() || ctx.is_unknown("peer_gcp_gateway") {
        let peer_region = peer.map(|p| res_region(ctx, p));
        match peer_region {
            Some(pr) if pr != local => out.push(inter_region_egress_component(ctx, &local, &pr, gb)),
            _ => {
                let mut c = ctx.fixed("Egress to peer Cloud VPN gateway", "GiB", gb, Some(0.0));
                c.usage_based = true;
                c.note = Some("same-region traffic between Cloud VPN gateways is billed as inter-zone data transfer".into());
                out.push(c);
            }
        }
    } else {
        let mut c = ctx.usage_qty(
            "Egress through the tunnel (internet rates)",
            "GiB",
            gb,
            Query::new(COMPUTE, &local, "^Network Vpn Internet Data Transfer Out from .* to Americas$")
                .group("VPNInternetEgress")
                .ondemand(),
        );
        c.note = Some("peer outside Google Cloud; worldwide destinations excl. China and Australia".into());
        out.push(c);
    }
    out
}

/// `BPS_1G` -> ("1Gbps", 1.0)
fn bandwidth(b: &str) -> Option<(&'static str, f64)> {
    Some(match b {
        "BPS_50M" => ("50Mbps", 0.05),
        "BPS_100M" => ("100Mbps", 0.1),
        "BPS_200M" => ("200Mbps", 0.2),
        "BPS_300M" => ("300Mbps", 0.3),
        "BPS_400M" => ("400Mbps", 0.4),
        "BPS_500M" => ("500Mbps", 0.5),
        "BPS_1G" => ("1Gbps", 1.0),
        "BPS_2G" => ("2Gbps", 2.0),
        "BPS_5G" => ("5Gbps", 5.0),
        "BPS_10G" => ("10Gbps", 10.0),
        "BPS_20G" => ("20Gbps", 20.0),
        "BPS_50G" => ("50Gbps", 50.0),
        "BPS_100G" => ("100Gbps", 100.0),
        "BPS_400G" => ("400Gbps", 400.0),
        _ => return None,
    })
}

/// Location name used by Cloud Interconnect data transfer SKUs.
fn interconnect_geo(region: &str) -> &'static str {
    match geo(region) {
        Geo::NorthAmerica if region.starts_with("northamerica-south") => "Mexico",
        Geo::NorthAmerica if region.starts_with("northamerica") => "Canada",
        Geo::NorthAmerica => "UnitedStates",
        Geo::Europe => "Europe",
        Geo::MiddleEast => "MiddleEast",
        Geo::Africa => "Africa",
        Geo::SouthAmerica => "SouthAmerica",
        Geo::Oceania => "Australia",
        Geo::Asia if region.starts_with("asia-south") && !region.starts_with("asia-southeast") => "India",
        Geo::Asia if region == "asia-southeast2" => "Indonesia",
        Geo::Asia => "Asia",
    }
}

fn interconnect_attachment(ctx: &Ctx) -> Vec<Component> {
    let dedicated_default = ctx.get("interconnect").is_some() || ctx.is_unknown("interconnect");
    let typ = ctx.str("type").unwrap_or(if dedicated_default { "DEDICATED" } else { "PARTNER" });
    if typ == "PARTNER_PROVIDER" {
        return vec![];
    }
    let partner = typ == "PARTNER";
    let bw = ctx.usage_str("bandwidth").or_else(|| ctx.str("bandwidth")).map(str::to_string).unwrap_or_else(|| {
        if partner {
            ctx.warn("partner VLAN attachment bandwidth is set by the partner; assuming BPS_1G (usage 'bandwidth')");
            "BPS_1G".into()
        } else {
            "BPS_10G".into()
        }
    });
    let Some((label, gbps)) = bandwidth(&bw) else {
        ctx.warn(format!("unknown VLAN attachment bandwidth '{bw}'"));
        return vec![];
    };
    let pat = if partner {
        format!("^Cloud Interconnect - {label} VLAN attachment via Google partner$")
    } else {
        format!("^Cloud Interconnect - {} VLAN attachment via Dedicated Interconnect$", if gbps <= 10.0 { "10Gbps" } else { label })
    };
    let kind = if partner { "Partner" } else { "Dedicated" };
    let region = ctx.region();
    let mut att = ctx.hourly(format!("VLAN attachment ({kind}, {label})"), 1.0, Query::new(COMPUTE, "global", &pat).ondemand());
    if partner {
        att.note = Some("the partner's own connectivity fees are billed separately by the partner".into());
    }
    let egress = format!("^Cloud Interconnect - Local Data Transfer in {}$", interconnect_geo(&region));
    vec![att, ctx.usage("Data transfer out", "GiB", "monthly_egress_gb", Query::new(COMPUTE, "global", &egress).ondemand())]
}

fn interconnect(ctx: &Ctx) -> Vec<Component> {
    if ctx.str_or("interconnect_type", "DEDICATED") == "PARTNER" {
        return vec![];
    }
    let (speed, aai) = match ctx.str_or("link_type", "LINK_TYPE_ETHERNET_10G_LR") {
        "LINK_TYPE_ETHERNET_100G_LR" => ("100Gbps", "100G"),
        "LINK_TYPE_ETHERNET_400G_LR4" => ("400Gbps", "400G"),
        _ => ("10Gbps", "10G"),
    };
    let links = ctx.num_or("requested_link_count", 1.0);
    let cross = ctx.get("remote_location").is_some();
    let pat = format!("^{} - {speed} Dedicated circuit$", if cross { "Cross-Cloud Interconnect" } else { "Cloud Interconnect" });
    let mut out = vec![ctx.hourly(
        format!("{} circuits ({speed} x{})", if cross { "Cross-Cloud Interconnect" } else { "Interconnect" }, fmt_number(links)),
        links,
        Query::new(COMPUTE, "global", &pat).ondemand(),
    )];
    if ctx.bool_or("aai_enabled", false) {
        let p = format!("^Cloud Interconnect - +Application Awareness on {aai} circuit$");
        out.push(ctx.hourly("Application Awareness", links, Query::new(COMPUTE, "global", &p).ondemand()));
    }
    out
}

// ---- Serverless VPC Access ---------------------------------------------------------

fn connector_instances(ctx: &Ctx) -> (f64, Option<f64>) {
    let min = ctx.num("min_instances").or_else(|| ctx.num("min_throughput").map(|t| (t / 100.0).ceil())).unwrap_or(2.0);
    let max = ctx.num("max_instances").or_else(|| ctx.num("max_throughput").map(|t| (t / 100.0).ceil()));
    (min, max)
}

fn vpc_connector(ctx: &Ctx) -> Vec<Component> {
    let machine = ctx.str_or("machine_type", "e2-micro");
    let (min, max) = connector_instances(ctx);
    let count = ctx.usage_num("instances").unwrap_or(min);
    let spec = VmSpec {
        machine: machine.to_string(),
        pricing: Pricing::OnDemand,
        count,
        disks: vec![],
        local_ssd_gb: 0.0,
        gpus: vec![],
        ephemeral_ips: 0.0,
        image: None,
    };
    let mut comps = vm_components(ctx, &ctx.region(), &spec);
    if let Some(c) = comps.first_mut() {
        c.name = format!("Connector instances ({machine} x{})", fmt_number(count));
        if ctx.usage_num("instances").is_none()
            && let Some(max) = max.filter(|m| *m > min)
        {
            c.note = Some(format!(
                "scales from {} to {} instances with traffic and does not scale down; estimated at the minimum",
                fmt_number(min),
                fmt_number(max)
            ));
        }
    }
    comps
}

// ---- Cloud DNS -----------------------------------------------------------------

fn dns_zone(ctx: &Ctx) -> Vec<Component> {
    let visibility = ctx.str_or("visibility", "public");
    let q = Query::new(DNS, "global", "^ManagedZone$").ondemand();
    let mut zone = ctx.monthly(format!("Managed zone ({visibility})"), "months", 1.0, q);
    let n = ctx.siblings.iter().filter(|s| s.rtype == "google_dns_managed_zone").count().max(1) as f64;
    if n > 25.0
        && let Some(p) = ctx.price(q)
    {
        let avg = p.cost(n) / n;
        zone.unit_price = Some(avg);
        zone.monthly_cost = Some(avg);
        zone.note = Some(format!("average of the tiered price over {} zones", fmt_number(n)));
    }
    vec![zone, ctx.usage("DNS queries", "requests", "monthly_queries", Query::new(DNS, "global", "^DNS Query \\(port 53\\)$").ondemand())]
}

// ---- other networking products ---------------------------------------------------

fn packet_mirroring(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    vec![ctx.usage(
        "Mirrored data processed",
        "GiB",
        "monthly_data_processed_gb",
        Query::new(COMPUTE, &region, "^Network Packet Mirroring: Data Processing Charge in ").ondemand(),
    )]
}

fn ncc_spoke(ctx: &Ctx) -> Vec<Component> {
    let linked = |k: &str| ctx.block(k).is_some();
    let (kind, pat) = if linked("linked_vpc_network") || linked("linked_producer_vpc_network") {
        ("VPC", "^Networking Network Connectivity Center Spoke Hours VPC Network Spoke$")
    } else if linked("linked_vpn_tunnels") {
        ("VPN", "^Networking Network Connectivity Center Spoke Hours VPN spoke$")
    } else if linked("linked_interconnect_attachments") {
        ("Interconnect", "^Networking Network Connectivity Center Spoke Hours Interconnect Spoke$")
    } else if linked("linked_router_appliance_instances") {
        ("Router appliance", "^Networking Network Connectivity Center Spoke Hours Router Appliance Spoke$")
    } else {
        ctx.warn("spoke type unknown (no linked_* block)");
        return vec![];
    };
    let mut spoke = ctx.hourly(format!("Spoke ({kind})"), 1.0, Query::new(NET, "global", pat).ondemand());
    let mut out = vec![];
    if matches!(kind, "VPN" | "Interconnect") {
        spoke.note = Some("spoke hours are waived for up to 3 VPN and 3 Interconnect spokes".into());
        out.push(spoke);
    } else {
        out.push(spoke);
        if kind == "VPC" {
            out.push(ctx.usage(
                "Advanced Data Networking (traffic between VPC spokes)",
                "GiB",
                "monthly_data_processed_gb",
                Query::new(NET, "global", "^Networking Data Transfer for advanced data networking$").ondemand(),
            ));
        }
    }
    out
}

fn swp_gateway(ctx: &Ctx) -> Vec<Component> {
    if ctx.str("type") != Some("SECURE_WEB_GATEWAY") {
        return vec![];
    }
    vec![
        ctx.hourly("Secure Web Proxy gateway", 1.0, Query::new(NET, "global", "^Networking Cloud SWP Standard Gateway$").ondemand()),
        ctx.usage(
            "Data processed",
            "GiB",
            "monthly_data_processed_gb",
            Query::new(NET, "global", "^Networking Cloud SWP Standard Data Processing$").ondemand(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geos() {
        assert_eq!(geo("europe-west1"), Geo::Europe);
        assert_eq!(geo("us-central1"), Geo::NorthAmerica);
        assert_eq!(geo("northamerica-northeast1"), Geo::NorthAmerica);
        assert_eq!(geo("asia-southeast1"), Geo::Asia);
        assert_eq!(geo("me-central2"), Geo::MiddleEast);
        assert_eq!(interconnect_geo("northamerica-south1"), "Mexico");
        assert_eq!(interconnect_geo("asia-south1"), "India");
        assert_eq!(interconnect_geo("asia-southeast1"), "Asia");
        assert_eq!(interconnect_geo("asia-southeast2"), "Indonesia");
    }

    #[test]
    fn bandwidths() {
        assert_eq!(bandwidth("BPS_1G"), Some(("1Gbps", 1.0)));
        assert_eq!(bandwidth("BPS_50M"), Some(("50Mbps", 0.05)));
        assert_eq!(bandwidth("BPS_3G"), None);
    }

    #[test]
    fn nat_ip_estimate() {
        assert_eq!(nat_ips_needed(0.0, 64.0), 1.0);
        assert_eq!(nat_ips_needed(1008.0, 64.0), 1.0);
        assert_eq!(nat_ips_needed(1009.0, 64.0), 2.0);
        assert_eq!(nat_ips_needed(10.0, 32768.0), 6.0);
    }
}

mod rules {
    use super::*;

    /// Several Cloud NGFW Enterprise endpoints in the same zone, or an
    /// endpoint in a non-production environment.
    pub fn ngfw_endpoint(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let uptime = cr.components.first().and_then(|c| c.monthly_cost).unwrap_or(0.0);
        if uptime <= 0.0 {
            return out;
        }
        let zone = ctx.str("location");
        let mut peers: Vec<&ResourceInstance> =
            ctx.siblings.iter().filter(|s| s.rtype == ctx.r.rtype && s.attrs.get("location").and_then(|v| v.as_str()) == zone).collect();
        peers.sort_by(|a, b| a.address.cmp(&b.address));
        if peers.len() > 1 && peers[0].address == ctx.r.address {
            out.push(ctx.rec(
                "ngfw-endpoint-consolidation",
                Severity::Medium,
                format!("{} Cloud NGFW Enterprise endpoints in {}", peers.len(), zone.unwrap_or("the same zone")),
                format!(
                    "Each endpoint costs ~{}/mo. A zonal endpoint can inspect traffic for several VPC networks through firewall endpoint associations, so one endpoint per zone is usually enough.",
                    ctx.money(uptime)
                ),
                Some(uptime * (peers.len() - 1) as f64),
            ));
        }
        if ctx.env == Env::NonProd {
            out.push(ctx.rec(
                "ngfw-endpoint-nonprod",
                Severity::Low,
                "Cloud NGFW Enterprise endpoint in a non-production environment",
                format!(
                    "The endpoint alone costs ~{}/mo before inspected traffic. If intrusion prevention (L7 inspection) is not required outside production, Cloud NGFW Standard firewall policies may be enough.",
                    ctx.money(uptime)
                ),
                None,
            ));
        }
        out
    }

    pub fn forwarding_rule(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = proxy_consolidation(ctx, cr);
        out.extend(rule_consolidation(ctx));
        out
    }

    /// Several Envoy-based internal Application Load Balancers in the same
    /// region each pay the minimum of 3 proxy instances.
    fn proxy_consolidation(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let Lb::Proxy { proxy, .. } = classify(ctx.r) else { return vec![] };
        let global = ctx.r.rtype == "google_compute_global_forwarding_rule";
        let loc = if global { "global".to_string() } else { ctx.region() };
        let mut peers: Vec<&ResourceInstance> = ctx
            .siblings
            .iter()
            .filter(|s| s.rtype == ctx.r.rtype)
            .filter(|s| matches!(classify(s), Lb::Proxy { proxy: p, .. } if p == proxy))
            .filter(|s| global || res_region(ctx, s) == loc)
            .collect();
        peers.sort_by(|a, b| a.address.cmp(&b.address));
        if peers.len() < 2 || peers[0].address != ctx.r.address {
            return vec![];
        }
        let per_lb = cr.components.iter().find(|c| c.name.starts_with("Proxy instances")).and_then(|c| c.monthly_cost);
        let Some(per_lb) = per_lb else { return vec![] };
        let n = peers.len() as f64;
        let s = per_lb * (n - 1.0);
        vec![ctx.rec(
            "lb-internal-alb-consolidation",
            Severity::Low,
            format!("{} internal Application Load Balancers in {loc} each pay the 3-proxy minimum", peers.len()),
            format!(
                "Every internal Application Load Balancer is allocated at least 3 proxy instances (~{}/mo) even without traffic. Serving these backends from a single load balancer (one forwarding rule and target proxy, with URL map host/path rules pointing to each backend service) would save ~{}/mo while traffic stays within the capacity of the shared proxies. Keep them separate if they need different networks, TLS policies or isolation. Load balancers: {}.",
                ctx.money(per_lb),
                ctx.money(s),
                peers.iter().map(|p| p.address.as_str()).collect::<Vec<_>>().join(", ")
            ),
            Some(s),
        )]
    }

    fn rule_consolidation(ctx: &Ctx) -> Vec<Recommendation> {
        let Lb::Rule { fr, global, external: true, .. } = classify(ctx.r) else { return vec![] };
        let loc = if global { "global".to_string() } else { ctx.region() };
        let bucket = fr_bucket(ctx, fr, global, &loc);
        let first_external = bucket.iter().find(|s| matches!(classify(s), Lb::Rule { external: true, .. }));
        if bucket.len() <= 5 || first_external.map(|s| s.address.as_str()) != Some(ctx.r.address.as_str()) {
            return vec![];
        }
        let (_, add_pat) = rule_patterns(fr, global);
        let Some(add) = ctx.price(Query::new(NET, &loc, &add_pat).ondemand()).map(|p| p.unit_price()) else { return vec![] };
        let extra = (bucket.len() - 5) as f64;
        let s = extra * add * ctx.monthly_hours();
        vec![ctx.rec(
            "lb-forwarding-rule-consolidation",
            Severity::Info,
            format!("{} forwarding rules billed on the same SKU in {loc}", bucket.len()),
            format!(
                "The first 5 forwarding rules per project and region share one hourly minimum; each additional rule costs {add}/h (~{}/mo for the {} extra). Consolidate load balancers where possible: one HTTPS load balancer can serve many hostnames with URL map host rules and multiple certificates (or Certificate Manager maps), and one forwarding rule can expose several ports.",
                ctx.money(s),
                fmt_number(extra)
            ),
            Some(s),
        )]
    }

    pub fn cdn(ctx: &Ctx, _cr: &CostedResource) -> Vec<Recommendation> {
        if !ctx.bool_or("enable_cdn", false) {
            return vec![];
        }
        let (egress, fill) = (ctx.usage_num("monthly_cache_egress_gb"), ctx.usage_num("monthly_cache_fill_gb"));
        if let (Some(e), Some(f)) = (egress, fill)
            && e > 0.0
            && f / e > 0.2
        {
            let s = cdn_skus(&ctx.region());
            let pat = format!("^Network CDN {} Intra-Region Cache Fill$", s.fill);
            let fill_price = ctx.price(Query::new(COMPUTE, s.fill_region, &pat).ondemand()).map(|p| p.unit_price()).unwrap_or(0.0);
            let target = e * 0.1;
            let savings = ctx.savings_vs(f * fill_price, Some(target * fill_price));
            return vec![ctx.rec(
                "cdn-cache-hit",
                Severity::Low,
                format!("Low Cloud CDN cache hit ratio (cache fill is {:.0}% of cache egress)", f / e * 100.0),
                "A high cache fill ratio means most requests miss the cache and also incur load balancer data processing and origin egress. Increase cdn_policy default_ttl/max_ttl, use cache_mode = \"CACHE_ALL_STATIC\" or \"FORCE_CACHE_ALL\" for static content, trim cache keys (cdn_policy.cache_key_policy: drop query strings/cookies that do not affect the response) and consider negative caching.",
                savings,
            )];
        }
        if ctx.str("cdn_policy.0.cache_mode") == Some("USE_ORIGIN_HEADERS") {
            return vec![ctx.rec(
                "cdn-cache-hit",
                Severity::Info,
                "Cloud CDN only caches responses with explicit Cache-Control headers",
                "cache_mode = \"USE_ORIGIN_HEADERS\" caches nothing unless the origin sends cache directives, so requests fall through to the origin and pay load balancer data processing and origin egress. Use \"CACHE_ALL_STATIC\" unless the origin controls caching carefully.",
                None,
            )];
        }
        vec![]
    }

    pub fn security_policy(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        if armor_tier_of(ctx).is_some_and(|t| t.starts_with("CA_ENTERPRISE")) {
            return out;
        }
        let rules = policy_rule_count(ctx);
        if rules > 20.0 {
            out.push(ctx.rec(
                "armor-rule-count",
                Severity::Info,
                format!("Security policy has {} rules billed per rule", fmt_number(rules)),
                format!(
                    "Cloud Armor Standard bills {}/rule/month. Merge rules with the same action: one rule can match up to 10 IP ranges (src_ip_ranges) and CEL expressions can combine conditions, while preconfigured WAF rules can be grouped in a single evaluatePreconfiguredWaf() expression.",
                    ctx.money(cr.components.iter().find(|c| c.name.starts_with("Rules")).and_then(|c| c.unit_price).unwrap_or(1.0))
                ),
                None,
            ));
        }
        if cr.monthly_cost > 200.0 {
            out.push(ctx.rec(
                "armor-enterprise-breakeven",
                Severity::Info,
                "Compare Cloud Armor Standard with Enterprise Paygo",
                format!(
                    "This policy costs ~{}/mo on the Standard tier. Cloud Armor Enterprise Paygo ($200/mo per project incl. 2 protected resources, plus a data processing fee on internet egress) includes policies, rules and requests, and adds adaptive protection and DDoS response. Enable it with google_compute_project_cloud_armor_tier if egress volumes keep the data processing fee low.",
                    ctx.money(cr.monthly_cost)
                ),
                None,
            ));
        }
        out
    }

    pub fn armor_tier(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        if ctx.env != Env::NonProd || cr.monthly_cost <= 1.0 || ctx.str("cloud_armor_tier") != Some("CA_ENTERPRISE_PAYGO") {
            return vec![];
        }
        vec![ctx.rec(
            "armor-enterprise-nonprod",
            Severity::Medium,
            "Cloud Armor Enterprise enabled in a non-production project",
            "Enterprise Paygo costs at least $200/mo per project plus data processing. Standard tier (cloud_armor_tier = \"CA_STANDARD\") is usually enough for dev/test: WAF rules and rate limiting work on both tiers; adaptive protection, DDoS response and threat intelligence are Enterprise-only.",
            Some(cr.monthly_cost),
        )]
    }

    pub fn nat(ctx: &Ctx, _cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let region = ctx
            .str("router")
            .and_then(|r| ctx.resolve_ref(r, "google_compute_router"))
            .map(|r| res_region(ctx, r))
            .unwrap_or_else(|| ctx.region());
        if ctx.str("type") != Some("PRIVATE")
            && ctx.usage_num("nat_ips").is_none()
            && ctx.str("nat_ip_allocate_option") == Some("MANUAL_ONLY")
        {
            let (total, _) = nat_manual_ips(ctx);
            if let Some((vms, _)) = nat_vms(ctx, &region) {
                let needed = nat_ips_needed(vms, nat_min_ports(ctx));
                let extra = total - needed;
                let price = ctx.price(Query::new(NET, "global", "^Networking Cloud NAT IP Usage$").ondemand()).map(|p| p.unit_price());
                if let (true, Some(p)) = (extra >= 1.0, price) {
                    let s = extra * p * ctx.monthly_hours();
                    out.push(ctx.rec(
                        "nat-idle-ips",
                        Severity::Low,
                        format!("{} manual NAT IPs for ~{} VMs; {} should suffice", fmt_number(total), fmt_number(vms), fmt_number(needed)),
                        format!(
                            "Each NAT IP provides 64,512 source ports; with min_ports_per_vm = {} about {} IP(s) cover {} VMs. Every NAT IP in use is billed hourly. Remove unneeded addresses from nat_ips (or move them to drain_nat_ips first). Keep extra IPs if partners allowlist them, or if you raise min_ports_per_vm / use many connections to the same destination.",
                            fmt_number(nat_min_ports(ctx)),
                            fmt_number(needed),
                            fmt_number(vms)
                        ),
                        Some(s),
                    ));
                }
            }
        }
        let log_all = ctx.bool_or("log_config.0.enable", false) && ctx.str("log_config.0.filter") == Some("ALL");
        if log_all && ctx.env == Env::NonProd {
            out.push(ctx.rec(
                "nat-logging-all",
                Severity::Low,
                "Cloud NAT logs every connection in a non-production environment",
                "log_config.filter = \"ALL\" logs every NAT translation; network telemetry logs are billed ($0.25/GiB for the first 10 TiB) in addition to Cloud Logging storage. Use filter = \"ERRORS_ONLY\" (or disable logging) outside production.",
                None,
            ));
        }
        out
    }

    pub fn classic_vpn(ctx: &Ctx, _cr: &CostedResource) -> Vec<Recommendation> {
        vec![ctx.rec(
            "vpn-classic-deprecated",
            Severity::Info,
            "Migrate Classic VPN to HA VPN",
            "Classic VPN offers a 99.9% SLA and its dynamic (BGP) routing is deprecated. HA VPN (google_compute_ha_vpn_gateway) costs the same per tunnel, offers a 99.99% SLA with two tunnels, and does not need the reserved external IP and ESP/UDP forwarding rules of Classic VPN.",
            None,
        )]
    }

    pub fn vpc_connector(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        let mut out = vec![];
        let (min, _) = connector_instances(ctx);
        let machine = ctx.str_or("machine_type", "e2-micro");
        // Savings are incremental (min_instances, then machine type, then the
        // remaining connector cost) so that they add up.
        let mut remaining = cr.monthly_cost;
        if ctx.env == Env::NonProd && ctx.usage_num("instances").is_none() {
            let lower_min = |a: &mut crate::hcl::value::Map| {
                a.insert("min_instances".into(), Value::from(2.0));
                a.shift_remove("min_throughput");
            };
            let mut lowered = false;
            if min > 2.0
                && let Some(s) = ctx.savings_vs(remaining, ctx.reprice(lower_min))
            {
                remaining -= s;
                lowered = true;
                out.push(ctx.rec(
                    "vpc-connector-min-instances",
                    Severity::Medium,
                    format!("Lower min_instances of this non-production connector from {} to 2", fmt_number(min)),
                    "Connector instances run (and are billed) permanently at min_instances and never scale back down. Two instances are the minimum and are enough for most dev/test traffic; the connector still scales up to max_instances under load. Changing min_instances recreates the connector.",
                    Some(s),
                ));
            }
            if !matches!(machine, "e2-micro" | "f1-micro") {
                let new = ctx.reprice(|a| {
                    if lowered {
                        lower_min(a);
                    }
                    a.insert("machine_type".into(), Value::from("e2-micro"));
                });
                if let Some(s) = ctx.savings_vs(remaining, new) {
                    remaining -= s;
                    out.push(ctx.rec(
                        "vpc-connector-machine-type",
                        Severity::Medium,
                        format!("Use e2-micro instead of {machine} for this non-production connector"),
                        "e2-micro connector instances provide ~100 Mbps each, usually sufficient outside production; add instances rather than larger machines if throughput is needed. Changing machine_type recreates the connector.",
                        Some(s),
                    ));
                }
            }
        }
        if remaining > 1.0 {
            out.push(ctx.rec(
                "vpc-connector-direct-egress",
                Severity::Low,
                "Consider Direct VPC egress instead of a Serverless VPC Access connector",
                "Cloud Run services/jobs (and Cloud Run functions) can send traffic directly into a VPC subnet (vpc_access { network_interfaces { ... } }) with no connector instances to pay for; only standard network egress applies. It uses IP addresses from the subnet (plan a /26 or larger) and is not available for App Engine standard or 1st gen functions.",
                Some(remaining),
            ));
        }
        out
    }

    pub fn dns_zone(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
        if ctx.str_or("visibility", "public") != "private" {
            return vec![];
        }
        let Some(dns_name) = ctx.str("dns_name") else { return vec![] };
        let mut same: Vec<&ResourceInstance> = ctx
            .siblings
            .iter()
            .filter(|s| s.rtype == "google_dns_managed_zone")
            .filter(|s| s.attrs.get("visibility").and_then(|v| v.as_str()) == Some("private"))
            .filter(|s| s.attrs.get("dns_name").and_then(|v| v.as_str()) == Some(dns_name))
            .collect();
        same.sort_by(|a, b| a.address.cmp(&b.address));
        if same.len() < 2 || same[0].address != ctx.r.address {
            return vec![];
        }
        let s = cr.components.first().and_then(|c| c.monthly_cost).map(|c| c * (same.len() - 1) as f64);
        vec![ctx.rec(
            "dns-private-zone-count",
            Severity::Info,
            format!("{} private zones serve the same name {dns_name}", same.len()),
            "Each managed zone is billed monthly. A single private zone can be visible from several VPC networks: list them all in private_visibility_config { networks { ... } } (or use DNS peering / cross-project binding) instead of duplicating the zone and its records.",
            s.filter(|v| *v >= 1.0),
        )]
    }
}
