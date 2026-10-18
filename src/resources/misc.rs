//! Security, operations and developer services: Cloud KMS, Secret Manager,
//! Cloud Logging, Cloud Monitoring, Cloud Build, Cloud Workstations,
//! Certificate Manager, Identity Platform, Cloud Deploy and Cloud IDS.

use super::compute::{disk_components, machine_hourly, sud_multiplier};
use super::machine::{self, COMPUTE, MachineType, Pricing};
use super::{Ctx, Env, Spec, set_path, usage};
use crate::cost::{Component, CostedResource, HOURS_PER_MONTH, Recommendation, Severity};
use crate::hcl::value::{Value, fmt_number};
use crate::pricing::query::Query;
use crate::tf::evaluate::ResourceInstance;

const KMS: &str = "Cloud Key Management Service (KMS)";
const SECRETS: &str = "Secret Manager";
const LOGGING: &str = "Cloud Logging";
const MONITORING: &str = "Cloud Monitoring";
const BUILD: &str = "Cloud Build";
const WORKSTATIONS: &str = "Cloud Workstations";
const CERTS: &str = "Certificate Manager";
const IDENTITY: &str = "Identity Platform";
const DEPLOY: &str = "Cloud Deploy";
const IDS: &str = "Cloud IDS";

const DAYS_PER_MONTH: f64 = 365.0 / 12.0;
const SECONDS_PER_MONTH: f64 = HOURS_PER_MONTH * 3600.0;

pub fn specs() -> Vec<Spec> {
    const KMS_KEY: &[super::UsageParam] = usage!(
        "key_versions" => "Active (enabled or disabled) key versions; grows with rotation until old versions are destroyed (default 1)",
        "monthly_key_operations" => "Cryptographic operations per month",
    );
    const SECRET: &[super::UsageParam] = usage!(
        "active_versions" => "Active secret versions (default 1, or 0 when versions are managed as separate resources)",
        "monthly_access_operations" => "Secret access operations per month",
    );
    const LOG_BUCKET: &[super::UsageParam] = usage!("monthly_logging_data_gb" => "Log volume ingested into this bucket per month in GiB");
    const BUILD_MINUTES: &[super::UsageParam] = usage!("monthly_build_minutes" => "Build minutes per month");
    const POOL_MINUTES: &[super::UsageParam] =
        usage!("monthly_build_minutes" => "Build minutes per month on this pool (excluding builds already estimated on triggers)");
    const WS_CONFIG: &[super::UsageParam] = usage!(
        "workstations" => "Workstations using this configuration not declared as google_workstations_workstation (default 1 if none are declared)",
        "monthly_hours_per_workstation" => "Hours per month each workstation runs",
    );
    const WS: &[super::UsageParam] = usage!("monthly_hours_per_workstation" => "Hours per month the workstation runs");
    const METRIC: &[super::UsageParam] = usage!(
        "time_series" => "Active time series written for this metric",
        "monthly_ingested_mib" => "Ingested data per month in MiB (byte-billed metrics, overrides time_series)",
        "monthly_samples" => "Ingested samples per month (sample-billed metrics, overrides time_series)",
    );
    const IDP: &[super::UsageParam] = usage!(
        "monthly_active_users" => "Monthly active users signing in with email, phone, anonymous or social providers (tier 1)",
        "monthly_saml_oidc_active_users" => "Monthly active users signing in with SAML or OIDC providers (tier 2)",
        "monthly_sms_mfa_verifications" => "Successful SMS multi-factor verifications per month in the US, Canada and India",
        "monthly_sms_mfa_verifications_other" => "Successful SMS multi-factor verifications per month in other countries",
    );
    vec![
        Spec::new("google_kms_crypto_key", kms_key).usage(KMS_KEY).rules(kms_rules),
        Spec::new("google_kms_crypto_key_version", kms_key_version),
        Spec::new("google_kms_key_handle", kms_key_handle).usage(KMS_KEY),
        Spec::new("google_secret_manager_secret", secret).usage(SECRET).rules(secret_rules),
        Spec::new("google_secret_manager_regional_secret", secret).usage(SECRET).rules(secret_rules),
        Spec::new("google_secret_manager_secret_version", secret_version),
        Spec::new("google_secret_manager_regional_secret_version", secret_version),
        Spec::new("google_logging_project_bucket_config", log_bucket).usage(LOG_BUCKET).rules(log_bucket_rules),
        Spec::new("google_logging_folder_bucket_config", log_bucket).usage(LOG_BUCKET).rules(log_bucket_rules),
        Spec::new("google_logging_organization_bucket_config", log_bucket).usage(LOG_BUCKET).rules(log_bucket_rules),
        Spec::new("google_logging_billing_account_bucket_config", log_bucket).usage(LOG_BUCKET).rules(log_bucket_rules),
        Spec::new("google_monitoring_uptime_check_config", uptime_check).rules(uptime_rules),
        Spec::new("google_monitoring_metric_descriptor", metric_descriptor).usage(METRIC),
        Spec::new("google_cloudbuild_trigger", build_trigger).usage(BUILD_MINUTES),
        Spec::new("google_cloudbuild_worker_pool", build_pool).usage(POOL_MINUTES).rules(build_pool_rules),
        Spec::new("google_workstations_workstation_cluster", ws_cluster).rules(ws_cluster_rules),
        Spec::new("google_workstations_workstation_config", ws_config).usage(WS_CONFIG).rules(ws_config_rules),
        Spec::new("google_workstations_workstation", workstation).usage(WS),
        Spec::new("google_certificate_manager_certificate", certificate),
        Spec::new("google_certificate_manager_trust_config", trust_config)
            .usage(usage!("monthly_mtls_connections" => "Load balancer connections authenticated with mTLS per month")),
        Spec::new("google_identity_platform_config", identity_platform).usage(IDP),
        Spec::new("google_identity_platform_tenant", identity_platform).usage(IDP),
        Spec::new("google_clouddeploy_delivery_pipeline", deploy_pipeline),
        Spec::new("google_cloud_ids_endpoint", ids_endpoint)
            .usage(usage!("monthly_inspected_data_gb" => "Traffic inspected by the endpoint per month in GB"))
            .rules(ids_rules),
    ]
}

// ---- helpers ---------------------------------------------------------------

/// Seconds of a protobuf duration such as `"7776000s"`.
fn duration_secs(s: &str) -> Option<f64> {
    s.trim().strip_suffix('s')?.parse().ok()
}

fn str_at<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    v.path(path).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
}

fn num_at(v: &Value, path: &str) -> Option<f64> {
    v.path(path).and_then(|x| x.as_f64())
}

fn list_at<'a>(v: &'a Value, path: &str) -> Vec<&'a Value> {
    match v.path(path) {
        Some(Value::List(l)) => l.iter().collect(),
        Some(m @ Value::Map(_)) => vec![m],
        _ => vec![],
    }
}

/// `locations/<loc>` segment of a resource id.
fn location_segment(id: &str) -> Option<String> {
    let mut it = id.split('/');
    while let Some(p) = it.next() {
        if p == "locations" {
            return it.next().filter(|l| !l.is_empty()).map(str::to_lowercase);
        }
    }
    None
}

