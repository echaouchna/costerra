//! Resource cost models.
//!
//! Each supported resource type registers a [`Spec`] with an `estimate`
//! function that turns evaluated attributes (+ usage) into priced
//! [`Component`]s.

pub mod machine;

mod ai;
mod compute;
mod container;
mod data;
mod database;
mod misc;
mod network;
mod serverless;
mod storage;

use crate::cost::{Basis, Component, CostedResource, HOURS_PER_MONTH, Recommendation, Severity};
use crate::hcl::value::{Map, Value};
use crate::observed::{CpuStats, ObservedResource};
use crate::pricing::query::{Pricer, Query, normalize_region};
use crate::tf::evaluate::ResourceInstance;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::LazyLock;

pub type EstimateFn = fn(&Ctx) -> Vec<Component>;
/// Optimization rules for a resource type. Receives the priced resource.
pub type RulesFn = fn(&Ctx, &CostedResource) -> Vec<Recommendation>;

pub struct UsageParam {
    pub key: &'static str,
    pub description: &'static str,
}

pub struct Spec {
    pub rtype: &'static str,
    pub estimate: EstimateFn,
    pub usage: &'static [UsageParam],
    pub rules: Option<RulesFn>,
}

impl Spec {
    pub fn new(rtype: &'static str, estimate: EstimateFn) -> Spec {
        Spec { rtype, estimate, usage: &[], rules: None }
    }
    pub fn usage(mut self, u: &'static [UsageParam]) -> Spec {
        self.usage = u;
        self
    }
    pub fn rules(mut self, r: RulesFn) -> Spec {
        self.rules = Some(r);
        self
    }
}

macro_rules! usage {
    ($($k:literal => $d:literal),* $(,)?) => {
        &[$($crate::resources::UsageParam { key: $k, description: $d }),*]
    };
}
pub(crate) use usage;

static REGISTRY: LazyLock<HashMap<&'static str, Spec>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for s in compute::specs()
        .into_iter()
        .chain(container::specs())
        .chain(database::specs())
        .chain(storage::specs())
        .chain(network::specs())
        .chain(serverless::specs())
        .chain(data::specs())
        .chain(ai::specs())
        .chain(misc::specs())
    {
        m.insert(s.rtype, s);
    }
    m
});

pub fn registry() -> &'static HashMap<&'static str, Spec> {
    &REGISTRY
}

