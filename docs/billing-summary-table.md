# Keeping `--billing-history` cheap

`--billing-history` reads the detailed Cloud Billing export in BigQuery. On a
large billing account the export holds terabytes, and BigQuery bills every
query on demand by the bytes it scans. This page explains what a run costs,
what to set up so that it stays cheap, and the setup that costs the least: a
small summary table, read with `--billing-summary-table` (see
[Turning it on](#turning-it-on)).

All table names below are placeholders: replace `BILLING_PROJECT`,
`billing_export` and `gcp_billing_export_resource_v1_XXXXXX_XXXXXX_XXXXXX` with
your billing project, the export dataset and the export table.

## What a run costs

The detailed export (`gcp_billing_export_resource_v1_…`) is partitioned by
export day (`_PARTITIONTIME`, ingestion time). It is not clustered, and cannot
usefully be, because the columns costerra filters on are nested. A query is
billed on **the columns it reads × the partitions it scans**.

costerra reads `usage_start_time`, `project.id`, `resource.name`,
`service.description`, `cost`, `credits.amount` and `currency`. When the code
has GKE clusters it also reads `labels`, to match nodes by the
`goog-k8s-cluster-name` label. `labels` is by far the heaviest column. It
typically accounts for around three quarters of the bytes, so a query with
GKE clusters costs several times more than one without.

As an order of magnitude, on an export of a few terabytes:

| Run | Scanned |
|---|---|
| 13 weeks, no GKE cluster in the code | tens of GB |
| 13 weeks, with GKE clusters | a few hundred GB |
| The last 1–2 weeks only (cache warm) | a few GB, ~5× more with GKE |

At on-demand prices (USD 6.25 per TiB at the time of writing, with the first
TiB of each month free), a cold 13-week run with GKE clusters costs about a
dollar. That is cheap once but adds up when every CI job pays it.

## What costerra already does

- **Partition pruning:** only the export days of the weeks being read are
  scanned (`_PARTITIONTIME >= start - 1 day`).
- **Column pruning:** `labels` is read only when the code has GKE clusters.
- **Per-week cache:** a week read more than 5 days after its end is final and
  never queried again. More recent weeks are re-read after an hour. A run only
  queries from the first week that is not cached for all its resources,
  usually the last one or two weeks instead of 13.
- **One attempt:** a failed query is never retried in another project, since
  each try is billed.

## Recommended setup with the raw export

1. **Keep the cache directory between CI jobs.** Point `COSTERRA_CACHE_DIR` to
   a directory inside the job workspace and cache it. Use the same cache key on
   every branch, so feature branches reuse the weeks read on the default
   branch. A job without the cache pays for the full 13 weeks.
2. **Run the query in a dedicated project with a daily cap.** Set
   `--quota-project` (`GOOGLE_CLOUD_QUOTA_PROJECT`) to a project meant for it.
   On that project, set the BigQuery custom quotas *Query usage per day* and
   *Query usage per user per day*. A run that would exceed them fails with a
   warning instead of costing more, and the rest of the report is unchanged.
3. **Run it where it is read.** Enable `--billing-history` on merge request
   pipelines, where the chart ends up in the comment, rather than on every
   push.
4. **One chart per environment costs nothing more.** `--billing-by-env` splits
   the rows of the same query.

## Cheapest setup: a summary table

The export has one row per SKU, per hour, per resource. The chart needs much
less: one row per export day, usage day, project, resource, GKE cluster,
Cloud Workstation, service and currency, with the cost net of credits. A small table
pre-aggregated once a day by a scheduled query makes each costerra run scan
megabytes instead of gigabytes, and moves the cost of `labels` from every run
to one refresh a day.

### Schema

| column | type | content |
|---|---|---|
| `export_day` | `DATE`, required | `DATE(_PARTITIONTIME)` of the export rows (the partition column) |
| `usage_day` | `DATE` | `DATE(usage_start_time)` |
| `project` | `STRING` | `project.id` |
| `resource` | `STRING` | `resource.name` |
| `cluster` | `STRING` | value of the `goog-k8s-cluster-name` label (GKE nodes) |
| `workstation` | `STRING` | value of the `goog-drz-workstation-uuid` label (Cloud Workstations) |
| `service` | `STRING` | `service.description` |
| `currency` | `STRING` | `currency` |
| `net` | `FLOAT` | `SUM(cost) + SUM(credits.amount)` |

Partitioned by `export_day` (120-day expiration), clustered by `project,
resource`. Export rows with no resource name and neither label are left out.

The export is only ever **read**. Do not alter, cluster or re-partition the
table Google writes to.

### What does not help

- **A logical view:** BigQuery expands it into the underlying query, so the
  same bytes are scanned.
- **Clustering the export:** clustering only applies to top-level columns, and
  `project.id`, `resource.name` and `labels` are nested. It would also change
  a table that Google manages.
- **A materialized view:** incremental refresh is limited with
  `UNNEST(labels)`, and a non-incremental one rescans the export on every
  refresh, with little control over when that happens.

`costerra billing summary-sql --export-table PROJECT.DATASET.TABLE --table
PROJECT.DATASET.TABLE [--days 120]` prints the SQL below for your tables.

### Creating the table

Create a dataset (here `costerra`) in the **same location** as the export
dataset, then:

```sql
CREATE TABLE `BILLING_PROJECT.costerra.daily_costs` (
  export_day DATE NOT NULL,
  usage_day DATE,
  project STRING,
  resource STRING,
  cluster STRING,
  workstation STRING,
  service STRING,
  currency STRING,
  net FLOAT64
)
PARTITION BY export_day
CLUSTER BY project, resource
OPTIONS (partition_expiration_days = 120);
```

### One-time backfill

```sql
INSERT INTO `BILLING_PROJECT.costerra.daily_costs`
SELECT DATE(_PARTITIONTIME) AS export_day,
       DATE(usage_start_time) AS usage_day,
       project.id AS project,
       resource.name AS resource,
       (SELECT l.value FROM UNNEST(labels) l WHERE l.key = 'goog-k8s-cluster-name' LIMIT 1) AS cluster,
       (SELECT l.value FROM UNNEST(labels) l WHERE l.key = 'goog-drz-workstation-uuid' LIMIT 1) AS workstation,
       service.description AS service,
       currency,
       SUM(cost) + SUM(IFNULL((SELECT SUM(c.amount) FROM UNNEST(credits) c), 0)) AS net
FROM `BILLING_PROJECT.billing_export.gcp_billing_export_resource_v1_XXXXXX_XXXXXX_XXXXXX`
WHERE DATE(_PARTITIONTIME) >= DATE_SUB(CURRENT_DATE(), INTERVAL 120 DAY)
  AND (resource.name IS NOT NULL
       OR EXISTS(SELECT 1 FROM UNNEST(labels) l WHERE l.key IN ('goog-k8s-cluster-name', 'goog-drz-workstation-uuid')))
GROUP BY 1, 2, 3, 4, 5, 6, 7, 8;
```

### Daily scheduled query

Each run replaces the last export days (from `@run_date` minus 2 days).
Rerunning it is harmless, and rows that the export writes late into those
partitions are picked up.

```sql
DELETE FROM `BILLING_PROJECT.costerra.daily_costs`
WHERE export_day >= DATE_SUB(@run_date, INTERVAL 2 DAY);

INSERT INTO `BILLING_PROJECT.costerra.daily_costs`
SELECT DATE(_PARTITIONTIME) AS export_day,
       DATE(usage_start_time) AS usage_day,
       project.id AS project,
       resource.name AS resource,
       (SELECT l.value FROM UNNEST(labels) l WHERE l.key = 'goog-k8s-cluster-name' LIMIT 1) AS cluster,
       (SELECT l.value FROM UNNEST(labels) l WHERE l.key = 'goog-drz-workstation-uuid' LIMIT 1) AS workstation,
       service.description AS service,
       currency,
       SUM(cost) + SUM(IFNULL((SELECT SUM(c.amount) FROM UNNEST(credits) c), 0)) AS net
FROM `BILLING_PROJECT.billing_export.gcp_billing_export_resource_v1_XXXXXX_XXXXXX_XXXXXX`
WHERE DATE(_PARTITIONTIME) >= DATE_SUB(@run_date, INTERVAL 2 DAY)
  AND (resource.name IS NOT NULL
       OR EXISTS(SELECT 1 FROM UNNEST(labels) l WHERE l.key IN ('goog-k8s-cluster-name', 'goog-drz-workstation-uuid')))
GROUP BY 1, 2, 3, 4, 5, 6, 7, 8;
```

### Why the chart stays the same

- **Nothing is counted twice:** the table is keyed by export day, and every
  export row belongs to exactly one `_PARTITIONTIME` partition. Costs exported
  late land in a later export day and are still counted.
- **Same weekly buckets:** `usage_day` keeps the weekly bucketing costerra does
  today.
- **Nothing costerra could use is dropped:** rows with neither a resource name
  nor a cluster or workstation label are left out, and costerra never matches
  those to a resource of the code.
- **Same matching:** costerra runs the query it runs on the export, on the
  summary columns: by `project/resource` name, by name prefix for the VMs of
  managed instance groups, by `project/cluster` for GKE nodes and by
  `project/workstation` for Cloud Workstations.

### Cost, compared

| | Raw export | Summary table |
|---|---|---|
| Backfill (~120 days) | — | one cold 13-week run, once |
| Daily refresh (2 export days) | — | a few GB a day |
| One costerra run, 13 weeks | tens to hundreds of GB | well under 1 GB |
| One costerra run, last 1–2 weeks | a few GB or more | megabytes |

The refresh costs about as much as a couple of warm costerra runs a day. It
pays for itself as soon as more than a few runs a day read the billing, and
from the first run when the CI cache is not kept.

### Permissions

- Enable the BigQuery Data Transfer API (scheduled queries) in the project
  running the refresh.
- Give the scheduled query a service account with:
  - BigQuery Data Viewer on the export dataset (read only);
  - BigQuery Data Editor on the `costerra` dataset;
  - BigQuery Job User on the project it runs in, which is billed for the
    refresh.
- Users and CI then only need BigQuery Data Viewer on the `costerra` dataset,
  not on the raw export, which holds the costs of the whole billing account,
  plus BigQuery Job User on the project their query runs in.
- Keep at least 13 weeks of history plus a margin. The 120-day partition
  expiration keeps the table bounded.

## Turning it on

Once the table is created, backfilled and refreshed daily, give it to
costerra with one of (the first one set wins):

- `--billing-summary-table PROJECT.DATASET.TABLE` on `breakdown` or `diff`;
- the `COSTERRA_BILLING_SUMMARY_TABLE` environment variable (for example a CI
  variable: see `ci/gitlab-ci.yml` and `ci/github-actions.yml`);
- `billing_summary_table: PROJECT.DATASET.TABLE` in `costerra.yml`.

It does not turn `--billing-history` on by itself:

```sh
COSTERRA_BILLING_SUMMARY_TABLE=BILLING_PROJECT.costerra.daily_costs \
  costerra breakdown live --billing-history
```

The chart then says `from the billing summary table BILLING_PROJECT.costerra.daily_costs`
(and `"source": "summary_table"` in the JSON). The export is not discovered
nor read, and `--billing-export-table` is ignored. The per-week cache, the
project the query runs in, the single attempt and `--billing-by-env` work as
with the export; the cache of a summary table is kept apart from the cache of
the export. When the table cannot be read (not found, permission denied,
quota exceeded), the warning names the table and the project the query ran
in, and costerra does not fall back to the export: each query is billed.

## Open questions

- Compare matching GKE nodes by label with matching them by node name prefix
  on one real day. If prefixes are enough, even the refresh can stop reading
  `labels`.
- A `--billing-refresh` flag to bypass the per-week cache.