/// First location with a SKU matching `pat` (list prices are mostly uniform,
/// but not every location is listed in the catalog).
fn pick<'a>(ctx: &Ctx, service: &str, locs: &[&'a str], pat: &str) -> &'a str {
    locs.iter().copied().find(|l| ctx.price(Query::new(service, l, pat)).is_some()).unwrap_or(locs[0])
}

/// Resources of type `rtype` whose `attr` points at the current resource.
fn referrers<'a>(ctx: &Ctx<'a>, rtype: &str, attr: &str) -> Vec<&'a ResourceInstance> {
    let short = format!("{}.{}", ctx.r.rtype, ctx.r.name);
    ctx.siblings
        .iter()
        .filter(|s| s.rtype == rtype)
        .filter(|s| {
            (s.module_path == ctx.r.module_path && s.refs.contains(&short))
                || s.attrs
                    .get(attr)
                    .and_then(|v| v.as_str())
                    .and_then(|v| ctx.resolve_ref(v, &ctx.r.rtype))
                    .is_some_and(|r| r.address == ctx.r.address)
        })
        .collect()
}

/// Mark a computed-price component as usage-based.
fn usage_based(mut c: Component) -> Component {
    c.usage_based = true;
    c
}

/// A usage-based component with a known quantity but no price is a missing
/// price (e.g. the SKU does not exist in the region), not a usage gap.
fn strict(mut comps: Vec<Component>) -> Vec<Component> {
    for c in comps.iter_mut().filter(|c| c.quantity.is_some() && c.unit_price.is_none()) {
        c.usage_based = false;
    }
    comps
}

fn with_note(mut c: Component, note: impl Into<String>) -> Component {
    c.note = Some(note.into());
    c
}

fn total_of(comps: &[Component], pred: impl Fn(&Component) -> bool) -> f64 {
    comps.iter().filter(|c| pred(c)).filter_map(|c| c.monthly_cost).sum()
}

// ---- Cloud KMS -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum KeyAlgo {
    Symmetric,
    Mac,
    Rsa(u32),
    EcP256,
    EcP384,
    Secp256k1,
    OtherAsymmetric,
}

impl KeyAlgo {
    fn parse(purpose: &str, algorithm: &str) -> KeyAlgo {
        let a = algorithm.to_uppercase();
        if a.starts_with("HMAC_") {
            KeyAlgo::Mac
        } else if a.starts_with("RSA_") {
            let bits = [2048, 3072, 4096].into_iter().find(|b| a.contains(&format!("_{b}"))).unwrap_or(2048);
            KeyAlgo::Rsa(bits)
        } else if a.starts_with("EC_SIGN_P256") {
            KeyAlgo::EcP256
        } else if a.starts_with("EC_SIGN_P384") {
            KeyAlgo::EcP384
        } else if a.starts_with("EC_SIGN_SECP256K1") {
            KeyAlgo::Secp256k1
        } else if a.contains("SYMMETRIC_ENCRYPTION") || a.starts_with("AES_") {
            KeyAlgo::Symmetric
        } else {
            match purpose {
                "MAC" => KeyAlgo::Mac,
                "ENCRYPT_DECRYPT" | "RAW_ENCRYPT_DECRYPT" => KeyAlgo::Symmetric,
                _ => KeyAlgo::OtherAsymmetric,
            }
        }
    }

    fn label(self) -> String {
        match self {
            KeyAlgo::Symmetric => "symmetric".into(),
            KeyAlgo::Mac => "MAC".into(),
            KeyAlgo::Rsa(b) => format!("RSA {b}"),
            KeyAlgo::EcP256 => "EC P-256".into(),
            KeyAlgo::EcP384 => "EC P-384".into(),
            KeyAlgo::Secp256k1 => "EC secp256k1".into(),
            KeyAlgo::OtherAsymmetric => "asymmetric".into(),
        }
    }

    /// Suffix of HSM SKU descriptions for asymmetric/MAC algorithms.
    fn hsm_stem(self) -> Option<String> {
        match self {
            KeyAlgo::Symmetric => None,
            KeyAlgo::Mac => Some("MAC".into()),
            KeyAlgo::Rsa(b) => Some(format!("RSA {b} bit")),
            KeyAlgo::EcP256 | KeyAlgo::OtherAsymmetric => Some("ECDSA P-256".into()),
            KeyAlgo::EcP384 => Some("ECDSA P-384".into()),
            KeyAlgo::Secp256k1 => Some("ECDSA SECP256K1".into()),
        }
    }
}

/// Internal protection level for Autokey-created keys (distinct SKUs).
const AUTOKEY: &str = "HSM_AUTOKEY";

struct KeySpec {
    location: String,
    protection: String,
    algo: KeyAlgo,
}

fn protection_label(p: &str) -> &'static str {
    match p {
        "HSM" => "HSM",
        AUTOKEY => "HSM, Autokey",
        "EXTERNAL" => "external",
        "EXTERNAL_VPC" => "external VPC",
        "HSM_SINGLE_TENANT" => "single-tenant HSM",
        _ => "software",
    }
}

/// SKU patterns (active key version, cryptographic operation).
fn kms_patterns(protection: &str, algo: KeyAlgo) -> (Option<String>, String) {
    let sym = algo == KeyAlgo::Symmetric;
    match protection {
        "HSM" | "HSM_SINGLE_TENANT" => {
            let ops = match algo.hsm_stem() {
                None => "^HSM symmetric cryptographic operations$".to_string(),
                Some(s) if algo == KeyAlgo::Mac => format!("^HSM cryptographic operations with a {s} key$"),
                Some(s) if s.starts_with("ECDSA") => format!("^HSM cryptographic operations with an {s} key$"),
                Some(s) => format!("^HSM cryptographic operations with a {s} key$"),
            };
            // Single-tenant HSM instances include their key versions.
            let version = (protection == "HSM").then(|| match algo.hsm_stem() {
                None => "^Active HSM symmetric key versions$".to_string(),
                Some(s) => format!("^Active HSM {s} key versions$"),
            });
            (version, ops)
        }
        AUTOKEY => {
            (Some("^Active HSM symmetric key versions for Autokey$".into()), "^HSM symmetric cryptographic operations for Autokey$".into())
        }
        "EXTERNAL" | "EXTERNAL_VPC" => {
            let kind = if sym { "symmetric" } else { "asymmetric" };
            (Some(format!("^Active external {kind} key versions$")), format!("^External {kind} cryptographic operations$"))
        }
        _ => match algo {
            KeyAlgo::Symmetric => {
                (Some("^Active software symmetric key versions$".into()), "^Cryptographic operations with a software symmetric key$".into())
            }
            KeyAlgo::Mac => {
                (Some("^Active software MAC key versions$".into()), "^Cryptographic operations with a software MAC key$".into())
            }
            _ => (Some("^Active software asymmetric key versions$".into()), "^Software asymmetric cryptographic operations$".into()),
        },
    }
}

fn kms_key_location(ctx: &Ctx, key: &Value) -> String {
    if let Some(kr) = key.get("key_ring").and_then(|v| v.as_str()) {
        if let Some(loc) = ctx.resolve_ref(kr, "google_kms_key_ring").and_then(|r| str_at(&r.attrs, "location")) {
            return loc.to_lowercase();
        }
        if let Some(loc) = location_segment(kr) {
            return loc;
        }
    }
    ctx.warn("key ring location unknown, using the resource region");
    ctx.region()
}