/// Resource types that are free of charge on their own.
pub static FREE: &[&str] = &[
    "google_project",
    "google_project_service",
    "google_project_services",
    "google_privileged_access_manager_entitlement",
    "google_monitoring_monitored_project",
    "google_resource_manager_lien",
    "google_service_usage_consumer_quota_override",
    "google_privileged_access_manager_settings",
    "google_cloud_quotas_quota_preference",
    "google_cloud_quotas_quota_adjuster_settings",
    "google_org_policy_custom_constraint",
    "google_folder_organization_policy",
    "google_resource_manager_capability",
    "google_folder",
    "google_organization_policy",
    "google_org_policy_policy",
    "google_compute_network",
    "google_compute_subnetwork",
    "google_compute_firewall",
    "google_compute_network_firewall_policy",
    "google_compute_network_firewall_policy_rule",
    "google_compute_network_firewall_policy_association",
    "google_compute_firewall_policy",
    "google_compute_firewall_policy_rule",
    "google_compute_firewall_policy_association",
    "google_compute_route",
    "google_compute_router",
    "google_compute_router_interface",
    "google_compute_router_peer",
    "google_compute_network_peering",
    "google_compute_network_peering_routes_config",
    "google_compute_shared_vpc_host_project",
    "google_compute_shared_vpc_service_project",
    "google_compute_instance_group",
    "google_compute_health_check",
    "google_compute_region_health_check",
    "google_compute_http_health_check",
    "google_compute_https_health_check",
    "google_compute_region_backend_service",
    "google_compute_url_map",
    "google_compute_region_url_map",
    "google_compute_target_http_proxy",
    "google_compute_target_https_proxy",
    "google_compute_region_target_http_proxy",
    "google_compute_region_target_https_proxy",
    "google_compute_target_tcp_proxy",
    "google_compute_target_ssl_proxy",
    "google_compute_target_pool",
    "google_compute_ssl_certificate",
    "google_compute_managed_ssl_certificate",
    "google_compute_region_ssl_certificate",
    "google_compute_ssl_policy",
    "google_compute_region_ssl_policy",
    "google_project_service_identity",
    "google_project_default_service_accounts",
    "google_project_organization_policy",
    "google_project_iam_member_remove",
    "google_compute_region_network_firewall_policy",
    "google_compute_region_network_firewall_policy_rule",
    "google_compute_region_network_firewall_policy_association",
    "google_compute_region_network_endpoint",
    "google_compute_region_target_tcp_proxy",
    "google_compute_instance_template",
    "google_compute_region_instance_template",
    "google_compute_autoscaler",
    "google_compute_region_autoscaler",
    "google_compute_resource_policy",
    "google_compute_disk_resource_policy_attachment",
    "google_compute_attached_disk",
    "google_compute_project_metadata",
    "google_compute_project_metadata_item",
    "google_compute_instance_iam_member",
    "google_compute_global_network_endpoint_group",
    "google_compute_network_endpoint_group",
    "google_compute_region_network_endpoint_group",
    "google_compute_network_endpoint",
    "google_compute_security_policy_rule",
    "google_compute_ha_vpn_gateway",
    "google_compute_external_vpn_gateway",
    "google_compute_region_target_tcp_proxy",
    "google_compute_target_grpc_proxy",
    "google_compute_target_instance",
    "google_compute_service_attachment",
    "google_network_connectivity_hub",
    "google_network_security_firewall_endpoint_association",
    "google_network_security_security_profile",
    "google_network_security_security_profile_group",
    "google_service_networking_peered_dns_domain",
    "google_network_security_address_group",
    "google_network_security_tls_inspection_policy",
    "google_compute_region_security_policy_rule",
    "google_network_security_gateway_security_policy",
    "google_network_security_gateway_security_policy_rule",
    "google_network_security_url_lists",
    "google_dns_response_policy",
    "google_dns_response_policy_rule",
    "google_compute_router_nat_address",
    "google_service_account",
    "google_service_account_key",
    "google_service_account_iam_member",
    "google_service_account_iam_binding",
    "google_service_account_iam_policy",
    "google_project_iam_member",
    "google_project_iam_binding",
    "google_project_iam_policy",
    "google_project_iam_custom_role",
    "google_project_iam_audit_config",
    "google_organization_iam_member",
    "google_organization_iam_binding",
    "google_organization_iam_custom_role",
    "google_folder_iam_member",
    "google_folder_iam_binding",
    "google_billing_account_iam_member",
    "google_storage_bucket_iam_member",
    "google_storage_bucket_iam_binding",
    "google_storage_bucket_iam_policy",
    "google_storage_bucket_object",
    "google_storage_bucket_acl",
    "google_storage_default_object_acl",
    "google_storage_notification",
    "google_storage_hmac_key",
    "google_filestore_snapshot",
    "google_backup_dr_backup_plan",
    "google_backup_dr_management_server",
    "google_storage_transfer_job",
    "google_storage_managed_folder",
    "google_storage_folder",
    "google_pubsub_topic_iam_member",
    "google_pubsub_topic_iam_binding",
    "google_pubsub_subscription_iam_member",
    "google_pubsub_subscription_iam_binding",
    "google_pubsub_schema",
    "google_secret_manager_secret_iam_member",
    "google_secret_manager_secret_iam_binding",
    "google_kms_key_ring",
    "google_kms_ekm_connection",
    "google_kms_autokey_config",
    "google_kms_project_autokey_config",
    "google_kms_key_ring_import_job",
    "google_kms_secret_ciphertext",
    "google_logging_project_sink",
    "google_logging_folder_sink",
    "google_logging_organization_sink",
    "google_logging_billing_account_sink",
    "google_logging_linked_dataset",
    "google_logging_log_view",
    "google_logging_saved_query",
    "google_logging_folder_exclusion",
    "google_logging_organization_exclusion",
    "google_logging_billing_account_exclusion",
    "google_logging_project_settings",
    "google_logging_folder_settings",
    "google_logging_organization_settings",
    "google_logging_project_cmek_settings",
    "google_certificate_manager_certificate_map",
    "google_certificate_manager_certificate_map_entry",
    "google_certificate_manager_certificate_issuance_config",
    "google_clouddeploy_target",
    "google_clouddeploy_automation",
    "google_clouddeploy_custom_target_type",
    "google_clouddeploy_deploy_policy",
    "google_identity_platform_default_supported_idp_config",
    "google_identity_platform_inbound_saml_config",
    "google_identity_platform_oauth_idp_config",
    "google_identity_platform_tenant_default_supported_idp_config",
    "google_identity_platform_tenant_inbound_saml_config",
    "google_identity_platform_tenant_oauth_idp_config",
    "google_cloudbuild_bitbucket_server_config",
    "google_kms_key_ring_iam_member",
    "google_kms_crypto_key_iam_member",
    "google_kms_crypto_key_iam_binding",
    "google_bigquery_dataset_iam_member",
    "google_bigquery_dataset_iam_binding",
    "google_bigquery_dataset_access",
    "google_bigquery_table_iam_member",
    "google_bigquery_routine",
    "google_bigquery_reservation_assignment",
    "google_bigquery_connection",
    "google_dataproc_autoscaling_policy",
    "google_datastream_connection_profile",
    "google_datastream_private_connection",
    "google_dataproc_workflow_template",
    "google_dataproc_session_template",
    "google_dataproc_job",
    "google_bigquery_datapolicy_data_policy",
    "google_dataplex_asset",
    "google_dataplex_entry_group",
    "google_dataplex_aspect_type",
    "google_dataplex_entry_type",
    "google_composer_user_workloads_secret",
    "google_composer_user_workloads_config_map",
    "google_cloud_run_service_iam_member",
    "google_cloud_run_service_iam_binding",
    "google_cloud_run_v2_service_iam_member",
    "google_cloud_run_v2_service_iam_binding",
    "google_cloud_run_v2_job_iam_member",
    "google_cloudfunctions_function_iam_member",
    "google_cloudfunctions2_function_iam_member",
    "google_cloud_run_domain_mapping",
    "google_artifact_registry_repository_iam_member",
    "google_artifact_registry_repository_iam_binding",
    "google_sql_database",
    "google_sql_user",
    "google_sql_ssl_cert",
    "google_sql_source_representation_instance",
    "google_alloydb_user",
    "google_spanner_instance_config",
    "google_spanner_backup_schedule",
    "google_bigtable_authorized_view",
    "google_firestore_index",
    "google_firestore_field",
    "google_firestore_document",
    "google_firestore_backup_schedule",
    "google_datastore_index",
    "google_redis_cluster_user_created_connections",
    "google_memorystore_instance_desired_user_created_endpoints",
    "google_spanner_database",
    "google_spanner_database_iam_member",
    "google_spanner_instance_iam_member",
    "google_bigtable_table",
    "google_bigtable_app_profile",
    "google_bigtable_gc_policy",
    "google_bigtable_instance_iam_member",
    "google_dns_record_set",
    "google_dns_policy",
    "google_logging_project_exclusion",
    "google_logging_metric",
    "google_logging_project_bucket_config_iam_member",
    "google_monitoring_alert_policy",
    "google_monitoring_notification_channel",
    "google_monitoring_dashboard",
    "google_monitoring_group",
    "google_monitoring_slo",
    "google_monitoring_custom_service",
    "google_monitoring_service",
    "google_container_registry",
    "google_iap_brand",
    "google_iap_client",
    "google_iap_web_iam_member",
    "google_iap_tunnel_iam_member",
    "google_iap_web_backend_service_iam_member",
    "google_iam_workload_identity_pool",
    "google_iam_workload_identity_pool_provider",
    "google_access_context_manager_access_policy",
    "google_access_context_manager_service_perimeter",
    "google_access_context_manager_access_level",
    "google_essential_contacts_contact",
    "google_tags_tag_key",
    "google_tags_tag_value",
    "google_tags_tag_binding",
    "google_cloudbuildv2_connection",
    "google_cloudbuildv2_repository",
    "google_eventarc_trigger",
    "google_app_engine_application",
    "google_app_engine_firewall_rule",
    "google_app_engine_domain_mapping",
    "google_app_engine_service_split_traffic",
    "google_app_engine_service_network_settings",
    "google_app_engine_application_url_dispatch_rules",
    "google_eventarc_channel",
    "google_eventarc_google_channel_config",
    "google_eventarc_enrollment",
    "google_vertex_ai_endpoint",
    "google_vertex_ai_index_endpoint",
    "google_vertex_ai_dataset",
    "google_vertex_ai_featurestore_entitytype",
    "google_vertex_ai_featurestore_entitytype_feature",
    "google_vertex_ai_feature_group",
    "google_vertex_ai_feature_group_feature",
    "google_colab_runtime_template",
    "google_ml_engine_model",
    "google_notebooks_environment",
    "google_billing_budget",
    "google_billing_project_info",
    "google_os_config_patch_deployment",
    "google_os_login_ssh_public_key",
    "google_service_networking_connection",
    "google_compute_global_address",
    "google_certificate_manager_dns_authorization",
    "google_firebase_project",
    "google_endpoints_service",
    "google_api_gateway_api",
    "google_api_gateway_api_config",
    "google_cloudfunctions2_function_iam_binding",
    "google_data_catalog_taxonomy",
    "google_data_catalog_policy_tag",
    "google_healthcare_dataset",
    "google_sourcerepo_repository",
    "google_dataplex_lake",
    "google_dataplex_zone",
    "google_binary_authorization_policy",
    "google_container_analysis_note",
    "google_gke_hub_membership",
    "google_gke_hub_feature",
    "google_gke_hub_feature_membership",
    "random_id",
    "random_string",
    "random_password",
    "random_pet",
    "random_integer",
    "random_uuid",
    "random_shuffle",
    "null_resource",
    "terraform_data",
    "time_sleep",
    "time_rotating",
    "time_static",
    "tls_private_key",
    "tls_self_signed_cert",
    "tls_cert_request",
    "tls_locally_signed_cert",
    "local_file",
    "local_sensitive_file",
    "kubernetes_namespace",
    "kubernetes_namespace_v1",
    "kubernetes_service_account",
    "kubernetes_service_account_v1",
    "kubernetes_secret",
    "kubernetes_secret_v1",
    "kubernetes_config_map",
    "kubernetes_config_map_v1",
    "kubernetes_manifest",
    "kubernetes_deployment",
    "kubernetes_deployment_v1",
    "kubernetes_role_binding",
    "kubernetes_cluster_role_binding",
    "helm_release",
];

