//! Optional `costerra.yml` configuration.
//!
//! ```yaml
//! default_region: europe-west1
//! currency: USD
//! billing_account: 012345-6789AB-CDEF01   # use your contract prices
//! billing_export_table: my-billing.billing.gcp_billing_export_resource_v1_012345_6789AB_CDEF01
//! usage_file: costerra-usage.yml
//! exclude_paths: ["examples/**"]
//! environments:
//!   nonprod: [dev, test, staging]
//!   prod: [prod, production]
//! projects:
//!   - match: "envs/dev/**"
//!     env: nonprod
//! recommendations:
//!   disabled: [region-cheaper]
//! policies:
//!   max_monthly_cost: 10000
//!   max_increase: 500
//!   max_increase_percent: 10
//! ```

use crate::resources::Env;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Environments {
    pub nonprod: Option<Vec<String>>,
    pub prod: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProjectRule {
    #[serde(rename = "match")]
    pub pattern: String,
    pub env: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct RecommendationCfg {
    pub disabled: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Policies {
    pub max_monthly_cost: Option<f64>,
    pub max_increase: Option<f64>,
    pub max_increase_percent: Option<f64>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_region: Option<String>,
    pub currency: Option<String>,
    /// Billing account whose contract prices should be used (`none` = list prices).
    pub billing_account: Option<String>,
    /// Refresh contract prices of the used SKUs (default true).
    pub refresh_prices: Option<bool>,
    /// Re-fetch a contract price only when cached longer than this (default 15m).
    pub price_cache_ttl: Option<String>,
    /// Cache directory (default ~/.cache/costerra).
    pub cache_dir: Option<PathBuf>,
    /// Quota project for Google APIs that need one (Recommender, Cloud Monitoring).
    pub quota_project: Option<String>,
    /// Usage-based recommendations from Google Cloud Recommender (default true).
    pub gcp_recommendations: Option<bool>,
    /// Observed usage of the deployed resources: Cloud Monitoring metrics and
    /// active commitments (default true).
    pub observed_usage: Option<bool>,
    /// Read `terraform_remote_state` outputs and the data sources of the
    /// configuration's own state (gcs backend) from the state buckets
    /// (default true).
    pub remote_state: Option<bool>,
    /// Service account to impersonate for Google API calls.
    pub impersonate_service_account: Option<String>,
    /// Cloud Billing export table read by `--billing-history`
    /// (`project.dataset.gcp_billing_export_resource_v1_…`; default: discovered).
    pub billing_export_table: Option<String>,
    pub usage_file: Option<PathBuf>,
    pub include_free_tier: bool,
    pub exclude_paths: Vec<String>,
    pub environments: Environments,
    pub projects: Vec<ProjectRule>,
    pub recommendations: RecommendationCfg,
    pub policies: Policies,
    #[serde(skip)]
    pub dir: Option<PathBuf>,
}

const NONPROD: &[&str] = &[
    "dev",
    "develop",
    "development",
    "test",
    "testing",
    "tst",
    "qa",
    "uat",
    "stg",
    "stage",
    "staging",
    "sandbox",
    "sbx",
    "nonprod",
    "non",
    "preprod",
    "pprod",
    "ppd",
    "demo",
    "lab",
    "poc",
    "int",
    "integration",
    "ephemeral",
    "preview",
    "rec",
    "recette",
    "experiment",
    "exp",
];
const PROD: &[&str] = &["prod", "prd", "production", "live", "prx"];

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let src = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut c: Config = if src.trim().is_empty() {
            Config::default()
        } else {
            // Unknown keys are rejected: a typo such as `polices:` must not silently disable a policy.
            crate::util::from_yaml(&src).map_err(|e| anyhow::anyhow!("{e}")).with_context(|| format!("parsing {}", path.display()))?
        };
        c.dir = path.parent().map(Path::to_path_buf);
        c.validate().with_context(|| format!("invalid {}", path.display()))?;
        Ok(c)
    }

    fn validate(&self) -> Result<()> {
        for p in self.projects.iter().map(|p| &p.pattern).chain(&self.exclude_paths) {
            globset::Glob::new(p).with_context(|| format!("invalid glob pattern '{p}'"))?;
        }
        if let Some(t) = &self.price_cache_ttl {
            crate::util::parse_duration_secs(t).map_err(|e| anyhow::anyhow!("price_cache_ttl: {e}"))?;
        }
        Ok(())
    }

    /// Look for `costerra.yml` / `.costerra.yml` in `start` and its parents.
    pub fn discover(start: &Path) -> Result<Config> {
        let start = std::path::absolute(start)?;
        let mut cur = Some(if start.is_file() { start.parent().map(Path::to_path_buf).unwrap_or_default() } else { start });
        while let Some(d) = cur {
            for name in ["costerra.yml", "costerra.yaml", ".costerra.yml", ".costerra.yaml"] {
                let p = d.join(name);
                if p.is_file() {
                    return Config::load(&p);
                }
            }
            if d.join(".git").exists() {
                break;
            }
            cur = d.parent().map(Path::to_path_buf);
        }
        Ok(Config::default())
    }

    pub fn nonprod_patterns(&self) -> Vec<String> {
        match &self.environments.nonprod {
            Some(v) => v.iter().map(|s| s.to_lowercase()).collect(),
            None => NONPROD.iter().map(|s| s.to_string()).collect(),
        }
    }

    pub fn prod_patterns(&self) -> Vec<String> {
        match &self.environments.prod {
            Some(v) => v.iter().map(|s| s.to_lowercase()).collect(),
            None => PROD.iter().map(|s| s.to_string()).collect(),
        }
    }

    pub fn project_env(&self, name: &str) -> Option<Env> {
        match self.project_env_name(name)?.as_str() {
            "prod" | "production" => Some(Env::Prod),
            _ => Some(Env::NonProd),
        }
    }

    /// The `env` of the first `projects:` rule matching a project name.
    pub fn project_env_name(&self, name: &str) -> Option<String> {
        for p in &self.projects {
            // Patterns are validated when the config is loaded.
            let Ok(g) = globset::Glob::new(&p.pattern) else { continue };
            if g.compile_matcher().is_match(name) {
                return p.env.clone();
            }
        }
        None
    }

    pub fn is_rule_disabled(&self, id: &str) -> bool {
        self.recommendations.disabled.iter().any(|d| d == id || (d.ends_with('*') && id.starts_with(d.trim_end_matches('*'))))
    }
}