fn key_spec(ctx: &Ctx, key: &Value) -> KeySpec {
    let purpose = str_at(key, "purpose").unwrap_or("ENCRYPT_DECRYPT");
    let algorithm = str_at(key, "version_template.0.algorithm").unwrap_or("GOOGLE_SYMMETRIC_ENCRYPTION");
    let protection = str_at(key, "version_template.0.protection_level").unwrap_or("SOFTWARE").to_uppercase();
    KeySpec { location: kms_key_location(ctx, key), protection, algo: KeyAlgo::parse(purpose, algorithm) }
}

fn kms_components(ctx: &Ctx, spec: &KeySpec, versions: f64, with_ops: bool) -> Vec<Component> {
    let (version_pat, ops_pat) = kms_patterns(&spec.protection, spec.algo);
    let multi = if spec.location.starts_with("eur") {
        "europe"
    } else if spec.location.starts_with("asia") {
        "asia"
    } else {
        "us"
    };
    let locs = [spec.location.as_str(), multi, "us"];
    let label = format!("{}, {}", protection_label(&spec.protection), spec.algo.label());
    let mut out = vec![];
    if spec.protection == "HSM" && spec.algo == KeyAlgo::OtherAsymmetric {
        ctx.warn("HSM algorithm not listed in the price catalog, priced like EC P-256");
    }
    match version_pat {
        Some(p) if versions > 0.0 => {
            let loc = pick(ctx, KMS, &locs, &p);
            out.push(ctx.monthly(
                format!("Active key versions ({label}) x{}", fmt_number(versions)),
                "key versions",
                versions,
                Query::new(KMS, loc, &p).ondemand(),
            ));
        }
        None if spec.protection == "HSM_SINGLE_TENANT" => {
            ctx.warn("key versions are covered by the single-tenant HSM instance fee (not estimated here)");
        }
        _ => {}
    }
    if with_ops {
        let loc = pick(ctx, KMS, &locs, &ops_pat);
        out.push(ctx.usage(
            format!("Cryptographic operations ({label})"),
            "operations",
            "monthly_key_operations",
            Query::new(KMS, loc, &ops_pat).ondemand(),
        ));
    }
    strict(out)
}

fn kms_key(ctx: &Ctx) -> Vec<Component> {
    let spec = key_spec(ctx, ctx.attrs);
    let no_initial = ctx.bool_or("skip_initial_version_creation", false) || ctx.bool_or("import_only", false);
    let versions = ctx.usage_num("key_versions").unwrap_or(if no_initial { 0.0 } else { 1.0 });
    let mut comps = kms_components(ctx, &spec, versions, true);
    let rotation_days = ctx.str("rotation_period").and_then(duration_secs).map(|s| s / 86400.0);
    if let (Some(days), None) = (rotation_days, ctx.usage_num("key_versions"))
        && let Some(c) = comps.iter_mut().find(|c| c.name.starts_with("Active key versions"))
    {
        c.note = Some(format!(
            "rotated every {} days: previous versions stay billable until destroyed (~{} versions after a year); set usage key_versions",
            fmt_number(days.round()),
            fmt_number((1.0 + 365.0 / days).floor())
        ));
    }
    comps
}

fn kms_key_version(ctx: &Ctx) -> Vec<Component> {
    if matches!(ctx.str("state"), Some("DESTROYED" | "DESTROY_SCHEDULED")) {
        return vec![];
    }
    let parent = ctx.str("crypto_key").and_then(|k| ctx.resolve_ref(k, "google_kms_crypto_key"));
    let spec = match parent {
        Some(p) => key_spec(ctx, &p.attrs),
        None => {
            ctx.warn("crypto key not found in this configuration, assuming a software symmetric key");
            let location = ctx.str("crypto_key").and_then(location_segment).unwrap_or_else(|| ctx.region());
            KeySpec { location, protection: "SOFTWARE".into(), algo: KeyAlgo::Symmetric }
        }
    };
    kms_components(ctx, &spec, 1.0, false)
}

fn kms_key_handle(ctx: &Ctx) -> Vec<Component> {
    let spec = KeySpec {
        location: ctx.str("location").map(str::to_lowercase).unwrap_or_else(|| ctx.region()),
        protection: AUTOKEY.into(),
        algo: KeyAlgo::Symmetric,
    };
    let versions = ctx.usage_num("key_versions").unwrap_or(1.0);
    let mut comps = kms_components(ctx, &spec, versions, true);
    if let Some(c) = comps.first_mut() {
        c.note = Some("Autokey keys rotate yearly; the first 100 Autokey key versions per billing account are free".into());
    }
    comps
}

fn kms_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let spec = key_spec(ctx, ctx.attrs);
    if ctx.env == Env::NonProd && spec.protection == "HSM" {
        let new = ctx.reprice(|a| set_path(a, "version_template.0.protection_level", Value::from("SOFTWARE")));
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "kms-hsm-nonprod",
                Severity::Low,
                "Use software protection for this non-production key",
                "HSM key versions cost $1-2.50 per month versus $0.06 for software keys, and asymmetric HSM operations are 5x more expensive. \
                 Non-production data rarely needs FIPS 140-2 Level 3. The protection level is immutable: create a new key with \
                 version_template.protection_level = \"SOFTWARE\" and re-encrypt the data.",
                Some(s),
            ));
        }
    }
    let rotation_days = ctx.str("rotation_period").and_then(duration_secs).map(|s| s / 86400.0);
    if let (Some(days), None) = (rotation_days, ctx.usage_num("key_versions"))
        && days < 90.0
    {
        let (version_pat, _) = kms_patterns(&spec.protection, spec.algo);
        let unit = version_pat.and_then(|p| {
            ctx.price(Query::new(KMS, &spec.location, &p).ondemand()).or_else(|| ctx.price(Query::new(KMS, "us", &p).ondemand()))
        });
        if let Some(unit) = unit {
            let yearly = (365.0 / days).floor();
            let extra = yearly * unit.unit_price();
            out.push(ctx.rec(
                "kms-rotation-versions",
                Severity::Info,
                format!("Key rotates every {} days: old versions accumulate", fmt_number(days.round())),
                format!(
                    "Each rotation creates a new key version and previous versions remain billable until destroyed. After a year this key \
                     would have ~{} extra active versions (~{}/month more at {} per version). Destroy versions that no longer protect data, \
                     or use a longer rotation_period (90 days is a common baseline).",
                    fmt_number(yearly),
                    ctx.money(extra),
                    ctx.money(unit.unit_price())
                ),
                None,
            ));
        }
    }
    out
}

// ---- Secret Manager --------------------------------------------------------

fn secret_locations(ctx: &Ctx, secret: &Value, regional: bool) -> Vec<String> {
    if regional {
        return vec![str_at(secret, "location").map(str::to_lowercase).unwrap_or_else(|| ctx.region())];
    }
    let replicas: Vec<String> = list_at(secret, "replication.0.user_managed.0.replicas")
        .iter()
        .filter_map(|r| r.get("location").and_then(|l| l.as_str()))
        .map(str::to_lowercase)
        .collect();
    if replicas.is_empty() { vec!["global".into()] } else { replicas }
}

