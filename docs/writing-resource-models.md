# Writing resource cost models

Every priced Terraform resource type has a **cost model**: a function that turns
the resource's evaluated attributes (plus optional usage estimates) into priced
cost components. Models live in `src/resources/<service-group>.rs` and are
registered through `specs()`.

## Anatomy

```rust
use super::{Ctx, Spec, usage};
use crate::cost::{Component, CostedResource, Recommendation, Severity};
use crate::pricing::query::Query;

const SQL: &str = "Cloud SQL"; // Billing Catalog service display name

pub fn specs() -> Vec<Spec> {
    vec![
        Spec::new("google_sql_database_instance", sql_instance)
            .usage(usage!("backup_storage_gb" => "Backup storage in GiB"))
            .rules(sql_rules),
    ]
}

fn sql_instance(ctx: &Ctx) -> Vec<Component> {
    let region = ctx.region();
    let tier = ctx.str_or("settings.0.tier", "db-f1-micro");
    vec![
        ctx.hourly("vCPU", 2.0, Query::new(SQL, &region, "^Cloud SQL for PostgreSQL: Zonal - vCPU in").ondemand()),
        ctx.usage("Backups", "GiB-months", "backup_storage_gb", Query::new(SQL, &region, "...")),
    ]
}
```

## Attributes (`Ctx`)

Attributes follow the Terraform schema. Nested blocks are **lists of maps**
(like `terraform show -json`), so paths use indexes: `settings.0.tier`.

| Helper | Meaning |
|---|---|
| `ctx.str("a.0.b")`, `ctx.num(..)`, `ctx.bool(..)` | Known, non-null values (None if unset/unknown) |
| `ctx.str_or / num_or / bool_or` | With a default (use the provider's documented default) |
| `ctx.blocks("node_config")` / `ctx.block(..)` | Nested blocks |
| `ctx.is_unknown(path)` | Value is computed / not known statically |
| `ctx.labels()` | `labels` / `resource_labels` / `user_labels` |
| `ctx.region()` / `ctx.zone()` | Resolved location (attributes → provider defaults → config default) |
| `ctx.env` | `Env::Prod`, `Env::NonProd` or `Env::Unknown` (from labels, paths, config) |
| `ctx.siblings` | All resources of the project (for cross-resource lookups) |
| `ctx.resolve_ref(value, "google_x")` | Resolve an id/self_link/name reference to another resource |
| `ctx.usage_num("key")` | Usage-file value for this resource |
| `ctx.warn(msg)` | Attach a warning shown to the user (e.g. assumed defaults) |

References to computed attributes of other resources (e.g.
`google_compute_instance_template.x.id`) evaluate to a synthetic id
`costerra::<address>`, which `resolve_ref` understands. `r.refs` lists the
`type.name` of every resource referenced by a resource's configuration, which
is handy for "is this used?" rules.

## Pricing (`Query`)

Prices come from the Cloud Billing Catalog. A `Query` selects one SKU:

```rust
Query::new(SERVICE_DISPLAY_NAME, region, "^Regex on SKU description")
    .ondemand()             // category.usageType == "OnDemand"  (.usage("Preemptible"), "Commit1Yr" …)
    .group("SSD")           // category.resourceGroup
    .family("Storage")      // category.resourceFamily
    .exclude("Regional")    // negative regex
```

* The regex is **case-insensitive** and matched against `description`.
* SKUs are indexed by `serviceRegions`. If no SKU is found in the region, the
  lookup falls back to `global`. Pass `"global"` for global SKUs, and multi-region
  codes (`"us"`, `"europe"`, `"asia"`, dual regions like `"eur4"`) where applicable.
* Among several matches the most specific SKU wins (fewest regions, then the
  shortest description), so make regexes precise. Anchor them with `^` and
  exclude look-alikes.
* Tiered rates are applied automatically. Free tiers (a 0-priced first tier)
  are removed by default because they apply per billing account.
* Quantities must be expressed in the **SKU's pricing unit** (`h`, `GiBy.mo`,
  `GiBy`, `count`, `mo`, …). Check with the explorer below.

Component builders:

| Builder | Quantity |
|---|---|
| `ctx.hourly(name, count, q)` | `count × 730` hours (honours `monthly_hrs` usage) |
| `ctx.hourly_unit(name, unit, count, q)` | same, custom unit label (`vCPU-hours`, `GiB-hours`) |
| `ctx.monthly(name, unit, qty, q)` | fixed monthly quantity from configuration |
| `ctx.usage(name, unit, usage_key, q)` | usage-based: quantity from the usage file (None → shown as "depends on usage") |
| `ctx.usage_qty(name, unit, Option<qty>, q)` | usage-based with a computed quantity |
| `ctx.fixed(name, unit, qty, unit_price)` | computed unit price (combine several SKUs) |
| `ctx.price(q)` | raw `Price` (`unit_price()`, `cost(qty)`, `tiers`) |

Name components clearly and include the key configuration in parentheses,
e.g. `Instance (db-custom-4-16384, zonal)` or `Storage (SSD, 100 GiB)`. Set
`component.note` for important context (e.g. "includes 20% SUD").

## Exploring SKUs

```sh
costerra pricing search '^Cloud SQL for PostgreSQL' --service 'Cloud SQL' --region europe-west1
costerra pricing search 'Redis' --region us-central1 --limit 100
```

Raw API dumps (one JSON SKU per line) are handy for `jq` exploration too.

## Recommendations (`rules`)

`fn rules(ctx: &Ctx, cr: &CostedResource) -> Vec<Recommendation>` receives the
priced resource. Quantify savings by repricing a modified configuration:

```rust
let new = ctx.reprice(|a| crate::resources::set_path(a, "settings.0.availability_type", "ZONAL".into()));
if let Some(s) = ctx.savings_vs(cr.monthly_cost, new) {
    out.push(ctx.rec("sql-ha-nonprod", Severity::Medium, "Title", "Why + how + caveats", Some(s)));
}
```

Guidelines:
* IDs are kebab-case and prefixed by service (`sql-`, `gke-`, `gcs-`, …).
* Severity: `High` = clear waste, `Medium` = significant and usually safe,
  `Low` = worthwhile but needs validation, `Info` = awareness / commitments.
* Only flag non-production-only optimizations (HA, Spot, …) when
  `ctx.env == Env::NonProd`.
* The detail must say *why*, *what to change* (Terraform attribute) and *caveats*.
* Don't emit a recommendation that saves less than ~1 currency unit, unless it is
  about risk (e.g. unbounded autoscaling).

## Free resources

Types that cost nothing on their own are listed in `FREE` in
`src/resources/mod.rs`, so that they are reported as free instead of
unsupported.

## Testing

Add a fixture under `testdata/services/<group>/main.tf` and run:

```sh
cargo build --release && ./target/release/costerra breakdown testdata/services/<group> --show-all
```

Cross-check a few numbers against the public pricing pages / GCP calculator.
Add unit tests for pure helpers (tier parsing, shape parsing, …).

Fixtures under `testdata/services/` must be fully priced by the live catalog
(`tests/fixtures.rs`, run with `just test` after `just sync-prices`). To lock
exact costs in, add the fixture to `FIXTURES` in `tests/golden.rs`, run
`just golden-catalog` (the committed price list then also contains its SKUs),
and assert the expected amounts there.
