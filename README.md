<p align="center">
  <img src="site/assets/costerra-icon-v2-256.png" alt="costerra — a C-shaped mountain ridge sculpted into terracotta and charcoal terrain" width="144" height="144">
</p>

# costerra

[![CI](https://github.com/echaouchna/costerra/actions/workflows/ci.yml/badge.svg)](https://github.com/echaouchna/costerra/actions/workflows/ci.yml)
[![Security scans](https://github.com/echaouchna/costerra/actions/workflows/security.yml/badge.svg)](https://github.com/echaouchna/costerra/actions/workflows/security.yml)
[![CodeQL](https://github.com/echaouchna/costerra/actions/workflows/codeql.yml/badge.svg)](https://github.com/echaouchna/costerra/actions/workflows/codeql.yml)
[![crates.io](https://img.shields.io/crates/v/costerra.svg)](https://crates.io/crates/costerra)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![MSRV 1.89](https://img.shields.io/badge/rust-1.89%2B-orange.svg)](Cargo.toml)
[![Website](https://img.shields.io/badge/website-costerra.echaouchna.dev-e5482f.svg)](https://costerra.echaouchna.dev/)

Fast cost estimates and optimization insights for **Google Cloud** Terraform
and Terragrunt code, written in Rust.

* **Fast and light.** No `terraform init` and no credentials are needed to
  evaluate code. HCL is evaluated statically, in parallel, and only the prices
  of the regions in use are decoded. A repository with 15 projects and 350
  resources is analysed in about 25 ms; 885 projects and 108,000 resources
  take under a second.
* **Accurate.** Prices come from the official Cloud Billing Catalog API
  (85k SKUs, cached locally in 2 MiB). Sustained use discounts, licenses, GPUs,
  tiered rates, Spot pricing and regional differences are applied.
* **Actionable.** More than 120 optimization rules, each with an estimated
  monthly saving: Spot for non-prod, machine family changes, committed use
  discounts, HA in non-prod, idle IPs and disks, lifecycle policies, cheaper
  regions, and more (`--recommendations`).
* **Real billing next to the estimate.** `--billing-history` charts what
  the resources of your code (not the whole projects) were actually billed,
  week by week over the last 3 months, stacked by service, from the Cloud
  Billing export to BigQuery.
* **Rightsizing from real usage.** With access to the metrics, `--rightsize`
  says which resources are oversized or saturated and why, which line of the
  code sets the value, the cost after the change, and links to the console to
  check the metrics. It never edits the code.
* **Made for CI.** Cost diffs against a git ref, Markdown comments for merge
  requests, JSON output, and policy thresholds that set the exit code.

```
$ costerra breakdown live --recommendations
Project: live/prod/app (terragrunt)

 Name                                                              Monthly Qty  Unit            Monthly Cost

 google_compute_instance.app[*] ×4                                                                  $1,012.47
 ├─ Instance usage (n2-standard-8, on-demand)                            2,920  hours                $999.27
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

* **With [Homebrew](https://brew.sh)** (macOS, Linux), from the
  [echaouchna/tap](https://github.com/echaouchna/homebrew-tap) tap:

  ```sh
  brew trust echaouchna/tap
  brew install echaouchna/tap/costerra
  ```

  The formula installs the bash, zsh and fish completions.

* **Release binaries** (Linux x86_64 / aarch64, macOS Intel / Apple silicon):
  download a `costerra-<version>-<target>.tar.gz` from the [releases page](https://github.com/echaouchna/costerra/releases),
  check it against `SHA256SUMS` (and its build provenance with
  `gh attestation verify <file> --repo echaouchna/costerra`, or offline with the
  release's `costerra-<version>.intoto.jsonl`: add `--bundle <that file>`), and put
  `costerra` on your `PATH`.
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

### Shell completions

`costerra completions <shell>` prints the completion script for bash, zsh,
fish, elvish, powershell, nu or xonsh (Homebrew installs bash, zsh and fish):

```sh
source <(costerra completions bash)                                        # ~/.bashrc
costerra completions zsh > "${fpath[1]}/_costerra"                         # zsh
costerra completions fish > ~/.config/fish/completions/costerra.fish       # fish
costerra completions nu | save -f ($nu.data-dir | path join completions costerra.nu)  # nu, then `use` it in config.nu
execx($(costerra completions xonsh))                                       # ~/.xonshrc
```

## Quick start

```sh
# 1. Download prices (uses Application Default Credentials / gcloud, or an API key; see Permissions)
costerra pricing sync                     # or: COSTERRA_API_KEY=... costerra pricing sync

# 2. Estimate a Terraform or Terragrunt repository (or a subdirectory)
costerra breakdown path/to/infra

# 3. Compare with the main branch
costerra diff path/to/infra --base origin/main

# 4. Optimize
costerra breakdown path/to/infra --recommendations    # optimization advice
costerra breakdown path/to/infra --rightsize          # from observed usage: why, where, cost after, console link
```

The default output is the cost table and totals. Add what you need:

| Flag | Shows |
|---|---|
| `--recommendations` (`COSTERRA_RECOMMENDATIONS=1`) | optimization recommendations and the potential savings |
| `--free-tier` | the free tiers of the services used (`--include-free-tier` also deducts them) |
| `--show-all` | zero-cost resources and per-resource warnings |
| `--details` | all of the above, plus price notes (sustained use discounts…), example usage per resource, the source of each recommendation, every warning, and notes about data sources that could not be read |
| `--rightsize` | [rightsizing in the code](#rightsizing-the-code---rightsize) |

The same flags decide what the JSON and Markdown reports contain.

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

**Terraform states.** costerra reads states, read only, so that what only a
`terraform plan` would know is known without `init` nor `plan`:

* the outputs of `terraform_remote_state` data sources (project ids, networks…
  taken from another configuration);
* the data sources of the configuration itself (Azure AD group members,
  existing projects…), from its own state, like `terraform plan` reads them
  before expanding what is built from them: a `for_each` over the members of
  a group is known. A data source is taken from the state only when its
  arguments are the ones of the state (otherwise it would read something
  else); its values are the ones of the last apply or refresh;
* of the managed resources of the own state, only the ids Google assigned
  (`uid` of Cloud Workstations): the billing export names their VMs and disks
  by them;
* of remote states, the workstation configs and clusters, for lookups only
  (never priced): a workstation declared here is priced with the machine type
  and disks of its config in the cluster's configuration.

Every backend type is read:

| Backend | How |
|---|---|
| `gcs` | Cloud Storage API, with your Google credentials (Storage Object Viewer on the bucket) |
| `local` (also without a backend block) | the state file (`terraform.tfstate`, `terraform.tfstate.d/<workspace>/…`) |
| `s3` | `aws s3 cp` (AWS CLI: profiles, SSO, assumed roles; `region`, `profile`, `endpoint` of the backend) |
| `azurerm` | `az storage blob download` (Azure CLI login, or `sas_token` / `access_key`, `ARM_SAS_TOKEN` / `ARM_ACCESS_KEY`) |
| `http` | GET of `address` (basic auth from `username` / `password` or `TF_HTTP_*`) |
| `remote`, `cloud` (HCP Terraform, Terraform Enterprise) | the API, with the token of `TF_TOKEN_<host>` or `terraform login` |
| any other (`pg`, `consul`, `kubernetes`…) | `terraform state pull` (or `tofu`, or `COSTERRA_TERRAFORM_BIN`) in the directory, when it is initialized for that state |

The own state is found like `terraform init` does: the backend block (the
default local backend when there is none), completed by the backend
configuration file of the environment when the block is partial
(`backends/<env>.tfvars`, `backend/<env>.hcl`, `<env>.tfbackend`, … as passed
to `terraform init -backend-config=…`; the environment is the one discovered,
or the name of a `--var-file`: `--var-file environments/prod.tfvars` →
`backends/prod.tfvars`). When the directory was initialized for that same
state, the configuration it was initialized with
(`.terraform/terraform.tfstate`, including `-backend-config=key=value`) and
the workspace it selected are used. The workspace is `--workspace` or
`TF_WORKSPACE`, else the one selected, else `default`. A Terragrunt unit's
state is the one of its `remote_state` block.

When a state cannot be read and something stays unknown because of it, a
warning (`--details`) says which state and why. Only what is listed above is
kept, in memory, for the run: never other managed resources, sensitive outputs
or attributes, nor data sources that hold secrets (secret versions, tokens,
keys); nothing is printed or cached. Turn it all off with `--no-remote-state`,
`COSTERRA_NO_REMOTE_STATE=1` or `remote_state: false` (and `--offline`).

Values that are only known after `terraform apply`, such as computed
attributes, and data sources not in the state yet, are unknown to static evaluation. `for` expressions filtered by such values keep
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

### Observed usage

When your credentials allow it, costerra also reads how the deployed resources
were used over the **last 30 days**, and fills the usage keys your usage file
does not set (a usage file value always wins):

| Resource | Observed in | Usage key |
|---|---|---|
| `google_compute_instance` | CPU utilization (Cloud Monitoring) | `monthly_hrs` (hours it ran), CPU mean and 95th percentile |
| `google_storage_bucket` | `storage/total_bytes`; `network/sent_bytes_count` (for cross-region reads) | `storage_gb` |
| `google_sql_database_instance` | `database/disk/bytes_used`, `database/cpu/utilization`, `database/memory/utilization` | `storage_gb`, CPU and memory mean and 95th percentile |
| `google_cloud_run_v2_service`, `google_cloud_run_service`, `google_cloudfunctions2_function` | `request_count` (and `container/instance_count`: instances running without requests count as 0 requests); `container/cpu/utilizations` and `container/memory/utilizations` (share of the limits, hourly 95th percentile of the busiest revision) | `monthly_requests`, CPU and memory use |
| on-demand VMs | active Compute Engine commitments | share of vCPUs already committed |
| `google_bigquery_dataset` | `INFORMATION_SCHEMA.TABLE_STORAGE` (current sizes) | `storage_gb`, `long_term_storage_gb`, `physical_storage_gb`, `long_term_physical_storage_gb`, `time_travel_storage_gb`, `fail_safe_storage_gb` |
| `google_bigquery_dataset`, without access to `INFORMATION_SCHEMA` | the tables API (`tables.list` / `tables.get`, dataset-level permission): the same bytes, fail-safe excepted | as above |
| `google_bigquery_dataset`, without access to either | `storage/stored_bytes` (Cloud Monitoring) | `storage_gb` (logical bytes) |

Resources are matched by GCP project and `name` (`dataset_id` for datasets),
so names computed at apply time are skipped. A Cloud SQL instance whose name
ends with a random suffix (`random_string`, `random_id`) is still matched when
it is the only unnamed instance of the code in its project and exactly one
instance of that project is not claimed by another resource of the code. BigQuery sizes are read with one
metadata query per project and dataset location, billed as the 10 MB query
minimum (a fraction of a cent). Values are scaled to a month over the time the resource has
existed (at least 3 days of data). A `Usage` line under the table says how many
resources were observed. Results are cached for 6 hours; the first run waits
at most 3 seconds for them. Sources that cannot be read are skipped: the run
never fails because of them, and `--details` (or `--rightsize`) shows a note
saying which role is missing. Turn it off with `--no-observed`,
`COSTERRA_NO_OBSERVED=1` or `observed_usage: false`. See
[Permissions](#permissions) for the roles.

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
costerra diff . --base origin/main --format table,markdown:comment.md   # table in the log, comment in a file
costerra diff . --baseline report.json            # compare with a saved report
costerra breakdown plan.json                       # plan JSON includes the diff (`plan_diff` in JSON)
```

`--format` takes `FORMAT[:FILE]` and can be repeated or comma-separated, so
one run can print the table and write the Markdown and JSON reports. At most
one format goes to stdout, or to `--out`. A table written to a file has no
colors.

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

Both include the optimization recommendations in the comment
(`COSTERRA_RECOMMENDATIONS: "true"`); set the CI/CD variable (GitLab) or
repository variable (GitHub) `COSTERRA_RECOMMENDATIONS` to `false` to leave
them out. Set `COSTERRA_IMPERSONATE_SERVICE_ACCOUNT` to run the Google API
calls as a service account (for example the one that can read your state
buckets and metrics).

Boolean environment variables (`COSTERRA_RECOMMENDATIONS`, `COSTERRA_NO_REFRESH`,
`COSTERRA_NO_INSIGHTS`, `COSTERRA_NO_OBSERVED`) accept `1`/`0`, `true`/`false`, `yes`/`no`.

Colors: `--color auto|always|never` (also `NO_COLOR` and `CLICOLOR_FORCE`).
Expensive items are highlighted: yellow above $100/month, red above $1,000/month.
Missing prices are shown in red, and diffs show increases in red and decreases
in green.

## Rightsizing the code (`--rightsize`)

With observed usage (see [Permissions](#permissions)), `--rightsize` lists
the resources that are oversized (cheaper to run smaller) or saturated (short
of CPU or memory), with:

* **Why**: the observed metrics and the threshold they crossed;
* **Where**: the line of the code that sets the value, with the suggested
  change; all candidate lines when several could set it;
* **Note**: what to know before changing it (a value shared with other
  resources, a module default, a downloaded module…);
* **Check**: a link to the resource in the Google Cloud console, to see the
  metrics for yourself;
* **Cost**: the monthly cost before and after the change.

```
📐 Rightsizing from observed usage (1)

 powerbi-vm (dev): google_compute_instance.pbi_vm_per_bu[*] (4 instances)
    machine_type  n2-standard-4 → n2-standard-2
    Why     CPU use over 28 days: mean 8.0%, 95th percentile 22.3% (suggested below a mean of 20% and a 95th
            percentile of 35%). Half the vCPUs would still leave headroom. Check memory use first (it needs the
            Ops Agent to be measured).
    Where   vm.tf:14
            - machine_type = "n2-standard-4"
            + machine_type = "n2-standard-2"
    Check   https://console.cloud.google.com/compute/instancesDetail/zones/europe-west1-b/instances/powerbi-vm-affi?project=…&tab=monitoring
    Cost    $118.45 → $85.11/month  saves $33.34/mo

 Total after rightsizing $154.76 → $121.42/month  (−$33.34/month)
 costerra never edits the code: check the metrics in the console, then change the lines above.
```

| Change | When |
|---|---|
| VM `machine_type` with half the vCPUs | CPU mean below 20% and 95th percentile below 35% over at least 7 days |
| VM `machine_type` with twice the vCPUs (costs more) | CPU 95th percentile at 85% or more: the VM is short of CPU |
| BigQuery `storage_billing_model = "PHYSICAL"` | the observed compressed bytes, time travel and fail-safe included, cost less than the logical bytes |
| BigQuery `storage_billing_model = "LOGICAL"` | the data does not compress enough to pay for the higher physical rate |
| Cloud SQL `settings.tier` with half the vCPUs (custom, legacy `db-n1-*`, `db-perf-optimized-N-*`) | CPU mean below 20% and 95th percentile below 35% over at least 7 days; the memory is halved too only when its 95th percentile is below 50% (custom tiers keep it otherwise, within 6.5 GiB per vCPU) |
| A larger Cloud SQL `settings.tier` (costs more) | CPU 95th percentile at 85% or more (twice the vCPUs), memory 95th percentile at 90% or more (twice the memory, or `db-n1-highmem-*`) |
| Cloud Run / Cloud Functions CPU (`limits.cpu`, `available_cpu`): the next smaller or larger size of 1, 2, 4, 6, 8 vCPUs | smaller: CPU 95th percentile below 30% and peak below 50%; larger: 95th percentile at 80% or more. The memory must stay within what that CPU allows |
| Cloud Run / Cloud Functions memory (`limits.memory`, `available_memory`): half or twice | half: even the peak is below 40%; twice: peak at 90% or 95th percentile at 80% (instances that run out of memory are restarted) |
| Cloud Run / Cloud Functions `min_instance_count = 0` | fewer than 10,000 requests a month were observed while min instances are kept warm |

costerra finds the line that sets the value: a literal in the resource block,
a Terragrunt `inputs` entry, a `.tfvars` file (the one of the environment for
[per-environment projects](#inputs)), a variable or `optional()` default of
the root module, or a parent Terragrunt file. An attribute that is not set
goes into the resource block. Only a value assigned to a key about the
attribute counts (not an entry of a per-environment map such as
`{ dev = "2", prod = "4" }`). When the value is computed (for example
`"${var.memory}Mi"`) or comes from a module input, no line is shown.

Changes that cost more (more capacity for a saturated resource) are shown
as "costs $X/mo more" and counted in the total after rightsizing; they are
never part of the potential savings.

costerra never edits the code: check the metrics in the console, then make
the change (a smaller size can be tried in a non-production environment
first). The BigQuery billing model can only be changed once every 14 days.

`--rightsize` works with `--format json` (`rightsizing` key) and
`--format markdown`. It needs observed usage, so it cannot be used with
`--offline` or `--no-observed`.

## Actual billing (`--billing-history`)

`--billing-history` (on `breakdown` and `diff`) adds a chart of what the
resources of the code were really billed (your stack only, not everything
else running in the same GCP projects), week by week over the last 3 months
(13 weeks, the current one in progress), stacked by service, with the weekly
equivalent of the estimate as a dashed line:

```
$ costerra breakdown live --billing-history
…
📊 Actual billing, last 13 weeks (net of credits, USD)
   resources of the code only: 29 of 31 resources found in the billing
   no cost found for: acme-prod/acme-tmp, acme-dev/web-cache
   from acme-billing.billing.gcp_billing_export_resource_v1_012345_6789AB_CDEF01

                                                      2.8k 2.8k 2.9k
 $3.0k ┤                               2.5k 2.6k 2.6k ▂▂▂▂ ▂▂▂▂ ▅▅▅▅
       │           2.2k 2.3k 2.4k 2.4k ▂▂▂▂ ▆▆▆▆ ▇▇▇▇ ████ ████ ████
       │╌2.0k╌2.1k╌▄▄▄▄╌▄▄▄▄╌▇▇▇▇╌████╌████╌████╌████╌████╌████╌████╌1.9k
       │ ▅▅▅▅ ████ ████ ████ ████ ████ ████ ████ ████ ████ ████ ████ ▄▄▄▄
       …
    $0 └─────────────────────────────────────────────────────────────────
         27   3    10   17   24   31   7    14   21   28   5    12  now
        Jul  Aug                      Sep                 Oct

   ██ Compute Engine     $14,715.00   46.8%
   ██ Cloud SQL           $6,854.40   21.8%
   …
   ╌╌ estimate of the code: $9,100.00/month ≈ $2,094.25/week

   Total over 13 weeks: $31,454.10
   Last full week (Oct 12): $2,870.70, ▲ 3.7% vs the week before
   Last 4 weeks vs the 4 before: ▲ 12.0%
```

In a terminal each service has its own color. Costs are net of credits
(sustained use and committed use discounts, promotions, free tier) and in the
currency of the billing account.

**One chart per environment** (`--billing-by-env`, `COSTERRA_BILLING_BY_ENV=1`;
it implies `--billing-history`). By default there is one chart for all the
resources of the code. With `--billing-by-env`, when the code spans several
environments (e.g. `costerra breakdown live --billing-by-env` over
`live/dev` and `live/prod`), there is one chart per environment (`📊 Actual
billing of dev, last 13 weeks`, …), each with the estimate of the projects
of that environment, from the same query. The environment of a project is the `env`
of its `projects:` rule in `costerra.yml`, else the last environment word
(`environments:`, e.g. `dev`, `staging`, `prod`) in its path or name
(`network (prod)` for per-environment tfvars), else in the GCP project ids of
its resources; projects without one are grouped under `other`. With a
single environment, there is one chart.

The data comes from the [Cloud Billing export to BigQuery](https://cloud.google.com/billing/docs/how-to/export-data-bigquery),
**detailed usage cost** export (`gcp_billing_export_resource_v1_…`), which
must be enabled on the billing account: only it names the resource of each
cost (the standard export stops at the project). A cost is counted when its
resource is one of the code:

* by name (`resource.name`): VMs, disks, buckets, Cloud SQL instances, Cloud
  Run services, BigQuery datasets and the other named resources;
* by name prefix for the VMs of managed instance groups (`base_instance_name`);
* by the `goog-k8s-cluster-name` label for the nodes of GKE clusters;
* by the `goog-drz-workstation-uuid` label for the VMs and disks of Cloud
  Workstations, with the `uid` of each workstation read from the state (the
  per-vCPU management fee is not billed to a resource, so it is left out).

Costs without a resource (network egress billed to the project, support…)
and the resources of the projects that are not in the code are left out. The
chart says how many resources of the code were found in the billing and
lists the ones without cost; resources whose name is only known after apply
cannot be looked up and are counted apart.

costerra finds the `gcp_billing_export_resource_v1_<billing account>` table by
itself, even in a separate billing project: it searches the organization of
the projects with Cloud Asset Inventory, then the datasets of the projects you
can see in BigQuery (the quota project, the projects of the code, then the
ones named like `billing`, `finops` or `cost` first). The table found is
remembered for 7 days; a search without result is not repeated for 6 hours
and the warning says where costerra looked and what was refused. You can also
give the table with `--billing-export-table PROJECT.DATASET.TABLE`
(`COSTERRA_BILLING_EXPORT_TABLE`, `billing_export_table:` in `costerra.yml`).
One query sums the cost of the resources of the code per week and service;
it runs in the quota project when one is set, otherwise in the project of the
service account of the credentials (`name@PROJECT.iam.gserviceaccount.com`:
key file, impersonation, workload identity or the metadata server), otherwise
in the project of the table. It runs once: when it fails (access denied,
quota exceeded), no other project is tried, and the warning names the
project it ran in.

The detailed export is large and a query is billed by the days of export it
scans (a few tens of GB for 13 weeks on a big billing account), so costerra
reads as little of it as it can:

* only the partitions (export days) of the weeks it reads are scanned, and
  only the columns it needs (the labels only when the code has GKE clusters);
* the weeks read are cached per week (in the cache directory). A week read
  more than 5 days after its end is final and never read again; the more
  recent ones are read again after an hour. A run reads only from the first
  week that is not cached for all its resources: usually the last week or
  two instead of 13. In CI, keep the cache directory between jobs to benefit
  from it.

A query still running when costerra stops waiting (30 seconds) is billed
anyway: its job is remembered, and the next run reads its result instead of
running the query again.

When the billing cannot be read, a warning says why and the rest of the
report is unchanged.

### Permissions for `--billing-history`

All read-only. What is missing is reported in the warning, and the rest of
the report is unchanged.

**Once, by a billing administrator: enable the detailed export.** In the
console, *Billing* > *Billing export* > *BigQuery export* > **Detailed usage
cost** > *Edit settings*, pick a project and a dataset. This needs Billing
Account Administrator (`roles/billing.admin`) on the billing account and
BigQuery User (`roles/bigquery.user`) on that project. Costs are exported
from that day on (and for the current and previous month of some services):
the chart fills up over time.

**Reading the billing (always needed):**

| What | Role | On |
|---|---|---|
| Read the export table | BigQuery Data Viewer (`roles/bigquery.dataViewer`): `bigquery.tables.get`, `bigquery.tables.getData` | the export dataset (or its project) |
| Run the query | BigQuery Job User (`roles/bigquery.jobUser`): `bigquery.jobs.create`, with the BigQuery API enabled | the project the query runs in: the quota project (`--quota-project`, `quota_project:`, `GOOGLE_CLOUD_QUOTA_PROJECT`, ADC `quota_project_id`), otherwise the project of the service account of the credentials, otherwise the project of the table. The query is billed there, and counts toward its BigQuery quotas (e.g. a custom `QueryUsagePerUserPerDay`). |

**Finding the table (not needed with `--billing-export-table`):**

| What | Role | On |
|---|---|---|
| Name of the table (it ends with the billing account) | Browser (`roles/browser`) or Viewer: `resourcemanager.projects.get` | the projects of the code |
| Organization of the projects | Browser: `resourcemanager.projects.get` (`getAncestry`) | the projects of the code |
| Search the organization (fast path) | Cloud Asset Viewer (`roles/cloudasset.viewer`): `cloudasset.assets.searchAllResources`, with the Cloud Asset API (`cloudasset.googleapis.com`) enabled in the quota project | the organization |
| Search the BigQuery projects (fallback) | BigQuery Metadata Viewer (`roles/bigquery.metadataViewer`): `bigquery.datasets.get`, `bigquery.tables.list` | the project of the export dataset |

For a CI service account, for example:

```sh
SA=costerra@ci-project.iam.gserviceaccount.com
# read the export (dataset-level grant)
bq add-iam-policy-binding --member="serviceAccount:$SA" --role=roles/bigquery.dataViewer billing-project:billing_dataset
# run the query, in the project of the service account (or set a quota project)
gcloud projects add-iam-policy-binding ci-project --member="serviceAccount:$SA" --role=roles/bigquery.jobUser
gcloud services enable bigquery.googleapis.com cloudasset.googleapis.com --project ci-project
# optional, to find the table: or set COSTERRA_BILLING_EXPORT_TABLE instead
gcloud organizations add-iam-policy-binding "$ORG_ID" --member="serviceAccount:$SA" --role=roles/cloudasset.viewer
```

Setting `billing_export_table:` in `costerra.yml` (or
`COSTERRA_BILLING_EXPORT_TABLE`) skips the discovery: only the two roles of
the first table are then needed.

It works with `--format json` (`billing_history` key, the total, and
`billing_history_by_env` with `--billing-by-env`) and `--format markdown`
(a Mermaid bar chart per environment, rendered by GitHub and GitLab, and the
weeks per service). It cannot be used with `--offline`.

## Usage-based recommendations (Google Cloud Recommender)

Besides its own rules (static analysis of the code), costerra shows
recommendations from **Google Cloud Recommender** (Active Assist), based on
observed usage: VM rightsizing, idle VMs, disks, IPs and images, idle and
overprovisioned Cloud SQL, GKE, BigQuery partitioning, and committed use
discounts for the billing account. Only the recommenders and locations matching
resources in the code are queried.

Every recommendation shows its source: a `costerra` or `Google` tag,
and a `Source:` line (the costerra rule ID, which you can disable in
`costerra.yml`; the Google recommender, with a ready-to-run
`gcloud recommender recommendations describe …` command), and what it is based
on: the code, your usage file, or observed usage. The JSON report has `source`,
`source_ref` and `basis` (`code`, `usage_file`, `observed`), and the MR comment
has a Source column.

When Google and a costerra rule give the same advice for a resource, only
Google's is kept, since it is based on observed usage: Google rightsizing
replaces costerra's machine family and rightsizing advice, an idle-resource
recommendation replaces the advice for that VM, disk or IP, and Google
commitment recommendations for the billing account replace costerra's
commitment advice in that region.

Only the recommendations about the code are shown. They are matched to the
Terraform resources they concern (by project, location and name; the VMs of
a managed instance group to the group, a boot disk to its VM), and
recommendations about other resources of the same projects are left out.
Billing-account commitments are shown in a "☁️ From your cloud usage"
section, with 1-year and 3-year options merged, when they concern the code:
resource-based commitments for a region and a machine family of its VMs,
spend-based commitments for a service it uses (Cloud SQL, Cloud Run,
Memorystore, GKE, Compute…). With `--details`, a note says how many
recommendations were left out. Results are cached for 6 hours and fetched in parallel with
pricing (2.5 s deadline), so runs stay instant.

The Recommender API needs a quota project with the API enabled when you use user
credentials. costerra uses `--quota-project` / `quota_project:` /
`GOOGLE_CLOUD_QUOTA_PROJECT`, otherwise the `quota_project_id` of your
Application Default Credentials. Disable with `--no-insights`,
or `gcp_recommendations: false`.

## Getting more accurate recommendations

costerra uses whatever it can read and falls back on the code otherwise; each
recommendation says what it is based on. From least to most accurate:

1. **The code only.** Rules see machine types, disks, regions and settings, and
   assume defaults where the code says nothing: VMs run all month, half of a
   bucket's data is cold, nothing is committed yet. Usage-based costs are left
   as unit prices.
2. **Your environments.** Spot, single-zone and no-HA advice only applies
   outside production. Label resources with `env` / `environment`, or map paths
   and project ids to environments in `costerra.yml` (`environments:`,
   `projects:`), so that production is never treated as dev.
3. **A usage file** (`costerra usage init`): your estimates of hours, storage,
   requests and egress. Commitment advice needs VMs that run at least 90% of the
   month; storage advice uses the stored volume.
4. **Observed usage** ([Cloud Monitoring and commitments](#observed-usage)),
   with Monitoring Viewer and Compute Viewer on the projects. This enables
   advice that needs real data:
   * `compute-idle`: CPU below 3% for at least a week;
   * `compute-rightsize`: halve the vCPUs when the mean CPU is below 20% and the
     95th percentile below 35%;
   * `compute-schedule-nonprod`: a non-production VM that runs around the clock
     can be stopped outside working hours;
   * cheaper machine families are not proposed for VMs whose CPU is saturated
     (95th percentile above 85%);
   * commitment advice leaves out the vCPUs your active commitments already
     cover, and is skipped for VMs that do not run all month;
   * `sql-rightsize`: a Cloud SQL tier with half the vCPUs when CPU use is
     low (memory kept unless it is little used too);
   * `compute-upsize` / `sql-upsize`: more capacity for a VM or Cloud SQL
     instance short of CPU or memory (it costs more);
   * `run-rightsize-cpu` / `run-rightsize-memory` (and `functions-…`): Cloud
     Run and Cloud Functions limits that are little used or saturated;
   * `run-min-instances-unused` / `functions-min-instances-unused`: warm
     instances of a service or function that gets almost no traffic;
   * `gcs-cross-region`: a bucket used by resources of the code in a region its
     location does not serve (a regional bucket read from another region, a
     dual-region outside its pair, a multi-region from another continent):
     reads are billed as inter-region data transfer. Found from the code; with
     observed usage, the bytes the bucket sent give an upper bound of that cost;
   * `bq-physical-storage` / `bq-logical-storage`: the cheaper BigQuery storage
     billing model, from the logical, physical, time travel and fail-safe bytes
     of each dataset (BigQuery Metadata Viewer and Job User on the project, or
     Metadata Viewer on the datasets). With logical bytes only (Monitoring),
     the break-even compression ratio is given instead.
5. **Google Cloud Recommender**, with Recommender Viewer: Google's own
   analysis of the usage (memory included, through the Ops Agent), and
   commitment recommendations for the billing account with Billing Account
   Viewer.
6. **Your contract prices** ([Billing Account Viewer](#your-prices-contract-discounts)):
   savings are computed with your discounts, not list prices.

In CI, give the service account the read-only roles of the
[Permissions](#permissions) section to get all of the above. Without them,
costerra still runs and says which sources were not available.

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
observed_usage: true          # read 30 days of Cloud Monitoring metrics and active commitments
quota_project: my-project     # quota project for the Recommender and Cloud Monitoring APIs
billing_export_table: my-billing.billing.gcp_billing_export_resource_v1_012345_6789AB_CDEF01   # --billing-history (default: discovered)
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
discounts, Cloud Billing Pricing API) require `billing.billingAccountPrices.list`
and `billing.billingAccountPrice.get`, which are included in the Billing Account
Viewer role.

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

## Permissions

**List prices** (Cloud Billing Catalog API) are public: no IAM role is needed,
only credentials. costerra looks for them in this order:

1. an API key (`--api-key` / `COSTERRA_API_KEY`), created in a project where
   the **Cloud Billing API** (`cloudbilling.googleapis.com`) is enabled;
2. Application Default Credentials (`gcloud auth application-default login`,
   a service account, workload identity);
3. `gcloud auth print-access-token`.

The API key is optional: if you are logged in with gcloud, you do not need one.
It is mainly for CI jobs that have no Google identity. Google refuses requests
without any credentials, so you need one of the three.

To create an API key, restricted to the Cloud Billing API (the Catalog API is
free; its quota is charged to the key's project):

```sh
gcloud services enable cloudbilling.googleapis.com --project "$PROJECT"
gcloud services api-keys create --project "$PROJECT" --display-name costerra \
  --api-target service=cloudbilling.googleapis.com
gcloud services api-keys get-key-string <KEY_NAME> --project "$PROJECT"   # KEY_NAME is printed by the previous command
export COSTERRA_API_KEY=…
```

An API key is used before ADC and only gives list prices: leave it unset where
you want contract prices.

**Contract prices** (Cloud Billing Pricing API) need OAuth credentials (2 or 3,
not an API key) and these permissions:

| Step | API call | Permission | Granted by |
|---|---|---|---|
| Detect the billing account of the projects in your code | `projects/{id}/billingInfo` | `resourcemanager.projects.get` on the project | Viewer (`roles/viewer`) or Browser (`roles/browser`) on the project |
| Fall back to the only open billing account you can see | `billingAccounts.list` | `billing.accounts.list` | Billing Account Viewer |
| Read contract prices | `billingAccounts/{id}/skus/-/prices` | `billing.billingAccountPrices.list` | Billing Account Viewer |
| Refresh the contract prices of the SKUs a run uses | `billingAccounts/{id}/skus/{sku}/price` | `billing.billingAccountPrice.get` | Billing Account Viewer |

**Billing Account Viewer** (`roles/billing.viewer`) on the billing account is
what reads the prices. To find the account, costerra also needs Viewer or
Browser on the projects, unless you can see only one billing account or you
set it with `--billing-account`, `COSTERRA_BILLING_ACCOUNT` or
`billing_account:` (detection is then skipped). When a permission is missing, costerra
falls back to list prices; the `Pricing` line under the table says which
prices were used. A refused billing account is not asked again for 6 hours.

Typical setups:

* **Local:** `gcloud auth application-default login`. You get contract prices
  if you are Billing Account Viewer.
* **CI, list prices:** `COSTERRA_API_KEY`.
* **CI, contract prices:** a service account (workload identity federation)
  with `roles/billing.viewer` on the billing account, and
  `COSTERRA_BILLING_ACCOUNT` set.
* **CI, most accurate:** the same service account, also with
  `roles/monitoring.viewer`, `roles/compute.viewer`,
  `roles/recommender.viewer`, `roles/bigquery.metadataViewer` and
  `roles/bigquery.jobUser` on the projects.

**Service account impersonation.** `--impersonate-service-account <email>`
(`COSTERRA_IMPERSONATE_SERVICE_ACCOUNT`, or `impersonate_service_account:` in
`costerra.yml`) makes every OAuth call (contract prices, observed usage,
recommendations, remote states) with that service account's token. Your
credentials need Service Account Token Creator
(`roles/iam.serviceAccountTokenCreator`) on it; an API key is used as is.
Cached observed usage and recommendations are kept per impersonated account.

**Terraform states** (`terraform_remote_state`, and the configuration's own
state): read access to the state. For `gcs`, Storage Object Viewer
(`roles/storage.objectViewer`) on the state buckets; for the other backends,
see [Terraform states](#inputs). A state that cannot be read leaves its
outputs or data sources unknown (a warning with `--details`).

**Observed usage and recommendations** are optional: each one that is missing is
skipped (costerra never fails because of it, and caches the refusal for
6 hours), and a note says what to grant (shown with `--details` or
`--rightsize`).

| Data | API | Role (read-only) | On |
|---|---|---|---|
| [Observed usage](#observed-usage): VM and Cloud SQL CPU, Cloud SQL memory, uptime, storage, requests, BigQuery stored bytes | Cloud Monitoring (`monitoring.googleapis.com`) | Monitoring Viewer (`roles/monitoring.viewer`) | the projects |
| Active commitments | Compute Engine (`compute.googleapis.com`) | Compute Viewer (`roles/compute.viewer`) | the projects |
| BigQuery dataset sizes (logical / physical storage) | BigQuery (`bigquery.googleapis.com`), `jobs.query` on `INFORMATION_SCHEMA.TABLE_STORAGE` | BigQuery Metadata Viewer (`roles/bigquery.metadataViewer`) and BigQuery Job User (`roles/bigquery.jobUser`), or the permissions `bigquery.tables.get`, `bigquery.tables.list` and `bigquery.jobs.create` | the projects of the datasets |
| BigQuery dataset sizes, without project-level access (fallback) | BigQuery tables API (`tables.list`, `tables.get`; no query job) | BigQuery Metadata Viewer (`roles/bigquery.metadataViewer`) | each dataset |
| [Google recommendations](#usage-based-recommendations-google-cloud-recommender) for resources | Recommender (`recommender.googleapis.com`) | Recommender Viewer (`roles/recommender.viewer`) | the projects |
| Google commitment recommendations for the billing account | Recommender | Billing Account Viewer (`roles/billing.viewer`), as for contract prices | the billing account |
| [Actual billing](#actual-billing---billing-history) (`--billing-history` only) | BigQuery (`bigquery.googleapis.com`), `jobs.query` on the detailed Cloud Billing export table | BigQuery Data Viewer (`roles/bigquery.dataViewer`) on the export dataset, and BigQuery Job User (`roles/bigquery.jobUser`) on the project the query runs in; finding the table: Cloud Asset Viewer (`roles/cloudasset.viewer`) on the organization, or BigQuery Metadata Viewer on the export dataset. Details: [Permissions for `--billing-history`](#permissions-for---billing-history) | the export dataset |

With user credentials, the Recommender and Cloud Monitoring APIs need a quota
project where they are enabled: `--quota-project`, `quota_project:`,
`GOOGLE_CLOUD_QUOTA_PROJECT`, or the `quota_project_id` of your Application
Default Credentials.

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
mise install              # Rust 1.98 (clippy, rustfmt), just, jq, hyperfine, samply, terraform, terragrunt, osv-scanner, trivy
just                      # list recipes
just check                # rustfmt --check, clippy (-D warnings), tests: what CI runs
just audit                # osv-scanner (dependencies) + trivy (vulnerabilities, secrets, misconfigurations)
just fixtures             # breakdown of all test fixtures
just breakdown ../infra   # breakdown of any path (extra args are passed through)
just diff . origin/main   # cost diff against a git ref
just sync-prices [BILLING_ACCOUNT]
just sku '^N2 Instance Core' 'Compute Engine' europe-west1
just bench                # hyperfine benchmark of a cached breakdown: time and peak memory
just profile [PATH]       # CPU profile with samply (mise.toml), opened in the Firefox Profiler
just mem [PATH]           # peak memory of a breakdown (profiling build, with symbols)
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
  ref, policies, config and usage file errors…);
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
is an independent project, not affiliated with or endorsed by Google.