fn secret_storage(ctx: &Ctx, locs: &[String], versions: f64) -> Component {
    const PAT: &str = "^Secret version replica storage$";
    let loc = pick(ctx, SECRETS, &[locs[0].as_str(), "global"], PAT);
    let replicas = locs.len() as f64;
    let name = if replicas > 1.0 {
        format!("Active secret versions ({} x {} replicas)", fmt_number(versions), fmt_number(replicas))
    } else {
        format!("Active secret versions ({})", fmt_number(versions))
    };
    ctx.monthly(name, "version replicas", versions * replicas, Query::new(SECRETS, loc, PAT).ondemand())
}

fn secret(ctx: &Ctx) -> Vec<Component> {
    let regional = ctx.r.rtype == "google_secret_manager_regional_secret";
    let locs = secret_locations(ctx, ctx.attrs, regional);
    let version_type = if regional { "google_secret_manager_regional_secret_version" } else { "google_secret_manager_secret_version" };
    let declared = referrers(ctx, version_type, "secret").len();
    let versions = ctx.usage_num("active_versions").unwrap_or(if declared > 0 { 0.0 } else { 1.0 });
    let mut out = vec![];
    if versions > 0.0 {
        out.push(secret_storage(ctx, &locs, versions));
    }
    let op_loc = pick(ctx, SECRETS, &[locs[0].as_str(), "us-central1"], "^Secret access operations$");
    out.push(ctx.usage(
        "Access operations",
        "operations",
        "monthly_access_operations",
        Query::new(SECRETS, op_loc, "^Secret access operations$").ondemand(),
    ));
    if let Some(period) = ctx.str("rotation.0.rotation_period").and_then(duration_secs).filter(|p| *p > 0.0) {
        let loc = pick(ctx, SECRETS, &[locs[0].as_str(), "us-central1"], "^Secret rotate operations$");
        out.push(ctx.monthly(
            format!("Rotation notifications (every {} days)", fmt_number((period / 86400.0 * 10.0).round() / 10.0)),
            "rotations",
            SECONDS_PER_MONTH / period,
            Query::new(SECRETS, loc, "^Secret rotate operations$").ondemand(),
        ));
    }
    strict(out)
}

fn secret_version(ctx: &Ctx) -> Vec<Component> {
    let regional = ctx.r.rtype == "google_secret_manager_regional_secret_version";
    let parent = ctx.str("secret").and_then(|s| ctx.resolve_ref(s, "google_secret_manager"));
    let locs = match parent {
        Some(p) => secret_locations(ctx, &p.attrs, regional),
        None => {
            ctx.warn("parent secret not found in this configuration, assuming automatic replication");
            vec![ctx.str("secret").and_then(location_segment).unwrap_or_else(|| "global".into())]
        }
    };
    vec![secret_storage(ctx, &locs, 1.0)]
}

fn secret_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let regional = ctx.r.rtype == "google_secret_manager_regional_secret";
    let replicas = secret_locations(ctx, ctx.attrs, regional).len() as f64;
    let storage = total_of(&cr.components, |c| c.name.starts_with("Active secret versions"));
    if replicas >= 3.0 {
        let new = cr.monthly_cost - storage * (replicas - 1.0) / replicas;
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, Some(new)) {
            out.push(ctx.rec(
                "secret-user-managed-replicas",
                Severity::Info,
                format!("Secret is replicated to {} locations", fmt_number(replicas)),
                "Every active version is billed once per replica location, while automatic replication is billed as a single location. \
                 Use replication { auto {} } unless data residency or latency requires user-managed replicas.",
                Some(s),
            ));
        }
    }
    if let Some(v) = ctx.usage_num("active_versions").filter(|v| *v >= 10.0) {
        let new = cr.monthly_cost - storage * (v - 2.0) / v;
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, Some(new)) {
            out.push(ctx.rec(
                "secret-old-versions",
                Severity::Low,
                format!("{} active versions are kept for this secret", fmt_number(v)),
                "Disabled versions are still billed. Destroy versions that are no longer used (keep the current and previous one for rollback) \
                 and set version_destroy_ttl so that destroyed versions are cleaned up automatically.",
                Some(s),
            ));
        }
    }
    out
}

// ---- Cloud Logging ---------------------------------------------------------

fn log_bucket(ctx: &Ctx) -> Vec<Component> {
    if ctx.str("bucket_id") == Some("_Required") {
        return vec![];
    }
    let loc = ctx.str_or("location", "global").to_lowercase();
    let retention = ctx.num_or("retention_days", 30.0);
    let gb = ctx.usage_num("monthly_logging_data_gb");
    let store_loc = pick(ctx, LOGGING, &[loc.as_str(), "us"], "^Log Storage cost$");
    let mut out = vec![with_note(
        ctx.usage(
            "Log ingestion (includes 30 days of retention)",
            "GiB",
            "monthly_logging_data_gb",
            Query::new(LOGGING, store_loc, "^Log Storage cost$").ondemand(),
        ),
        "the first 50 GiB per project per month are free",
    )];
    if retention > 30.0 {
        out.push(ctx.usage_qty(
            format!("Log retention beyond 30 days ({} days)", fmt_number(retention)),
            "GiB-months",
            gb.map(|g| g * (retention - 30.0) / DAYS_PER_MONTH),
            Query::new(LOGGING, "global", "^Log Retention cost$").ondemand(),
        ));
    }
    strict(out)
}

fn log_bucket_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    if ctx.str("bucket_id") == Some("_Required") {
        return out;
    }
    let retention = ctx.num_or("retention_days", 30.0);
    let target = if ctx.env == Env::NonProd && retention > 30.0 {
        Some(30.0)
    } else if retention > 365.0 {
        Some(365.0)
    } else {
        None
    };
    if let Some(t) = target.filter(|_| !ctx.bool_or("locked", false)) {
        let new = ctx.reprice(|a| {
            a.insert("retention_days".into(), Value::Number(t));
        });
        let savings = ctx.savings_vs(cr.monthly_cost, new);
        let quantify = if ctx.usage_num("monthly_logging_data_gb").is_some() {
            ""
        } else {
            " Set usage monthly_logging_data_gb to quantify the savings."
        };
        out.push(ctx.rec(
            "logging-long-retention",
            Severity::Low,
            format!("Reduce log retention from {} to {} days", fmt_number(retention), fmt_number(t)),
            format!(
                "Logs kept longer than 30 days are billed $0.01/GiB-month. {}Set retention_days = {} or route long-term logs to a Cloud Storage \
                 archive bucket through a sink, which is cheaper for rarely-read data. Check compliance requirements first.{quantify}",
                if t == 30.0 { "Non-production logs rarely need extended retention. " } else { "" },
                fmt_number(t)
            ),
            savings,
        ));
    }
    if ctx.bool_or("enable_analytics", false) && !referrers(ctx, "google_logging_linked_dataset", "bucket").is_empty() {
        out.push(ctx.rec(
            "logging-analytics",
            Severity::Info,
            "Queries through the linked BigQuery dataset are billed by BigQuery",
            "Log Analytics itself adds no charge, but queries run from BigQuery against the linked dataset are billed as BigQuery analysis \
             (on-demand bytes scanned or reservation slots). Prefer the Log Analytics page for ad-hoc queries and filter on timestamp.",
            None,
        ));
    }
    out
}

