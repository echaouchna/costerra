//! Example usage profiles: typical monthly usage values that give an idea of
//! what usage-based resources could cost before real estimates are known.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Small,
    Medium,
    Large,
}

impl Profile {
    pub const ALL: [Profile; 3] = [Profile::Small, Profile::Medium, Profile::Large];

    pub fn name(self) -> &'static str {
        match self {
            Profile::Small => "small",
            Profile::Medium => "medium",
            Profile::Large => "large",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Profile::Small => "dev / low traffic",
            Profile::Medium => "typical production service",
            Profile::Large => "high traffic / data-heavy",
        }
    }

    fn idx(self) -> usize {
        self as usize
    }
}

const K: f64 = 1_000.0;
const M: f64 = 1_000_000.0;

/// (usage key, [small, medium, large], unit)
pub const VALUES: &[(&str, [f64; 3], &str)] = &[
    // Storage (GiB-months)
    ("storage_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("long_term_storage_gb", [0.0, 50.0, 500.0], "GiB"),
    ("physical_storage_gb", [5.0, 50.0, 500.0], "GiB"),
    ("long_term_physical_storage_gb", [0.0, 25.0, 250.0], "GiB"),
    ("time_travel_storage_gb", [1.0, 10.0, 100.0], "GiB"),
    ("fail_safe_storage_gb", [1.0, 10.0, 100.0], "GiB"),
    ("backup_storage_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("continuous_backup_storage_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("pitr_storage_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("soft_delete_storage_gb", [1.0, 5.0, 50.0], "GiB"),
    ("snapshot_storage_gb", [0.0, 10.0, 100.0], "GiB"),
    ("retained_acked_storage_gb", [1.0, 10.0, 100.0], "GiB"),
    ("online_storage_gb", [1.0, 10.0, 100.0], "GiB"),
    ("offline_storage_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("bigtable_storage_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("cache_storage_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("database_storage_gb", [5.0, 20.0, 100.0], "GiB"),
    ("monthly_shuffle_storage_gb", [10.0, 100.0, 1000.0], "GiB"),
    // Data transfer / processing (GiB per month)
    ("monthly_egress_data_transfer_gb", [1.0, 50.0, 500.0], "GiB"),
    ("monthly_egress_gb", [1.0, 50.0, 500.0], "GiB"),
    ("monthly_internet_egress_gb", [1.0, 50.0, 500.0], "GiB"),
    ("monthly_outbound_data_gb", [1.0, 50.0, 500.0], "GiB"),
    ("monthly_data_processed_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("monthly_outbound_data_processed_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("monthly_logging_data_gb", [5.0, 50.0, 500.0], "GiB"),
    ("monthly_shuffle_data_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("monthly_storage_write_api_gb", [1.0, 10.0, 100.0], "GiB"),
    ("monthly_cache_egress_gb", [10.0, 500.0, 5000.0], "GiB"),
    ("monthly_cache_fill_gb", [1.0, 50.0, 500.0], "GiB"),
    ("monthly_cache_ingest_gb", [1.0, 10.0, 100.0], "GiB"),
    ("monthly_cache_data_transfer_out_gb", [10.0, 100.0, 1000.0], "GiB"),
    ("monthly_data_retrieval_gb", [0.0, 10.0, 100.0], "GiB"),
    ("monthly_replicated_data_gb", [1.0, 10.0, 100.0], "GiB"),
    ("monthly_streaming_update_gb", [0.0, 1.0, 10.0], "GiB"),
    ("monthly_index_build_gb", [1.0, 10.0, 100.0], "GiB"),
    ("monthly_cdc_data_gb", [1.0, 50.0, 500.0], "GiB"),
    ("monthly_backfill_data_gb", [0.0, 10.0, 100.0], "GiB"),
    ("monthly_inspected_data_gb", [10.0, 500.0, 5000.0], "GiB"),
    ("monthly_message_data_gb", [1.0, 50.0, 500.0], "GiB"),
    ("monthly_streaming_inserts_mb", [100.0, 10_000.0, 100_000.0], "MiB"),
    ("monthly_ingested_mib", [10.0, 100.0, 1000.0], "MiB"),
    ("monthly_queries_tib", [0.1, 1.0, 10.0], "TiB"),
    ("monthly_storage_read_api_tib", [0.0, 0.1, 1.0], "TiB"),
    // Requests / operations (per month)
    ("monthly_requests", [100.0 * K, 5.0 * M, 50.0 * M], "requests"),
    ("average_request_duration_ms", [200.0, 200.0, 200.0], "ms"),
    ("request_duration_ms", [200.0, 200.0, 200.0], "ms"),
    ("monthly_function_invocations", [100.0 * K, 2.0 * M, 20.0 * M], "invocations"),
    ("monthly_calls", [100.0 * K, 2.0 * M, 20.0 * M], "calls"),
    ("monthly_class_a_operations", [10.0 * K, 100.0 * K, 1.0 * M], "operations"),
    ("monthly_class_b_operations", [100.0 * K, 1.0 * M, 10.0 * M], "operations"),
    ("monthly_access_operations", [10.0 * K, 100.0 * K, 1.0 * M], "operations"),
    ("monthly_key_operations", [10.0 * K, 100.0 * K, 1.0 * M], "operations"),
    ("monthly_operations", [10.0 * K, 1.0 * M, 10.0 * M], "operations"),
    ("monthly_cache_operations", [100.0 * K, 1.0 * M, 10.0 * M], "operations"),
    ("monthly_events", [10.0 * K, 1.0 * M, 10.0 * M], "events"),
    ("monthly_queries", [1.0 * M, 10.0 * M, 100.0 * M], "queries"),
    ("monthly_cache_lookup_requests", [1.0 * M, 20.0 * M, 200.0 * M], "requests"),
    ("monthly_document_reads", [100.0 * K, 5.0 * M, 50.0 * M], "reads"),
    ("monthly_document_writes", [20.0 * K, 1.0 * M, 10.0 * M], "writes"),
    ("monthly_document_deletes", [5.0 * K, 100.0 * K, 1.0 * M], "deletes"),
    ("monthly_executions", [30.0, 300.0, 3000.0], "executions"),
    ("average_task_duration_seconds", [60.0, 300.0, 600.0], "s"),
    ("monthly_internal_steps", [10.0 * K, 100.0 * K, 1.0 * M], "steps"),
    ("monthly_external_steps", [1.0 * K, 10.0 * K, 100.0 * K], "steps"),
    ("monthly_transformations", [0.0, 100.0 * K, 1.0 * M], "transformations"),
    ("monthly_mtls_connections", [0.0, 100.0 * K, 1.0 * M], "connections"),
    ("monthly_samples", [1.0 * M, 10.0 * M, 100.0 * M], "samples"),
    ("monthly_runs", [5.0, 30.0, 300.0], "runs"),
    ("time_series", [10.0, 100.0, 1000.0], "time series"),
    ("monthly_active_users", [1.0 * K, 20.0 * K, 100.0 * K], "users"),
    ("monthly_saml_oidc_active_users", [0.0, 50.0, 500.0], "users"),
    ("monthly_sms_mfa_verifications", [0.0, 100.0, 1000.0], "verifications"),
    ("monthly_sms_mfa_verifications_other", [0.0, 0.0, 100.0], "verifications"),
    // Compute usage
    ("monthly_build_minutes", [100.0, 1000.0, 5000.0], "minutes"),
    ("monthly_hours_per_workstation", [40.0, 120.0, 160.0], "hours"),
    ("monthly_instance_hours", [0.0, 730.0, 2190.0], "instance-hours"),
    ("monthly_dcu_hours", [10.0, 100.0, 1000.0], "DCU-hours"),
    ("monthly_dcus", [10.0, 100.0, 1000.0], "DCUs"),
    ("monthly_streaming_engine_compute_units", [10.0, 100.0, 1000.0], "units"),
    ("monthly_data_processing_node_hours", [10.0, 100.0, 1000.0], "node-hours"),
    ("autopilot_vcpu_count", [2.0, 8.0, 32.0], "vCPU"),
    ("autopilot_memory_gb", [8.0, 32.0, 128.0], "GiB"),
    ("autopilot_ephemeral_storage_gb", [10.0, 50.0, 200.0], "GiB"),
];

/// Example value of a usage key for a profile.
pub fn value(key: &str, p: Profile) -> Option<f64> {
    VALUES.iter().find(|(k, _, _)| *k == key).map(|(_, v, _)| v[p.idx()])
}

pub fn unit(key: &str) -> Option<&'static str> {
    VALUES.iter().find(|(k, _, _)| *k == key).map(|(_, _, u)| *u)
}

pub fn fmt_value(v: f64) -> String {
    if v >= M && (v / M).fract() == 0.0 {
        format!("{}M", v / M)
    } else if v >= K && (v / K).fract() == 0.0 {
        format!("{}k", v / K)
    } else {
        crate::hcl::value::fmt_number(v)
    }
}
