# Changelog

All notable changes to costerra are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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

[Unreleased]: https://github.com/echaouchna/costerra/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/echaouchna/costerra/releases/tag/v0.1.0