// ---- Cloud Monitoring ------------------------------------------------------

/// Number of checker locations of an uptime check (all 6 when unset).
fn uptime_locations(attrs: &Value) -> f64 {
    let regions: Vec<&str> = list_at(attrs, "selected_regions").iter().filter_map(|v| v.as_str()).collect();
    if regions.is_empty() {
        return 6.0;
    }
    regions.iter().map(|r| if *r == "USA" { 3.0 } else { 1.0 }).sum()
}

fn uptime_check(ctx: &Ctx) -> Vec<Component> {
    let period = ctx.str("period").and_then(duration_secs).filter(|p| *p > 0.0).unwrap_or(300.0);
    let locations = uptime_locations(ctx.attrs);
    let executions = SECONDS_PER_MONTH / period * locations;
    let label = format!("every {}, {} locations", humantime(period), fmt_number(locations));
    let (name, pat) = if ctx.block("synthetic_monitor").is_some() {
        (format!("Synthetic monitor executions ({label})"), "^Synthetic Monitor Executions$")
    } else {
        (format!("Uptime check executions ({label})"), "^Monitoring Uptime Checks$")
    };
    vec![ctx.monthly(name, "executions", executions, Query::new(MONITORING, "global", pat).ondemand())]
}

fn humantime(secs: f64) -> String {
    if secs >= 60.0 && secs % 60.0 == 0.0 { format!("{} min", fmt_number(secs / 60.0)) } else { format!("{}s", fmt_number(secs)) }
}

fn uptime_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let locations = uptime_locations(ctx.attrs);
    if locations > 3.0 {
        let new = ctx.reprice(|a| {
            a.insert(
                "selected_regions".into(),
                Value::List(["USA_VIRGINIA", "EUROPE", "ASIA_PACIFIC"].into_iter().map(Value::from).collect()),
            );
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "uptime-check-regions",
                Severity::Info,
                "Run this uptime check from 3 locations",
                format!(
                    "Each execution is billed once per checker location ({} here; an unset selected_regions runs from all 6, \"USA\" counts as 3). \
                     Three locations (the minimum) are usually enough to detect outages without false positives: set selected_regions accordingly.",
                    fmt_number(locations)
                ),
                Some(s),
            ));
        }
    }
    let period = ctx.str("period").and_then(duration_secs).unwrap_or(300.0);
    if ctx.env == Env::NonProd && period < 300.0 {
        let new = ctx.reprice(|a| {
            a.insert("period".into(), Value::from("300s"));
        });
        if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
            out.push(ctx.rec(
                "uptime-check-frequency-nonprod",
                Severity::Low,
                "Check this non-production endpoint every 5 minutes",
                "A 1-minute period runs 5x more executions than the default 300s. Non-production endpoints rarely need minute-level detection: set period = \"300s\".",
                Some(s),
            ));
        }
    }
    out
}

fn metric_descriptor(ctx: &Ctx) -> Vec<Component> {
    let mtype = ctx.str_or("type", "");
    let series = ctx.usage_num("time_series");
    let period = ctx.str("metadata.0.sample_period").and_then(duration_secs).filter(|p| *p > 0.0).unwrap_or(60.0);
    let points = series.map(|s| s * SECONDS_PER_MONTH / period);
    let sample_billed = if mtype.starts_with("prometheus.googleapis.com/") {
        Some("^Prometheus Samples Ingested$")
    } else if mtype.starts_with("workload.googleapis.com/") {
        Some("^Workload Metrics Samples Ingested$")
    } else {
        None
    };
    if let Some(pat) = sample_billed {
        let qty = ctx.usage_num("monthly_samples").or(points);
        return vec![ctx.usage_qty("Samples ingested", "samples", qty, Query::new(MONITORING, "global", pat).ondemand())];
    }
    let bytes = if ctx.str("value_type") == Some("DISTRIBUTION") { 80.0 } else { 8.0 };
    let mib = ctx.usage_num("monthly_ingested_mib").or(points.map(|p| p * bytes / 1_048_576.0));
    vec![with_note(
        ctx.usage_qty("Metric data ingested", "MiB", mib, Query::new(MONITORING, "global", "^Metric Volume$").ondemand()),
        format!(
            "{} bytes per point, one point every {} per time series; the first 150 MiB per billing account are free",
            fmt_number(bytes),
            humantime(period)
        ),
    )]
}

// ---- Cloud Build -----------------------------------------------------------

/// Price per build minute of a machine type (vCPU + memory).
fn build_minute_price(ctx: &Ctx, region: &str, mt: &MachineType) -> Option<f64> {
    let (cpu, ram) = match mt.family.as_str() {
        "e2" => ("^E2 cpu utilization per minute in", "^E2 ram utilization per minute in"),
        "n2d" => ("^N2D cpu utilization per minute in .*Private Pool", "^N2D ram utilization per minute in .*Private Pool"),
        "c3" => ("^C3 CPU utilization per minute in .*Private Pool", "^C3 ram utilization per minute in .*Private Pool"),
        _ => {
            ctx.warn(format!("Cloud Build does not offer {} workers", mt.name));
            return None;
        }
    };
    let cpu = ctx.price(Query::new(BUILD, region, cpu).ondemand())?.unit_price();
    let ram = ctx.price(Query::new(BUILD, region, ram).ondemand())?.unit_price();
    Some(cpu * machine::billed_vcpus(mt) + ram * mt.memory_gb)
}

fn build_components(ctx: &Ctx, region: &str, machine_name: &str, disk_gb: f64, private: bool) -> Vec<Component> {
    let Some(mt) = machine::parse(machine_name) else {
        ctx.warn(format!("unknown machine type '{machine_name}'"));
        return vec![];
    };
    let minutes = ctx.usage_num("monthly_build_minutes");
    let pool = if private { "private pool" } else { "default pool" };
    let mut out = vec![usage_based(ctx.fixed(
        format!("Build minutes ({}, {pool}, {region})", mt.name),
        "minutes",
        minutes,
        build_minute_price(ctx, region, &mt),
    ))];
    // The default pool includes 100 GB of disk.
    let billable = if private { disk_gb } else { (disk_gb - 100.0).max(0.0) };
    if billable > 0.0 {
        let pat =
            if private { "^SSD Utilization per GiBy month in .*Private Pool" } else { "^SSD Utilization per GiBy month in .*Default Pool" };
        out.push(ctx.usage_qty(
            format!("Worker disk ({} GiB{})", fmt_number(disk_gb), if private { "" } else { ", 100 GiB included" }),
            "GiB-months",
            minutes.map(|m| billable * m / (60.0 * HOURS_PER_MONTH)),
            Query::new(BUILD, region, pat).ondemand(),
        ));
    }
    strict(out)
}

fn pool_machine<'a>(ctx: &Ctx, pool: &'a Value) -> &'a str {
    str_at(pool, "worker_config.0.machine_type").unwrap_or_else(|| {
        ctx.warn("worker pool machine_type not set, assuming e2-standard-2");
        "e2-standard-2"
    })
}

