//! Command-line arguments.

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::builder::{FalseyValueParser, PossibleValue, TypedValueParser};
use clap::{Args, Parser, Subcommand, ValueEnum};
use costerra::output::Format;
use std::path::PathBuf;

/// Colors of `--help` and of usage errors.
const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD));

#[derive(Parser)]
#[command(name = "costerra", version, about = "Cost estimates and optimization insights for GCP Terraform / Terragrunt", styles = STYLES)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
    /// Verbose logging (-v, -vv)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// When to use colors
    #[arg(long, global = true, value_enum, default_value = "auto")]
    pub color: ColorChoice,
    /// Disable colored output (same as --color never)
    #[arg(long, global = true)]
    pub no_color: bool,
    /// Cache directory for prices and modules (default: ~/.cache/costerra)
    #[arg(long, global = true, env = "COSTERRA_CACHE_DIR")]
    pub cache_dir: Option<PathBuf>,
    /// Service account to impersonate for Google API calls (needs Service Account Token Creator on it)
    #[arg(long, global = true, env = "COSTERRA_IMPERSONATE_SERVICE_ACCOUNT", value_name = "EMAIL")]
    pub impersonate_service_account: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    pub fn enabled(self, no_color: bool, tty: bool) -> bool {
        if no_color || std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return false;
        }
        match self {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => tty || std::env::var("CLICOLOR_FORCE").is_ok_and(|v| v != "0"),
        }
    }
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Show a cost breakdown of Terraform / Terragrunt code or a plan JSON
    Breakdown(BreakdownArgs),
    /// Show the cost difference against a git ref or a baseline report
    Diff(DiffArgs),
    /// Manage the local pricing catalog
    #[command(subcommand)]
    Pricing(PricingCmd),
    /// Usage file helpers
    #[command(subcommand)]
    Usage(UsageCmd),
    /// List supported resource types
    Resources {
        /// Only show resources matching this substring
        filter: Option<String>,
    },
    /// Print the shell completion script
    ///
    /// bash: `source <(costerra completions bash)` in ~/.bashrc;
    /// zsh: `costerra completions zsh > "${fpath[1]}/_costerra"`;
    /// fish: `costerra completions fish > ~/.config/fish/completions/costerra.fish`;
    /// nu: `costerra completions nu | save -f ($nu.data-dir | path join completions costerra.nu)`, then `use` it in config.nu;
    /// xonsh: `execx($(costerra completions xonsh))` in ~/.xonshrc.
    /// Homebrew installs the bash, zsh and fish completions.
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
    Nu,
    Xonsh,
}

/// `--var name=value`: the `=` is required.
fn parse_var(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_string(), v.to_string())),
        _ => Err(format!("expected NAME=VALUE, got '{s}'")),
    }
}

/// An output: a format, written to a file or to stdout (`markdown:costerra.md`).
#[derive(Clone, Debug)]
pub struct Output {
    pub format: Format,
    pub file: Option<PathBuf>,
}

fn parse_output(s: &str) -> Result<Output, String> {
    let (name, file) = match s.split_once(':') {
        Some((f, p)) if !p.is_empty() => (f, Some(PathBuf::from(p))),
        _ => (s, None),
    };
    let format = Format::from_str(name, true)
        .map_err(|_| format!("unknown format '{name}' (expected table, json or markdown, optionally followed by :FILE)"))?;
    Ok(Output { format, file })
}

/// `--format` parser: `FORMAT[:FILE]`, with the formats listed for help and
/// shell completions.
#[derive(Clone)]
struct OutputParser;

impl TypedValueParser for OutputParser {
    type Value = Output;

    fn parse_ref(&self, cmd: &clap::Command, arg: Option<&clap::Arg>, value: &std::ffi::OsStr) -> Result<Output, clap::Error> {
        let s = value.to_str().ok_or_else(|| clap::Error::new(clap::error::ErrorKind::InvalidUtf8).with_cmd(cmd))?;
        parse_output(s).map_err(|e| {
            let arg = arg.map(|a| a.to_string()).unwrap_or_else(|| "--format".into());
            clap::Error::raw(clap::error::ErrorKind::InvalidValue, format!("invalid value '{s}' for '{arg}': {e}\n")).with_cmd(cmd)
        })
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        Some(Box::new(Format::value_variants().iter().filter_map(|f| f.to_possible_value())))
    }
}

fn parse_duration(s: &str) -> Result<u64, String> {
    costerra::util::parse_duration_secs(s)
}

