//! Google Cloud free tiers per service. They apply per billing account (or per
//! project), not per resource, so costerra does not deduct them from resource
//! estimates by default (`--include-free-tier` applies catalog free tiers).
//! Source: https://cloud.google.com/free/docs/free-cloud-features and the
//! product pricing pages.

use serde::Serialize;

pub struct FreeTier {
    pub service: &'static str,
    /// Resource type prefixes covered by this service.
    pub types: &'static [&'static str],
    pub allowance: &'static str,
    pub url: &'static str,
    /// Regions where the free tier applies (empty: all regions).
    pub regions: &'static [&'static str],
}

pub static FREE_TIERS: &[FreeTier] = &[
    FreeTier {
        service: "Compute Engine",
        types: &["google_compute_instance", "google_compute_instance_from_template", "google_compute_disk", "google_compute_address"],
        allowance: "1 non-preemptible e2-micro VM per month in us-west1, us-central1 or us-east1; 30 GB-months of standard persistent disk; 1 GB of outbound data transfer from North America per month (excl. China and Australia).",
        url: "https://cloud.google.com/compute/all-pricing",
        regions: &["us-west1", "us-central1", "us-east1"],
    },
    FreeTier {
        service: "Google Kubernetes Engine",
        types: &["google_container_cluster"],
        allowance: "One Autopilot or zonal Standard cluster per billing account has no cluster management fee ($74.40/month credit). Nodes, networking and storage are billed.",
        url: "https://cloud.google.com/kubernetes-engine/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Cloud Storage",
        types: &["google_storage_bucket"],
        allowance: "5 GB-months of Standard storage, 5,000 Class A and 50,000 Class B operations, and 100 GB of outbound data transfer from North America per month; storage and operations only in us-east1, us-west1 and us-central1.",
        url: "https://cloud.google.com/storage/pricing",
        regions: &["us-west1", "us-central1", "us-east1"],
    },
    FreeTier {
        service: "Artifact Registry",
        types: &["google_artifact_registry_repository"],
        allowance: "0.5 GB of storage per month.",
        url: "https://cloud.google.com/artifact-registry/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Cloud Run",
        types: &["google_cloud_run_service", "google_cloud_run_v2_service", "google_cloud_run_v2_job", "google_cloud_run_v2_worker_pool"],
        allowance: "Request-based billing: 2 million requests, 180,000 vCPU-seconds and 360,000 GiB-seconds per month, and 1 GB of outbound data transfer from North America. Instance-based billing has its own free tier (see pricing page).",
        url: "https://cloud.google.com/run/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Cloud Run functions",
        types: &["google_cloudfunctions_function", "google_cloudfunctions2_function"],
        allowance: "2 million invocations, 400,000 GB-seconds and 200,000 GHz-seconds of compute time, and 5 GB of outbound data transfer per month.",
        url: "https://cloud.google.com/functions/pricing",
        regions: &[],
    },
    FreeTier {
        service: "App Engine",
        types: &["google_app_engine_standard_app_version"],
        allowance: "Standard environment only: 28 hours per day of F1 instances, 9 hours per day of B1 instances and 1 GB of outbound data transfer per day.",
        url: "https://cloud.google.com/appengine/pricing",
        regions: &[],
    },
    FreeTier {
        service: "BigQuery",
        types: &["google_bigquery_dataset", "google_bigquery_table", "google_bigquery_data_transfer_config"],
        allowance: "1 TiB of on-demand queries and 10 GiB of storage per month.",
        url: "https://cloud.google.com/bigquery/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Pub/Sub",
        types: &["google_pubsub_topic", "google_pubsub_subscription"],
        allowance: "10 GiB of messages per month.",
        url: "https://cloud.google.com/pubsub/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Firestore",
        types: &["google_firestore_database"],
        allowance: "Per project (one database): 1 GiB of storage; 50,000 reads, 20,000 writes and 20,000 deletes per day; 10 GiB of outbound data transfer per month.",
        url: "https://cloud.google.com/firestore/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Secret Manager",
        types: &["google_secret_manager_secret", "google_secret_manager_secret_version", "google_secret_manager_regional_secret"],
        allowance: "6 active secret versions, 10,000 access operations and 3 rotation notifications per month.",
        url: "https://cloud.google.com/secret-manager/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Cloud Key Management Service",
        types: &["google_kms_crypto_key", "google_kms_crypto_key_version", "google_kms_key_handle"],
        allowance: "100 active key versions and 10,000 cryptographic operations per month, only for keys created with Cloud KMS Autokey.",
        url: "https://cloud.google.com/kms/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Cloud Logging",
        types: &[
            "google_logging_project_bucket_config",
            "google_logging_folder_bucket_config",
            "google_logging_organization_bucket_config",
            "google_logging_billing_account_bucket_config",
        ],
        allowance: "First 50 GiB of log data per project per month; logs kept for the default retention period (30 days) have no retention cost; the _Required bucket is free.",
        url: "https://cloud.google.com/stackdriver/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Cloud Monitoring",
        types: &["google_monitoring_uptime_check_config", "google_monitoring_metric_descriptor"],
        allowance: "All non-chargeable Google Cloud metrics; first 1 million time series returned by Monitoring API read calls per billing account.",
        url: "https://cloud.google.com/stackdriver/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Cloud Build",
        types: &["google_cloudbuild_trigger", "google_cloudbuild_worker_pool"],
        allowance: "2,500 build-minutes per month on the default pool with e2-standard-2 (private pools are not covered).",
        url: "https://cloud.google.com/build/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Cloud Deploy",
        types: &["google_clouddeploy_delivery_pipeline"],
        allowance: "First active delivery pipeline per billing account.",
        url: "https://cloud.google.com/deploy/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Workflows",
        types: &["google_workflows_workflow"],
        allowance: "5,000 internal steps and 2,000 external HTTP calls per month.",
        url: "https://cloud.google.com/workflows/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Cloud Scheduler",
        types: &["google_cloud_scheduler_job"],
        allowance: "3 jobs per billing account per month.",
        url: "https://cloud.google.com/scheduler/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Cloud Tasks",
        types: &["google_cloud_tasks_queue"],
        allowance: "First 1 million billable operations per month.",
        url: "https://cloud.google.com/tasks/pricing",
        regions: &[],
    },
    FreeTier {
        service: "API Gateway",
        types: &["google_api_gateway_gateway"],
        allowance: "First 2 million API calls per month.",
        url: "https://cloud.google.com/api-gateway/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Identity Platform",
        types: &["google_identity_platform_config", "google_identity_platform_tenant"],
        allowance: "50,000 monthly active users with Tier 1 providers (email, social, anonymous…) and 50 SAML/OIDC monthly active users.",
        url: "https://cloud.google.com/identity-platform/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Datastream",
        types: &["google_datastream_stream"],
        allowance: "100 GiB of change data capture per month, only for AlloyDB for PostgreSQL or Spanner sources streaming to BigQuery.",
        url: "https://cloud.google.com/datastream/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Network data transfer",
        types: &["google_compute_forwarding_rule", "google_compute_global_forwarding_rule", "google_compute_router_nat"],
        allowance: "Standard Tier: first 200 GiB of internet egress per month (all regions); Premium Tier: first 1 GiB to most destinations. Inbound traffic is free.",
        url: "https://cloud.google.com/vpc/network-pricing",
        regions: &[],
    },
    FreeTier {
        service: "Network Intelligence Center",
        types: &["google_network_management_connectivity_test"],
        allowance: "First 20 Connectivity Tests runs per billing account per month.",
        url: "https://cloud.google.com/network-intelligence-center/pricing",
        regions: &[],
    },
    FreeTier {
        service: "Spanner",
        types: &["google_spanner_instance"],
        allowance: "No always-free tier; a 90-day free trial instance with 10 GB of storage is available (instance_type = FREE_INSTANCE).",
        url: "https://cloud.google.com/spanner/pricing",
        regions: &[],
    },
    FreeTier {
        service: "AlloyDB",
        types: &["google_alloydb_cluster", "google_alloydb_instance"],
        allowance: "No always-free tier; a 30-day free trial cluster (8 vCPU basic instance, 1 TB storage) is available.",
        url: "https://cloud.google.com/alloydb/pricing",
        regions: &[],
    },
];

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct FreeTierInfo {
    pub service: String,
    pub allowance: String,
    pub url: String,
}

/// Free tiers of the services used by these (resource type, region) pairs
/// (deduplicated, in table order). Region-limited free tiers are only listed
/// when a resource is in one of their regions (or its region is unknown).
pub fn for_resources<'a>(resources: impl Iterator<Item = (&'a str, Option<&'a str>)>) -> Vec<FreeTierInfo> {
    let resources: Vec<(&str, Option<&str>)> = resources.collect();
    FREE_TIERS
        .iter()
        .filter(|f| {
            resources.iter().any(|(t, region)| {
                f.types.contains(t) && (f.regions.is_empty() || region.is_none_or(|r| f.regions.contains(&r.to_lowercase().as_str())))
            })
        })
        .map(|f| FreeTierInfo { service: f.service.into(), allowance: f.allowance.into(), url: f.url.into() })
        .collect()
}