fn build_trigger(ctx: &Ctx) -> Vec<Component> {
    if ctx.bool_or("disabled", false) {
        return vec![];
    }
    let opts = ctx.get("build.0.options.0");
    let pool_ref = opts.and_then(|o| str_at(o, "worker_pool"));
    if let Some(p) = pool_ref {
        if let Some(pool) = ctx.resolve_ref(p, "google_cloudbuild_worker_pool") {
            let region = str_at(&pool.attrs, "location").map(str::to_lowercase).unwrap_or_else(|| ctx.region());
            let disk = num_at(&pool.attrs, "worker_config.0.disk_size_gb").unwrap_or(100.0);
            return build_components(ctx, &region, pool_machine(ctx, &pool.attrs), disk, true);
        }
        ctx.warn("worker pool not found in this configuration, assuming e2-standard-2 workers");
        let region = location_segment(p).unwrap_or_else(|| ctx.region());
        return build_components(ctx, &region, "e2-standard-2", 100.0, true);
    }
    let region = ctx.str("location").map(str::to_lowercase).unwrap_or_else(|| "global".into());
    let machine = match opts.and_then(|o| str_at(o, "machine_type")) {
        Some("E2_MEDIUM") => "e2-medium",
        Some("E2_HIGHCPU_8" | "N1_HIGHCPU_8") => "e2-highcpu-8",
        Some("E2_HIGHCPU_32" | "N1_HIGHCPU_32") => "e2-highcpu-32",
        _ => "e2-standard-2",
    };
    let disk = opts.and_then(|o| num_at(o, "disk_size_gb")).unwrap_or(100.0);
    let mut comps = build_components(ctx, &region, machine, disk, false);
    if let Some(c) = comps.first_mut().filter(|_| ctx.get("build").is_none()) {
        c.note = Some("build steps are defined outside Terraform: assumes the default machine type".into());
    } else if let Some(c) = comps.first_mut().filter(|_| region == "global" && machine == "e2-standard-2") {
        c.note = Some("the first 2,500 e2-standard-2 minutes per billing account are free".into());
    }
    comps
}

fn build_pool(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.str("location").map(str::to_lowercase).unwrap_or_else(|| ctx.region());
    let disk = ctx.num_or("worker_config.0.disk_size_gb", 100.0);
    build_components(ctx, &region, pool_machine(ctx, ctx.attrs), disk, true)
}

fn build_pool_rules(ctx: &Ctx, _cr: &CostedResource) -> Vec<Recommendation> {
    let Some(mt) = str_at(ctx.attrs, "worker_config.0.machine_type").and_then(machine::parse) else { return vec![] };
    let threshold = if ctx.env == Env::NonProd { 8.0 } else { 16.0 };
    if mt.vcpus < threshold {
        return vec![];
    }
    let region = ctx.str("location").map(str::to_lowercase).unwrap_or_else(|| ctx.region());
    let small = machine::parse("e2-standard-4");
    let (Some(cur), Some(alt)) = (build_minute_price(ctx, &region, &mt), small.and_then(|s| build_minute_price(ctx, &region, &s))) else {
        return vec![];
    };
    vec![ctx.rec(
        "build-pool-oversized",
        Severity::Info,
        format!("Private pool workers use {} ({} vCPUs)", mt.name, fmt_number(mt.vcpus)),
        format!(
            "Build minutes on {} cost {cur:.4}/min versus {alt:.4}/min on e2-standard-4 ({}x). Large workers only pay off when build steps \
             parallelize across cores; compare build durations and consider a smaller worker_config.machine_type or a separate pool for heavy builds.",
            mt.name,
            fmt_number((cur / alt * 10.0).round() / 10.0)
        ),
        None,
    )]
}

// ---- Cloud Workstations ----------------------------------------------------

struct WsHost {
    machine: String,
    boot_gb: f64,
    pool: f64,
    gpus: Vec<(String, f64)>,
    /// (disk type, size GiB, assumed default)
    home: Option<(String, f64, bool)>,
}

fn ws_host(cfg: Option<&Value>) -> WsHost {
    let gce = cfg.and_then(|c| c.path("host.0.gce_instance.0"));
    let gpus = gce
        .map(|g| list_at(g, "accelerators"))
        .unwrap_or_default()
        .iter()
        .filter_map(|a| Some((a.get("type")?.as_str()?.to_string(), a.get("count").and_then(|c| c.as_f64()).unwrap_or(1.0))))
        .collect();
    let pd = cfg.and_then(|c| c.path("persistent_directories.0"));
    let home = match pd {
        Some(p) if p.path("gce_hd.0").is_some() => {
            Some(("hyperdisk-balanced-high-availability".to_string(), num_at(p, "gce_hd.0.size_gb").unwrap_or(200.0), false))
        }
        Some(p) if p.path("gce_pd.0").is_some() => {
            let t = str_at(p, "gce_pd.0.disk_type").unwrap_or("pd-standard").to_string();
            Some((t, num_at(p, "gce_pd.0.size_gb").unwrap_or(200.0), false))
        }
        Some(_) => None,
        None => Some(("pd-standard".to_string(), 200.0, true)),
    };
    WsHost {
        machine: gce.and_then(|g| str_at(g, "machine_type")).unwrap_or("e2-standard-4").to_string(),
        boot_gb: gce.and_then(|g| num_at(g, "boot_disk_size_gb")).unwrap_or(50.0),
        pool: gce.and_then(|g| num_at(g, "pool_size")).unwrap_or(0.0),
        gpus,
        home,
    }
}

/// VM price per running hour (before SUD) and the vCPU count.
fn ws_vm_hourly(ctx: &Ctx, region: &str, host: &WsHost) -> Option<(MachineType, Option<f64>)> {
    let Some(mt) = machine::parse(&host.machine) else {
        ctx.warn(format!("unknown machine type '{}'", host.machine));
        return None;
    };
    let hourly = machine_hourly(ctx, region, &mt, Pricing::OnDemand);
    Some((mt, hourly))
}