pub fn is_free(rtype: &str) -> bool {
    FREE.contains(&rtype)
        || rtype.starts_with("google_tags_")
        || rtype.ends_with("_iam_member")
        || rtype.ends_with("_iam_binding")
        || rtype.ends_with("_iam_policy")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Env {
    Prod,
    NonProd,
    Unknown,
}

/// Evaluation context passed to cost models.
pub struct Ctx<'a> {
    pub r: &'a ResourceInstance,
    pub attrs: &'a Value,
    pub pricer: &'a Pricer,
    pub usage: &'a Map,
    pub default_region: &'a str,
    pub env: Env,
    /// All resources of the same project (for cross-resource lookups).
    pub siblings: &'a [ResourceInstance],
    pub warnings: RefCell<Vec<String>>,
    /// Location reported for the resource when it is not a plain region
    /// attribute (e.g. Spanner config, AlloyDB cluster). Set via `set_location`.
    pub location: RefCell<Option<String>>,
    /// Usage observed in Google Cloud for this resource.
    pub observed: Option<&'a ObservedResource>,
    /// Keys of `usage` whose value was observed (not from a usage file).
    pub observed_keys: &'a [String],
    /// Most accurate data read so far (for recommendations).
    pub basis: Cell<Basis>,
}

impl<'a> Ctx<'a> {
    // ---- attribute helpers -------------------------------------------------

