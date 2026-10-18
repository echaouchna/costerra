//! costerra — fast cost estimation and optimization insights for Google Cloud
//! Terraform & Terragrunt code.

pub mod config;
pub mod cost;
pub mod diff;
pub mod engine;
pub mod free_tier;
pub mod hcl;
pub mod insights;
pub mod output;
pub mod pricing;
pub mod profiles;
pub mod project;
pub mod recommend;
pub mod resources;
pub mod tf;
pub mod usage;
pub mod util;

/// Prefix of synthetic identifiers assigned to resources whose `id` /
/// `self_link` are only known after apply.
pub const REF_PREFIX: &str = "costerra::";