/// Per-workstation components for `count` workstations.
fn ws_components(ctx: &Ctx, region: &str, host: &WsHost, count: f64) -> Vec<Component> {
    let mut out = vec![];
    if count <= 0.0 {
        return out;
    }
    let Some((mt, hourly)) = ws_vm_hourly(ctx, region, host) else { return out };
    let hours = ctx.usage_num("monthly_hours_per_workstation");
    let running = hours.map(|h| h * count);
    let times = if count != 1.0 { format!(" x{}", fmt_number(count)) } else { String::new() };
    let sud = sud_multiplier(&mt.family, hours.unwrap_or(0.0));
    let mut vm = usage_based(ctx.fixed(format!("Workstation VM ({}){times}", mt.name), "hours", running, hourly.map(|h| h * sud)));
    if sud < 1.0 {
        vm.note = Some(format!("includes {:.0}% sustained use discount", (1.0 - sud) * 100.0));
    }
    out.push(vm);
    out.push(ctx.usage_qty(
        format!("Management fee ({} vCPU){times}", fmt_number(mt.vcpus)),
        "vCPU-hours",
        running.map(|r| r * mt.vcpus),
        Query::new(WORKSTATIONS, region, "^Cloud Workstations instance VM \\(vCPU\\) management fee").ondemand(),
    ));
    for (t, n) in &host.gpus {
        match machine::gpu_pattern(t, false) {
            Some(pat) => out.push(ctx.usage_qty(
                format!("GPU ({t}) x{}", fmt_number(n * count)),
                "GPU-hours",
                running.map(|r| r * n),
                Query::new(COMPUTE, region, &pat).group("GPU").ondemand(),
            )),
            None => ctx.warn(format!("unknown accelerator type '{t}'")),
        }
    }
    out.push(ctx.usage_qty(
        format!("Boot disk (pd-balanced, {} GiB){times}", fmt_number(host.boot_gb)),
        "GiB-months",
        running.map(|r| r * host.boot_gb / HOURS_PER_MONTH),
        Query::new(COMPUTE, region, "^Balanced PD Capacity").ondemand(),
    ));
    if let Some((dtype, size, assumed)) = &host.home {
        let mut disks = disk_components(ctx, region, &format!("Home disk{times}"), dtype, *size, None, None, count, true);
        if *assumed && let Some(d) = disks.first_mut() {
            d.note = Some("default persistent home directory (200 GiB pd-standard)".into());
        }
        out.extend(disks);
    }
    strict(out)
}

fn ws_region(ctx: &Ctx) -> String {
    ctx.str("location").map(str::to_lowercase).unwrap_or_else(|| ctx.region())
}

fn ws_cluster(ctx: &Ctx) -> Vec<Component> {
    let region = ws_region(ctx);
    vec![ctx.hourly("Control plane", 1.0, Query::new(WORKSTATIONS, &region, "^Cloud Workstations control plane fee").ondemand())]
}

fn ws_cluster_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let region = ws_region(ctx);
    let earlier = ctx.siblings.iter().any(|s| {
        s.rtype == ctx.r.rtype
            && s.address < ctx.r.address
            && str_at(&s.attrs, "location").map(str::to_lowercase).as_deref() == Some(region.as_str())
            && str_at(&s.attrs, "network") == ctx.str("network")
    });
    if !earlier || cr.monthly_cost <= 1.0 {
        return vec![];
    }
    vec![ctx.rec(
        "workstations-cluster-consolidate",
        Severity::Info,
        "Another workstation cluster exists in the same region and network",
        "Each cluster bills a control plane fee around the clock. Multiple clusters are only needed for different regions or VPCs; \
         workstation configurations of several teams can share one cluster.",
        Some(cr.monthly_cost),
    )]
}

fn ws_config(ctx: &Ctx) -> Vec<Component> {
    let region = ws_region(ctx);
    let host = ws_host(Some(ctx.attrs));
    let declared = !ws_workstations(ctx).is_empty();
    let count = ctx.usage_num("workstations").unwrap_or_else(|| {
        if declared {
            0.0
        } else {
            ctx.warn("assuming 1 workstation for this configuration (set usage 'workstations')");
            1.0
        }
    });
    let mut out = vec![];
    if host.pool > 0.0
        && let Some((mt, hourly)) = ws_vm_hourly(ctx, &region, &host)
    {
        let sud = sud_multiplier(&mt.family, HOURS_PER_MONTH);
        let mut c = ctx.fixed(
            format!("Pooled VMs ({}) x{}", mt.name, fmt_number(host.pool)),
            "hours",
            Some(host.pool * HOURS_PER_MONTH),
            hourly.map(|h| h * sud),
        );
        c.note = Some("pre-started VMs run continuously to speed up workstation start".into());
        out.push(c);
        out.push(ctx.monthly(
            format!("Pooled VMs boot disks (pd-balanced, {} GiB) x{}", fmt_number(host.boot_gb), fmt_number(host.pool)),
            "GiB-months",
            host.boot_gb * host.pool,
            Query::new(COMPUTE, &region, "^Balanced PD Capacity").ondemand(),
        ));
    }
    out.extend(ws_components(ctx, &region, &host, count));
    out
}

/// Workstations declared in this configuration that use the current config.
fn ws_workstations<'a>(ctx: &Ctx<'a>) -> Vec<&'a ResourceInstance> {
    let id = ctx.str("workstation_config_id");
    let mut out = referrers(ctx, "google_workstations_workstation", "workstation_config_id");
    for s in ctx.siblings.iter().filter(|s| s.rtype == "google_workstations_workstation") {
        if id.is_some() && str_at(&s.attrs, "workstation_config_id") == id && !out.iter().any(|o| o.address == s.address) {
            out.push(s);
        }
    }
    out
}

fn workstation(ctx: &Ctx) -> Vec<Component> {
    let region = ws_region(ctx);
    let id = ctx.str("workstation_config_id");
    let cfg = ctx.siblings.iter().filter(|s| s.rtype == "google_workstations_workstation_config").find(|s| {
        (id.is_some() && str_at(&s.attrs, "workstation_config_id") == id)
            || (s.module_path == ctx.r.module_path && ctx.r.refs.contains(&format!("{}.{}", s.rtype, s.name)))
    });
    if cfg.is_none() {
        ctx.warn("workstation config not found in this configuration, assuming e2-standard-4 with a 200 GiB home disk");
    }
    ws_components(ctx, &region, &ws_host(cfg.map(|c| &c.attrs)), 1.0)
}

fn ws_config_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let mut out = vec![];
    let idle = ctx.str("idle_timeout").and_then(duration_secs).unwrap_or(1200.0);
    let running = ctx.str("running_timeout").and_then(duration_secs).unwrap_or(43200.0);
    let region = ws_region(ctx);
    let host = ws_host(Some(ctx.attrs));
    if idle == 0.0 || idle > 7200.0 || running == 0.0 {
        let hourly = ws_vm_hourly(ctx, &region, &host).and_then(|(mt, vm)| {
            let fee =
                ctx.price(Query::new(WORKSTATIONS, &region, "^Cloud Workstations instance VM \\(vCPU\\) management fee").ondemand())?;
            Some(vm? + fee.unit_price() * mt.vcpus)
        });
        let mut problems = vec![];
        if idle == 0.0 {
            problems.push("idle_timeout is disabled".to_string());
        } else if idle > 7200.0 {
            problems.push(format!("idle_timeout is {} hours", fmt_number((idle / 360.0).round() / 10.0)));
        }
        if running == 0.0 {
            problems.push("running_timeout is disabled".into());
        }
        let cost = hourly
            .map(|h| {
                format!(
                    " Each running workstation costs ~{}/hour, i.e. up to {}/month if left on.",
                    ctx.money(h),
                    ctx.money(h * HOURS_PER_MONTH)
                )
            })
            .unwrap_or_default();
        out.push(ctx.rec(
            "workstations-idle-timeout",
            Severity::Medium,
            "Workstations may keep running when unused",
            format!(
                "{}: forgotten workstations keep billing compute and management fees.{cost} Set idle_timeout (default \"1200s\") and \
                 running_timeout (default \"43200s\") so that workstations stop automatically.",
                problems.join(" and ")
            ),
            None,
        ));
    }
    if host.pool > 0.0 && ctx.env == Env::NonProd {
        let pooled = total_of(&cr.components, |c| c.name.starts_with("Pooled VMs"));
        if pooled > 1.0 {
            out.push(ctx.rec(
                "workstations-pool-nonprod",
                Severity::Medium,
                format!("{} pre-started VMs are kept running for a non-production configuration", fmt_number(host.pool)),
                "Pooled VMs run continuously whether or not anyone uses them. Faster start-up is rarely worth it outside production: set host.gce_instance.pool_size = 0.",
                Some(pooled),
            ));
        }
    }
    out
}

