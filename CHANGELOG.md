# Changelog

All notable changes to costerra are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.4.0] - 2026-10-19

### Added
- `--billing-history` (`breakdown` and `diff`): a chart of the actual billing
  of the resources of the code (the stack only, not the rest of the GCP
  projects), week by week over the last 3 months (13
  weeks, the current one in progress), net of credits. In the terminal, a
  colored bar chart stacked by the 5 most expensive services (the others
  grouped), with value labels, the weekly equivalent of the estimate as a
  dashed line, a legend with each service's total and share, and a summary:
  total, weekly average, last full week vs the week before, last 4 weeks vs
  the 4 before, and how far the estimate is from the last full week. In
  Markdown, a Mermaid bar chart (rendered by GitHub and GitLab) and the weeks
  per service; in JSON, a `billing_history` key. Off unless asked for
  (`COSTERRA_BILLING_HISTORY=1` also turns it on).
- The data comes from the detailed usage cost export of Cloud Billing to
  BigQuery, which names the resource of each cost: costs are kept when their
  resource is one of the code (by name, by the instance name prefix of
  managed instance groups, by the GKE cluster label of the nodes). The chart
  says how many resources of the code were found in the billing and lists
  the ones without cost. The
  `gcp_billing_export_resource_v1_<billing account>` table is discovered, even in a
  separate billing project: Cloud Asset Inventory search over the
  organization of the projects, then the datasets of the projects visible in
  BigQuery (billing-like names first). It can also be set with
  `--billing-export-table` (`COSTERRA_BILLING_EXPORT_TABLE`,
  `billing_export_table:` in `costerra.yml`). The table found is remembered
  for 7 days, a fruitless search for 6 hours. Needs BigQuery Data Viewer on the export dataset and
  BigQuery Job User on the project the query runs in. The result is cached
  for an hour; when the billing cannot be read, a warning says why and the
  report is unchanged.

## [0.3.1] - 2026-10-18

### Added
- Several outputs in one run: `--format` takes `FORMAT[:FILE]` and can be
  repeated or comma-separated, e.g. `--format table,markdown:costerra.md`
  prints the table in the CI log and writes the MR comment to a file. At most
  one format goes to stdout (or to `--out`). A table written to a file has no
  colors. `console` is an alias of `table`.
- `costerra completions <shell>`: completion scripts for bash, zsh, fish,
  elvish, powershell, nu and xonsh. The Homebrew formula installs the bash,
  zsh and fish completions.
- Colored `--help` and usage errors (off with `NO_COLOR`).

### Fixed
- `costerra diff`: a root module with one project per environment
  (`environments/*.tfvars`) was named after the temporary checkout on the
  base side (`costerra-base-XXXXXX (dev)`), so every environment showed as
  removed and added. The base checkout now has the same repository root as
  the head one, which also fixes `get_repo_root()` and the discovery of
  `costerra.yml` and `costerra-usage.yml` on the base side.
- `--rightsize`: when the same value is set in several module calls, the
  line in the module call of the resource is kept (`module "web_proxy"`),
  instead of every line with a matching entry. The note names the blocks of
  the remaining lines rather than `(in ?, ?)`.
- `--rightsize`: a cost after rightsizing that rounds to zero is shown as
  `$0.00`, not `-$0.00`.
- Contract prices: when the billing account is detected but its prices are
  refused (HTTP 403), costerra now prints a single-line warning, uses list
  prices and does not ask again for 6 hours. Before, every run downloaded the
  whole price list again and printed the raw API error. The error now names
  the permission the bulk download checks (`billing.billingAccountPrices.list`).
- `--format markdown`: projects are named as in the terminal table
  (`terraform (prod)` rather than `. (prod)`), and the rightsizing section
  comes before the footer.

## [0.3.0] - 2026-10-18

### Added
- `--rightsize`: rightsizing from observed usage. Each suggestion says why
  (the observed metrics and the threshold they crossed), where the value is
  set in the code (resource block, Terragrunt `inputs`, `.tfvars`, root
  variable or `optional()` default, parent Terragrunt file; every candidate
  line when several could set it), what to know before changing it (shared
  value, module default, downloaded module), a link to the resource in the
  Google Cloud console to check the metrics, and the monthly cost before and
  after, with the total after rightsizing. costerra never edits the code. In
  JSON (`rightsizing`) and Markdown too.
- BigQuery dataset sizes in observed usage: logical, physical, time travel and
  fail-safe bytes from `INFORMATION_SCHEMA.TABLE_STORAGE` (needs
  `roles/bigquery.metadataViewer` and `roles/bigquery.jobUser`; skipped with a
  note otherwise).