/// How to evaluate the code (shared by `breakdown`, `diff` and `usage init`).
#[derive(Args, Clone)]
pub struct EvalArgs {
    /// Directory (Terraform / Terragrunt), terragrunt.hcl, or plan JSON file
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Config file (default: costerra.yml discovered upwards)
    #[arg(short, long)]
    pub config: Option<PathBuf>,
    /// Extra .tfvars files (Terraform projects)
    #[arg(long = "var-file")]
    pub var_files: Vec<PathBuf>,
    /// Variable values: --var name=value
    #[arg(long = "var", value_parser = parse_var)]
    pub vars: Vec<(String, String)>,
    /// Terraform workspace name
    #[arg(long)]
    pub workspace: Option<String>,
    /// Do not evaluate one project per environment tfvars file (environments/*.tfvars…)
    #[arg(long)]
    pub no_env_discovery: bool,
    /// Glob patterns of paths to exclude
    #[arg(long)]
    pub exclude: Vec<String>,
    /// Do not download remote modules or pricing
    #[arg(long)]
    pub offline: bool,
    /// Allow terragrunt run_cmd() to execute commands
    #[arg(long)]
    pub allow_run_cmd: bool,
    /// Do not read terraform_remote_state outputs from the GCS state buckets
    #[arg(long, env = "COSTERRA_NO_REMOTE_STATE", value_parser = FalseyValueParser::new())]
    pub no_remote_state: bool,
}

#[derive(Args, Clone)]
pub struct RunArgs {
    #[command(flatten)]
    pub eval: EvalArgs,
    /// Output format: table (the terminal table, alias console), json or
    /// markdown, optionally written to a file (`markdown:costerra.md`).
    /// Repeat it, or separate with commas, for several outputs:
    /// `--format table,markdown:costerra.md`
    #[arg(short, long, value_name = "FORMAT[:FILE]", value_delimiter = ',', value_parser = OutputParser, default_value = "table", hide_possible_values = true)]
    pub format: Vec<Output>,
    /// Write the output of the format without a file to this file instead of stdout
    #[arg(short, long)]
    pub out: Option<PathBuf>,
    /// Usage file with estimates for usage-based costs
    #[arg(short, long, env = "COSTERRA_USAGE_FILE")]
    pub usage_file: Option<PathBuf>,
    /// Default region when none can be determined
    #[arg(long, env = "COSTERRA_DEFAULT_REGION")]
    pub region: Option<String>,
    /// Pricing currency (requires a catalog synced in this currency)
    #[arg(long, env = "COSTERRA_CURRENCY")]
    pub currency: Option<String>,
    /// Billing account whose contract prices to use (default: detected from the projects; `none` = list prices)
    #[arg(long, env = "COSTERRA_BILLING_ACCOUNT")]
    pub billing_account: Option<String>,
    /// Ignore the billing account and use list prices only
    #[arg(long)]
    pub list_prices: bool,
    /// Do not refresh contract prices of the used SKUs from the Pricing API
    #[arg(long, env = "COSTERRA_NO_REFRESH", value_parser = FalseyValueParser::new())]
    pub no_refresh: bool,
    /// Re-fetch a contract price only if cached longer than this (e.g. 0, 90s, 15m, 1h; default 15m)
    #[arg(long, env = "COSTERRA_PRICE_CACHE_TTL", value_parser = parse_duration)]
    pub price_cache_ttl: Option<u64>,
    /// Show optimization recommendations
    #[arg(long, env = "COSTERRA_RECOMMENDATIONS", value_parser = FalseyValueParser::new())]
    pub recommendations: bool,
    /// Skip optimization recommendations (the default; overrides --recommendations)
    #[arg(long, hide = true)]
    pub no_recommendations: bool,
    /// Skip usage-based recommendations from Google Cloud Recommender
    #[arg(long, env = "COSTERRA_NO_INSIGHTS", value_parser = FalseyValueParser::new())]
    pub no_insights: bool,
    /// Do not read the observed usage of deployed resources (Cloud Monitoring metrics, active commitments)
    #[arg(long, env = "COSTERRA_NO_OBSERVED", value_parser = FalseyValueParser::new())]
    pub no_observed: bool,
    /// Quota project for the Recommender and Cloud Monitoring APIs (default: ADC quota_project_id)
    #[arg(long, env = "GOOGLE_CLOUD_QUOTA_PROJECT")]
    pub quota_project: Option<String>,
    /// Price usage-based resources without estimates with an example profile
    #[arg(long, value_enum, env = "COSTERRA_USAGE_PROFILE")]
    pub usage_profile: Option<costerra::profiles::Profile>,
    /// Include per-billing-account free tiers in estimates (and show them)
    #[arg(long)]
    pub include_free_tier: bool,
    /// Show the free tiers of the services used
    #[arg(long)]
    pub free_tier: bool,
    /// Show zero-cost resources and warnings in the table
    #[arg(long)]
    pub show_all: bool,
    /// Show all details: recommendations, free tiers, zero-cost resources,
    /// warnings, price notes and notes about data sources
    #[arg(long)]
    pub details: bool,
    /// Show every instance of counted resources instead of `name[*] ×N` groups
    #[arg(long)]
    pub no_group: bool,
    /// Exit with code 2 if the total monthly cost exceeds this amount
    #[arg(long)]
    pub fail_above: Option<f64>,
}