    pub fn get(&self, path: &str) -> Option<&'a Value> {
        self.attrs.path(path).filter(|v| !v.is_null() && !v.is_unknown())
    }

    pub fn is_unknown(&self, path: &str) -> bool {
        self.attrs.path(path).is_some_and(|v| v.is_unknown())
    }

    pub fn str(&self, path: &str) -> Option<&'a str> {
        self.get(path).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
    }

    pub fn str_or(&self, path: &str, d: &'a str) -> &'a str {
        self.str(path).unwrap_or(d)
    }

    pub fn num(&self, path: &str) -> Option<f64> {
        self.get(path).and_then(|v| v.as_f64())
    }

    pub fn num_or(&self, path: &str, d: f64) -> f64 {
        self.num(path).unwrap_or(d)
    }

    pub fn bool(&self, path: &str) -> Option<bool> {
        self.get(path).and_then(|v| v.as_bool())
    }

    pub fn bool_or(&self, path: &str, d: bool) -> bool {
        self.bool(path).unwrap_or(d)
    }

    /// Nested blocks (list of maps) under `path`.
    pub fn blocks(&self, path: &str) -> Vec<&'a Value> {
        match self.get(path) {
            Some(Value::List(l)) => l.iter().collect(),
            Some(v @ Value::Map(_)) => vec![v],
            _ => vec![],
        }
    }

    pub fn block(&self, path: &str) -> Option<&'a Value> {
        self.blocks(path).into_iter().next()
    }

    pub fn labels(&self) -> Option<&'a Map> {
        self.get("labels").or_else(|| self.get("resource_labels")).or_else(|| self.get("user_labels")).and_then(|v| v.as_map())
    }

    // ---- usage -------------------------------------------------------------

    pub fn usage_num(&self, key: &str) -> Option<f64> {
        let v = self.usage.get(key).and_then(|v| v.as_f64());
        self.used(key, v.is_some());
        v
    }

    pub fn usage_str(&self, key: &str) -> Option<&str> {
        let v = self.usage.get(key).and_then(|v| v.as_str());
        self.used(key, v.is_some());
        v
    }

    fn used(&self, key: &str, found: bool) {
        if found {
            let b = if self.observed_keys.iter().any(|k| k == key) { Basis::Observed } else { Basis::UsageFile };
            self.basis.set(self.basis.get().max(b));
        }
    }

    /// Observed CPU utilization (VMs).
    pub fn observed_cpu(&self) -> Option<CpuStats> {
        let c = self.observed.and_then(|o| o.cpu);
        if c.is_some() {
            self.basis.set(Basis::Observed);
        }
        c
    }

    /// An observed value that is not a usage key of the cost model (e.g. the
    /// bytes a bucket sent).
    pub fn observed_value(&self, key: &str) -> Option<f64> {
        let v = self.observed.and_then(|o| o.usage.get(key).copied());
        if v.is_some() {
            self.basis.set(Basis::Observed);
        }
        v
    }

    /// Observed memory utilization (Cloud SQL).
    pub fn observed_memory(&self) -> Option<CpuStats> {
        let m = self.observed.and_then(|o| o.memory);
        if m.is_some() {
            self.basis.set(Basis::Observed);
        }
        m
    }

    /// Share of this VM's vCPUs already covered by active commitments.
    pub fn committed_fraction(&self) -> Option<f64> {
        let f = self.observed.and_then(|o| o.committed_fraction);
        if f.is_some() {
            self.basis.set(Basis::Observed);
        }
        f
    }

    /// Forget the data read so far (before running a new set of rules).
    pub fn reset_basis(&self) {
        self.basis.set(Basis::Code);
    }

    // ---- location ----------------------------------------------------------

    pub fn zone(&self) -> Option<String> {
        for k in ["zone", "location", "gce_zone"] {
            if let Some(z) = self.str(k)
                && is_zone(z)
            {
                return Some(z.to_string());
            }
        }
        self.r.provider.zone.clone()
    }

    /// Report the resource location shown in outputs (region, multi-region or
    /// instance configuration) when it differs from `region()`.
    pub fn set_location(&self, loc: impl Into<String>) {
        *self.location.borrow_mut() = Some(loc.into());
    }

    /// Region if it can be determined, without warnings or defaults.
    pub fn region_opt(&self) -> Option<String> {
        for k in ["region", "location"] {
            if let Some(r) = self.str(k)
                && r != "global"
            {
                return Some(normalize_region(&r.to_lowercase()));
            }
        }
        self.str("zone")
            .map(normalize_region)
            .or_else(|| self.r.provider.region.clone())
            .or_else(|| self.r.provider.zone.as_deref().map(normalize_region))
    }

    /// Resolved region of the resource (attributes, then provider defaults).
    pub fn region(&self) -> String {
        for k in ["region", "location"] {
            if let Some(r) = self.str(k)
                && !r.is_empty()
                && r != "global"
            {
                return normalize_region(&r.to_lowercase());
            }
        }
        if let Some(z) = self.str("zone") {
            return normalize_region(z);
        }
        if let Some(r) = &self.r.provider.region {
            return r.clone();
        }
        if let Some(z) = &self.r.provider.zone {
            return normalize_region(z);
        }
        self.warn(format!("region unknown, using default {}", self.default_region));
        self.default_region.to_string()
    }

    pub fn warn(&self, msg: impl Into<String>) {
        let m = msg.into();
        let mut w = self.warnings.borrow_mut();
        if !w.contains(&m) {
            w.push(m);
        }
    }

    // ---- cross-resource lookups -------------------------------------------

    /// Resolve a reference value (synthetic `costerra::<address>` id, self_link,
    /// id or name) to another resource of the project.
    pub fn resolve_ref(&self, v: &str, rtype_prefix: &str) -> Option<&'a ResourceInstance> {
        if let Some(addr) = v.strip_prefix(crate::REF_PREFIX) {
            let addr = addr.split('/').next().unwrap_or(addr);
            return self.siblings.iter().find(|r| r.address == addr);
        }
        let last = v.rsplit('/').next().unwrap_or(v);
        self.siblings.iter().filter(|r| r.rtype.starts_with(rtype_prefix)).find(|r| {
            ["self_link", "id", "name"]
                .iter()
                .any(|k| r.attrs.get(k).and_then(|x| x.as_str()).is_some_and(|x| x == v || (*k == "name" && x == last)))
        })
    }

    // ---- pricing helpers ---------------------------------------------------

    pub fn price(&self, q: Query) -> Option<std::sync::Arc<crate::pricing::query::Price>> {
        self.pricer.find(q)
    }

    fn build(&self, name: String, unit: &str, qty: Option<f64>, q: Query, usage_based: bool) -> Component {
        let price = self.price(q);
        if price.is_none() && self.pricer.catalog.is_some() {
            self.warn(format!("no price found for '{name}' in {}", q.region));
        }
        let monthly_cost = match (&price, qty) {
            (Some(p), Some(qn)) => Some(p.cost(qn)),
            _ => None,
        };
        Component {
            name,
            unit: unit.to_string(),
            quantity: qty,
            unit_price: price.as_ref().map(|p| p.unit_price()),
            monthly_cost,
            list_unit_price: None,
            list_monthly_cost: None,
            sku: price.as_ref().map(|p| p.sku_id.clone()),
            sku_description: price.as_ref().map(|p| p.description.clone()),
            usage_based,
            note: None,
        }
    }

    /// Always-on hourly component: `count` units running all month.
    pub fn hourly(&self, name: impl Into<String>, count: f64, q: Query) -> Component {
        let hours = self.usage_num("monthly_hrs").unwrap_or(HOURS_PER_MONTH);
        self.build(name.into(), "hours", Some(count * hours), q, false)
    }

    /// Hourly component with an explicit unit (e.g. vCPU-hours, GiB-hours).
    pub fn hourly_unit(&self, name: impl Into<String>, unit: &str, count: f64, q: Query) -> Component {
        let hours = self.usage_num("monthly_hrs").unwrap_or(HOURS_PER_MONTH);
        self.build(name.into(), unit, Some(count * hours), q, false)
    }

    /// Fixed monthly quantity derived from configuration.
    pub fn monthly(&self, name: impl Into<String>, unit: &str, qty: f64, q: Query) -> Component {
        self.build(name.into(), unit, Some(qty), q, false)
    }

    /// Usage-based component; quantity comes from the usage file.
    pub fn usage(&self, name: impl Into<String>, unit: &str, usage_key: &str, q: Query) -> Component {
        let qty = self.usage_num(usage_key);
        self.build(name.into(), unit, qty, q, true)
    }

    /// Usage-based component with an explicit (possibly scaled) quantity.
    pub fn usage_qty(&self, name: impl Into<String>, unit: &str, qty: Option<f64>, q: Query) -> Component {
        self.build(name.into(), unit, qty, q, true)
    }

    /// Component with a computed unit price (e.g. vCPU + RAM combined).
    pub fn fixed(&self, name: impl Into<String>, unit: &str, qty: Option<f64>, unit_price: Option<f64>) -> Component {
        let name = name.into();
        if unit_price.is_none() && self.pricer.catalog.is_some() {
            self.warn(format!("no price found for '{name}'"));
        }
        Component {
            name,
            unit: unit.to_string(),
            quantity: qty,
            unit_price,
            monthly_cost: match (qty, unit_price) {
                (Some(q), Some(p)) => Some(q * p),
                _ => None,
            },
            list_unit_price: None,
            list_monthly_cost: None,
            sku: None,
            sku_description: None,
            usage_based: false,
            note: None,
        }
    }

    pub fn monthly_hours(&self) -> f64 {
        self.usage_num("monthly_hrs").unwrap_or(HOURS_PER_MONTH)
    }

    /// Format an amount in the report currency (for recommendation texts).
    pub fn money(&self, v: f64) -> String {
        crate::output::money(self.pricer.currency(), v)
    }

    /// Format a unit price in the report currency (e.g. `$0.026`).
    pub fn unit_price(&self, p: f64) -> String {
        crate::output::unit_price(self.pricer.currency(), p)
    }

    /// Build a recommendation for this resource.
    pub fn rec(
        &self,
        id: &str,
        severity: Severity,
        title: impl Into<String>,
        detail: impl Into<String>,
        monthly_savings: Option<f64>,
    ) -> Recommendation {
        Recommendation {
            id: id.to_string(),
            source: "costerra".into(),
            source_ref: Some(format!("rule {id}")),
            severity,
            address: self.r.address.clone(),
            title: title.into(),
            detail: detail.into(),
            monthly_savings: monthly_savings.map(|s| (s * 100.0).round() / 100.0),
            basis: self.basis.get(),
            change: None,
            // Only for advice that relies on observed metrics.
            console_url: self.observed.filter(|_| self.basis.get() == Basis::Observed).and_then(|o| o.console.clone()),
        }
    }

    /// Savings from a reprice, if meaningful (> 1 currency unit and > 1%).
    pub fn savings_vs(&self, current: f64, new: Option<f64>) -> Option<f64> {
        let n = new?;
        let s = current - n;
        (s > 1.0 && s / current.max(1e-9) > 0.01).then_some(s)
    }

    /// Re-run the cost model of this resource with modified attributes and
    /// return the new monthly total (used by recommendations).
    pub fn reprice(&self, modify: impl FnOnce(&mut Map)) -> Option<f64> {
        let spec = registry().get(self.r.rtype.as_str())?;
        let mut attrs = self.attrs.as_map()?.clone();
        modify(&mut attrs);
        let v = Value::Map(attrs);
        let ctx = Ctx {
            r: self.r,
            attrs: &v,
            pricer: self.pricer,
            usage: self.usage,
            default_region: self.default_region,
            env: self.env,
            siblings: self.siblings,
            warnings: RefCell::new(vec![]),
            location: RefCell::new(None),
            observed: self.observed,
            observed_keys: self.observed_keys,
            basis: Cell::new(Basis::Code),
        };
        let comps = (spec.estimate)(&ctx);
        self.basis.set(self.basis.get().max(ctx.basis.get()));
        if comps.iter().any(|c| c.is_unpriced()) {
            return None;
        }
        Some(crate::cost::CostedResource::total(&comps))
    }
}