- Cloud SQL CPU and memory utilization in observed usage, and
  `sql-rightsize`: a tier with half the vCPUs (custom, legacy `db-n1-*`,
  `db-perf-optimized-N-*`) when CPU use is low; the memory is halved only
  when it is little used too. Shown by `--rightsize` (`settings.tier`).
- `run-min-instances-unused` / `functions-min-instances-unused`: warm
  instances of a service or function that observed fewer than 10,000 requests
  a month. Shown by `--rightsize` (`min_instance_count = 0`). Cloud Run
  instances running without requests count as 0 requests.
- Cloud SQL instances whose name has a random suffix are matched by
  elimination (the only unnamed instance of the code and the only unclaimed
  instance in the project).
- Upsizing: `compute-upsize` and `sql-upsize` suggest more capacity (twice
  the vCPUs, more memory) for a VM or Cloud SQL instance whose metrics show
  it is saturated. Shown as "costs $X/mo more" by `--rightsize`,
  never counted in the potential savings.
- Cloud Run and Cloud Functions CPU and memory use in observed usage, and
  `run-rightsize-cpu` / `run-rightsize-memory` (`functions-…`): smaller or
  larger limits within the shapes Cloud Run allows, shown by `--rightsize`.
- BigQuery dataset sizes from the tables API (dataset-level BigQuery Metadata
  Viewer) where `INFORMATION_SCHEMA` cannot be read, so the billing model can
  still be compared exactly; with Cloud Monitoring (`stored_bytes`, logical
  bytes) as the last resort, real storage costs and the break-even
  compression ratio.
- `terraform_remote_state` with the `gcs` backend is read from the state
  bucket (Storage Object Viewer): project ids and other outputs of another
  configuration are known, so observed usage and rightsizing work on code
  that takes its project ids from a remote state. Outputs are kept in memory
  only, sensitive outputs stay unknown. `--no-remote-state`,
  `COSTERRA_NO_REMOTE_STATE`, `remote_state: false`.
- `--impersonate-service-account` (`COSTERRA_IMPERSONATE_SERVICE_ACCOUNT`,
  `impersonate_service_account:`): every OAuth call as a service account;
  caches are kept per account, and a failed impersonation is reported.
- CI templates: `COSTERRA_IMPERSONATE_SERVICE_ACCOUNT`.
- CI templates: `COSTERRA_RECOMMENDATIONS` (default `true`) puts the
  recommendations in the merge / pull request comment; set it to `false` to
  leave them out.
- `bq-logical-storage`: switch a dataset billed on physical storage back to
  logical storage when its data does not compress enough. Both billing model
  rules come with the code change, so `--rightsize` shows them (adding
  `storage_billing_model` when it is not set).
- `gcs-cross-region`: a bucket used from a region its location does not
  serve (read by a Cloud Run service, VM, function… of the code in another
  region) is billed for inter-region data transfer; with observed usage
  (`storage/network/sent_bytes_count`), an upper bound of that cost.
- Homebrew: `brew install echaouchna/tap/costerra`; the release workflow
  publishes the formula to the tap (repository variable `HOMEBREW_TAP`,
  secret `HOMEBREW_TAP_TOKEN`).
- `just profile` (CPU profile with samply, from `mise.toml`) and `just mem`
  (peak memory), with a `profiling` Cargo profile.
- `--details`: everything at once (recommendations, free tiers, zero-cost
  resources, warnings, price notes, example usage per resource,
  recommendation sources, notes about unavailable data sources).
- `--free-tier`: show the free tiers of the services used.
- Recommendations carry the attribute change that applies them (`change` in
  JSON) when it is a single value of the code.

### Changed
- Recommendations are opt-in: `--recommendations` (or
  `COSTERRA_RECOMMENDATIONS=1`, or `--details`). Without it they are neither
  computed nor shown, and Google Cloud Recommender is not queried.
  `--no-recommendations` is still accepted.
- Free tiers are shown only with `--free-tier`, `--include-free-tier` or
  `--details` (also in JSON and Markdown).
- Quieter default output: price notes, per-resource example usage, the
  contract price hint, recommendation sources, notes about data sources and
  the contract price refresh summary are shown with `--details` (or `-v`).
  At most 5 warnings of a kind are printed (all with `--details`).
- `bq-physical-storage` is a low (not informational) recommendation when its
  savings are computed from sizes, so it counts in the potential savings.
- Long recommendation titles wrap on narrow terminals.
- Much faster and lighter: references such as `local.x` or `var.y[k]` no
  longer copy the containers they go through (a 108,000-resource repository
  went from 19 s to 0.7 s); prices are decoded only for the regions in use,
  once per service and region (Compute Engine keeps its 4.8 MB decompressed
  list instead of ~20 MiB of decoded SKUs); repeated SKU strings are shared.
  A typical run takes about half the time and two thirds of the memory of
  0.2.0.

## [0.2.0] - 2026-10-18

