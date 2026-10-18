<p align="center">
  <img src="site/assets/costerra-icon-v2-256.png" alt="costerra — a C-shaped mountain ridge sculpted into terracotta and charcoal terrain" width="144" height="144">
</p>

# costerra

[![CI](https://github.com/echaouchna/costerra/actions/workflows/ci.yml/badge.svg)](https://github.com/echaouchna/costerra/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/costerra.svg)](https://crates.io/crates/costerra)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![MSRV 1.89](https://img.shields.io/badge/rust-1.89%2B-orange.svg)](Cargo.toml)
[![Website](https://img.shields.io/badge/website-costerra.echaouchna.dev-e5482f.svg)](https://costerra.echaouchna.dev/)

Fast cost estimates and optimization insights for **Google Cloud** Terraform
and Terragrunt code. costerra is written in Rust and is an alternative to
Infracost that focuses on GCP.

* **Fast.** No `terraform init` and no credentials are needed to evaluate
  code. HCL is evaluated statically, in parallel. A repository with 15
  projects and 350 resources is analysed in about 60 ms, and 5,000 resources
  in about 100 ms.
* **Accurate.** Prices come from the official Cloud Billing Catalog API
  (85k SKUs, cached locally in 2 MiB). Sustained use discounts, licenses, GPUs,
  tiered rates, Spot pricing and regional differences are applied.
* **Actionable.** More than 120 optimization rules, each with an estimated
  monthly saving: Spot for non-prod, machine family changes, committed use
  discounts, HA in non-prod, idle IPs and disks, lifecycle policies, cheaper
  regions, and more.
* **Made for CI.** Cost diffs against a git ref, Markdown comments for merge
  requests, JSON output, and policy thresholds that set the exit code.

```
Project: live/prod/app (terragrunt)

 Name                                                              Monthly Qty  Unit            Monthly Cost

 google_compute_instance.app[*] ×4                                                                  $1,012.47
 ├─ Instance usage (n2-standard-8, on-demand)                            2,920  hours                $999.27
 │     includes 20% sustained use discount
 └─ Boot disk (pd-balanced, 30 GiB)                                        120  GiB-months            $13.20
 …
💡 Recommendations (6) — potential savings $336.06/month (best option per resource: alternatives are not added up)

 [MEDIUM] Use Spot VMs for this non-production instance  saves ~$175.10/mo
       live/dev/app: google_compute_instance.app[*] (2 instances)
 [LOW] Switch n2-standard-8 to e2-standard-8  saves ~$137.60/mo
       live/prod/app: google_compute_instance.app[*] (4 instances)
```

## Installation

* **From [crates.io](https://crates.io/crates/costerra)** (Rust 1.89 or newer):

  ```sh
  cargo install costerra
  ```

* **Release binaries** (Linux x86_64 / aarch64, macOS Intel / Apple silicon):
  download a `costerra-<version>-<target>.tar.gz` from the [releases page](https://github.com/echaouchna/costerra/releases),
  check it against `SHA256SUMS` (and its build provenance with
  `gh attestation verify <file> --repo echaouchna/costerra`), and put `costerra` on
  your `PATH`.
* **From source** (Rust 1.89 or newer), to track the latest `main`:

  ```sh
  cargo install --locked --git https://github.com/echaouchna/costerra     # or, from a clone:
  mise install && just install                      # toolchain from mise.toml, then cargo install --locked --path .
  ```

* **Container image** (80 MB, multi-arch, non-root, Wolfi base with git, curl
  and jq), on GitHub Container Registry (`ghcr.io/echaouchna/costerra`):

  ```sh
  docker run --rm -v "$PWD:/work" -v "$HOME/.cache/costerra:/home/nonroot/.cache/costerra" \
    ghcr.io/echaouchna/costerra breakdown .
  just docker-build && just docker-run breakdown .     # build and run it locally
  ```

## Quick start

```sh
# 1. Download prices (uses Application Default Credentials / gcloud, or an API key)
costerra pricing sync                     # or: COSTERRA_API_KEY=... costerra pricing sync

# 2. Estimate a Terraform or Terragrunt repository (or a subdirectory)
costerra breakdown path/to/infra

# 3. Compare with the main branch
costerra diff path/to/infra --base origin/main
```

If no pricing catalog exists, `breakdown` downloads one automatically.
Prices, detected billing accounts and downloaded modules are cached in
`~/.cache/costerra` (or `$XDG_CACHE_HOME/costerra`). Change it with
`--cache-dir <dir>`, `COSTERRA_CACHE_DIR` or `cache_dir:` in `costerra.yml`.
The catalog is split per service and decoded lazily, so a run only loads the
prices it needs.

## Inputs

| Input | How it is read |
|---|---|
| Terraform directories | Static HCL evaluation: variables (incl. `optional()` defaults), `*.tfvars`/`*.auto.tfvars`, `--var-file`, `--var`, locals, `count`/`for_each`, dynamic blocks, ~90 built-in functions, local and remote modules, provider default region/zone |
| Terragrunt repositories | `include` (shallow/deep merge, `expose`), `read_terragrunt_config`, `find_in_parent_folders`, `path_relative_to_include`, `get_repo_root`, … `dependency` outputs are evaluated statically from the other unit, with `mock_outputs` as fallback. `generate` blocks (e.g. provider region) are honoured. `run_cmd` is disabled unless `--allow-run-cmd` |
| Plan JSON | `terraform show -json plan.out > plan.json && costerra breakdown plan.json`. The most accurate mode; it shows the diff between prior state and planned state |
| Mixed repositories | Terragrunt units and plain Terraform root modules are discovered together. Modules used by other code are not reported as projects |

When a Terraform root module keeps its variables in per-environment files
(`environments/`, `envs/`, `env/`, `vars/` or `tfvars/` containing
`*.tfvars`) and no `--var-file` is given, costerra evaluates one project per
environment, e.g. `30-network (dev)` and `30-network (prod)`. Disable this with
`--no-env-discovery`.

Values that are only known after `terraform apply`, such as
`terraform_remote_state` outputs, data sources and computed attributes, are
unknown to static evaluation. `for` expressions filtered by such values keep
their items (conservative for cost). A `count` / `for_each` that stays unknown
is estimated as one instance named `["<unknown>"]`, with a warning. Use a plan
JSON for exact results.

Remote modules (Terraform registry, `git::`, GitHub, archives, `tfr://`) are
downloaded once into the cache, using your git credentials. `.terraform/modules`
is used when present. Use `--offline` to disable downloads.

## Usage-based costs

Storage, requests, egress and similar costs depend on usage. Without estimates
they are shown as `usage-based` with their unit price (e.g. `$0.026` per `GiB-months`).
Generate a usage file with the keys relevant to your resources and fill it in:

```sh
costerra usage init path/to/infra     # writes costerra-usage.yml (--force to overwrite)
```

`usage init` evaluates the code with the same options as `breakdown`
(`costerra.yml`, `--var-file`, `--var`, `--exclude`).

```yaml
version: 0.1
resource_type_default_usage:
  google_storage_bucket:
    storage_gb: 100
resource_usage:
  google_storage_bucket.datalake:
    storage_gb: 20000
    monthly_class_a_operations: 1000000
  module.api.google_cloud_run_v2_service.this:
    monthly_requests: 50000000
    average_request_duration_ms: 120
  "module.workers[*].google_compute_instance.this":   # wildcards
    monthly_hrs: 200
```

### Example usage profiles

Without estimates, costerra still shows what usage-based resources *could*
cost, using three built-in example profiles: **small** (dev / low traffic),
**medium** (typical production service) and **large** (high traffic /
data-heavy):

```
 google_storage_bucket.assets                                                   usage-based
     ≈ with example usage: small $0.43 · medium $9.00 · large $90.00
 …
 OVERALL TOTAL (monthly)                                                              $0.12
 With usage  small $0.72 · medium $15.21 · large $151.00  (example usage, see `costerra usage profiles`)
```

* `costerra usage profiles [--type google_storage_bucket]` lists the assumed
  values (e.g. `storage_gb` 10 / 100 / 1,000 GiB, `monthly_requests` 100k / 5M /
  50M).
* `--usage-profile medium` (or `COSTERRA_USAGE_PROFILE`) prices every usage key
  that has no estimate with that profile, so totals, diffs and policies include
  plausible usage costs.
* Real values from the usage file always take precedence. Only volume-like keys
  (storage, requests, operations, egress, …) have example values;
  configuration overrides such as `instances` or `monthly_hrs` keep the model
  defaults.
* `costerra usage init` writes the example values next to each key as a hint.

The format is compatible with Infracost usage files. costerra looks for
`costerra-usage.yml` in the analysed directory and its parents up to the
repository root, and also in each project directory, where it overrides the
global file. `-u/--usage-file` sets the file explicitly.

Entries that have no effect are reported as warnings: unknown top-level keys,
addresses that match no resource (`google_compute_instanc.web`), and keys the
resource type does not use (`monthly_hrz`), with the list of valid keys.

## Outputs and CI

```sh
costerra breakdown . --format json --out report.json
costerra breakdown . --format markdown
costerra diff . --base origin/main --format markdown --out comment.md
costerra diff . --baseline report.json            # compare with a saved report
costerra breakdown plan.json                       # plan JSON includes the diff (`plan_diff` in JSON)
```

`diff --base` reads the base ref with `git archive`: it never writes to the
repository, so it also works on read-only checkouts and in containers.

In the table, identical instances of a counted resource are grouped as
`name[*] ×N` with summed quantities and costs; `--no-group` lists every
instance. The potential savings total counts the best option per resource
(Spot, a cheaper machine family and a commitment for the same VM are
alternatives), in the table, the Markdown comment and the JSON report alike.

Policies return exit code **2** when violated:

```sh
costerra breakdown . --fail-above 10000
costerra diff . --base origin/main --fail-increase-above 500 --fail-increase-percent 10
```

`ci/gitlab-ci.yml` is a ready-to-use GitLab job that posts (and updates) a cost
comment on merge requests. It runs the published container image (set
`COSTERRA_IMAGE` to pin a version), re-downloads prices only when the cached
catalog is older than 7 days (`pricing sync --if-older-than 7d`), and finds its
previous comment on any page of the merge request notes:

```yaml
include:
  - remote: https://raw.githubusercontent.com/echaouchna/costerra/main/ci/gitlab-ci.yml   # or a tag
```

For GitHub, [`ci/github-actions.yml`](ci/github-actions.yml) is the
equivalent workflow: copy it to `.github/workflows/costerra.yml`. It runs the
container image and creates or updates one pull request comment.

Boolean environment variables (`COSTERRA_NO_REFRESH`, `COSTERRA_NO_INSIGHTS`,
`COSTERRA_RABBIT_REVIEW`) accept `1`/`0`, `true`/`false`, `yes`/`no`.

Colors: `--color auto|always|never` (also `NO_COLOR` and `CLICOLOR_FORCE`).
Expensive items are highlighted: yellow above $100/month, red above $1,000/month.
Missing prices are shown in red, and diffs show increases in red and decreases
in green.

## Usage-based recommendations (Google Cloud Recommender, Rabbit)

Besides its own rules (static analysis of the code), costerra shows
recommendations based on **observed usage**:

* **Google Cloud Recommender** (Active Assist): VM rightsizing, idle VMs,
  disks, IPs and images, idle and overprovisioned Cloud SQL, GKE, BigQuery
  partitioning, and committed use discounts for the billing account. Only the
  recommenders and locations matching resources in the code are queried.
* **Rabbit** ([followrabbit.ai](https://followrabbit.ai)): open
  recommendations saved for the repository (rightsizing, idle resources,
  commitments), when an API key is available (`FOLLOWRABBIT_API_KEY`, or the
  `followrabbit auth login --key …` credentials).

### Rabbit API key

Create the key on the Rabbit agentic subscriptions site:
**[subscriptions.agentic.followrabbit.ai](https://subscriptions.agentic.followrabbit.ai)**.
Sign in, then use *Generate API keys for the followrabbit CLI*. Keys look like
`rbt_live_…`; the free plan works for costerra. The BigQuery Job Optimizer key
from `app.followrabbit.ai/api-keys` is a different key and is not used.

```sh
export FOLLOWRABBIT_API_KEY=rbt_live_…          # or: followrabbit auth login --key rbt_live_…
curl -s -H "X-Rabbit-Api-Key: $FOLLOWRABBIT_API_KEY" https://api.agentic.followrabbit.ai/cli/status
```

Rabbit stores recommendations per repository: costerra sends the `origin` git
remote URL of the analysed directory (as the `followrabbit` CLI does, then its
normalized https form, e.g. `ssh://git@gitlab.com:2222/example/infra.git` →
`https://gitlab.com/example/infra`). Credentials embedded in the remote URL, such
as the `gitlab-ci-token:…@` of GitLab CI checkouts, are always removed first:
they are never sent, cached or printed.

> **Note:** the saved recommendation list is filled by Rabbit's own review
> flows, such as the Rabbit GitHub App's PR reviews. Repositories that Rabbit
> has not reviewed, like GitLab repositories when only the GitHub App is used,
> return an empty list. costerra then shows
> "Rabbit: no open recommendations saved for <repo>". Use `--rabbit-review`
> (below) to get findings for any repository.

> **Experimental.** The Rabbit integration uses the API of the `followrabbit`
> CLI, which is not publicly documented and may change. It is only active when
> a Rabbit API key is configured.

### Rabbit AI cost review (`--rabbit-review`)

`costerra breakdown --rabbit-review` (or `rabbit_review: true`, or
`COSTERRA_RABBIT_REVIEW=1`) runs Rabbit's AI cost review, the one behind
`followrabbit costreview`, on the analysed directory:

* it **uploads the Terraform files** (`*.tf`, `*.tfvars`, `*.hcl`, up to 512 KiB)
  plus a resource index to `https://api.agentic.followrabbit.ai/cli/engine`.
  Files ignored by git (`.gitignore`, `.git/info/exclude`), hidden paths,
  `.terraform`, `.terragrunt-cache`, `node_modules`, `exclude_paths` / `--exclude`
  and paths listed in a `.costerraignore` file (gitignore syntax) are never
  uploaded;
* **secret values are redacted** before upload: literal values of attributes
  and variables named like `password`, `secret`, `token`, `api_key`,
  `private_key`, `credentials`… (including heredocs and JSON), and PEM private
  key blocks. References such as `var.db_password` are kept;
* files that do not fit in the 512 KiB budget are listed in a note (review a
  subdirectory, or exclude paths), never dropped silently;
* `--rabbit-review-dry-run` prints the files that would be uploaded and the
  number of redacted values, without sending anything;
* it **uses your Rabbit quota**, about $0.01 per review (the free plan includes
  $10 per month), and takes 10 to 30 seconds;
* findings are matched to the resources they concern (`file:line` or resource
  names) and tagged `Rabbit` with their source (model, request id, location);
* results are **cached by content hash**, so unchanged code is not reviewed or
  billed again; costerra prints what each review cost.

It is opt-in because it sends your code to a third party and is not instant.
Without `--rabbit-review`, costerra only reads the saved recommendation list
and uploads nothing. Rabbit allows 10 requests per minute per key; costerra
makes at most two per run. Saved recommendations are cached for 6 hours, AI
reviews for as long as the uploaded content is unchanged. Treat the key as a
secret: in CI, store it as a masked, protected variable.

Every recommendation shows its source: a `costerra`, `Google` or `Rabbit` tag,
and a `Source:` line (the costerra rule ID, which you can disable in
`costerra.yml`; the Google recommender, with a ready-to-run
`gcloud recommender recommendations describe …` command; or the Rabbit
recommendation type and link). The JSON report has `source` and `source_ref`,
and the MR comment has a Source column.

Recommendations are matched to the Terraform resources they concern. Others, such as billing-account commitments,
are shown in a "☁️ From your cloud usage" section, with 1-year and 3-year
options merged. Results are cached for 6 hours and fetched in parallel with
pricing (2.5 s deadline), so runs stay instant.

The Recommender API needs a quota project with the API enabled when you use user
credentials. costerra uses `--quota-project` / `quota_project:` /
`GOOGLE_CLOUD_QUOTA_PROJECT`, otherwise the `quota_project_id` of your
Application Default Credentials. Disable with `--no-insights`,
`gcp_recommendations: false` or `rabbit_recommendations: false`.

## Configuration

Optional `costerra.yml` at the repository root (see `costerra.example.yml`):

```yaml
default_region: europe-west1
currency: USD                 # requires `costerra pricing sync --currency EUR` for others
billing_account: 012345-6789AB-CDEF01   # optional: detected automatically (see "Your prices"); `none` = list prices
refresh_prices: true          # re-fetch contract prices of used SKUs on every run
usage_file: costerra-usage.yml
exclude_paths: ["examples/**"]
environments:                 # tokens matched in project paths, GCP project ids and env labels
  nonprod: [dev, test, staging, sandbox]
  prod: [prod, production]
projects:
  - match: "live/prod/**"
    env: prod
recommendations:
  disabled: [region-cheaper]
policies:
  max_monthly_cost: 50000
  max_increase_percent: 15
```

The file is strict: unknown keys (a typo such as `polices:`) and invalid glob
patterns are errors, so a misspelled policy cannot silently disable a CI gate.
YAML follows version 1.2, like Terraform: only `true` / `false` are booleans.

Some recommendations only apply outside production (Spot, single-zone, no HA,
…). The environment is detected from `env`/`environment` labels, from labels of
referenced resources (e.g. node pool → cluster), from the project path, and from
the GCP project id.

## Coverage

122 resource types are priced, and about 300 more are known to be free.

| Area | Resources |
|---|---|
| Compute Engine | instances (incl. from template), MIGs + autoscalers, disks, Hyperdisk, IPs, snapshots, images, machine images, reservations, commitments, GPUs, local SSD, Windows/RHEL/SLES licenses |
| GKE | Standard & Autopilot clusters, node pools (zones × nodes, Spot, GPUs, local SSD), Extended channel |
| Databases | Cloud SQL (MySQL, PostgreSQL, SQL Server, Enterprise/Enterprise Plus, HA, replicas), AlloyDB, Spanner (editions, multi-region), Bigtable, Memorystore (Redis, Redis Cluster, Valkey, Memcached), Firestore |
| Storage | Cloud Storage (all location types and classes, Autoclass, soft delete, operations, egress), Anywhere Cache, Filestore, Artifact Registry, Backup and DR |
| Networking | Cloud NAT, load balancer forwarding rules, Cloud Armor, Cloud CDN, VPN, Interconnect, Serverless VPC connector, Cloud DNS, Packet Mirroring, NCC, Secure Web Proxy, internet & inter-region egress |
| Serverless | Cloud Run (services, jobs, worker pools, GPUs), Cloud Functions gen1/gen2, App Engine, Pub/Sub, Scheduler, Tasks, Workflows, Eventarc, API Gateway |
| Data | BigQuery (storage, on-demand, editions, reservations, commitments, BI Engine, DTS), Dataflow, Dataproc (+ batches), Composer 1/2/3, Data Fusion, Datastream, Dataplex |
| AI / ML | Vertex AI Workbench, Colab Enterprise, legacy notebooks, Vector Search, Feature Store, prediction pools, TensorBoard, Metadata, Cloud TPU |
| Operations & security | KMS (software/HSM/external), Secret Manager, Cloud Logging buckets, Monitoring uptime checks & custom metrics, Cloud Build, Cloud Workstations, Certificate Manager, Identity Platform, Cloud Deploy, Cloud IDS |

`costerra resources` lists the supported types and their usage keys.

## Your prices (contract discounts)

costerra prices with **your** billing account when it can. It detects the
account linked to the GCP projects referenced by your code (Cloud Billing
`projects/{id}/billingInfo`), or falls back to the only open billing account
you can access. The result is cached for a day. Contract prices (negotiated
discounts, Cloud Billing Pricing API) require `billing.billingAccountPrice.get`,
which is included in the Billing Account Viewer role.

* The full contract price list is downloaded on first use and re-synced when it
  is more than 7 days old (`costerra pricing sync --billing-account <id>` does it
  explicitly).
* The prices of the SKUs used by a run are kept fresh without slowing it down:
  * prices refreshed within the last 15 minutes are used as-is
    (`--price-cache-ttl 0|90s|15m|1h`, `price_cache_ttl:` in `costerra.yml`);
  * stale prices are re-fetched in parallel while the code is evaluated, for at
    most 1.5 s; whatever is not done by then is finished by a detached
    background process, so the next run is fresh;
  * Pricing API calls use exponential backoff with jitter; on rate limiting
    (HTTP 429) costerra stops immediately, uses cached prices and pauses
    refreshes for a minute;
  * disable refreshes with `--no-refresh`, `COSTERRA_NO_REFRESH=1` or
    `refresh_prices: false`.
* Public list prices are always shown next to yours. On usage-based lines, where no monthly amount exists yet, the Public Cost column shows the public unit price (`$0.40 / 1M requests`). A legend under the table explains the columns:

```
 Name                                          Monthly Qty  Unit            Your Price  Your Cost  Public Cost
 google_compute_instance.vm                                                               $104.20      $129.81
 ├─ Instance usage (n2-standard-4, on-demand)          730  hours              $0.1368     $99.85      $124.81
 ├─ Storage (Standard, EU multi-region)                     GiB-months        $0.02262  usage-based  $0.026 / GiB-month
 …
 Pricing     contract prices of billing account 012345-6789AB-CDEF01 · saves $25.61/month (19.7%) vs list price ($129.81)
```

Override the account with `--billing-account <id>`, `COSTERRA_BILLING_ACCOUNT`
or `billing_account:` in `costerra.yml`. Use `--list-prices` (or
`billing_account: none`) for public prices only. Without permissions or
credentials (e.g. in CI with an API key), costerra silently uses list prices.

## Pricing details

* Prices come from the Cloud Billing Catalog (list prices) or, when a billing
  account is configured, from the Cloud Billing Pricing API (contract prices), in
  the chosen currency. Credits and CUDs you have already purchased are not
  deducted.
* Free tiers are removed by default because they apply per billing account, not
  per resource. Use `--include-free-tier` to keep them. The report lists the
  free tier of every service it contains (🎁 section, and in the MR comment and
  JSON), e.g. "Secret Manager: 6 active secret versions, 10,000 access
  operations and 3 rotation notifications per month".
* With contract prices, the example usage totals are shown both with your
  contract and at public list prices.
* Sustained use discounts are applied to eligible families (N1, N2, N2D, C2, M1,
  M2) for the configured monthly hours.
* `costerra pricing search '<regex>' --service 'Cloud SQL' --region europe-west1`
  explores SKUs.

## Development

Tools are pinned in `mise.toml` and commands live in the `justfile`:

```sh
mise install              # Rust 1.98 (clippy, rustfmt), just, jq, hyperfine, terraform, terragrunt, osv-scanner, trivy
just                      # list recipes
just check                # rustfmt --check, clippy (-D warnings), tests: what CI runs
just audit                # osv-scanner (dependencies) + trivy (vulnerabilities, secrets, misconfigurations)
just fixtures             # breakdown of all test fixtures
just breakdown ../infra   # breakdown of any path (extra args are passed through)
just diff . origin/main   # cost diff against a git ref
just sync-prices [BILLING_ACCOUNT]
just sku '^N2 Instance Core' 'Compute Engine' europe-west1
just bench                # hyperfine benchmark of a cached breakdown
just plan-fixture         # plan JSON fixture (needs GCP credentials)
just docker-build         # container image; just docker-scan scans it with trivy
just golden-catalog       # rebuild the golden test price list from the synced catalog
just disk / just sweep    # disk used by builds and caches / free it
```

Tests:

* **golden tests** (`tests/golden.rs`) check exact monthly costs of the
  fixtures with a small committed price list (`testdata/pricing/golden-catalog.json`):
  no network or credentials needed. When a model starts using other SKUs, run
  `just golden-catalog` and review the diff of expected values;
* **CLI tests** (`tests/cli.rs`) run the binary end to end (diff against a git
  ref, policies, config and usage file errors, Rabbit dry run…);
* **fixture tests** (`tests/fixtures.rs`) price every fixture with the live
  catalog (`just sync-prices`) to catch SKUs renamed by Google. Without a
  catalog they are skipped locally, and fail in CI unless
  `COSTERRA_SKIP_CATALOG_TESTS=1` is set. A scheduled pipeline runs them with a
  fresh catalog.

Disk usage: tests and clippy use a lean dev profile (no debug info for
dependencies, no incremental cache; `CARGO_INCREMENTAL=1` re-enables it), and
only the shipped binary is built in release. `.cargo/config.toml` cleans unused
crates from the global cargo cache. Docker builds keep cargo's registry and
target directory in BuildKit cache mounts (not image layers), and
`.dockerignore` sends only the sources to the daemon. `just sweep` removes
dev artifacts, release intermediates (the binary is kept) and BuildKit cache
mounts.

`docs/writing-resource-models.md` explains how to add a resource type or a
recommendation rule.

## Contributing

Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md), the
[code of conduct](CODE_OF_CONDUCT.md), and [SECURITY.md](SECURITY.md) to report
vulnerabilities privately. Changes are listed in [CHANGELOG.md](CHANGELOG.md).

CI runs on both GitHub Actions (`.github/workflows`: lint, tests on Linux
x86_64 / arm64 and macOS, MSRV, cargo-deny, osv-scanner, trivy, CodeQL,
multi-arch image, releases) and GitLab CI (`.gitlab-ci.yml`).

## License

costerra is licensed under the [Apache License, Version 2.0](LICENSE).
Copyright 2026 Nabil Echaouch; see [NOTICE](NOTICE).

The test price list (`testdata/pricing/golden-catalog.json`) contains Google
Cloud list prices from the public Cloud Billing Catalog API, and
`data/machine_types.tsv` describes Google Cloud machine types from public
documentation. All project names, ids and accounts in the test fixtures and
examples are fictitious. Google Cloud is a trademark of Google LLC; costerra
is an independent project, not affiliated with or endorsed by Google, Rabbit
(followrabbit.ai) or Infracost.
