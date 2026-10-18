# Contributing to costerra

Thanks for your interest! Bug reports, cost model fixes, new resource types
and recommendation rules are all welcome.

## Reporting issues

* **Wrong or missing price**: include the resource block (anonymized), the
  region, the price costerra shows and the expected one (Google pricing page or
  calculator link). `costerra breakdown <path> --show-all -f json` shows the SKU
  each component was priced with.
* **Evaluation problem** (a resource missing, an unknown `count`): include a
  minimal Terraform / Terragrunt snippet and the warning printed with
  `--show-all`.
* **Security issues**: do not open a public issue, see [SECURITY.md](SECURITY.md).

Never paste real credentials, billing account ids or internal project names in
issues: replace them with placeholders.

## Development setup

```sh
mise install     # pinned toolchain: Rust, just, terraform, osv-scanner, trivy, cargo-deny…
                 # (or any Rust toolchain >= rust-version; see Cargo.toml)
just             # list recipes
just check       # what CI runs: rustfmt, clippy (-D warnings), tests
just audit       # cargo-deny, osv-scanner, trivy
```

The minimum supported Rust version is in `Cargo.toml` (`rust-version`).

## Tests

* `just test` runs unit, golden, CLI and fixture tests. Golden and CLI tests
  use the committed price list `testdata/pricing/golden-catalog.json` and need
  no network.
* Fixture tests price `testdata/` with a live catalog (`just sync-prices`,
  needs Google Cloud credentials or `COSTERRA_API_KEY`); without one they are
  skipped locally.
* Adding a resource type or a rule: follow
  [docs/writing-resource-models.md](docs/writing-resource-models.md), add a
  fixture, and lock its costs in `tests/golden.rs` (`just golden-catalog`).

## Pull / merge requests

* Keep changes focused; one topic per pull / merge request.
* `just check` and `just audit` must pass.
* Add or update tests for behaviour changes, and a line in `CHANGELOG.md`
  under *Unreleased*.
* New dependencies must use a licence allowed in `deny.toml`.
* By contributing, you agree that your contributions are licensed under the
  Apache License 2.0, like the project (see [LICENSE](LICENSE)).