### Added
- Observed usage: when the credentials allow it, the last 30 days of Cloud
  Monitoring metrics (VM CPU and uptime, bucket and Cloud SQL storage, Cloud
  Run requests) fill the usage keys the usage file does not set, and active
  Compute Engine commitments are taken into account. Optional, cached for
  6 hours; `--no-observed`, `COSTERRA_NO_OBSERVED` or `observed_usage: false`
  turn it off. A `Usage` line shows how many resources were observed, and
  notes say which permissions are missing.
- Data-based rules: `compute-idle`, `compute-rightsize` and
  `compute-schedule-nonprod`.
- Every recommendation says what it is based on (`basis` in JSON: `code`,
  `usage_file` or `observed`).

### Changed
- Commitment advice leaves out vCPUs already covered by active commitments,
  and cheaper machine families are not proposed for VMs whose CPU is saturated.
- A Google Cloud Recommender recommendation replaces costerra's advice of the
  same kind for the same resource (rightsizing, idle resources, commitments).
- The potential savings total includes Google recommendations matched to the
  code (best option per resource).

### Removed
- Rabbit (followrabbit.ai) integration: saved recommendations, the AI cost
  review (`--rabbit-review`, `--rabbit-review-dry-run`,
  `COSTERRA_RABBIT_REVIEW`) and `.costerraignore`. The `rabbit_recommendations`
  and `rabbit_review` keys of `costerra.yml` are now rejected as unknown keys:
  remove them.

## [0.1.0] - 2026-10-18

First public release.

### Added

#### Estimates
- Static evaluation of Terraform and Terragrunt (variables, tfvars, locals,
  `count` / `for_each`, dynamic blocks, ~90 built-in functions, local and
  remote modules, Terragrunt includes and dependencies) and of plan JSON, with
  no `terraform init` and no credentials.
- 122 priced Google Cloud resource types, about 300 more known to be free.
- Prices from the Cloud Billing Catalog API, cached locally; contract prices of
  your billing account (detected automatically), refreshed in the background.
- Sustained use discounts, licenses, GPUs, tiered rates, Spot pricing and
  regional differences; free tiers listed per service (region-aware).
- Usage files (Infracost-compatible) with warnings for unknown keys and
  addresses matching no resource; `usage init` template generator; small /
  medium / large example usage profiles.
- One project per environment tfvars file (`environments/*.tfvars`…).

#### Recommendations
- 120+ optimization rules with estimated monthly savings (Spot for non-prod,
  machine families, committed use discounts, idle resources, lifecycle
  policies, cheaper regions…); the savings total counts the best option per
  resource.
- Usage-based recommendations from Google Cloud Recommender (paginated) and
  Rabbit (experimental), matched to the resources in the code.
- Opt-in Rabbit AI cost review (`--rabbit-review`): git-ignored, hidden and
  excluded files (`exclude_paths`, `.costerraignore`) are never uploaded,
  secret values are redacted, files over the upload budget are reported, and
  `--rabbit-review-dry-run` shows what would be sent. Credentials embedded in
  git remote URLs are never sent, cached or printed.

#### Outputs and CI
- Terminal table (identical instances grouped as `name[*] ×N`, `--no-group`),
  JSON (with `plan_diff` for plans) and Markdown merge / pull request comments.
- Cost diffs against a git ref (read-only, with `git archive`) or a saved
  report; policies (`--fail-above`, `--fail-increase-above`,
  `--fail-increase-percent`) with exit code 2.
- Strict `costerra.yml` (unknown keys and invalid globs are errors); YAML 1.2
  semantics, as in Terraform's `yamldecode`.
- `pricing sync --if-older-than` for CI caches; boolean environment variables
  accept `1`/`0`/`yes`/`no`.
- Ready-to-use CI jobs for GitLab (`ci/gitlab-ci.yml`) and GitHub
  (`ci/github-actions.yml`) that create or update one cost comment.
- Container image: multi-arch, non-root, Wolfi base with git, curl and jq.
- Release binaries for Linux (x86_64, aarch64) and macOS (Intel, Apple
  silicon), with checksums and build provenance.

#### Project
- Apache License 2.0; contributing guide, security policy and code of conduct.
- Unit, golden-value (committed price list, no network) and end-to-end CLI
  tests; minimum supported Rust version 1.89.
- GitHub Actions: lint, tests on Linux x86_64 / arm64 and macOS, MSRV check,
  cargo-deny, osv-scanner, trivy, CodeQL, image publishing, releases with
  crates.io trusted publishing, weekly advisory and catalog drift checks;
  Dependabot.

[Unreleased]: https://github.com/echaouchna/costerra/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/echaouchna/costerra/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/echaouchna/costerra/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/echaouchna/costerra/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/echaouchna/costerra/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/echaouchna/costerra/releases/tag/v0.1.0