#[derive(Args)]
pub struct BreakdownArgs {
    #[command(flatten)]
    pub run: RunArgs,
    /// Rightsizing from observed usage (Cloud Monitoring): why, where in the
    /// code, the cost after the change and a console link to check it
    #[arg(long)]
    pub rightsize: bool,
}

impl RunArgs {
    /// The outputs, with `--out` given to the format without a file. At most
    /// one format goes to stdout.
    pub fn outputs(&self) -> anyhow::Result<Vec<Output>> {
        let mut outputs = self.format.clone();
        let mut to_stdout = outputs.iter_mut().filter(|o| o.file.is_none());
        match (to_stdout.next(), to_stdout.next(), &self.out) {
            (Some(_), Some(_), _) => anyhow::bail!("only one --format can go to stdout: give the others a file (`markdown:costerra.md`)"),
            (Some(o), None, Some(out)) => o.file = Some(out.clone()),
            (None, _, Some(_)) => anyhow::bail!("--out needs a --format without a file"),
            _ => {}
        }
        Ok(outputs)
    }

    /// Whether recommendations are shown.
    pub fn show_recommendations(&self) -> bool {
        (self.recommendations || self.details) && !self.no_recommendations
    }

    pub fn show_free_tier(&self) -> bool {
        self.free_tier || self.include_free_tier || self.details
    }
}

#[derive(Args)]
pub struct DiffArgs {
    #[command(flatten)]
    pub run: RunArgs,
    /// Git ref to compare against (e.g. origin/main)
    #[arg(long, conflicts_with = "baseline")]
    pub base: Option<String>,
    /// Baseline JSON report produced by `costerra breakdown --format json`
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Exit with code 2 if the monthly increase exceeds this amount
    #[arg(long)]
    pub fail_increase_above: Option<f64>,
    /// Exit with code 2 if the monthly increase exceeds this percentage
    #[arg(long)]
    pub fail_increase_percent: Option<f64>,
}

#[derive(Subcommand)]
pub enum PricingCmd {
    /// Download SKUs from the Cloud Billing Catalog API
    Sync {
        /// API key (otherwise Application Default Credentials / gcloud are used)
        #[arg(long, env = "COSTERRA_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        #[arg(long, default_value = "USD", env = "COSTERRA_CURRENCY")]
        currency: String,
        /// Download all GCP services, not only the ones costerra prices
        #[arg(long)]
        all_services: bool,
        /// Also download the contract prices (negotiated discounts) of this billing account
        #[arg(long, env = "COSTERRA_BILLING_ACCOUNT")]
        billing_account: Option<String>,
        /// Only download when the catalog is missing or older than this (e.g. 7d); for CI caches
        #[arg(long, value_parser = parse_duration)]
        if_older_than: Option<u64>,
    },
    /// Build the catalog from raw API dumps (services.jsonl + <id>.jsonl)
    Import {
        dir: PathBuf,
        #[arg(long, default_value = "USD")]
        currency: String,
    },
    /// Refresh contract prices of specific SKUs (used in the background)
    #[command(hide = true)]
    Refresh {
        #[arg(long)]
        billing_account: String,
        #[arg(long)]
        currency: String,
        /// Comma-separated SKU ids
        #[arg(long)]
        skus: String,
    },
    /// Show catalog information
    Status {
        #[arg(long, default_value = "USD", env = "COSTERRA_CURRENCY")]
        currency: String,
        #[arg(long, env = "COSTERRA_BILLING_ACCOUNT")]
        billing_account: Option<String>,
    },
    /// Search SKUs (regex on description)
    Search {
        query: String,
        #[arg(long, default_value = "")]
        service: String,
        #[arg(long, default_value = "")]
        region: String,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long, default_value = "USD", env = "COSTERRA_CURRENCY")]
        currency: String,
    },
}

#[derive(Subcommand)]
pub enum UsageCmd {
    /// Show the example usage profiles (small / medium / large)
    Profiles {
        /// Only show the keys used by this resource type
        #[arg(long = "type")]
        rtype: Option<String>,
    },
    /// Generate a usage file template for usage-based resources found in PATH
    Init {
        #[command(flatten)]
        eval: EvalArgs,
        #[arg(short, long, default_value = "costerra-usage.yml")]
        out: PathBuf,
        /// Overwrite the output file if it exists
        #[arg(long)]
        force: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn vars_need_a_value() {
        assert_eq!(parse_var("a=b=c"), Ok(("a".into(), "b=c".into())));
        assert_eq!(parse_var("a="), Ok(("a".into(), String::new())));
        assert!(parse_var("foo").is_err());
        assert!(parse_var("=x").is_err());
    }
}
