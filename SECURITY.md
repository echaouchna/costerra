# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately, never in a public issue: use GitHub's
private vulnerability reporting ([*Security* tab → *Report a vulnerability*](https://github.com/echaouchna/costerra/security/advisories/new)).

Do not disclose the issue publicly until a fix is released.

Include the affected version (`costerra --version`), a description of the
impact, and steps to reproduce. You should get a first answer within a week.

## Supported versions

Security fixes are made on the latest release and the default branch.

## Scope and design notes

costerra reads Terraform / Terragrunt code and talks to a few APIs:

* **Google Cloud** (Cloud Billing Catalog and Pricing APIs, Cloud Billing,
  Recommender) with your Application Default Credentials or `COSTERRA_API_KEY`.
  Access is read-only.
* **Rabbit** (followrabbit.ai), only when a Rabbit API key is configured. The
  saved-recommendation lookup sends the repository URL (credentials removed).
  The opt-in AI review (`--rabbit-review`) uploads Terraform files: git-ignored
  and excluded paths are skipped and secret-looking values are redacted;
  `--rabbit-review-dry-run` shows what would be sent.
* **Module sources** (Terraform registry, git, archives) are downloaded into
  the cache directory unless `--offline` is used.
* Terragrunt `run_cmd()` is **disabled** unless `--allow-run-cmd` is passed:
  only enable it for code you trust.

Caches (`~/.cache/costerra`) contain prices, detected billing account ids and
recommendations, not credentials.