// ---- Certificate Manager ---------------------------------------------------

fn certificate(ctx: &Ctx) -> Vec<Component> {
    let loc = ctx.str_or("location", "global").to_lowercase();
    let c = if loc == "global" {
        ctx.monthly("Certificate (global)", "certificates", 1.0, Query::new(CERTS, "global", "^Certificates usage$").ondemand())
    } else {
        ctx.monthly(
            format!("Certificate (regional, {loc})"),
            "certificates",
            1.0,
            Query::new(CERTS, &loc, "^Regional Certificates usage$").ondemand(),
        )
    };
    vec![with_note(c, "the first 100 certificates per project are free")]
}

fn trust_config(ctx: &Ctx) -> Vec<Component> {
    vec![ctx.usage(
        "mTLS connections",
        "connections",
        "monthly_mtls_connections",
        Query::new(CERTS, "global", "^LB connections with enabled mTLS").ondemand(),
    )]
}

// ---- Identity Platform -----------------------------------------------------

fn identity_platform(ctx: &Ctx) -> Vec<Component> {
    let tenant = ctx.r.rtype == "google_identity_platform_tenant";
    let prefix = if tenant { "Tenant " } else { "" };
    let t1 = format!("^{prefix}Tier 1 Monthly Active Users$");
    let t2 = format!("^{prefix}Tier 2 Monthly Active Users$");
    vec![
        with_note(
            ctx.usage("Monthly active users (tier 1)", "users", "monthly_active_users", Query::new(IDENTITY, "global", &t1).ondemand()),
            "the first 49,999 tier-1 MAUs per project are free",
        ),
        with_note(
            ctx.usage(
                "Monthly active users (SAML/OIDC)",
                "users",
                "monthly_saml_oidc_active_users",
                Query::new(IDENTITY, "global", &t2).ondemand(),
            ),
            "the first 49 SAML/OIDC MAUs per project are free",
        ),
        ctx.usage(
            "SMS MFA verifications (US, Canada, India)",
            "verifications",
            "monthly_sms_mfa_verifications",
            Query::new(IDENTITY, "global", "^Successful SMS-based multi-factor verifications in US/Canada/India").ondemand(),
        ),
        ctx.usage(
            "SMS MFA verifications (other countries)",
            "verifications",
            "monthly_sms_mfa_verifications_other",
            Query::new(IDENTITY, "global", "^Successful SMS-based multi-factor verifications in countries other than").ondemand(),
        ),
    ]
}

// ---- Cloud Deploy ----------------------------------------------------------

fn deploy_pipeline(ctx: &Ctx) -> Vec<Component> {
    let targets = ctx.blocks("serial_pipeline.0.stages").len();
    if targets < 2 || ctx.bool_or("suspended", false) {
        return vec![];
    }
    vec![with_note(
        ctx.monthly(
            format!("Active delivery pipeline ({targets} targets)"),
            "pipelines",
            1.0,
            Query::new(DEPLOY, "global", "^Active Multiple Target Delivery Pipelines$").ondemand(),
        ),
        "single-target pipelines and the first multi-target pipeline per billing account are free",
    )]
}

// ---- Cloud IDS -------------------------------------------------------------

fn ids_endpoint(ctx: &Ctx) -> Vec<Component> {
    vec![
        ctx.hourly("IDS endpoint", 1.0, Query::new(IDS, "global", "^Cloud IDS$").unit("h").ondemand()),
        ctx.usage("Traffic inspected", "GB", "monthly_inspected_data_gb", Query::new(IDS, "global", "^Cloud IDS$").unit("GBy").ondemand()),
    ]
}

fn ids_rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation> {
    let endpoint = total_of(&cr.components, |c| c.name == "IDS endpoint");
    if ctx.env != Env::NonProd || endpoint <= 1.0 {
        return vec![];
    }
    vec![ctx.rec(
        "ids-endpoint-nonprod",
        Severity::Low,
        "Cloud IDS endpoint in a non-production environment",
        "Each IDS endpoint bills a fixed hourly fee around the clock, on top of inspected traffic. Unless threat detection is required in \
         this environment, remove the endpoint (and its packet mirroring policy) or create it only for security testing windows.",
        Some(endpoint),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration_secs("7776000s"), Some(7_776_000.0));
        assert_eq!(duration_secs("1.5s"), Some(1.5));
        assert_eq!(duration_secs("90d"), None);
    }

    #[test]
    fn key_algorithms() {
        assert_eq!(KeyAlgo::parse("ENCRYPT_DECRYPT", "GOOGLE_SYMMETRIC_ENCRYPTION"), KeyAlgo::Symmetric);
        assert_eq!(KeyAlgo::parse("MAC", "HMAC_SHA256"), KeyAlgo::Mac);
        assert_eq!(KeyAlgo::parse("ASYMMETRIC_SIGN", "RSA_SIGN_PSS_3072_SHA256"), KeyAlgo::Rsa(3072));
        assert_eq!(KeyAlgo::parse("ASYMMETRIC_DECRYPT", "RSA_DECRYPT_OAEP_4096_SHA512"), KeyAlgo::Rsa(4096));
        assert_eq!(KeyAlgo::parse("ASYMMETRIC_SIGN", "EC_SIGN_P384_SHA384"), KeyAlgo::EcP384);
        assert_eq!(KeyAlgo::parse("ASYMMETRIC_SIGN", "EC_SIGN_ED25519"), KeyAlgo::OtherAsymmetric);
    }

    #[test]
    fn kms_sku_patterns() {
        let (v, o) = kms_patterns("HSM", KeyAlgo::EcP256);
        assert_eq!(v.as_deref(), Some("^Active HSM ECDSA P-256 key versions$"));
        assert_eq!(o, "^HSM cryptographic operations with an ECDSA P-256 key$");
        let (v, o) = kms_patterns("HSM", KeyAlgo::Rsa(2048));
        assert_eq!(v.as_deref(), Some("^Active HSM RSA 2048 bit key versions$"));
        assert_eq!(o, "^HSM cryptographic operations with a RSA 2048 bit key$");
        let (v, _) = kms_patterns("HSM_SINGLE_TENANT", KeyAlgo::Symmetric);
        assert!(v.is_none());
        let (v, _) = kms_patterns("EXTERNAL_VPC", KeyAlgo::Rsa(2048));
        assert_eq!(v.as_deref(), Some("^Active external asymmetric key versions$"));
    }

    #[test]
    fn locations() {
        assert_eq!(location_segment("projects/p/locations/europe-west1/keyRings/kr").as_deref(), Some("europe-west1"));
        assert_eq!(location_segment("kr"), None);
    }
}