pub fn is_zone(s: &str) -> bool {
    let p: Vec<&str> = s.split('-').collect();
    p.len() == 3 && p[2].len() == 1 && p[1].chars().last().is_some_and(|c| c.is_ascii_digit())
}

/// Set a nested attribute path like `settings.0.tier` in a map.
pub fn set_path(m: &mut Map, path: &str, v: Value) {
    let segs: Vec<&str> = path.split('.').collect();
    fn rec(cur: &mut Value, segs: &[&str], v: Value) {
        if segs.is_empty() {
            *cur = v;
            return;
        }
        let s = segs[0];
        match cur {
            Value::Map(m) => {
                let e = m.entry(s.to_string()).or_insert(Value::Null);
                if e.is_null() && segs.len() > 1 {
                    *e = if segs[1].parse::<usize>().is_ok() { Value::List(vec![Value::Map(Map::new())]) } else { Value::Map(Map::new()) };
                }
                rec(e, &segs[1..], v)
            }
            Value::List(l) => {
                if let Ok(i) = s.parse::<usize>()
                    && let Some(e) = l.get_mut(i)
                {
                    rec(e, &segs[1..], v)
                }
            }
            _ => {}
        }
    }
    let mut root = Value::Map(std::mem::take(m));
    rec(&mut root, &segs, v);
    if let Value::Map(x) = root {
        *m = x;
    }
}
